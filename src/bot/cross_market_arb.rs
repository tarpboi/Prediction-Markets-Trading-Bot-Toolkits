//! Cross-venue arbitrage: Predict.fun against Polymarket.
//!
//! Predict publishes which Polymarket market each of its markets is linked to, so
//! pairing relies on that link rather than on matching titles. When Yes on one venue
//! plus No on the other costs less than $1 after fees, the difference is locked in,
//! provided both venues resolve the question the same way. Check their rules before
//! trusting a link: differing resolution sources are how a hedge becomes two losses.
//!
//! **This bot reports opportunities and refuses to trade live.** A live version has
//! to send both legs, and the engine's Polymarket order path predates Polymarket's
//! CLOB V2 and hasn't been shown to work against it. One filled leg without the other
//! isn't arbitrage, it's a naked position, so this stays read-only until both legs
//! are proven. The Kalshi leg is unbuilt; Predict links almost no markets to it.

use crate::bot::runtime::{drive, shares_for, Ctx, Strategy, Universe, MIN_ORDER_USD};
use crate::bot::quoting::SCAN_PAGES;
use crate::config::{AppConfig, CrossArbConfig, PredictNetwork};
use crate::models::BookSnapshot;
use crate::service::polymarket_read::{LinkedMarket, PolymarketReader};
use crate::service::venue::{MarketQuery, TradableMarket};
use anyhow::{bail, Result};
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// An unchanged gap is reported again after this long, not on every tick.
const REPORT_EVERY: Duration = Duration::from_secs(300);

pub async fn run(cfg: AppConfig) -> Result<()> {
    if cfg.live_trading_allowed() {
        bail!(
            "cross-arb reports opportunities only and won't run with live trading on: it can't \
             send both legs safely until the Polymarket CLOB V2 order path is proven"
        );
    }
    let polymarket = PolymarketReader::new(&cfg.site.clob_api_base)?;
    let ctx = Ctx::connect(cfg, "Cross-venue arbitrage")?;
    if ctx.cfg.predict_fun.network == PredictNetwork::Testnet {
        warn!(
            "Predict testnet books aren't real money, so gaps against Polymarket here prove the \
             plumbing only: set network to mainnet for real ones"
        );
    }
    let mut bot = CrossArb {
        cfg: ctx.cfg.strategies.cross_arb.clone(),
        polymarket,
        universe: Universe::new(
            MarketQuery {
                variants: Vec::new(),
                max_pages: SCAN_PAGES,
            },
            ctx.cfg.strategies.rescan_secs,
        ),
        links: HashMap::new(),
        unlinkable: HashSet::new(),
        cursor: 0,
        reported: HashMap::new(),
        compared: 0,
        found: 0,
        locked_usd: 0.0,
    };
    drive(&mut bot, &ctx).await
}

struct CrossArb {
    cfg: CrossArbConfig,
    polymarket: PolymarketReader,
    universe: Universe,
    links: HashMap<u64, LinkedMarket>,
    /// Links that can't be used: closed on Polymarket, or different outcomes.
    unlinkable: HashSet<u64>,
    cursor: usize,
    reported: HashMap<u64, Instant>,
    compared: u64,
    found: u64,
    locked_usd: f64,
}

#[async_trait]
impl Strategy for CrossArb {
    fn name(&self) -> &'static str {
        "cross-arb"
    }

    async fn tick(&mut self, ctx: &Ctx) -> Result<()> {
        self.universe.refresh(ctx.data.as_ref()).await?;
        let linked: Vec<TradableMarket> = self
            .universe
            .markets()
            .iter()
            .filter(|m| {
                m.open
                    && !m.contested
                    && m.polymarket_condition_ids.len() == 1
                    && !self.unlinkable.contains(&m.id)
            })
            .cloned()
            .collect();
        if linked.is_empty() {
            return Ok(());
        }

        let count = self.cfg.markets_per_tick.min(linked.len());
        let batch: Vec<TradableMarket> = (0..count)
            .map(|i| linked[(self.cursor + i) % linked.len()].clone())
            .collect();
        self.cursor = (self.cursor + count) % linked.len();

        let ids: Vec<u64> = batch.iter().map(|m| m.id).collect();
        let books = ctx.data.books(&ids).await?;
        for market in batch {
            let Some(book) = books.get(&market.id) else {
                continue;
            };
            if let Err(e) = self.check(ctx, &market, book).await {
                debug!(market = market.id, error = ?e, "couldn't compare with Polymarket");
            }
        }
        Ok(())
    }
}

