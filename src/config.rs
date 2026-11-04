//! Configuration loading and types.
//!
//! Two-file split:
//! - `config.json` — public settings (committed)
//! - `config.yaml` — credentials (gitignored, must never be committed)

use crate::models::VenueId;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Which venue the copy-trading bot trades. Defaults to Polymarket.
    #[serde(default)]
    pub venue: VenueId,

    pub bot: BotConfig,
    pub site: SiteConfig,
    pub strategy: StrategyConfig,
    pub trading: TradingConfig,
    pub risk: RiskConfig,
    pub exchange: ExchangeConfig,

    /// Predict.fun settings, used when `venue` is `predict_fun`.
    #[serde(default)]
    pub predict_fun: PredictFunConfig,

    /// Market eligibility filter (allowlist + blocklist by category, tag, slug).
    #[serde(default)]
    pub filters: FiltersConfig,

    /// Take-profit / stop-loss configuration.
    #[serde(default)]
    pub tp_sl: TpSlConfig,

    /// Hard caps and tunables for the strategy bots other than copy trading.
    #[serde(default)]
    pub strategies: StrategiesConfig,

    /// Loaded from `config.yaml`. Not serialised back out.
    #[serde(skip)]
    pub credentials: Credentials,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotConfig {
    pub wallets_to_track: Vec<String>,

    /// When `false`, decisions are computed but no orders are sent.
    #[serde(default)]
    pub enable_trading: bool,

    /// When `true`, every order path returns early with a log line.
    /// Independent of `enable_trading` — both must be permissive for live trades.
    #[serde(default = "default_true")]
    pub mock_trading: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SiteConfig {
    pub gamma_api_base: String,
    pub data_api_base: String,
    pub clob_api_base: String,
    pub clob_wss_url: String,
    pub polygon_rpc_url: String,
    pub polygon_ws_url: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum CopyStrategy {
    Percentage,
    Fixed,
    Adaptive,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyConfig {
    pub copy_strategy: CopyStrategy,

    /// For PERCENTAGE: percent of whale notional (e.g. 20.0 = 20%).
    /// For FIXED: USD per copy leg.
    /// For ADAPTIVE: ignored (uses adaptive_* below).
    pub copy_size: f64,

    pub trade_multiplier: f64,
    pub min_order_size_usd: f64,
    pub max_order_size_usd: f64,
    pub min_whale_shares_to_copy: f64,
    pub adaptive_threshold_usd: f64,
    pub adaptive_min_percent: f64,
    pub adaptive_max_percent: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradingConfig {
    /// Max API requests per `rate_window_secs`.
    pub rate_limit: u32,
    pub rate_window_secs: u64,
    pub poll_interval_secs: u64,

    /// Slippage tolerance applied to the order limit price (fractional, e.g. 0.005 = 0.5¢ at $1 scale).
    pub price_buffer: f64,

    pub fee_rate_bps: u32,
    pub order_expiration_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskConfig {
    pub large_trade_shares: f64,
    pub consecutive_trigger: u32,
    pub sequence_window_secs: u64,
    pub min_depth_usd: f64,
    pub trip_duration_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExchangeConfig {
    pub ctf_exchange_address: String,
    pub neg_risk_exchange_address: String,
    pub chain_id: u64,
    pub domain_name: String,
    pub domain_version: String,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PredictNetwork {
    #[default]
    Testnet,
    Mainnet,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PredictFunConfig {
    /// Selects the API host, chain and exchange contracts together, so they
    /// can't disagree.
    #[serde(default)]
    pub network: PredictNetwork,

    /// Predict.fun addresses to copy. Separate from `bot.wallets_to_track`,
    /// since a wallet's address differs between venues.
    #[serde(default)]
    pub wallets_to_track: Vec<String>,
}

/// Market eligibility filter.
///
/// Behavior:
/// 1. Block lists always win — anything matching is skipped.
/// 2. If *any* allow list is non-empty, the market must match at least one
///    allow rule for ANY of (slug / category / tag) to be eligible.
/// 3. If all allow lists are empty, the bot falls back to allow-everything-
///    minus-blocklist semantics (so users who haven't curated yet aren't
///    stranded with zero trades).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FiltersConfig {
    #[serde(default)]
    pub slug_allow: Vec<String>,
    #[serde(default)]
    pub slug_block: Vec<String>,
    #[serde(default)]
    pub categories_allow: Vec<String>,
    #[serde(default)]
    pub categories_block: Vec<String>,
    #[serde(default)]
    pub tags_allow: Vec<String>,
    #[serde(default)]
    pub tags_block: Vec<String>,

    /// Per-category cap on simultaneously open USD notional.
    /// e.g. `{ "Politics": 500, "Sports": 300 }`.
    #[serde(default)]
    pub per_category_max_open_usd: std::collections::HashMap<String, f64>,

    /// Per-tag cap on simultaneously open USD notional.
    #[serde(default)]
    pub per_tag_max_open_usd: std::collections::HashMap<String, f64>,
}

impl FiltersConfig {
    /// True when at least one allow list is populated — strict allowlist mode.
    pub fn is_strict(&self) -> bool {
        !self.slug_allow.is_empty()
            || !self.categories_allow.is_empty()
            || !self.tags_allow.is_empty()
    }
}

/// Limits every strategy bot runs under, enforced by [`crate::bot::runtime::Broker`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StrategyLimits {
    /// Largest single order.
    pub max_order_usd: f64,
    /// Open cost plus resting buys in one market.
    pub max_market_usd: f64,
    /// Open cost plus resting buys across all markets.
    pub max_total_usd: f64,
    /// Realised plus marked loss that halts new entries and pulls quotes.
    pub max_loss_usd: f64,
}

impl Default for StrategyLimits {
    fn default() -> Self {
        Self {
            max_order_usd: 5.0,
            max_market_usd: 25.0,
            max_total_usd: 100.0,
            max_loss_usd: 25.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StrategiesConfig {
    pub poll_secs: u64,
    /// How often the market list is read again.
    pub rescan_secs: u64,
    /// Lifetime signed into every resting order, so a crash can't leave one
    /// open forever.
    pub quote_ttl_secs: u64,
    pub limits: StrategyLimits,
    pub btc_arb: BtcArbConfig,
    pub cross_arb: CrossArbConfig,
    pub directional_arb: DirectionalArbConfig,
    pub spread_farming: SpreadFarmingConfig,
    pub sports: SportsConfig,
    pub resolution_sniper: ResolutionSniperConfig,
    pub orderbook_imbalance: OrderbookImbalanceConfig,
    pub market_making: MarketMakingConfig,
    pub whale_signal: WhaleSignalConfig,
}

impl Default for StrategiesConfig {
    fn default() -> Self {
        Self {
            poll_secs: 5,
            rescan_secs: 300,
            quote_ttl_secs: 300,
            limits: Default::default(),
            btc_arb: Default::default(),
            cross_arb: Default::default(),
            directional_arb: Default::default(),
            spread_farming: Default::default(),
            sports: Default::default(),
            resolution_sniper: Default::default(),
            orderbook_imbalance: Default::default(),
            market_making: Default::default(),
            whale_signal: Default::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BtcArbConfig {
    /// Price feed symbols to trade, as the venue names them.
    pub symbols: Vec<String>,
    /// Model value minus cost and fee required to buy, in price units.
    pub min_edge: f64,
    pub order_usd: f64,
    /// No new entries this close to the end of a round.
    pub min_secs_left: i64,
    /// Lowest volatility the model will assume, per root second.
    pub vol_floor: f64,
}

impl Default for BtcArbConfig {
    fn default() -> Self {
        Self {
            symbols: vec!["BTCUSDT".into()],
            min_edge: 0.03,
            order_usd: 5.0,
            min_secs_left: 30,
            vol_floor: 0.00005,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CrossArbConfig {
    /// Locked profit per pair after fees required to report an opportunity.
    pub min_edge: f64,
    pub order_usd: f64,
    /// Assumed Polymarket taker fee, as `bps x min(p, 1 - p)` per share.
    pub polymarket_fee_bps: u32,
    /// Linked markets checked per tick.
    pub markets_per_tick: usize,
}

impl Default for CrossArbConfig {
    fn default() -> Self {
        Self {
            min_edge: 0.02,
            order_usd: 5.0,
            polymarket_fee_bps: 200,
            markets_per_tick: 20,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DirectionalArbConfig {
    /// Most the Yes and No bids may add up to. `0.99` requires a 1c floor.
    pub max_basket_cost: f64,
    /// Book-implied edge on a side, in cents, before tilting toward it.
    pub tilt_threshold_cents: f64,
    /// Largest size ratio of the tilted side to the hedge side.
    pub max_tilt_ratio: f64,
    /// Size of the smaller side.
    pub order_usd: f64,
    pub max_markets: usize,
    /// Stop quoting this close to a market's end.
    pub min_secs_left: i64,
}

impl Default for DirectionalArbConfig {
    fn default() -> Self {
        Self {
            max_basket_cost: 0.99,
            tilt_threshold_cents: 1.5,
            max_tilt_ratio: 3.0,
            order_usd: 2.0,
            max_markets: 5,
            min_secs_left: 3600,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SpreadFarmingConfig {
    /// Narrowest spread worth quoting into.
    pub min_spread: f64,
    pub order_usd: f64,
    pub max_markets: usize,
    /// A position still unsold after this long is sold at the bid.
    pub max_hold_secs: i64,
    /// Stop quoting this close to a market's end.
    pub min_secs_left: i64,
}

impl Default for SpreadFarmingConfig {
    fn default() -> Self {
        Self {
            min_spread: 0.04,
            order_usd: 5.0,
            max_markets: 5,
            max_hold_secs: 3600,
            min_secs_left: 3600,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SportsConfig {
    /// Player props outnumber fixtures by far, so they're hidden unless asked for.
    pub include_props: bool,
    /// Largest number of markets a listing prints.
    pub list_limit: usize,
}

impl Default for SportsConfig {
    fn default() -> Self {
        Self {
            include_props: false,
            list_limit: 25,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ResolutionSniperConfig {
    pub min_price: f64,
    pub max_price: f64,
    /// Payout minus price and fee required to buy.
    pub min_net_edge: f64,
    pub order_usd: f64,
    /// Only markets due to end within this long.
    pub max_secs_to_end: i64,
}

impl Default for ResolutionSniperConfig {
    fn default() -> Self {
        Self {
            min_price: 0.95,
            max_price: 0.99,
            min_net_edge: 0.005,
            order_usd: 5.0,
            max_secs_to_end: 86_400,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OrderbookImbalanceConfig {
    /// Levels from the touch counted on each side.
    pub levels: usize,
    /// Absolute imbalance in [0, 1] required to enter.
    pub threshold: f64,
    /// Trade against the heavy side rather than with it.
    pub fade: bool,
    pub max_spread: f64,
    pub order_usd: f64,
    pub take_profit: f64,
    pub stop_loss: f64,
    pub max_hold_secs: i64,
    pub max_markets: usize,
    /// Positions open at once. On a live book the signal fires across many markets
    /// at the same moment, and each entry pays the spread.
    pub max_positions: usize,
    /// Only trade prices in this band. A 3c spread is 60% of a 5c price.
    pub min_price: f64,
    pub max_price: f64,
}

impl Default for OrderbookImbalanceConfig {
    fn default() -> Self {
        Self {
            levels: 3,
            threshold: 0.6,
            fade: false,
            max_spread: 0.03,
            order_usd: 5.0,
            take_profit: 0.02,
            stop_loss: 0.02,
            max_hold_secs: 600,
            max_markets: 50,
            max_positions: 5,
            min_price: 0.15,
            max_price: 0.85,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MarketMakingConfig {
    /// Distance from the mid to each quote.
    pub half_spread: f64,
    pub order_usd: f64,
    /// Price shift per USD of net inventory.
    pub skew_per_usd: f64,
    /// Quotes further than this from where they should be are replaced.
    pub requote_distance: f64,
    pub max_markets: usize,
    /// Stop quoting this close to a market's end.
    pub min_secs_left: i64,
}

impl Default for MarketMakingConfig {
    fn default() -> Self {
        Self {
            half_spread: 0.02,
            order_usd: 5.0,
            skew_per_usd: 0.001,
            requote_distance: 0.01,
            max_markets: 5,
            min_secs_left: 3600,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WhaleSignalConfig {
    /// With no wallets in `predict_fun.wallets_to_track`, follow any taker this large.
    pub min_trade_usd: f64,
    pub order_usd: f64,
    /// Each wallet's own bucket: the most it can have opened through this bot.
    pub per_wallet_max_usd: f64,
    /// Never follow into a price above this.
    pub max_price: f64,
}

impl Default for WhaleSignalConfig {
    fn default() -> Self {
        Self {
            min_trade_usd: 500.0,
            order_usd: 5.0,
            per_wallet_max_usd: 25.0,
            max_price: 0.9,
        }
    }
}

/// Take-profit / stop-loss config (percent of entry price).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TpSlConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_tp_pct")]
    pub default_take_profit_pct: f64,
    #[serde(default = "default_sl_pct")]
    pub default_stop_loss_pct: f64,
    #[serde(default)]
    pub per_category_tp_pct: std::collections::HashMap<String, f64>,
    #[serde(default)]
    pub per_category_sl_pct: std::collections::HashMap<String, f64>,
    #[serde(default = "default_monitor_secs")]
    pub poll_interval_secs: u64,
}

impl Default for TpSlConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            default_take_profit_pct: 50.0,
            default_stop_loss_pct: 30.0,
            per_category_tp_pct: Default::default(),
            per_category_sl_pct: Default::default(),
            poll_interval_secs: 15,
        }
    }
}

fn default_tp_pct() -> f64 { 50.0 }
fn default_sl_pct() -> f64 { 30.0 }
fn default_monitor_secs() -> u64 { 15 }

#[derive(Debug, Clone, Default)]
pub struct Credentials {
    pub private_key: String,
    pub funder_address: String,
    pub api_key: Option<String>,
    pub api_secret: Option<String>,
    pub api_passphrase: Option<String>,
    pub predict_api_key: Option<String>,
    pub predict_account: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CredentialsFile {
    bot: CredentialsBot,
}

#[derive(Debug, Deserialize)]
struct CredentialsBot {
    private_key: String,
    #[serde(default)]
    funder_address: String,
    api_key: Option<String>,
    api_secret: Option<String>,
    api_passphrase: Option<String>,
    predict_api_key: Option<String>,
    predict_account: Option<String>,
}

impl AppConfig {
    pub fn load(config_path: &Path, credentials_path: &Path) -> Result<Self> {
        let raw =
            std::fs::read_to_string(config_path).with_context(|| {
                format!("reading public config from {}", config_path.display())
            })?;
        let mut cfg: AppConfig =
            serde_json::from_str(&raw).context("parsing config.json")?;

        // Credentials file is optional — without it we can still run in mock mode.
        if credentials_path.exists() {
            let raw = std::fs::read_to_string(credentials_path).with_context(|| {
                format!("reading credentials from {}", credentials_path.display())
            })?;
            let parsed: CredentialsFile =
                serde_yaml::from_str(&raw).context("parsing config.yaml")?;
            cfg.credentials = Credentials {
                private_key: parsed.bot.private_key,
                funder_address: parsed.bot.funder_address,
                api_key: parsed.bot.api_key,
                api_secret: parsed.bot.api_secret,
                api_passphrase: parsed.bot.api_passphrase,
                predict_api_key: parsed.bot.predict_api_key,
                predict_account: parsed.bot.predict_account,
            };
        }

        // Environment-variable overrides (handy for CI / Docker).
        if let Ok(key) = std::env::var("PM_PRIVATE_KEY") {
            cfg.credentials.private_key = key;
        }
        if let Ok(addr) = std::env::var("PM_FUNDER_ADDRESS") {
            cfg.credentials.funder_address = addr;
        }
        if let Ok(key) = std::env::var("PM_PREDICT_API_KEY") {
            cfg.credentials.predict_api_key = Some(key);
        }
        if let Ok(account) = std::env::var("PM_PREDICT_ACCOUNT") {
            cfg.credentials.predict_account = Some(account);
        }

        Ok(cfg)
    }

    pub fn live_trading_allowed(&self) -> bool {
        self.bot.enable_trading && !self.bot.mock_trading
    }
}

fn default_true() -> bool {
    true
}
