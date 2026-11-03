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
