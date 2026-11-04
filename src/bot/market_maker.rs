//! Market making bot: two-sided post-only quotes with inventory skew.
//!
//! Quotes around the mid, and leans both quotes away from the inventory it holds
//! so fills tend to flatten it. Predict's book is shared between outcomes, so the
//! offer side is a buy of the No token unless there are Yes shares to sell. Every
//! quote carries an expiry, and quotes are pulled when books stop arriving.
//!
//! This is a plain textbook maker. It doesn't use an external fair value, so it
//! will be picked off by anyone who has one: on Predict's crypto rounds, that's
//! anyone watching Binance. Prefer slower markets, and measure before sizing up.

use crate::bot::quoting::Picker;
use crate::bot::runtime::{drive, shares_for, snap_down, snap_up, Ctx, Quote, Strategy};
use crate::config::{AppConfig, MarketMakingConfig};
use crate::models::{BookSnapshot, Side};
use crate::service::venue::TradableMarket;
use anyhow::Result;
use async_trait::async_trait;

pub async fn run(cfg: AppConfig) -> Result<()> {
    let ctx = Ctx::connect(cfg, "Market making")?;
    let mut bot = MarketMaker::new(&ctx);
    drive(&mut bot, &ctx).await
}

struct MarketMaker {
    cfg: MarketMakingConfig,
    picker: Picker,
}

impl MarketMaker {
    fn new(ctx: &Ctx) -> Self {
        let cfg = ctx.cfg.strategies.market_making.clone();
        Self {
            picker: Picker::new(ctx, cfg.half_spread * 2.0, cfg.min_secs_left, cfg.max_markets),
            cfg,
        }
    }
}

#[async_trait]
impl Strategy for MarketMaker {
    fn name(&self) -> &'static str {
        "market-making"
    }

    async fn tick(&mut self, ctx: &Ctx) -> Result<()> {
        let books = self.picker.step(ctx).await?;
        let cfg = self.cfg.clone();
        let broker = ctx.broker.clone();
        self.picker
            .quote(ctx, &books, cfg.requote_distance, move |market, book| {
                let held = |token: &str| broker.holding(token).map_or(0.0, |h| h.shares);
                mm_quotes(market, book, &cfg, held(&market.yes_token), held(&market.no_token))
            })
            .await
    }
}

/// Bid and offer for one market. A long Yes position lowers both quotes, and a
/// long No position raises them.
fn mm_quotes(
    market: &TradableMarket,
    book: &BookSnapshot,
    cfg: &MarketMakingConfig,
    yes_held: f64,
    no_held: f64,
) -> Vec<Quote> {
    let (Some(mid), Some(best_bid), Some(best_ask)) = (book.mid(), book.best_bid(), book.best_ask())
    else {
        return Vec::new();
    };
    let tick = market.tick;
    let skew = (yes_held - no_held) * mid * cfg.skew_per_usd;

    let bid = snap_down(mid - cfg.half_spread - skew, tick).min(snap_down(best_ask.price - tick, tick));
    let ask = snap_up(mid + cfg.half_spread - skew, tick).max(snap_up(best_bid.price + tick, tick));

    let mut quotes = Vec::new();
    if bid >= tick && bid < ask {
        // Buying Yes at `bid` and selling No at `1 - bid` are the same trade.
        // Selling what's held frees capital instead of tying up more.
        let no_price = snap_up(1.0 - bid, tick);
        let sell_no = shares_for(cfg.order_usd, no_price);
        if no_held >= sell_no && sell_no > 0.0 {
            quotes.push(Quote {
                token: market.no_token.clone(),
                side: Side::Sell,
                price: no_price,
                shares: sell_no,
            });
        } else {
            quotes.push(Quote {
                token: market.yes_token.clone(),
                side: Side::Buy,
                price: bid,
                shares: shares_for(cfg.order_usd, bid),
            });
        }
    }
    if ask <= 1.0 - tick && bid < ask {
        let sell_yes = shares_for(cfg.order_usd, ask);
        if yes_held >= sell_yes && sell_yes > 0.0 {
            quotes.push(Quote {
                token: market.yes_token.clone(),
                side: Side::Sell,
                price: ask,
                shares: sell_yes,
            });
        } else {
            let no_price = snap_down(1.0 - ask, tick);
            quotes.push(Quote {
                token: market.no_token.clone(),
                side: Side::Buy,
                price: no_price,
                shares: shares_for(cfg.order_usd, no_price),
            });
        }
    }
    quotes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::runtime::test_support::{book, market};

    fn cfg() -> MarketMakingConfig {
        MarketMakingConfig {
            half_spread: 0.02,
            order_usd: 5.0,
            skew_per_usd: 0.001,
            ..Default::default()
        }
    }

    #[test]
    fn flat_quotes_a_yes_bid_and_a_no_bid() {
        let quotes = mm_quotes(&market(1), &book(&[(0.4, 50.0)], &[(0.6, 50.0)]), &cfg(), 0.0, 0.0);
        assert_eq!(quotes.len(), 2);
        assert_eq!((quotes[0].token.as_str(), quotes[0].side, quotes[0].price), ("yes-1", Side::Buy, 0.48));
        // An offer at 0.52 is a No bid at 0.48.
        assert_eq!((quotes[1].token.as_str(), quotes[1].side, quotes[1].price), ("no-1", Side::Buy, 0.48));
    }

    #[test]
    fn never_quotes_through_the_touch() {
        let quotes = mm_quotes(&market(1), &book(&[(0.49, 50.0)], &[(0.5, 50.0)]), &cfg(), 0.0, 0.0);
        assert_eq!(quotes[0].price, 0.47);
        let quotes = mm_quotes(&market(1), &book(&[(0.3, 50.0)], &[(0.31, 50.0)]), &cfg(), 0.0, 0.0);
        assert!(quotes[0].price < 0.31);
    }

    #[test]
    fn long_inventory_lowers_quotes_and_sells_what_it_holds() {
        let wide = book(&[(0.4, 50.0)], &[(0.6, 50.0)]);
        let flat = mm_quotes(&market(1), &wide, &cfg(), 0.0, 0.0);
        let long = mm_quotes(&market(1), &wide, &cfg(), 40.0, 0.0);
        assert!(long[0].price < flat[0].price, "bid lowered");
        let offer = &long[1];
        assert_eq!((offer.token.as_str(), offer.side), ("yes-1", Side::Sell));
        assert!(offer.price < 0.52, "offer lowered, to sell inventory sooner");
    }

    #[test]
    fn long_no_sells_no_rather_than_buying_more_yes() {
        let wide = book(&[(0.4, 50.0)], &[(0.6, 50.0)]);
        let quotes = mm_quotes(&market(1), &wide, &cfg(), 0.0, 40.0);
        assert_eq!((quotes[0].token.as_str(), quotes[0].side), ("no-1", Side::Sell));
    }

    #[test]
    fn extreme_prices_drop_the_leg_that_falls_off_the_book() {
        let quotes = mm_quotes(&market(1), &book(&[(0.005, 50.0)], &[(0.02, 50.0)]), &cfg(), 0.0, 0.0);
        assert!(quotes.iter().all(|q| q.price >= 0.01));
        assert_eq!(quotes.len(), 1);
    }

    #[test]
    fn one_sided_book_quotes_nothing() {
        assert!(mm_quotes(&market(1), &book(&[(0.4, 50.0)], &[]), &cfg(), 0.0, 0.0).is_empty());
    }
}
