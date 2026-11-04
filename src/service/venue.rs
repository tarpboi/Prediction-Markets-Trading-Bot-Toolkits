//! The seam between the shared pipeline and a venue.
//!
//! Sizing, eligibility, exposure, risk and TP/SL are venue-neutral. A venue
//! supplies market metadata ([`MarketSource`]), order placement
//! ([`OrderVenue`]) and quotes ([`crate::service::midprice::MidpriceSource`]).

use crate::models::{BookSnapshot, PlannedOrder, Side, VenueId};
use crate::service::market_cache::MarketInfo;
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::collections::HashMap;

#[async_trait]
pub trait MarketSource: Send + Sync {
    /// Resolve an outcome token id to its market metadata.
    async fn by_token_id(&self, token_id: &str) -> Result<MarketInfo>;
}

/// What a venue did with a [`PlannedOrder`].
#[derive(Debug, Clone)]
pub struct OrderTicket {
    pub token_id: String,
    pub maker_amount: String,
    pub taker_amount: String,
    /// `None` until the venue has accepted the order.
    pub order_id: Option<String>,
    /// Shares the venue confirmed filled. `None` when the venue doesn't report
    /// fills synchronously, in which case callers assume the planned size.
    pub filled_shares: Option<f64>,
}

#[async_trait]
pub trait OrderVenue: Send + Sync {
    fn id(&self) -> VenueId;

    /// Build and sign `planned`. Transmits it only when `live` is true, so a
    /// dry run exercises everything up to the wire.
    async fn place(
        &self,
        planned: &PlannedOrder,
        expiration_secs: u64,
        live: bool,
    ) -> Result<OrderTicket>;
}

/// A market as the strategy bots see it.
#[derive(Debug, Clone, Default)]
pub struct TradableMarket {
    pub id: u64,
    pub question: String,
    pub category_slug: String,
    pub variant: String,
    pub yes_token: String,
    pub no_token: String,
    pub yes_name: String,
    pub no_name: String,
    /// Accepting orders now: open for matching and not past its end.
    pub open: bool,
    /// Under an oracle dispute or paused, whatever the book says.
    pub contested: bool,
    pub tick: f64,
    pub fee_rate_bps: u32,
    pub starts_at: Option<DateTime<Utc>>,
    pub ends_at: Option<DateTime<Utc>>,
    pub start_price: Option<f64>,
    pub end_price: Option<f64>,
    pub feed_symbol: Option<String>,
    pub polymarket_condition_ids: Vec<String>,
    /// What one Yes share pays, once that's known.
    pub payout_yes: Option<f64>,
    /// Category and tags, when they came along with the market.
    pub labels: Option<(Option<String>, Vec<String>)>,
}

impl TradableMarket {
    /// Taker fee per share at `price`. Makers pay none.
    pub fn taker_fee(&self, price: f64) -> f64 {
        f64::from(self.fee_rate_bps) / 10_000.0 * price.min(1.0 - price).max(0.0)
    }

    pub fn secs_left(&self, now: DateTime<Utc>) -> Option<i64> {
        self.ends_at.map(|end| (end - now).num_seconds())
    }

    pub fn is_yes(&self, token: &str) -> bool {
        self.yes_token == token
    }
}

#[derive(Debug, Clone, Default)]
pub struct MarketQuery {
    /// Those variants, with their schedules. Empty means the most traded markets
    /// of any variant, without schedules: see [`MarketData::with_schedule`].
    pub variants: Vec<String>,
    pub max_pages: u32,
}

/// Read side the strategy bots share. Books are quoted for the Yes outcome.
#[async_trait]
pub trait MarketData: Send + Sync {
    async fn markets(&self, query: &MarketQuery) -> Result<Vec<TradableMarket>>;
    async fn market(&self, id: u64) -> Result<TradableMarket>;
    /// `market` with its end time, category and tags filled in. A scan leaves these
    /// out so that it doesn't need a request per market.
    async fn with_schedule(&self, market: &TradableMarket) -> Result<TradableMarket>;
    async fn books(&self, ids: &[u64]) -> Result<HashMap<u64, BookSnapshot>>;
}

#[derive(Debug, Clone)]
pub struct LimitOrder {
    pub market_id: u64,
    pub token_id: String,
    pub side: Side,
    pub shares: f64,
    pub price: f64,
    pub ttl_secs: u64,
    pub post_only: bool,
}

#[derive(Debug, Clone)]
pub struct PostedOrder {
    pub order_hash: String,
    /// `None` on a dry run, where nothing reached the venue.
    pub order_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoveResult {
    pub removed: Vec<String>,
    /// Already filled, removed or cancelled.
    pub noop: Vec<String>,
    /// Inside the market's removal lock; retry later.
    pub rejected: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RestingState {
    pub open: bool,
    pub filled_shares: f64,
}

#[derive(Debug, Clone)]
pub struct HeldPosition {
    pub market: TradableMarket,
    pub token_id: String,
    pub shares: f64,
    pub avg_price: f64,
}

/// Resting orders, on top of the taker path in [`OrderVenue`].
#[async_trait]
pub trait LimitVenue: Send + Sync {
    /// Build and sign `order`. Transmits it only when `live` is true.
    async fn post_limit(&self, order: &LimitOrder, live: bool) -> Result<PostedOrder>;
    /// Take orders off the book. The venue says this doesn't cancel them on-chain.
    async fn remove(&self, order_ids: &[String]) -> Result<RemoveResult>;
    async fn order_state(&self, order_hash: &str) -> Result<RestingState>;
    async fn positions(&self) -> Result<Vec<HeldPosition>>;
}
