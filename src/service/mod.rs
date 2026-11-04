//! Shared services consumed by every bot:
//!
//! - [`strategy`]         — copy-sizing strategies (percentage / fixed / adaptive)
//! - [`risk_guard`]       — circuit breaker + depth check
//! - [`market_cache`]     — slug ↔ CLOB token-id ↔ category/tags resolution
//! - [`onchain`]          — Polygon WebSocket subscription
//! - [`parse`]            — ABI log decoding for Polymarket exchange events
//! - [`clob`]             — Polymarket CLOB v2 client: EIP-712 signing, order POST
//! - [`order_executor`]   — applies sizing, eligibility, exposure, risk, and dispatch
//! - [`eligibility`]      — allowlist/blocklist filter for slugs, categories, tags
//! - [`position_store`]   — open-position tracking + exposure totals
//! - [`midprice`]         — `/midpoint` HTTP client (trait, swappable)
//! - [`position_monitor`] — TP/SL polling loop, posts exit orders through the venue
//! - [`venue`]            — traits a venue implements to plug into the pipeline
//! - [`predict_fun`]      — Predict.fun adapter: REST feed, signing, order POST

pub mod clob;
pub mod eligibility;
pub mod market_cache;
pub mod midprice;
pub mod onchain;
pub mod order_executor;
pub mod parse;
pub mod position_monitor;
pub mod position_store;
pub mod polymarket_read;
pub mod predict_fun;
pub mod risk_guard;
pub mod spot;
pub mod strategy;
pub mod venue;
