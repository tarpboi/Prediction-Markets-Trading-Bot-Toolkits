//! High-level execution surface: glue between metadata → eligibility →
//! sizing → exposure → risk → signing → POST → position recording.
//!
//! Every bot routes its decided trades through [`OrderExecutor::execute`].
//! Safety flags are checked here, so individual bots never accidentally
//! bypass `enable_trading` or `mock_trading`.

use std::sync::Arc;

use crate::config::AppConfig;
use crate::models::{OrderType, PlannedOrder, Side, WhaleTrade};
use crate::service::eligibility::{self, Eligibility};
use crate::service::market_cache::MarketInfo;
use crate::service::position_store::{OpenPosition, PositionStore};
use crate::service::risk_guard::{BlockReason, RiskCheck, RiskGuard};
use crate::service::strategy;
use crate::service::venue::{MarketSource, OrderTicket, OrderVenue};
use crate::utils;
use anyhow::Result;
use chrono::Utc;
use tracing::{info, warn};

pub struct OrderExecutor {
    cfg: AppConfig,
    venue: Option<Arc<dyn OrderVenue>>,
    risk: Arc<RiskGuard>,
    markets: Arc<dyn MarketSource>,
    positions: Arc<PositionStore>,
}

#[derive(Debug, Clone)]
pub enum ExecutionOutcome {
    Skipped(SkipReason),
    DryRun(OrderTicket),
    Submitted {
        order_id: Option<String>,
        ticket: OrderTicket,
    },
}

#[derive(Debug, Clone)]
pub enum SkipReason {
    BelowSizing,
    MarketMetadataUnavailable,
    Ineligible(Eligibility),
    ExposureCategoryCap { category: String, cap: f64, current: f64, want: f64 },
    ExposureTagCap { tag: String, cap: f64, current: f64, want: f64 },
    RiskBlocked(BlockReason),
    TradingDisabled,
    AlreadyOpen,
    /// Whale closed/reduced their position. We delegate exits to the TP/SL
    /// monitor rather than mirroring the unwind, so this is a no-op by design.
    WhaleExitIgnored,
}

impl OrderExecutor {
    pub fn new(
        cfg: AppConfig,
        venue: Result<Arc<dyn OrderVenue>>,
        risk: Arc<RiskGuard>,
        markets: Arc<dyn MarketSource>,
        positions: Arc<PositionStore>,
    ) -> Result<Self> {
        let venue = match venue {
            Ok(v) => Some(v),
            Err(e) if cfg.bot.mock_trading || !cfg.bot.enable_trading => {
                warn!(error = ?e, "venue client not initialised; dry-run only");
                None
            }
            Err(e) => return Err(e),
        };
        Ok(Self {
            cfg,
            venue,
            risk,
            markets,
            positions,
        })
    }

    /// Return the underlying venue client (for the position monitor's exit path).
    pub fn venue(&self) -> Option<Arc<dyn OrderVenue>> {
        self.venue.as_ref().cloned()
    }

    pub fn positions(&self) -> Arc<PositionStore> {
        Arc::clone(&self.positions)
    }

