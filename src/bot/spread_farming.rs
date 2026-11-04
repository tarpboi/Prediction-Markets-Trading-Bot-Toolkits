//! Spread-farming bot: systematic bid-ask capture, one round trip at a time.
//!
//! Joins the bid just inside a wide spread, then offers what it bought just
//! inside the ask. Unlike the market maker it holds one position per market and
//! never quotes both ways at once. A position that won't sell within the hold
//! limit is sold at the bid, which takes the loss rather than the risk.

use crate::bot::quoting::Picker;
use crate::bot::runtime::{drive, shares_for, snap_down, snap_up, Ctx, Quote, Strategy, TakeResult};
use crate::config::{AppConfig, SpreadFarmingConfig};
use crate::models::{BookSnapshot, Side};
use crate::service::venue::TradableMarket;
use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use tracing::{info, warn};

pub async fn run(cfg: AppConfig) -> Result<()> {
    let ctx = Ctx::connect(cfg, "Spread farming")?;
    let mut bot = SpreadFarmer::new(&ctx);
    drive(&mut bot, &ctx).await
}

struct SpreadFarmer {
    cfg: SpreadFarmingConfig,
    picker: Picker,
}

impl SpreadFarmer {
    fn new(ctx: &Ctx) -> Self {
        let cfg = ctx.cfg.strategies.spread_farming.clone();
        Self {
            picker: Picker::new(ctx, cfg.min_spread, cfg.min_secs_left, cfg.max_markets),
            cfg,
        }
    }

    /// Sell anything held past the limit at the bid.
    async fn dump_stale(
        &self,
        ctx: &Ctx,
        books: &std::collections::HashMap<u64, BookSnapshot>,
    ) -> Result<()> {
        let now = Utc::now();
        for holding in ctx.broker.holdings() {
            if (now - holding.opened_at).num_seconds() < self.cfg.max_hold_secs {
                continue;
            }
            let (Some(market), Some(book)) =
                (self.picker.known().get(holding.market_id), books.get(&holding.market_id))
            else {
                continue;
            };
            let view = crate::bot::runtime::token_book(market, &holding.token, book);
            let Some(bid) = view.best_bid() else {
                continue;
            };
            let ids: Vec<String> = ctx
                .broker
                .resting_in(market.id)
                .into_iter()
                .filter(|o| o.token == holding.token && o.side == Side::Sell)
                .map(|o| o.id)
                .collect();
            let refused = ctx.broker.cancel(&ids).await?;
            if !refused.is_empty() {
                continue;
            }
            match ctx
                .broker
                .take(market, &holding.token, Side::Sell, holding.shares, bid.price, book)
                .await?
            {
                TakeResult::Filled { shares, price } => {
                    info!(market = market.id, shares, price, "sold a position held too long");
                }
                TakeResult::Blocked(reason) => {
                    warn!(market = market.id, ?reason, "couldn't sell a position held too long");
                }
            }
        }
        Ok(())
    }
}

#[async_trait]
impl Strategy for SpreadFarmer {
    fn name(&self) -> &'static str {
        "spread-farming"
    }

    async fn tick(&mut self, ctx: &Ctx) -> Result<()> {
        let books = self.picker.step(ctx).await?;
        self.dump_stale(ctx, &books).await?;
        let cfg = self.cfg.clone();
        let broker = ctx.broker.clone();
        self.picker
            .quote(ctx, &books, 0.0, move |market, book| {
                let held = broker.holding(&market.yes_token);
                farm_quotes(
                    market,
                    book,
                    &cfg,
                    held.as_ref().map_or(0.0, |h| h.shares),
                    held.as_ref().map_or(0.0, |h| h.avg_price()),
                )
            })
            .await
    }
}

/// Flat: bid inside a wide spread. Holding: offer inside the ask, never below
/// cost plus a tick, so each round trip that completes makes money.
fn farm_quotes(
    market: &TradableMarket,
    book: &BookSnapshot,
    cfg: &SpreadFarmingConfig,
    held: f64,
    avg_price: f64,
) -> Vec<Quote> {
    let (Some(best_bid), Some(best_ask)) = (book.best_bid(), book.best_ask()) else {
        return Vec::new();
    };
    let tick = market.tick;
    if held > 0.0 {
        let price = snap_down(best_ask.price - tick, tick).max(snap_up(avg_price + tick, tick));
        if price >= 1.0 {
            return Vec::new();
        }
        return vec![Quote {
            token: market.yes_token.clone(),
            side: Side::Sell,
            price,
            shares: held,
        }];
    }
    if best_ask.price - best_bid.price < cfg.min_spread - 1e-9 {
        return Vec::new();
    }
    let price = snap_up(best_bid.price + tick, tick);
    if price >= best_ask.price {
        return Vec::new();
    }
    vec![Quote {
        token: market.yes_token.clone(),
        side: Side::Buy,
        price,
        shares: shares_for(cfg.order_usd, price),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::runtime::test_support::{book, market};

    fn cfg() -> SpreadFarmingConfig {
        SpreadFarmingConfig {
            min_spread: 0.04,
            order_usd: 5.0,
            ..Default::default()
        }
    }

    #[test]
    fn flat_bids_one_tick_inside_a_wide_spread() {
        let q = farm_quotes(&market(1), &book(&[(0.4, 9.0)], &[(0.5, 9.0)]), &cfg(), 0.0, 0.0);
        assert_eq!((q[0].side, q[0].price, q[0].shares), (Side::Buy, 0.41, 12.19));
    }

    #[test]
    fn a_tight_spread_is_left_alone() {
        let q = farm_quotes(&market(1), &book(&[(0.48, 9.0)], &[(0.5, 9.0)]), &cfg(), 0.0, 0.0);
        assert!(q.is_empty());
    }

    #[test]
    fn holding_offers_inside_the_ask() {
        let q = farm_quotes(&market(1), &book(&[(0.4, 9.0)], &[(0.5, 9.0)]), &cfg(), 12.0, 0.41);
        assert_eq!((q[0].side, q[0].price, q[0].shares), (Side::Sell, 0.49, 12.0));
    }

    #[test]
    fn holding_never_offers_below_cost_while_the_price_is_down() {
        let q = farm_quotes(&market(1), &book(&[(0.3, 9.0)], &[(0.35, 9.0)]), &cfg(), 12.0, 0.41);
        assert_eq!(q[0].price, 0.42);
    }
}
