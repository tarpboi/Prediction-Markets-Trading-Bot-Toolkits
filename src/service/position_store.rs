//! Open-position tracking + per-category / per-tag exposure accounting.
//!
//! Single source of truth for "what are we currently long/short?". The order
//! executor inserts on successful entries; the position monitor reads
//! snapshots and removes on exit. Exposure totals are derived on demand —
//! cheap because the working set is bounded by `max_open_positions`-ish.

use crate::models::Side;
use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct OpenPosition {
    pub token_id: String,
    pub slug: String,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub side: Side,
    pub entry_price: f64,
    pub shares: f64,
    pub usd_notional: f64,
    pub take_profit_pct: f64,
    pub stop_loss_pct: f64,
    pub opened_at: DateTime<Utc>,
}

impl OpenPosition {
    /// Current unrealised P&L percentage given a midprice quote.
    ///
    /// For BUY entries (long the outcome), gain = +(midprice - entry) / entry.
    /// For SELL entries (rare on Polymarket, short the outcome), gain is
    /// inverted.
    pub fn pnl_pct(&self, midprice: f64) -> f64 {
        if self.entry_price <= 0.0 {
            return 0.0;
        }
        let raw = (midprice - self.entry_price) / self.entry_price * 100.0;
        match self.side {
            Side::Buy => raw,
            Side::Sell => -raw,
        }
    }
}

#[derive(Debug, Default)]
pub struct PositionStore {
    inner: RwLock<HashMap<String, OpenPosition>>,
}

impl PositionStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn open(&self, pos: OpenPosition) {
        self.inner.write().insert(pos.token_id.clone(), pos);
    }

    pub fn close(&self, token_id: &str) -> Option<OpenPosition> {
        self.inner.write().remove(token_id)
    }

    /// Take `shares` off a position after a partial exit.
    pub fn reduce(&self, token_id: &str, shares: f64) {
        if let Some(pos) = self.inner.write().get_mut(token_id) {
            let left = (pos.shares - shares).max(0.0);
            if pos.shares > 0.0 {
                pos.usd_notional *= left / pos.shares;
            }
            pos.shares = left;
        }
    }

    pub fn get(&self, token_id: &str) -> Option<OpenPosition> {
        self.inner.read().get(token_id).cloned()
    }

    pub fn snapshot(&self) -> Vec<OpenPosition> {
        self.inner.read().values().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.read().is_empty()
    }

    pub fn open_usd_by_category(&self, category: &str) -> f64 {
        self.inner
            .read()
            .values()
            .filter(|p| {
                p.category
                    .as_ref()
                    .map(|c| ci_eq(c, category))
                    .unwrap_or(false)
            })
            .map(|p| p.usd_notional)
            .sum()
    }

    pub fn open_usd_by_tag(&self, tag: &str) -> f64 {
        self.inner
            .read()
            .values()
            .filter(|p| p.tags.iter().any(|t| ci_eq(t, tag)))
            .map(|p| p.usd_notional)
            .sum()
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

    fn pos(slug: &str, cat: &str, tags: &[&str], usd: f64) -> OpenPosition {
        OpenPosition {
            token_id: format!("tok-{slug}"),
            slug: slug.into(),
            category: Some(cat.into()),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            side: Side::Buy,
            entry_price: 0.50,
            shares: usd * 2.0,
            usd_notional: usd,
            take_profit_pct: 50.0,
            stop_loss_pct: 30.0,
            opened_at: Utc::now(),
        }
    }

    #[test]
    fn category_exposure_sums_only_same_category() {
        let s = PositionStore::default();
        s.open(pos("a", "Politics", &[], 100.0));
        s.open(pos("b", "Politics", &[], 50.0));
        s.open(pos("c", "Crypto", &[], 200.0));
        assert_eq!(s.open_usd_by_category("Politics"), 150.0);
        assert_eq!(s.open_usd_by_category("crypto"), 200.0);
    }

    #[test]
    fn tag_exposure_sums_across_multiple_tags() {
        let s = PositionStore::default();
        s.open(pos("a", "Politics", &["election2024", "us"], 100.0));
        s.open(pos("b", "Politics", &["us"], 50.0));
        assert_eq!(s.open_usd_by_tag("us"), 150.0);
        assert_eq!(s.open_usd_by_tag("election2024"), 100.0);
    }

    #[test]
    fn pnl_pct_buy_long_direction() {
        let mut p = pos("a", "X", &[], 100.0);
        p.entry_price = 0.50;
        assert!((p.pnl_pct(0.75) - 50.0).abs() < 1e-9);
        assert!((p.pnl_pct(0.25) + 50.0).abs() < 1e-9);
    }

    #[test]
    fn pnl_pct_sell_short_direction() {
        let mut p = pos("a", "X", &[], 100.0);
        p.side = Side::Sell;
        p.entry_price = 0.50;
        assert!((p.pnl_pct(0.25) - 50.0).abs() < 1e-9);
    }
}