    pub async fn execute(&self, trade: &WhaleTrade) -> Result<ExecutionOutcome> {
        // 0a. Whale sells are not mirrored — TP/SL is the configured exit path.
        if trade.side == Side::Sell {
            return Ok(ExecutionOutcome::Skipped(SkipReason::WhaleExitIgnored));
        }
        // 0b. Don't pyramid into a market we're already long.
        if self.positions.get(&trade.token_id).is_some() {
            return Ok(ExecutionOutcome::Skipped(SkipReason::AlreadyOpen));
        }

        // 1. Resolve market metadata for category/tags/closed lookup.
        let market = match self.markets.by_token_id(&trade.token_id).await {
            Ok(m) => m,
            Err(e) => {
                warn!(token = %trade.token_id, error = ?e, "market metadata lookup failed");
                return Ok(ExecutionOutcome::Skipped(
                    SkipReason::MarketMetadataUnavailable,
                ));
            }
        };

        // 2. Eligibility (allowlist / blocklist / closed).
        let eligibility = eligibility::check(&self.cfg.filters, &market);
        if eligibility != Eligibility::Allowed {
            info!(
                slug = %market.slug,
                category = ?market.category,
                ?eligibility,
                "market not eligible"
            );
            return Ok(ExecutionOutcome::Skipped(SkipReason::Ineligible(
                eligibility,
            )));
        }

        // 3. Sizing.
        let sizing = strategy::size_for_trade(&self.cfg.strategy, trade);
        if let Some(r) = sizing.skipped {
            info!(?r, "sizing skipped trade");
            return Ok(ExecutionOutcome::Skipped(SkipReason::BelowSizing));
        }

        // 4. Per-category / per-tag exposure caps (entries only).
        if trade.side == Side::Buy {
            if let Some(skip) = self.check_exposure(&market, sizing.copy_usd) {
                info!(?skip, "exposure cap blocked entry");
                return Ok(ExecutionOutcome::Skipped(skip));
            }
        }

        // 5. Risk guard.
        match self.risk.check_fast(trade) {
            RiskCheck::Allow => {}
            RiskCheck::FetchBook => {
                // Caller (or a follow-up) should pull the book and call
                // `risk.check_with_book`. For now we proceed but flag the
                // pre-trade book fetch as the next integration point.
            }
            RiskCheck::Block(reason) => {
                warn!(?reason, "risk guard blocked trade");
                return Ok(ExecutionOutcome::Skipped(SkipReason::RiskBlocked(reason)));
            }
        }

        // 6. Build the order.
        let limit_price = limit_price_for(trade, &self.cfg);
        let shares = utils::usd_to_shares(sizing.copy_usd, limit_price);
        let order_type = order_type_for(trade.side);
        let planned = PlannedOrder {
            venue: trade.venue,
            token_id: trade.token_id.clone(),
            side: trade.side,
            shares,
            limit_price,
            usd_notional: sizing.copy_usd,
            order_type,
            source_trade_hash: trade.tx_hash.clone(),
        };

        let Some(venue) = self.venue.as_ref() else {
            return Ok(ExecutionOutcome::Skipped(SkipReason::TradingDisabled));
        };

        // 7. Sign, and dispatch unless this is a dry run.
        let live = self.cfg.live_trading_allowed();
        let ticket = venue
            .place(&planned, self.cfg.trading.order_expiration_secs, live)
            .await?;

        // 8. Record.
        if !live {
            info!(
                token = %planned.token_id,
                side = ?planned.side,
                shares = planned.shares,
                price = planned.limit_price,
                "dry-run: signed order built but not submitted"
            );
            // Even in dry-run, record the position so the monitor exercises
            // the TP/SL path against real midprices.
            self.record_open(&market, &planned);
            return Ok(ExecutionOutcome::DryRun(ticket));
        }

        // Track what actually filled, where the venue says. An order that
        // filled nothing must not count against exposure or arm TP/SL.
        match ticket.filled_shares {
            Some(filled) if filled <= 0.0 => {
                warn!(token = %planned.token_id, "order accepted but nothing filled");
            }
            Some(filled) => {
                let actual = PlannedOrder {
                    shares: filled,
                    usd_notional: filled * planned.limit_price,
                    ..planned.clone()
                };
                self.record_open(&market, &actual);
            }
            None => self.record_open(&market, &planned),
        }
        Ok(ExecutionOutcome::Submitted {
            order_id: ticket.order_id.clone(),
            ticket,
        })
    }

    fn check_exposure(&self, market: &MarketInfo, want_usd: f64) -> Option<SkipReason> {
        let filters = &self.cfg.filters;
        if let Some(cat) = market.category.as_ref() {
            if let Some(&cap) = filters
                .per_category_max_open_usd
                .iter()
                .find(|(k, _)| ci_eq(k, cat))
                .map(|(_, v)| v)
            {
                let current = self.positions.open_usd_by_category(cat);
                if current + want_usd > cap {
                    return Some(SkipReason::ExposureCategoryCap {
                        category: cat.clone(),
                        cap,
                        current,
                        want: want_usd,
                    });
                }
            }
        }
        for tag in &market.tags {
            if let Some(&cap) = filters
                .per_tag_max_open_usd
                .iter()
                .find(|(k, _)| ci_eq(k, tag))
                .map(|(_, v)| v)
            {
                let current = self.positions.open_usd_by_tag(tag);
                if current + want_usd > cap {
                    return Some(SkipReason::ExposureTagCap {
                        tag: tag.clone(),
                        cap,
                        current,
                        want: want_usd,
                    });
                }
            }
        }
        None
    }

    fn record_open(&self, market: &MarketInfo, planned: &PlannedOrder) {
        // Only entries open positions; exits remove them in the monitor.
        if planned.side != Side::Buy {
            return;
        }
        let (tp_pct, sl_pct) = self.tp_sl_for(market);
        let pos = OpenPosition {
            token_id: planned.token_id.clone(),
            slug: market.slug.clone(),
            category: market.category.clone(),
            tags: market.tags.clone(),
            side: planned.side,
            entry_price: planned.limit_price,
            shares: planned.shares,
            usd_notional: planned.usd_notional,
            take_profit_pct: tp_pct,
            stop_loss_pct: sl_pct,
            opened_at: Utc::now(),
        };
        self.positions.open(pos);
    }

