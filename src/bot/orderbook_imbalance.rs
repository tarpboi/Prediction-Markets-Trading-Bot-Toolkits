//! Orderbook imbalance bot: the signal is the book, with no external feed.
//!
//! Compares near-touch bid and ask size, buys the side the imbalance favours, and
//! exits on a fixed take-profit, stop-loss or time limit. By default it trades with
//! the heavy side, where short-term price tends to go; `fade` reverses that.
//! Displayed size is cheap to fake, and each entry crosses a spread, so the signal
//! has to beat that cost. It's untested for profit on this venue. Books are polled
//! every `poll_secs`, so this reacts in seconds rather than milliseconds.

use crate::bot::quoting::{Known, SCAN_PAGES};
use crate::bot::runtime::{
    drive, is_quotable, shares_for, token_book, Ctx, Strategy, TakeResult, Universe,
};
use crate::config::{AppConfig, OrderbookImbalanceConfig};
use crate::models::{BookSnapshot, Side};
use crate::service::venue::{MarketQuery, TradableMarket};
use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tracing::{debug, info};

/// Wait after leaving a market before trading it again.
const COOLDOWN: Duration = Duration::from_secs(120);
const MIN_SECS_LEFT: i64 = 600;

pub async fn run(cfg: AppConfig) -> Result<()> {
    let ctx = Ctx::connect(cfg, "Orderbook imbalance")?;
    let mut bot = ImbalanceBot {
        cfg: ctx.cfg.strategies.orderbook_imbalance.clone(),
        universe: Universe::new(
            MarketQuery {
                variants: Vec::new(),
                max_pages: SCAN_PAGES,
            },
            ctx.cfg.strategies.rescan_secs,
        ),
        known: Known::default(),
        watched: Vec::new(),
        cooling: HashMap::new(),
    };
    drive(&mut bot, &ctx).await
}

struct ImbalanceBot {
    cfg: OrderbookImbalanceConfig,
    universe: Universe,
    known: Known,
    watched: Vec<u64>,
    cooling: HashMap<u64, Instant>,
}

#[async_trait]
impl Strategy for ImbalanceBot {
    fn name(&self) -> &'static str {
        "orderbook-imbalance"
    }

    async fn tick(&mut self, ctx: &Ctx) -> Result<()> {
        if self.universe.refresh(ctx.data.as_ref()).await? {
            let markets: Vec<TradableMarket> = self
                .universe
                .markets()
                .iter()
                .filter(|m| is_quotable(m, MIN_SECS_LEFT, Utc::now()))
                .cloned()
                .collect();
            let ids: Vec<u64> = markets.iter().map(|m| m.id).collect();
            // Narrowest books first: they cost least to cross.
            let books = ctx.data.books(&ids).await?;
            // The narrowest books tend to sit at extreme prices, so the band has to
            // apply here or it would rule out everything chosen.
            let mut ranked: Vec<(&TradableMarket, f64)> = markets
                .iter()
                .filter(|m| {
                    books
                        .get(&m.id)
                        .and_then(BookSnapshot::mid)
                        .is_some_and(|mid| in_price_band(mid, &self.cfg))
                })
                .filter_map(|m| Some((m, books.get(&m.id)?.spread()?)))
                .filter(|(_, spread)| *spread <= self.cfg.max_spread + 1e-9)
                .collect();
            ranked.sort_by(|a, b| a.1.total_cmp(&b.1));

            self.watched.clear();
            for (market, _) in &ranked {
                if self.watched.len() >= self.cfg.max_markets {
                    break;
                }
                let Ok(market) = ctx.data.with_schedule(market).await else {
                    continue;
                };
                if is_quotable(&market, MIN_SECS_LEFT, Utc::now()) && ctx.allowed(&market).await {
                    self.watched.push(market.id);
                    self.known.remember(std::slice::from_ref(&market));
                }
            }
            info!(
                scanned = ids.len(),
                narrow = ranked.len(),
                watching = self.watched.len(),
                "markets chosen"
            );
        }

        let watched = self.watched.clone();
        let books = self.known.observe(ctx, &watched).await?;
        self.exits(ctx, &books).await?;
        if ctx.broker.is_halted() {
            return Ok(());
        }
        self.entries(ctx, &books, &watched).await
    }
}

impl ImbalanceBot {
    async fn exits(&mut self, ctx: &Ctx, books: &HashMap<u64, BookSnapshot>) -> Result<()> {
        for holding in ctx.broker.holdings() {
            let (Some(market), Some(book)) =
                (self.known.get(holding.market_id).cloned(), books.get(&holding.market_id))
            else {
                continue;
            };
            let view = token_book(&market, &holding.token, book);
            let Some(bid) = view.best_bid() else {
                continue;
            };
            let held_secs = (Utc::now() - holding.opened_at).num_seconds();
            let Some(mid) = view.mid() else {
                continue;
            };
            let Some(reason) = exit_reason(bid.price, mid, holding.avg_price(), held_secs, &self.cfg)
            else {
                continue;
            };
            info!(market = market.id, reason, bid = bid.price, entry = holding.avg_price(), "exiting");
            let result = ctx
                .broker
                .take(&market, &holding.token, Side::Sell, holding.shares, bid.price, book)
                .await?;
            match result {
                TakeResult::Filled { shares, .. } if shares > 0.0 => {
                    self.cooling.insert(market.id, Instant::now());
                }
                TakeResult::Filled { .. } => {}
                TakeResult::Blocked(reason) => info!(market = market.id, ?reason, "exit blocked"),
            }
        }
        Ok(())
    }

