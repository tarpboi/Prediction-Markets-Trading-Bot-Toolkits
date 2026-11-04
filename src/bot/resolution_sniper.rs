//! Resolution-sniper bot: near-certainty buys held to the resolution payout.
//!
//! Buys an outcome priced close to $1 in a market about to end. Each win is small
//! and each loss costs nearly the whole stake, so it takes dozens of wins to pay
//! for one surprise: a price of 97c only pays if the outcome is right more than
//! 97% of the time. Nothing here checks the real-world outcome. Disputed or paused
//! markets are skipped, and everything else is the market's word for it.

use crate::bot::quoting::Known;
use crate::bot::runtime::{drive, shares_for, token_book, Ctx, Strategy, TakeResult, Universe};
use crate::config::{AppConfig, ResolutionSniperConfig};
use crate::models::{BookSnapshot, Side};
use crate::service::venue::{MarketQuery, TradableMarket};
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::collections::HashSet;
use tracing::info;

/// Near-certain markets are often quiet ones, so this reads further down the list.
const SNIPER_SCAN_PAGES: u32 = 10;

pub async fn run(cfg: AppConfig) -> Result<()> {
    let ctx = Ctx::connect(cfg, "Resolution sniper")?;
    let mut bot = Sniper {
        cfg: ctx.cfg.strategies.resolution_sniper.clone(),
        universe: Universe::new(
            MarketQuery {
                variants: Vec::new(),
                max_pages: SNIPER_SCAN_PAGES,
            },
            ctx.cfg.strategies.rescan_secs,
        ),
        known: Known::default(),
        candidates: Vec::new(),
        entered: HashSet::new(),
    };
    drive(&mut bot, &ctx).await
}

struct Sniper {
    cfg: ResolutionSniperConfig,
    universe: Universe,
    known: Known,
    /// In the price band and due to end soon, as of the last scan.
    candidates: Vec<TradableMarket>,
    entered: HashSet<u64>,
}

#[async_trait]
impl Strategy for Sniper {
    fn name(&self) -> &'static str {
        "resolution-sniper"
    }

    async fn tick(&mut self, ctx: &Ctx) -> Result<()> {
        if self.universe.refresh(ctx.data.as_ref()).await? {
            let cfg = self.cfg.clone();
            let open: Vec<TradableMarket> = self
                .universe
                .markets()
                .iter()
                .filter(|m| m.open && !m.contested && m.payout_yes.is_none())
                .cloned()
                .collect();
            let ids: Vec<u64> = open.iter().map(|m| m.id).collect();
            let books = ctx.data.books(&ids).await?;
            self.candidates.clear();
            for market in &open {
                let priced = books.get(&market.id).is_some_and(|b| pick_snipe(market, b, &cfg).is_some());
                if !priced {
                    continue;
                }
                let Ok(market) = ctx.data.with_schedule(market).await else {
                    continue;
                };
                if in_window(&market, cfg.max_secs_to_end, Utc::now()) {
                    self.candidates.push(market);
                }
            }
            self.known.remember(&self.candidates);
            info!(
                scanned = ids.len(),
                ending_soon_in_band = self.candidates.len(),
                "markets scanned"
            );
        }
        let now = Utc::now();
        let candidates: Vec<TradableMarket> = self
            .candidates
            .iter()
            .filter(|m| !self.entered.contains(&m.id) && in_window(m, self.cfg.max_secs_to_end, now))
            .cloned()
            .collect();
        let ids: Vec<u64> = candidates.iter().map(|m| m.id).collect();
        let books = self.known.observe(ctx, &ids).await?;
        if ctx.broker.is_halted() {
            return Ok(());
        }
        for market in candidates {
            let Some(book) = books.get(&market.id) else {
                continue;
            };
            let Some(snipe) = pick_snipe(&market, book, &self.cfg) else {
                continue;
            };
            if !ctx.allowed(&market).await {
                continue;
            }
            info!(
                market = market.id,
                question = %market.question,
                price = snipe.price,
                net = snipe.net_edge,
                "near-certain outcome"
            );
            let shares = shares_for(self.cfg.order_usd, snipe.price);
            match ctx
                .broker
                .take(&market, &snipe.token, Side::Buy, shares, snipe.price, book)
                .await?
            {
                TakeResult::Filled { shares, .. } if shares > 0.0 => {
                    self.entered.insert(market.id);
                }
                TakeResult::Filled { .. } => {}
                TakeResult::Blocked(reason) => info!(market = market.id, ?reason, "entry blocked"),
            }
        }
        Ok(())
    }
}

