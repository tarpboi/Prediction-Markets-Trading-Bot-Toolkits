//! Directional Arbitrage bot.
//!
//! A hybrid of pure arbitrage and directional trading: buy both sides of a binary
//! market for less than $1 in total, then *tilt* size toward the side the book
//! rates as undervalued. The basket caps downside on the matched size; the tilt
//! adds a directional source of edge on top.
//!
//! Example: the Yes and No bids total 96c, and heavy Yes bids put the microprice
//! above the mid. The bot rests more Yes than No, keeping a 4c floor on the
//! matched part while the net position is long Yes.
//!
//! Predict.fun has one book per market, quoted for Yes, with No as its mirror. A
//! Yes ask plus a No ask therefore always costs 1 + the spread, so this basket can
//! never be taken: it can only be built by resting bids inside a spread that's
//! wide enough. That has two consequences:
//!
//! - It needs someone else to sell into each leg. When only one fills, that part
//!   is an ordinary directional position until the other leg fills or the market
//!   resolves, and the floor doesn't apply to it. Fills on both legs are likeliest
//!   in slow markets, and it's the fast moves that fill only one.
//! - The matched part is held to resolution, which ties up the capital.
//!
//! Limit orders only: it never crosses the spread.

use crate::bot::quoting::Picker;
use crate::bot::runtime::{drive, shares_for, snap_up, Ctx, Quote, Strategy, MIN_ORDER_USD};
use crate::config::{AppConfig, DirectionalArbConfig};
use crate::models::{BookSnapshot, Side};
use crate::service::venue::TradableMarket;
use anyhow::Result;
use async_trait::async_trait;

pub async fn run(cfg: AppConfig) -> Result<()> {
    let ctx = Ctx::connect(cfg, "Directional arbitrage")?;
    let mut bot = DirectionalArb::new(&ctx);
    drive(&mut bot, &ctx).await
}

struct DirectionalArb {
    cfg: DirectionalArbConfig,
    picker: Picker,
}

impl DirectionalArb {
    fn new(ctx: &Ctx) -> Self {
        let cfg = ctx.cfg.strategies.directional_arb.clone();
        // Both bids improve the touch by a tick, so the spread has to leave room
        // for that plus the required floor.
        let min_spread = 1.0 - cfg.max_basket_cost + 0.02;
        Self {
            picker: Picker::new(ctx, min_spread, cfg.min_secs_left, cfg.max_markets),
            cfg,
        }
    }
}

#[async_trait]
impl Strategy for DirectionalArb {
    fn name(&self) -> &'static str {
        "directional-arb"
    }

    async fn tick(&mut self, ctx: &Ctx) -> Result<()> {
        let books = self.picker.step(ctx).await?;
        let cfg = self.cfg.clone();
        let max_order = ctx.cfg.strategies.limits.max_order_usd;
        let broker = ctx.broker.clone();
        self.picker
            .quote(ctx, &books, 0.0, move |market, book| {
                let held = |token: &str| broker.holding(token).map_or(0.0, |h| h.shares);
                basket_quotes(
                    market,
                    book,
                    &cfg,
                    max_order,
                    held(&market.yes_token),
                    held(&market.no_token),
                )
            })
            .await
    }
}

/// How much bigger the favoured side should be: 1 at the threshold, reaching
/// `max_tilt_ratio` at three times the threshold.
fn tilt_ratio(edge_cents: f64, cfg: &DirectionalArbConfig) -> f64 {
    let strength = edge_cents.abs() / cfg.tilt_threshold_cents;
    if strength < 1.0 {
        return 1.0;
    }
    let scaled = 1.0 + (cfg.max_tilt_ratio - 1.0) * ((strength - 1.0) / 2.0).min(1.0);
    scaled.min(cfg.max_tilt_ratio)
}

