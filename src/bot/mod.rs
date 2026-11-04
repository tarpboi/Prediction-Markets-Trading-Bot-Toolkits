//! Bot dispatch.
//!
//! Each bot has its own file in this directory. [`runtime`] holds what they share,
//! including the hard limits every order passes through. Copy trading runs on
//! Polymarket and Predict.fun; the others need market listing and resting orders,
//! which only the Predict.fun adapter has so far. All default to dry run, and no
//! live order has been sent through any of them yet.

pub mod arbitrage;
pub mod copy_trading;
pub mod cross_market_arb;
pub mod directional_arb;
pub mod market_maker;
pub mod orderbook_imbalance;
pub mod quoting;
pub mod resolution_sniper;
pub mod runtime;
pub mod spread_farming;
pub mod sports_execution;
pub mod whale_signal;

use crate::config::AppConfig;
use anyhow::Result;

#[derive(Debug, Clone, Copy)]
pub enum BotKind {
    CopyTrading,
    BtcArb,
    CrossArb,
    DirectionalArb,
    SpreadFarming,
    Sports,
    ResolutionSniper,
    OrderbookImbalance,
    MarketMaking,
    WhaleSignal,
}

impl BotKind {
    pub fn label(self) -> &'static str {
        match self {
            BotKind::CopyTrading => "Copy Trading",
            BotKind::BtcArb => "BTC 5m / 15m / 1hr Arbitrage",
            BotKind::CrossArb => "Predict.fun ↔ Polymarket Cross-Venue Arb",
            BotKind::DirectionalArb => "Directional Arbitrage",
            BotKind::SpreadFarming => "Spread Farming",
            BotKind::Sports => "Sports Betting Execution",
            BotKind::ResolutionSniper => "Resolution Sniper",
            BotKind::OrderbookImbalance => "Orderbook Imbalance",
            BotKind::MarketMaking => "Market Making",
            BotKind::WhaleSignal => "On-Chain Whale Signal",
        }
    }

    /// Honest status for the TUI. Nothing has traded live through this engine yet.
    pub fn status(self) -> &'static str {
        match self {
            BotKind::CrossArb => "Reports gaps only: won't trade live",
            BotKind::Sports => "Manual tool, dry-run tested: live orders untested",
            _ => "Dry-run tested: live orders untested",
        }
    }
}

pub async fn run(kind: BotKind, cfg: AppConfig) -> Result<()> {
    match kind {
        BotKind::CopyTrading => copy_trading::run(cfg).await,
        BotKind::BtcArb => arbitrage::run(cfg).await,
        BotKind::CrossArb => cross_market_arb::run(cfg).await,
        BotKind::DirectionalArb => directional_arb::run(cfg).await,
        BotKind::SpreadFarming => spread_farming::run(cfg).await,
        BotKind::Sports => sports_execution::run(cfg).await,
        BotKind::ResolutionSniper => resolution_sniper::run(cfg).await,
        BotKind::OrderbookImbalance => orderbook_imbalance::run(cfg).await,
        BotKind::MarketMaking => market_maker::run(cfg).await,
        BotKind::WhaleSignal => whale_signal::run(cfg).await,
    }
}
