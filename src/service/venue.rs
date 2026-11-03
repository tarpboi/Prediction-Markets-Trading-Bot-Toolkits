//! The seam between the shared pipeline and a venue.
//!
//! Sizing, eligibility, exposure, risk and TP/SL are venue-neutral. A venue
//! supplies market metadata ([`MarketSource`]), order placement
//! ([`OrderVenue`]) and quotes ([`crate::service::midprice::MidpriceSource`]).

use crate::models::{PlannedOrder, VenueId};
use crate::service::market_cache::MarketInfo;
use anyhow::Result;
use async_trait::async_trait;

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