/// Bids on both outcomes, one tick inside the touch. Stops adding to a side once
/// it's ahead of the other by more than the largest tilt allows.
///
/// Every leg is sized between the venue's minimum and `max_order_usd`. When both
/// can't fit, it quotes neither: one leg resting alone is the unhedged position
/// this strategy exists to avoid.
fn basket_quotes(
    market: &TradableMarket,
    book: &BookSnapshot,
    cfg: &DirectionalArbConfig,
    max_order_usd: f64,
    yes_held: f64,
    no_held: f64,
) -> Vec<Quote> {
    let (Some(best_bid), Some(best_ask), Some(mid), Some(micro)) =
        (book.best_bid(), book.best_ask(), book.mid(), book.microprice())
    else {
        return Vec::new();
    };
    let tick = market.tick;
    let yes_price = snap_up(best_bid.price + tick, tick);
    // The best No bid is `1 - best Yes ask`.
    let no_price = snap_up(1.0 - best_ask.price + tick, tick);
    let basket = yes_price + no_price;
    if basket > cfg.max_basket_cost + 1e-9 || yes_price <= 0.0 || no_price <= 0.0 {
        return Vec::new();
    }

    let edge_cents = (micro - mid) * 100.0;
    let ratio = tilt_ratio(edge_cents, cfg);
    // Enough of the smaller side for the cheaper leg to clear the venue minimum.
    let floor = (MIN_ORDER_USD / yes_price.min(no_price) * 100.0).ceil() / 100.0;
    let base = shares_for(cfg.order_usd, basket).max(floor);
    let (yes_wanted, no_wanted) = if edge_cents > 0.0 {
        (base * ratio, base)
    } else {
        (base, base * ratio)
    };
    let round = |shares: f64| (shares * 100.0).floor() / 100.0;
    let yes_shares = round(yes_wanted.min(max_order_usd / yes_price));
    let no_shares = round(no_wanted.min(max_order_usd / no_price));
    if yes_shares * yes_price < MIN_ORDER_USD - 1e-9 || no_shares * no_price < MIN_ORDER_USD - 1e-9 {
        return Vec::new();
    }
    // Capping one leg can leave the pair further apart than a tilt is allowed to be.
    if yes_shares.max(no_shares) > yes_shares.min(no_shares) * cfg.max_tilt_ratio + 0.01 {
        return Vec::new();
    }

    // A position can run ahead by one tilted order, but no further.
    let max_lead = base * cfg.max_tilt_ratio;
    let mut quotes = Vec::new();
    if yes_held - no_held < max_lead {
        quotes.push(Quote {
            token: market.yes_token.clone(),
            side: Side::Buy,
            price: yes_price,
            shares: yes_shares,
        });
    }
    if no_held - yes_held < max_lead {
        quotes.push(Quote {
            token: market.no_token.clone(),
            side: Side::Buy,
            price: no_price,
            shares: no_shares,
        });
    }
    quotes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::runtime::test_support::{book, market};

    fn cfg() -> DirectionalArbConfig {
        DirectionalArbConfig {
            max_basket_cost: 0.99,
            tilt_threshold_cents: 1.5,
            max_tilt_ratio: 3.0,
            order_usd: 2.0,
            ..Default::default()
        }
    }

    #[test]
    fn taking_both_asks_can_never_beat_a_dollar_on_a_shared_book() {
        // The reason this strategy rests instead of taking.
        for (bid, ask) in [(0.40, 0.42), (0.10, 0.90), (0.49, 0.50)] {
            let b = book(&[(bid, 10.0)], &[(ask, 10.0)]);
            let yes_ask = b.best_ask().unwrap().price;
            let no_ask = b.mirrored().best_ask().unwrap().price;
            assert!(yes_ask + no_ask >= 1.0);
        }
    }

    #[test]
    fn rests_a_basket_below_the_cap_inside_a_wide_spread() {
        let q = basket_quotes(&market(1), &book(&[(0.4, 10.0)], &[(0.5, 10.0)]), &cfg(), 50.0, 0.0, 0.0);
        assert_eq!(q.len(), 2);
        assert_eq!((q[0].token.as_str(), q[0].side, q[0].price), ("yes-1", Side::Buy, 0.41));
        assert_eq!((q[1].token.as_str(), q[1].side, q[1].price), ("no-1", Side::Buy, 0.51));
        assert!(q[0].price + q[1].price <= 0.99);
    }

    #[test]
    fn skips_a_spread_too_tight_for_the_floor() {
        let q = basket_quotes(&market(1), &book(&[(0.45, 10.0)], &[(0.47, 10.0)]), &cfg(), 50.0, 0.0, 0.0);
        assert!(q.is_empty());
    }

    #[test]
    fn even_book_stays_even_and_heavy_bids_tilt_toward_yes() {
        let even = basket_quotes(&market(1), &book(&[(0.4, 10.0)], &[(0.5, 10.0)]), &cfg(), 50.0, 0.0, 0.0);
        assert_eq!(even[0].shares, even[1].shares);

        let heavy = basket_quotes(&market(1), &book(&[(0.4, 900.0)], &[(0.5, 10.0)]), &cfg(), 50.0, 0.0, 0.0);
        assert!((heavy[0].shares / heavy[1].shares - 3.0).abs() < 0.01, "{heavy:?}");

        let light = basket_quotes(&market(1), &book(&[(0.4, 10.0)], &[(0.5, 900.0)]), &cfg(), 50.0, 0.0, 0.0);
        assert!((light[1].shares / light[0].shares - 3.0).abs() < 0.01);
    }

    #[test]
    fn stops_adding_to_a_side_that_is_too_far_ahead() {
        let wide = book(&[(0.4, 10.0)], &[(0.5, 10.0)]);
        let q = basket_quotes(&market(1), &wide, &cfg(), 50.0, 100.0, 0.0);
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].token, "no-1");
    }

    #[test]
    fn every_leg_clears_the_venue_minimum() {
        let wide = book(&[(0.1, 10.0)], &[(0.3, 10.0)]);
        let q = basket_quotes(&market(1), &wide, &cfg(), 50.0, 0.0, 0.0);
        assert_eq!(q.len(), 2);
        for leg in &q {
            assert!(leg.shares * leg.price >= MIN_ORDER_USD, "{leg:?}");
        }
    }

    #[test]
    fn quotes_neither_leg_when_both_cant_fit_the_order_cap() {
        // Yes at 0.05 and No at 0.90: the 1 USD minimum on Yes is 20 shares, which
        // is 18 USD of No. A 5 USD cap can't hold that pair anywhere near balanced.
        let lopsided = book(&[(0.04, 10.0)], &[(0.11, 10.0)]);
        assert_eq!(basket_quotes(&market(1), &lopsided, &cfg(), 50.0, 0.0, 0.0).len(), 2);
        assert!(basket_quotes(&market(1), &lopsided, &cfg(), 5.0, 0.0, 0.0).is_empty());
    }

    #[test]
    fn tilted_leg_is_capped_rather_than_refused() {
        let heavy = book(&[(0.4, 900.0)], &[(0.5, 10.0)]);
        let q = basket_quotes(&market(1), &heavy, &cfg(), 2.0, 0.0, 0.0);
        assert_eq!(q.len(), 2);
        assert!(q[0].shares * q[0].price <= 2.0 + 1e-9);
    }

    #[test]
    fn tilt_grows_with_the_edge_and_is_capped() {
        assert_eq!(tilt_ratio(1.0, &cfg()), 1.0);
        assert_eq!(tilt_ratio(1.5, &cfg()), 1.0);
        assert!((tilt_ratio(3.0, &cfg()) - 2.0).abs() < 1e-9);
        assert_eq!(tilt_ratio(99.0, &cfg()), 3.0);
    }
}
