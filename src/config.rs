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