impl CrossArb {
    async fn check(&mut self, ctx: &Ctx, market: &TradableMarket, book: &BookSnapshot) -> Result<()> {
        if !self.links.contains_key(&market.id) {
            let found = self
                .polymarket
                .linked_market(&market.polymarket_condition_ids[0], &market.yes_name, &market.no_name)
                .await?;
            match found {
                Some(link) => {
                    self.links.insert(market.id, link);
                }
                None => {
                    debug!(market = market.id, "closed on Polymarket, or different outcomes");
                    self.unlinkable.insert(market.id);
                    return Ok(());
                }
            }
        }
        let Some(link) = self.links.get(&market.id) else {
            return Ok(());
        };
        let pm_yes = self.polymarket.book(&link.yes_token).await?;
        let pm_no = self.polymarket.book(&link.no_token).await?;
        self.compared += 1;
        debug!(
            market = market.id,
            predict_bid = book.best_bid().map(|l| l.price),
            predict_ask = book.best_ask().map(|l| l.price),
            polymarket_yes_ask = pm_yes.best_ask().map(|l| l.price),
            polymarket_no_ask = pm_no.best_ask().map(|l| l.price),
            compared = self.compared,
            linked = self.links.len(),
            closed_or_mismatched = self.unlinkable.len(),
            "compared with Polymarket"
        );
        let Some(gap) = best_gap(market, book, &pm_yes, &pm_no, &self.cfg) else {
            return Ok(());
        };
        let due = self.reported.get(&market.id).is_none_or(|t| t.elapsed() >= REPORT_EVERY);
        if !due || !ctx.allowed(market).await {
            return Ok(());
        }
        self.reported.insert(market.id, Instant::now());
        self.found += 1;
        self.locked_usd += gap.locked_usd;
        info!(
            market = market.id,
            question = %market.question,
            polymarket = %link.question,
            route = gap.route,
            cost = gap.cost,
            edge = gap.edge,
            pairs = gap.pairs,
            locked_usd = gap.locked_usd,
            found = self.found,
            found_usd = self.locked_usd,
            "cross-venue gap (not traded)"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Gap {
    route: &'static str,
    cost: f64,
    edge: f64,
    pairs: f64,
    locked_usd: f64,
}

/// The better way to buy both outcomes across the two venues, if it locks in at
/// least `min_edge` after fees and has room for a minimum-size order on both legs.
fn best_gap(
    market: &TradableMarket,
    predict_yes: &BookSnapshot,
    pm_yes: &BookSnapshot,
    pm_no: &BookSnapshot,
    cfg: &CrossArbConfig,
) -> Option<Gap> {
    let predict_no = predict_yes.mirrored();
    let pm_fee = |p: f64| f64::from(cfg.polymarket_fee_bps) / 10_000.0 * p.min(1.0 - p).max(0.0);
    let route = |name: &'static str, predict: &BookSnapshot, polymarket: &BookSnapshot| {
        let (a, b) = (predict.best_ask()?, polymarket.best_ask()?);
        let cost = a.price + b.price;
        let edge = 1.0 - cost - market.taker_fee(a.price) - pm_fee(b.price);
        let pairs = shares_for(cfg.order_usd, cost).min(a.size).min(b.size);
        let big_enough = pairs * a.price >= MIN_ORDER_USD && pairs * b.price >= MIN_ORDER_USD;
        (edge >= cfg.min_edge && big_enough).then(|| Gap {
            route: name,
            cost,
            edge,
            pairs,
            locked_usd: edge * pairs,
        })
    };
    [
        route("yes on predict, no on polymarket", predict_yes, pm_no),
        route("no on predict, yes on polymarket", &predict_no, pm_yes),
    ]
    .into_iter()
    .flatten()
    .max_by(|a, b| a.edge.total_cmp(&b.edge))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::runtime::test_support::{book, market};

    fn cfg() -> CrossArbConfig {
        CrossArbConfig {
            min_edge: 0.02,
            order_usd: 5.0,
            polymarket_fee_bps: 0,
            ..Default::default()
        }
    }

    #[test]
    fn finds_yes_here_and_no_there() {
        // Predict sells Yes at 0.40 while Polymarket sells No at 0.50.
        let predict = book(&[(0.38, 100.0)], &[(0.40, 100.0)]);
        let pm_yes = book(&[(0.48, 100.0)], &[(0.52, 100.0)]);
        let pm_no = book(&[(0.47, 100.0)], &[(0.50, 100.0)]);
        let m = TradableMarket { fee_rate_bps: 0, ..market(1) };
        let gap = best_gap(&m, &predict, &pm_yes, &pm_no, &cfg()).unwrap();
        assert_eq!(gap.route, "yes on predict, no on polymarket");
        assert!((gap.edge - 0.10).abs() < 1e-9);
    }

    #[test]
    fn finds_no_here_and_yes_there_through_the_mirror() {
        // A Predict Yes bid at 0.60 is a No ask at 0.40.
        let predict = book(&[(0.60, 100.0)], &[(0.62, 100.0)]);
        let pm_yes = book(&[(0.48, 100.0)], &[(0.50, 100.0)]);
        let pm_no = book(&[(0.47, 100.0)], &[(0.60, 100.0)]);
        let m = TradableMarket { fee_rate_bps: 0, ..market(1) };
        let gap = best_gap(&m, &predict, &pm_yes, &pm_no, &cfg()).unwrap();
        assert_eq!(gap.route, "no on predict, yes on polymarket");
        assert!((gap.edge - 0.10).abs() < 1e-9);
    }

    #[test]
    fn fees_and_a_small_gap_rule_it_out() {
        let predict = book(&[(0.38, 100.0)], &[(0.48, 100.0)]);
        let pm_yes = book(&[(0.48, 100.0)], &[(0.60, 100.0)]);
        let pm_no = book(&[(0.47, 100.0)], &[(0.50, 100.0)]);
        // 2c gross clears the bar with no fees, and doesn't once fees are paid.
        let free = TradableMarket { fee_rate_bps: 0, ..market(1) };
        assert!(best_gap(&free, &predict, &pm_yes, &pm_no, &cfg()).is_some());
        assert!(best_gap(&market(1), &predict, &pm_yes, &pm_no, &cfg()).is_none());
    }

    #[test]
    fn a_leg_too_thin_for_the_venue_minimum_is_no_opportunity() {
        let predict = book(&[(0.38, 100.0)], &[(0.40, 1.0)]);
        let pm_yes = book(&[(0.48, 100.0)], &[(0.52, 100.0)]);
        let pm_no = book(&[(0.47, 100.0)], &[(0.50, 100.0)]);
        let m = TradableMarket { fee_rate_bps: 0, ..market(1) };
        assert!(best_gap(&m, &predict, &pm_yes, &pm_no, &cfg()).is_none());
    }
}
