//! Shared data types used across venue adapters, services, and bots.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn as_u8(self) -> u8 {
        match self {
            Side::Buy => 0,
            Side::Sell => 1,
        }
    }

    pub fn flip(self) -> Self {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VenueId {
    #[default]
    Polymarket,
    PredictFun,
    Kalshi,
    Limitless,
}

/// Internal canonical view of a whale trade observed on-chain or via API.
#[derive(Debug, Clone)]
pub struct WhaleTrade {
    pub venue: VenueId,
    pub maker: String,
    pub side: Side,
    pub token_id: String,
    pub shares: f64,
    pub price: f64,
    pub usd_notional: f64,
    pub tx_hash: Option<String>,
    pub block_number: Option<u64>,
    pub observed_at: chrono::DateTime<chrono::Utc>,
}

impl WhaleTrade {
    pub fn is_buy(&self) -> bool {
        self.side == Side::Buy
    }
}

/// A decided order ready for execution. Sizing + risk gates have already passed.
#[derive(Debug, Clone)]
pub struct PlannedOrder {
    pub venue: VenueId,
    pub token_id: String,
    pub side: Side,
    pub shares: f64,
    pub limit_price: f64,
    pub usd_notional: f64,
    pub order_type: OrderType,
    pub source_trade_hash: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderType {
    /// Fill-or-kill (Polymarket label FOK in the SDK).
    Fak,
    /// Good-till-date — limit order with explicit expiration.
    Gtd,
    /// Good-till-cancel — limit order with no expiration.
    Gtc,
}

/// Top-of-book snapshot used by the depth guard.
#[derive(Debug, Clone, Default)]
pub struct BookSnapshot {
    pub bids: Vec<BookLevel>,
    pub asks: Vec<BookLevel>,
}

#[derive(Debug, Clone, Copy)]
pub struct BookLevel {
    pub price: f64,
    pub size: f64,
}

impl BookSnapshot {
    pub fn best_bid(&self) -> Option<&BookLevel> {
        self.bids.first()
    }
    pub fn best_ask(&self) -> Option<&BookLevel> {
        self.asks.first()
    }

    pub fn mid(&self) -> Option<f64> {
        Some((self.best_bid()?.price + self.best_ask()?.price) / 2.0)
    }

    pub fn spread(&self) -> Option<f64> {
        Some(self.best_ask()?.price - self.best_bid()?.price)
    }

    /// Mid weighted toward the thinner side of the touch, where price tends to go.
    pub fn microprice(&self) -> Option<f64> {
        let (bid, ask) = (self.best_bid()?, self.best_ask()?);
        let depth = bid.size + ask.size;
        if depth <= 0.0 {
            return self.mid();
        }
        Some((bid.price * ask.size + ask.price * bid.size) / depth)
    }

    /// Size imbalance over the top `levels`, from -1 (all asks) to 1 (all bids).
    pub fn imbalance(&self, levels: usize) -> Option<f64> {
        let bids: f64 = self.bids.iter().take(levels).map(|l| l.size).sum();
        let asks: f64 = self.asks.iter().take(levels).map(|l| l.size).sum();
        if self.bids.is_empty() || self.asks.is_empty() || bids + asks <= 0.0 {
            return None;
        }
        Some((bids - asks) / (bids + asks))
    }

    /// The other outcome's view of a Yes-quoted book: a Yes bid at `p` is a No
    /// ask at `1 - p`, and vice versa.
    pub fn mirrored(&self) -> Self {
        let flip = |levels: &[BookLevel]| -> Vec<BookLevel> {
            levels
                .iter()
                .map(|l| BookLevel {
                    // Rounded so `1 - 0.07` is 0.93: a limit read from this level
                    // has to match the price the venue will compute exactly.
                    price: ((1.0 - l.price) * 1e9).round() / 1e9,
                    size: l.size,
                })
                .collect()
        };
        Self {
            bids: flip(&self.asks),
            asks: flip(&self.bids),
        }
    }

    /// USD-equivalent depth on a given side within `levels` from the top.
    pub fn depth_usd(&self, side: Side, levels: usize) -> f64 {
        let book = match side {
            Side::Buy => &self.asks,
            Side::Sell => &self.bids,
        };
        book.iter()
            .take(levels)
            .map(|lv| lv.price * lv.size)
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirrored_prices_are_exact_decimals() {
        let book = BookSnapshot {
            bids: vec![BookLevel { price: 0.07, size: 1.0 }],
            asks: vec![BookLevel { price: 0.01, size: 1.0 }],
        };
        let no = book.mirrored();
        assert_eq!(no.best_ask().unwrap().price, 0.93);
        assert_eq!(no.best_bid().unwrap().price, 0.99);
    }
}