    async fn entries(
        &mut self,
        ctx: &Ctx,
        books: &HashMap<u64, BookSnapshot>,
        watched: &[u64],
    ) -> Result<()> {
        self.cooling.retain(|_, at| at.elapsed() < COOLDOWN);
        for id in watched {
            if ctx.broker.holdings().len() >= self.cfg.max_positions {
                return Ok(());
            }
            if self.cooling.contains_key(id) {
                continue;
            }
            let (Some(market), Some(book)) = (self.known.get(*id).cloned(), books.get(id)) else {
                continue;
            };
            if ctx.broker.holding(&market.yes_token).is_some()
                || ctx.broker.holding(&market.no_token).is_some()
            {
                continue;
            }
            let Some(token) = signal(&market, book, &self.cfg) else {
                continue;
            };
            let view = token_book(&market, &token, book);
            let Some(ask) = view.best_ask() else {
                continue;
            };
            if !in_price_band(ask.price, &self.cfg) {
                continue;
            }
            let shares = shares_for(self.cfg.order_usd, ask.price);
            let result = ctx
                .broker
                .take(&market, &token, Side::Buy, shares, ask.price, book)
                .await?;
            match result {
                TakeResult::Blocked(reason) => {
                    debug!(market = market.id, ?reason, price = ask.price, "signal, but no entry");
                }
                TakeResult::Filled { shares, price } => info!(
                    market = market.id,
                    imbalance = book.imbalance(self.cfg.levels),
                    shares,
                    price,
                    "imbalance entry"
                ),
            }
        }
        Ok(())
    }
}

/// Which outcome the imbalance says to buy, if it's strong and the spread is
/// narrow enough to be worth crossing.
fn signal(
    market: &TradableMarket,
    book: &BookSnapshot,
    cfg: &OrderbookImbalanceConfig,
) -> Option<String> {
    if book.spread()? > cfg.max_spread + 1e-9 {
        return None;
    }
    let imbalance = book.imbalance(cfg.levels)?;
    if imbalance.abs() < cfg.threshold {
        return None;
    }
    let bid_heavy = imbalance > 0.0;
    Some(if bid_heavy != cfg.fade {
        market.yes_token.clone()
    } else {
        market.no_token.clone()
    })
}

fn in_price_band(price: f64, cfg: &OrderbookImbalanceConfig) -> bool {
    price >= cfg.min_price - 1e-9 && price <= cfg.max_price + 1e-9
}

/// The target is what can actually be sold for, so it's measured at the bid. The
/// stop is measured at the mid: at the bid, a spread as wide as the stop would fire
/// it the moment the entry filled.
fn exit_reason(
    bid: f64,
    mid: f64,
    entry: f64,
    held_secs: i64,
    cfg: &OrderbookImbalanceConfig,
) -> Option<&'static str> {
    if bid >= entry + cfg.take_profit - 1e-9 {
        Some("take profit")
    } else if mid <= entry - cfg.stop_loss + 1e-9 {
        Some("stop loss")
    } else if held_secs >= cfg.max_hold_secs {
        Some("time limit")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::runtime::test_support::{book, market};

    fn cfg() -> OrderbookImbalanceConfig {
        OrderbookImbalanceConfig::default()
    }

    #[test]
    fn imbalance_is_measured_over_the_configured_levels() {
        let b = book(&[(0.5, 90.0), (0.49, 1000.0)], &[(0.51, 10.0), (0.52, 1.0)]);
        assert!((b.imbalance(1).unwrap() - 0.8).abs() < 1e-9);
        assert!(book(&[], &[(0.5, 1.0)]).imbalance(3).is_none());
    }

    #[test]
    fn heavy_bids_buy_yes_and_heavy_asks_buy_no() {
        let m = market(1);
        let bids = book(&[(0.5, 90.0)], &[(0.51, 10.0)]);
        let asks = book(&[(0.5, 10.0)], &[(0.51, 90.0)]);
        assert_eq!(signal(&m, &bids, &cfg()).unwrap(), "yes-1");
        assert_eq!(signal(&m, &asks, &cfg()).unwrap(), "no-1");
    }

    #[test]
    fn fade_reverses_the_signal() {
        let m = market(1);
        let bids = book(&[(0.5, 90.0)], &[(0.51, 10.0)]);
        let fade = OrderbookImbalanceConfig { fade: true, ..cfg() };
        assert_eq!(signal(&m, &bids, &fade).unwrap(), "no-1");
    }

    #[test]
    fn weak_signals_and_wide_spreads_are_ignored() {
        let m = market(1);
        assert!(signal(&m, &book(&[(0.5, 55.0)], &[(0.51, 45.0)]), &cfg()).is_none());
        assert!(signal(&m, &book(&[(0.4, 90.0)], &[(0.6, 10.0)]), &cfg()).is_none());
    }

    #[test]
    fn exits_on_profit_loss_or_time() {
        assert_eq!(exit_reason(0.52, 0.53, 0.50, 0, &cfg()), Some("take profit"));
        assert_eq!(exit_reason(0.47, 0.48, 0.50, 0, &cfg()), Some("stop loss"));
        assert_eq!(exit_reason(0.50, 0.51, 0.50, 601, &cfg()), Some("time limit"));
        assert_eq!(exit_reason(0.51, 0.515, 0.50, 10, &cfg()), None);
    }

    #[test]
    fn extreme_prices_are_out_of_band() {
        assert!(in_price_band(0.5, &cfg()));
        assert!(!in_price_band(0.05, &cfg()), "3c of spread would be 60% of the price");
        assert!(!in_price_band(0.95, &cfg()));
    }

    #[test]
    fn crossing_the_spread_doesnt_trip_the_stop_by_itself() {
        // Bought at the 0.52 ask with the bid at 0.50: nothing has moved.
        assert_eq!(exit_reason(0.50, 0.51, 0.52, 0, &cfg()), None);
    }
}
