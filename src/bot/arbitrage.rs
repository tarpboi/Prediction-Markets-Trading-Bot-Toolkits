//! BTC Up/Down rounds: buy when the book lags the underlying.
//!
//! A round is a digital option on the price feed. Its value follows from spot, the
//! round's start price and the time left, so when the book is slower than spot a
//! taker can buy below that value. Despite the name this isn't riskless: a position
//! is held to the end of the round and can lose everything it cost. The edge is
//! being right on average, and the model is a plain driftless lognormal with
//! trailing volatility, not a fitted one.
//!
//! Rounds settle on Binance's top of book. Where Binance isn't reachable the bot
//! still runs on a Coinbase price to exercise the path, but only ever on paper:
//! the feeds differ by enough to swamp a small edge.

use crate::bot::quoting::Known;
use crate::bot::runtime::{drive, shares_for, Ctx, Strategy, TakeResult};
use crate::config::{AppConfig, BtcArbConfig};
use crate::models::{BookSnapshot, Side};
use crate::service::spot::{digital_up, EwmaVol, SpotFeed, SpotSource};
use crate::service::venue::TradableMarket;
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

const VOL_HALF_LIFE_SECS: f64 = 300.0;
/// Spot samples before the volatility estimate is trusted.
const MIN_VOL_SAMPLES: u32 = 20;
/// A round's start price appears when it starts, so the list can't go stale for long.
const ROUND_RESCAN: Duration = Duration::from_secs(15);

pub async fn run(cfg: AppConfig) -> Result<()> {
    let ctx = Ctx::connect(cfg, "BTC arbitrage")?;
    let mut bot = RoundTaker {
        cfg: ctx.cfg.strategies.btc_arb.clone(),
        spot: SpotFeed::new()?,
        vols: HashMap::new(),
        known: Known::default(),
        rounds: Vec::new(),
        last_scan: None,
        entered: HashSet::new(),
    };
    drive(&mut bot, &ctx).await
}

struct RoundTaker {
    cfg: BtcArbConfig,
    spot: SpotFeed,
    vols: HashMap<String, EwmaVol>,
    known: Known,
    rounds: Vec<TradableMarket>,
    last_scan: Option<Instant>,
    /// Markets already bought: one entry a round, so a wrong model can't average
    /// into a losing round.
    entered: HashSet<u64>,
}