    fn tp_sl_for(&self, market: &MarketInfo) -> (f64, f64) {
        let tp_default = self.cfg.tp_sl.default_take_profit_pct;
        let sl_default = self.cfg.tp_sl.default_stop_loss_pct;
        if let Some(cat) = market.category.as_ref() {
            let tp = self
                .cfg
                .tp_sl
                .per_category_tp_pct
                .iter()
                .find(|(k, _)| ci_eq(k, cat))
                .map(|(_, v)| *v)
                .unwrap_or(tp_default);
            let sl = self
                .cfg
                .tp_sl
                .per_category_sl_pct
                .iter()
                .find(|(k, _)| ci_eq(k, cat))
                .map(|(_, v)| *v)
                .unwrap_or(sl_default);
            (tp, sl)
        } else {
            (tp_default, sl_default)
        }
    }
}

fn limit_price_for(trade: &WhaleTrade, cfg: &AppConfig) -> f64 {
    let buf = cfg.trading.price_buffer;
    let raw = match trade.side {
        Side::Buy => trade.price + buf,
        Side::Sell => trade.price - buf,
    };
    utils::clamp_price(raw)
}

fn order_type_for(side: Side) -> OrderType {
    match side {
        Side::Buy => OrderType::Fak,
        Side::Sell => OrderType::Gtd,
    }
}

fn ci_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.chars().zip(b.chars()).all(|(x, y)| {
        x.to_ascii_lowercase() == y.to_ascii_lowercase()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::VenueId;
    use async_trait::async_trait;

    struct FakeMarkets;

    #[async_trait]
    impl MarketSource for FakeMarkets {
        async fn by_token_id(&self, token_id: &str) -> Result<MarketInfo> {
            Ok(MarketInfo {
                slug: "s".into(),
                question: "q".into(),
                yes_token_id: token_id.into(),
                no_token_id: "other".into(),
                category: None,
                tags: vec![],
                closed: false,
            })
        }
    }

    struct FakeVenue(Option<f64>);

    #[async_trait]
    impl OrderVenue for FakeVenue {
        fn id(&self) -> VenueId {
            VenueId::PredictFun
        }
        async fn place(&self, p: &PlannedOrder, _: u64, _: bool) -> Result<OrderTicket> {
            Ok(OrderTicket {
                token_id: p.token_id.clone(),
                maker_amount: "1".into(),
                taker_amount: "1".into(),
                order_id: Some("1".into()),
                filled_shares: self.0,
            })
        }
    }

    /// Run one live buy through the pipeline; returns the shares recorded.
    async fn recorded_shares(filled: Option<f64>) -> Option<f64> {
        let mut cfg: AppConfig = serde_json::from_str(include_str!("../../config.json")).unwrap();
        cfg.filters = Default::default();
        cfg.bot.enable_trading = true;
        cfg.bot.mock_trading = false;
        let positions = PositionStore::new();
        let risk = RiskGuard::new(cfg.risk.clone());
        let executor = OrderExecutor::new(
            cfg,
            Ok(Arc::new(FakeVenue(filled))),
            risk,
            Arc::new(FakeMarkets),
            Arc::clone(&positions),
        )
        .unwrap();
        let trade = WhaleTrade {
            venue: VenueId::PredictFun,
            maker: "0xwhale".into(),
            side: Side::Buy,
            token_id: "t".into(),
            shares: 500.0,
            price: 0.5,
            usd_notional: 250.0,
            tx_hash: None,
            block_number: None,
            observed_at: Utc::now(),
        };
        let outcome = executor.execute(&trade).await.unwrap();
        assert!(matches!(outcome, ExecutionOutcome::Submitted { .. }));
        positions.get("t").map(|p| p.shares)
    }

    #[tokio::test]
    async fn unknown_fill_records_the_planned_size() {
        // PERCENTAGE 20% of $250 at a 0.505 limit, floored to whole shares.
        assert_eq!(recorded_shares(None).await, Some(99.0));
    }

    #[tokio::test]
    async fn partial_fill_records_what_filled() {
        assert_eq!(recorded_shares(Some(12.5)).await, Some(12.5));
    }

    #[tokio::test]
    async fn empty_fill_records_nothing() {
        assert_eq!(recorded_shares(Some(0.0)).await, None);
    }
}
