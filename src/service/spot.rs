//! Underlying prices for up/down rounds, and the model that values them.
//!
//! Predict's rounds settle on the mid of Binance's top of book, so Binance is the
//! only feed that measures the right thing. Coinbase is a fallback for places
//! Binance refuses: its USD pairs sit a few basis points away because of the
//! USDT/USD basis, so a value computed from one is good enough to watch but not
//! to trade on.

use anyhow::{anyhow, bail, Context, Result};
use reqwest::Client;
use serde::Deserialize;
use std::time::{Duration, Instant};
use tracing::warn;

const BINANCE: &str = "https://api.binance.com/api/v3/ticker/bookTicker";
const COINBASE: &str = "https://api.coinbase.com/v2/prices";
/// Once Binance has refused us, don't ask again for a while.
const BINANCE_RETRY_AFTER: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpotSource {
    Binance,
    Coinbase,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpotQuote {
    pub price: f64,
    pub source: SpotSource,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BinanceTicker {
    bid_price: String,
    ask_price: String,
}

#[derive(Debug, Deserialize)]
struct CoinbaseEnvelope {
    data: CoinbasePrice,
}

#[derive(Debug, Deserialize)]
struct CoinbasePrice {
    amount: String,
}

pub struct SpotFeed {
    http: Client,
    binance_refused_at: Option<Instant>,
}

impl SpotFeed {
    pub fn new() -> Result<Self> {
        Ok(Self {
            http: Client::builder()
                .user_agent("polymarket-toolkits/0.1")
                .timeout(Duration::from_secs(5))
                .build()?,
            binance_refused_at: None,
        })
    }

    /// `symbol` as Predict's feeds name it, e.g. `BTCUSDT`.
    pub async fn quote(&mut self, symbol: &str) -> Result<SpotQuote> {
        if self
            .binance_refused_at
            .is_none_or(|t| t.elapsed() >= BINANCE_RETRY_AFTER)
        {
            match self.binance(symbol).await {
                Ok(price) => {
                    self.binance_refused_at = None;
                    return Ok(SpotQuote {
                        price,
                        source: SpotSource::Binance,
                    });
                }
                Err(e) => {
                    warn!(error = ?e, "Binance unavailable; falling back to Coinbase, which can't be traded on");
                    self.binance_refused_at = Some(Instant::now());
                }
            }
        }
        Ok(SpotQuote {
            price: self.coinbase(symbol).await?,
            source: SpotSource::Coinbase,
        })
    }

    async fn binance(&self, symbol: &str) -> Result<f64> {
        let resp = self
            .http
            .get(format!("{BINANCE}?symbol={symbol}"))
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("HTTP {}", resp.status().as_u16());
        }
        let ticker: BinanceTicker = resp.json().await?;
        let bid: f64 = ticker.bid_price.parse()?;
        let ask: f64 = ticker.ask_price.parse()?;
        Ok((bid + ask) / 2.0)
    }

    async fn coinbase(&self, symbol: &str) -> Result<f64> {
        let pair = coinbase_pair(symbol)?;
        let price: CoinbaseEnvelope = self
            .http
            .get(format!("{COINBASE}/{pair}/spot"))
            .send()
            .await
            .context("Coinbase price")?
            .error_for_status()?
            .json()
            .await?;
        Ok(price.data.amount.parse()?)
    }
}

fn coinbase_pair(symbol: &str) -> Result<String> {
    let base = symbol
        .strip_suffix("USDT")
        .or_else(|| symbol.strip_suffix("USD"))
        .ok_or_else(|| anyhow!("can't map {symbol} to a Coinbase pair"))?;
    Ok(format!("{}-USD", base.trim_end_matches('_')))
}

/// Exponentially weighted volatility of log returns, per root second.
#[derive(Debug, Clone)]
pub struct EwmaVol {
    half_life_secs: f64,
    variance_per_sec: Option<f64>,
    last: Option<(f64, f64)>,
    samples: u32,
}

impl EwmaVol {
    pub fn new(half_life_secs: f64) -> Self {
        Self {
            half_life_secs,
            variance_per_sec: None,
            last: None,
            samples: 0,
        }
    }

    pub fn update(&mut self, price: f64, at_secs: f64) {
        if price <= 0.0 {
            return;
        }
        if let Some((last_price, last_at)) = self.last {
            let dt = at_secs - last_at;
            if dt <= 0.0 {
                return;
            }
            let ret = (price / last_price).ln();
            let sample = ret * ret / dt;
            let weight = 1.0 - 0.5f64.powf(dt / self.half_life_secs);
            self.variance_per_sec = Some(match self.variance_per_sec {
                Some(v) => v + weight * (sample - v),
                None => sample,
            });
            self.samples += 1;
        }
        self.last = Some((price, at_secs));
    }

    pub fn samples(&self) -> u32 {
        self.samples
    }

    pub fn vol(&self, floor: f64) -> f64 {
        self.variance_per_sec.map_or(floor, |v| v.sqrt().max(floor))
    }
}

/// Standard normal CDF (Abramowitz & Stegun 26.2.17).
pub fn normal_cdf(x: f64) -> f64 {
    if x < 0.0 {
        return 1.0 - normal_cdf(-x);
    }
    let t = 1.0 / (1.0 + 0.2316419 * x);
    let poly = t
        * (0.319381530
            + t * (-0.356563782 + t * (1.781477937 + t * (-1.821255978 + t * 1.330274429))));
    let pdf = (-0.5 * x * x).exp() / (2.0 * std::f64::consts::PI).sqrt();
    1.0 - pdf * poly
}

/// Chance the price ends above `strike`, under a driftless lognormal model.
pub fn digital_up(spot: f64, strike: f64, vol_per_root_sec: f64, secs_left: f64) -> f64 {
    if secs_left <= 0.0 || vol_per_root_sec <= 0.0 {
        return if spot > strike {
            1.0
        } else if spot < strike {
            0.0
        } else {
            0.5
        };
    }
    let total = vol_per_root_sec * secs_left.sqrt();
    normal_cdf((spot / strike).ln() / total - total / 2.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_cdf_matches_known_values() {
        assert!((normal_cdf(0.0) - 0.5).abs() < 1e-6);
        assert!((normal_cdf(1.96) - 0.975).abs() < 1e-3);
        assert!((normal_cdf(-1.96) - 0.025).abs() < 1e-3);
    }

    #[test]
    fn at_the_money_is_a_coin_flip_and_moves_with_spot() {
        let flat = digital_up(100.0, 100.0, 0.0001, 300.0);
        assert!((flat - 0.5).abs() < 0.001);
        assert!(digital_up(100.1, 100.0, 0.0001, 300.0) > 0.7);
        assert!(digital_up(99.9, 100.0, 0.0001, 300.0) < 0.3);
    }

    #[test]
    fn value_converges_as_the_round_ends() {
        let far = digital_up(100.05, 100.0, 0.0001, 600.0);
        let near = digital_up(100.05, 100.0, 0.0001, 10.0);
        assert!(near > far);
        assert_eq!(digital_up(100.05, 100.0, 0.0001, 0.0), 1.0);
        assert_eq!(digital_up(100.0, 100.0, 0.0001, 0.0), 0.5);
    }

    #[test]
    fn ewma_tracks_a_known_volatility_and_respects_the_floor() {
        let mut vol = EwmaVol::new(60.0);
        assert_eq!(vol.vol(0.00005), 0.00005);
        // A steady 0.1% move every 10 seconds is 0.001 / sqrt(10) per root second.
        let mut price = 100.0;
        for i in 0..200 {
            price *= if i % 2 == 0 { 1.001 } else { 1.0 / 1.001 };
            vol.update(price, f64::from(i) * 10.0);
        }
        let expected = 0.001 / 10f64.sqrt();
        assert!((vol.vol(0.0) - expected).abs() < expected * 0.02, "{}", vol.vol(0.0));
        assert_eq!(vol.vol(0.01), 0.01);
    }

    #[test]
    fn coinbase_pairs_map_from_feed_symbols() {
        assert_eq!(coinbase_pair("BTCUSDT").unwrap(), "BTC-USD");
        assert_eq!(coinbase_pair("BTC_USD").unwrap(), "BTC-USD");
        assert!(coinbase_pair("nonsense").is_err());
    }
}