#[async_trait]
impl Strategy for RoundTaker {
    fn name(&self) -> &'static str {
        "btc-arb"
    }

    async fn tick(&mut self, ctx: &Ctx) -> Result<()> {
        if self.last_scan.is_none_or(|t| t.elapsed() >= ROUND_RESCAN) {
            let rounds = ctx.client.live_rounds().await?;
            self.known.remember(&rounds);
            let now = Utc::now();
            self.rounds = rounds
                .into_iter()
                .filter(|m| {
                    m.open
                        && !m.contested
                        && m.start_price.is_some()
                        && m.starts_at.is_some_and(|s| s <= now)
                        && m.feed_symbol.as_ref().is_some_and(|s| self.cfg.symbols.contains(s))
                })
                .collect();
            self.last_scan = Some(Instant::now());
            info!(live_rounds = self.rounds.len(), "up/down rounds read");
        }

        let at_secs = Utc::now().timestamp_millis() as f64 / 1000.0;
        let mut quotes = HashMap::new();
        for symbol in self.cfg.symbols.clone() {
            let quote = self.spot.quote(&symbol).await?;
            self.vols
                .entry(symbol.clone())
                .or_insert_with(|| EwmaVol::new(VOL_HALF_LIFE_SECS))
                .update(quote.price, at_secs);
            quotes.insert(symbol, quote);
        }

        let ids: Vec<u64> = self.rounds.iter().map(|m| m.id).collect();
        let books = self.known.observe(ctx, &ids).await?;
        if ctx.broker.is_halted() {
            return Ok(());
        }

        let now = Utc::now();
        for market in self.rounds.clone() {
            if self.entered.contains(&market.id) {
                continue;
            }
            let (Some(symbol), Some(book)) = (market.feed_symbol.as_ref(), books.get(&market.id))
            else {
                continue;
            };
            let (Some(quote), Some(vol)) = (quotes.get(symbol), self.vols.get(symbol)) else {
                continue;
            };
            if vol.samples() < MIN_VOL_SAMPLES {
                continue;
            }
            if ctx.broker.is_live() && quote.source != SpotSource::Binance {
                warn!("no Binance price: not trading live on a substitute feed");
                continue;
            }
            let sigma = vol.vol(self.cfg.vol_floor);
            debug!(
                market = market.id,
                spot = quote.price,
                strike = market.start_price,
                sigma,
                secs_left = market.secs_left(now),
                fair_up = market.secs_left(now).zip(market.start_price).map(|(left, strike)| {
                    digital_up(quote.price, strike, sigma, left as f64)
                }),
                bid = book.best_bid().map(|l| l.price),
                ask = book.best_ask().map(|l| l.price),
                "round valued"
            );
            let Some(entry) = round_entry(&market, book, quote.price, sigma, &self.cfg, now) else {
                continue;
            };
            let shares = shares_for(self.cfg.order_usd, entry.price);
            info!(
                market = market.id,
                fair = entry.fair,
                price = entry.price,
                edge = entry.edge,
                spot = quote.price,
                source = ?quote.source,
                "book below model value"
            );
            match ctx
                .broker
                .take(&market, &entry.token, Side::Buy, shares, entry.price, book)
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

#[derive(Debug, Clone, PartialEq)]
struct RoundEntry {
    token: String,
    price: f64,
    fair: f64,
    edge: f64,
}

/// The side worth buying in a live round, if either clears `min_edge` after fees.
fn round_entry(
    market: &TradableMarket,
    book: &BookSnapshot,
    spot: f64,
    vol: f64,
    cfg: &BtcArbConfig,
    now: DateTime<Utc>,
) -> Option<RoundEntry> {
    let secs_left = market.secs_left(now)?;
    if secs_left < cfg.min_secs_left {
        return None;
    }
    let strike = market.start_price?;
    let fair_up = digital_up(spot, strike, vol, secs_left as f64);

    let up = book.best_ask().map(|ask| RoundEntry {
        token: market.yes_token.clone(),
        price: ask.price,
        fair: fair_up,
        edge: fair_up - ask.price - market.taker_fee(ask.price),
    });
    let down = book.best_bid().map(|bid| {
        let price = 1.0 - bid.price;
        RoundEntry {
            token: market.no_token.clone(),
            price,
            fair: 1.0 - fair_up,
            edge: 1.0 - fair_up - price - market.taker_fee(price),
        }
    });
    [up, down]
        .into_iter()
        .flatten()
        .filter(|e| e.edge >= cfg.min_edge)
        .max_by(|a, b| a.edge.total_cmp(&b.edge))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::runtime::test_support::{book, market};

    fn cfg() -> BtcArbConfig {
        BtcArbConfig {
            min_edge: 0.03,
            min_secs_left: 30,
            ..Default::default()
        }
    }

    fn round(now: DateTime<Utc>, secs: i64) -> TradableMarket {
        TradableMarket {
            start_price: Some(100.0),
            ends_at: Some(now + chrono::Duration::seconds(secs)),
            ..market(1)
        }
    }

    #[test]
    fn buys_up_when_spot_has_moved_and_the_book_hasnt() {
        let now = Utc::now();
        let stale = book(&[(0.48, 50.0)], &[(0.52, 50.0)]);
        let e = round_entry(&round(now, 300), &stale, 100.1, 0.0001, &cfg(), now).unwrap();
        assert_eq!(e.token, "yes-1");
        assert_eq!(e.price, 0.52);
        assert!(e.fair > 0.7 && e.edge > 0.03);
    }

    #[test]
    fn buys_down_through_the_mirrored_book_when_spot_has_fallen() {
        let now = Utc::now();
        let stale = book(&[(0.48, 50.0)], &[(0.52, 50.0)]);
        let e = round_entry(&round(now, 300), &stale, 99.9, 0.0001, &cfg(), now).unwrap();
        assert_eq!(e.token, "no-1");
        assert!((e.price - 0.52).abs() < 1e-9);
    }

    #[test]
    fn a_book_in_line_with_spot_is_left_alone() {
        let now = Utc::now();
        let fair = book(&[(0.48, 50.0)], &[(0.52, 50.0)]);
        assert!(round_entry(&round(now, 300), &fair, 100.0, 0.0001, &cfg(), now).is_none());
    }

    #[test]
    fn fees_count_against_the_edge() {
        let now = Utc::now();
        let book = book(&[(0.3, 50.0)], &[(0.5, 50.0)]);
        let free = TradableMarket { fee_rate_bps: 0, ..round(now, 300) };
        let dear = TradableMarket { fee_rate_bps: 2000, ..round(now, 300) };
        // Value about 0.533 against a 0.50 ask: only clears without the fee.
        let spot = 100.0 + 0.000_01 * 100.0 * 100.0;
        let vol = 0.0005;
        let fair = digital_up(spot, 100.0, vol, 300.0);
        assert!(fair - 0.5 > 0.03 && fair - 0.5 < 0.1, "{fair}");
        assert!(round_entry(&free, &book, spot, vol, &cfg(), now).is_some());
        assert!(round_entry(&dear, &book, spot, vol, &cfg(), now).is_none());
    }

    #[test]
    fn stays_out_near_the_end_and_before_the_start_price_is_known() {
        let now = Utc::now();
        let stale = book(&[(0.48, 50.0)], &[(0.52, 50.0)]);
        assert!(round_entry(&round(now, 10), &stale, 100.1, 0.0001, &cfg(), now).is_none());
        let unstarted = TradableMarket { start_price: None, ..round(now, 300) };
        assert!(round_entry(&unstarted, &stale, 100.1, 0.0001, &cfg(), now).is_none());
    }
}