/// Open, uncontested and due to end soon. A market with no known end can't be
/// called near resolution, whatever its price.
fn in_window(market: &TradableMarket, max_secs: i64, now: DateTime<Utc>) -> bool {
    market.open
        && !market.contested
        && market.payout_yes.is_none()
        && market.secs_left(now).is_some_and(|left| left > 0 && left <= max_secs)
}

#[derive(Debug, Clone, PartialEq)]
struct Snipe {
    token: String,
    price: f64,
    net_edge: f64,
}

/// The outcome priced in the near-certain band, if either is and its best ask
/// has room for the order.
fn pick_snipe(
    market: &TradableMarket,
    yes_book: &BookSnapshot,
    cfg: &ResolutionSniperConfig,
) -> Option<Snipe> {
    [&market.yes_token, &market.no_token]
        .into_iter()
        .filter_map(|token| {
            let view = token_book(market, token, yes_book);
            let ask = view.best_ask()?;
            let in_band = ask.price >= cfg.min_price - 1e-9 && ask.price <= cfg.max_price + 1e-9;
            let net_edge = 1.0 - ask.price - market.taker_fee(ask.price);
            let room = ask.size >= shares_for(cfg.order_usd, ask.price);
            (in_band && room && net_edge >= cfg.min_net_edge).then(|| Snipe {
                token: token.clone(),
                price: ask.price,
                net_edge,
            })
        })
        .max_by(|a, b| a.net_edge.total_cmp(&b.net_edge))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::runtime::test_support::{book, market};

    fn cfg() -> ResolutionSniperConfig {
        ResolutionSniperConfig::default()
    }

    #[test]
    fn buys_yes_at_a_near_certain_price() {
        let s = pick_snipe(&market(1), &book(&[(0.95, 50.0)], &[(0.97, 50.0)]), &cfg()).unwrap();
        assert_eq!((s.token.as_str(), s.price), ("yes-1", 0.97));
        assert!((s.net_edge - (0.03 - 0.02 * 0.03)).abs() < 1e-9);
    }

    #[test]
    fn buys_no_through_the_mirror_when_yes_is_nearly_dead() {
        let s = pick_snipe(&market(1), &book(&[(0.03, 50.0)], &[(0.05, 50.0)]), &cfg()).unwrap();
        assert_eq!(s.token, "no-1");
        assert!((s.price - 0.97).abs() < 1e-9);
    }

    #[test]
    fn ignores_prices_outside_the_band_and_thin_asks() {
        assert!(pick_snipe(&market(1), &book(&[(0.5, 50.0)], &[(0.6, 50.0)]), &cfg()).is_none());
        // 99.5c leaves too little after fees.
        assert!(pick_snipe(&market(1), &book(&[(0.98, 50.0)], &[(0.995, 50.0)]), &cfg()).is_none());
        assert!(pick_snipe(&market(1), &book(&[(0.95, 50.0)], &[(0.97, 1.0)]), &cfg()).is_none());
    }

    #[test]
    fn window_excludes_disputed_ended_and_undated_markets() {
        let now = Utc::now();
        let soon = TradableMarket {
            ends_at: Some(now + chrono::Duration::hours(2)),
            ..market(1)
        };
        assert!(in_window(&soon, 86_400, now));
        assert!(!in_window(&TradableMarket { contested: true, ..soon.clone() }, 86_400, now));
        assert!(!in_window(&TradableMarket { open: false, ..soon.clone() }, 86_400, now));
        assert!(!in_window(&TradableMarket { payout_yes: Some(1.0), ..soon.clone() }, 86_400, now));
        assert!(!in_window(&soon, 3_600, now));
        assert!(!in_window(&market(2), 86_400, now), "no known end");
    }
}
