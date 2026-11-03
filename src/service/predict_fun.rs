//! Predict.fun adapter — BNB Chain, USDT collateral, CTF-exchange order book.
//!
//! Looks like Polymarket but differs where it counts:
//!
//! - Amounts carry 18 decimals, not 6.
//! - `side` and `signatureType` are `uint8` in the EIP-712 `Order`, so the
//!   typehash differs from [`crate::service::clob`]'s.
//! - Each market signs against one of four exchanges, chosen by its neg-risk
//!   and yield-bearing flags.
//! - The fee rate comes from the market, not from config.
//! - The order book is quoted for the Yes outcome only; a No token sees it
//!   mirrored around 1.
//! - Auth is an API key (mainnet) plus a JWT from a signed message.
//!
//! Addresses and signing constants are compiled in from the official SDK
//! (`PredictDotFun/sdk` v1.3.8) and API hosts from the spec the API serves at
//! `/docs`. They are deliberately not configurable: where a signature is valid
//! must not depend on an editable file. `dev.predict.fun` was taken offline
//! after the October 2026 incident, so don't source anything from it.

use crate::config::{AppConfig, PredictNetwork};
use crate::models::{OrderType, PlannedOrder, Side, VenueId, WhaleTrade};
use crate::service::market_cache::MarketInfo;
use crate::service::midprice::MidpriceSource;
use crate::service::venue::{MarketSource, OrderTicket, OrderVenue};
use alloy_primitives::{address, eip191_hash_message, keccak256, Address, B256, U256};
use alloy_signer::Signer;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{eip712_domain, sol, Eip712Domain, SolStruct};
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use parking_lot::RwLock;
use reqwest::{Client, Method, StatusCode};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

sol! {
    /// Predict.fun CTF Exchange order. Field types are part of the typehash:
    /// `side` and `signatureType` must stay `uint8`.
    struct Order {
        uint256 salt;
        address maker;
        address signer;
        address taker;
        uint256 tokenId;
        uint256 makerAmount;
        uint256 takerAmount;
        uint256 expiration;
        uint256 nonce;
        uint256 feeRateBps;
        uint8 side;
        uint8 signatureType;
    }
}

const DOMAIN_NAME: &str = "predict.fun CTF Exchange";
const DOMAIN_VERSION: &str = "1";
const KERNEL_DOMAIN_NAME: &str = "Kernel";
const KERNEL_DOMAIN_VERSION: &str = "0.3.1";
const ECDSA_VALIDATOR: Address = address!("845ADb2C711129d4f3966735eD98a9F09fC4cE57");

/// The exchange takes the salt as a value below 2^31.
const MAX_SALT: u32 = 1 << 31;
/// The API ignores the requested lifetime of a MARKET order; this is the SDK's.
const MARKET_ORDER_TTL_SECS: i64 = 300;
/// 0.01 shares, the smallest order the SDK will build.
const MIN_QUANTITY_WEI: u128 = 10_000_000_000_000_000;
const WEI: u128 = 1_000_000_000_000_000_000;

const FEED_PAGE_SIZE: u32 = 50;
const FEED_MAX_PAGES: u32 = 5;
/// Trades can be indexed after later ones, so each poll re-reads this far back
/// and relies on dedupe.
const FEED_OVERLAP_MS: i64 = 60_000;
const FEED_SEEN_CAP: usize = 5_000;

const FILL_POLL_ATTEMPTS: u32 = 5;
const FILL_POLL_DELAY: Duration = Duration::from_secs(1);

struct Deployment {
    chain_id: u64,
    api_base: &'static str,
    ctf_exchange: Address,
    neg_risk_exchange: Address,
    yield_ctf_exchange: Address,
    yield_neg_risk_exchange: Address,
}

const MAINNET: Deployment = Deployment {
    chain_id: 56,
    api_base: "https://api.predict.fun/v1",
    ctf_exchange: address!("8BC070BEdAB741406F4B1Eb65A72bee27894B689"),
    neg_risk_exchange: address!("365fb81bd4A24D6303cd2F19c349dE6894D8d58A"),
    yield_ctf_exchange: address!("6bEb5a40C032AFc305961162d8204CDA16DECFa5"),
    yield_neg_risk_exchange: address!("8A289d458f5a134bA40015085A8F50Ffb681B41d"),
};

const TESTNET: Deployment = Deployment {
    chain_id: 97,
    api_base: "https://api-testnet.predict.fun/v1",
    ctf_exchange: address!("2A6413639BD3d73a20ed8C95F634Ce198ABbd2d7"),
    neg_risk_exchange: address!("d690b2bd441bE36431F6F6639D7Ad351e7B29680"),
    yield_ctf_exchange: address!("8a6B4Fa700A1e310b106E7a48bAFa29111f66e89"),
    yield_neg_risk_exchange: address!("95D5113bc50eD201e319101bbca3e0E250662fCC"),
};

fn deployment(network: PredictNetwork) -> &'static Deployment {
    match network {
        PredictNetwork::Mainnet => &MAINNET,
        PredictNetwork::Testnet => &TESTNET,
    }
}

impl Deployment {
    fn exchange(&self, is_neg_risk: bool, is_yield_bearing: bool) -> Address {
        match (is_neg_risk, is_yield_bearing) {
            (false, false) => self.ctf_exchange,
            (true, false) => self.neg_risk_exchange,
            (false, true) => self.yield_ctf_exchange,
            (true, true) => self.yield_neg_risk_exchange,
        }
    }
}

// --- API shapes -------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Envelope<T> {
    data: T,
    #[serde(default)]
    cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiMarket {
    pub id: u64,
    #[serde(default)]
    pub question: String,
    pub trading_status: String,
    pub is_neg_risk: bool,
    pub is_yield_bearing: bool,
    pub fee_rate_bps: u32,
    pub outcomes: Vec<ApiOutcome>,
    #[serde(default)]
    pub category_slug: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiOutcome {
    pub on_chain_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiLeg {
    pub quote_type: String,
    pub amount: String,
    pub price: String,
    pub outcome: ApiOutcome,
    pub signer: String,
    pub hash: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiMatch {
    pub market: ApiMarket,
    pub taker: ApiLeg,
    pub makers: Vec<ApiLeg>,
    #[serde(default)]
    pub transaction_hash: Option<String>,
    pub executed_at: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ApiBook {
    #[serde(default)]
    asks: Vec<(f64, f64)>,
    #[serde(default)]
    bids: Vec<(f64, f64)>,
}

#[derive(Debug, Deserialize)]
struct ApiCategory {
    #[serde(default)]
    tags: Vec<ApiTag>,
}

#[derive(Debug, Deserialize)]
struct ApiTag {
    name: String,
    #[serde(default)]
    level: u32,
}

#[derive(Debug, Deserialize)]
struct ApiAuthMessage {
    message: String,
}

#[derive(Debug, Deserialize)]
struct ApiAuthToken {
    token: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiCreatedOrder {
    order_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiOrderState {
    status: String,
    amount_filled: String,
}

#[derive(Debug, Deserialize)]
struct ApiPosition {
    outcome: ApiOutcome,
    amount: String,
}

#[derive(Debug, Deserialize)]
struct ApiAccount {
    address: String,
}

// --- Amounts ----------------------------------------------------------------

/// Parse a plain decimal string into 18-decimal fixed point, truncating extra
/// digits. The API's prices and sizes arrive as JSON doubles, so parsing their
/// shortest decimal form is what keeps `0.37` from becoming `0.36999…`.
fn decimal_to_wei(raw: &str) -> Result<U256> {
    let (whole, frac) = raw.split_once('.').unwrap_or((raw, ""));
    let digits = format!("{whole}{frac:0<18}", frac = &frac[..frac.len().min(18)]);
    U256::from_str_radix(&digits, 10).map_err(|e| anyhow!("not a decimal number {raw:?}: {e}"))
}

fn f64_to_wei(v: f64) -> Result<U256> {
    if !v.is_finite() || v < 0.0 {
        bail!("not a non-negative amount: {v}");
    }
    decimal_to_wei(&v.to_string())
}

fn wei_to_f64(v: U256) -> f64 {
    v.to_string().parse::<f64>().unwrap_or(0.0) / WEI as f64
}

/// Keep the leading `digits` significant digits, zeroing the rest. The exchange
/// matches on exact ratios, so the SDK trims amounts this way before signing.
fn retain_significant_digits(v: U256, digits: usize) -> U256 {
    let len = v.to_string().len();
    if len <= digits {
        return v;
    }
    let divisor = U256::from(10u8).pow(U256::from(len - digits));
    v / divisor * divisor
}

/// An outcome's book in wei. Asks ascending, bids descending: best first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct WeiBook {
    asks: Vec<(U256, U256)>,
    bids: Vec<(U256, U256)>,
}

impl WeiBook {
    /// `mirror` turns the API's Yes-quoted book into the No outcome's view: a
    /// Yes bid at `p` is a No ask at `1 - p`, and vice versa.
    fn from_api(raw: &ApiBook, mirror: bool) -> Result<Self> {
        let parse = |levels: &[(f64, f64)]| -> Result<Vec<(U256, U256)>> {
            levels
                .iter()
                .map(|(price, size)| {
                    let price = f64_to_wei(*price)?;
                    let price = if mirror {
                        U256::from(WEI)
                            .checked_sub(price)
                            .ok_or_else(|| anyhow!("book price above 1"))?
                    } else {
                        price
                    };
                    Ok((price, f64_to_wei(*size)?))
                })
                .collect()
        };
        let (mut asks, mut bids) = if mirror {
            (parse(&raw.bids)?, parse(&raw.asks)?)
        } else {
            (parse(&raw.asks)?, parse(&raw.bids)?)
        };
        asks.sort_by(|a, b| a.0.cmp(&b.0));
        bids.sort_by(|a, b| b.0.cmp(&a.0));
        Ok(Self { asks, bids })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MarketAmounts {
    maker_amount: U256,
    taker_amount: U256,
    /// Average fill price across the levels walked.
    price_per_share: U256,
    /// Worst level the order reaches.
    last_price: U256,
    quantity: U256,
}

/// Walk the book for `quantity_wei` shares, as the SDK's MARKET helper does with
/// no slippage: the signed ratio is the worst level reached. Unlike the SDK this
/// refuses a partial fill, because callers record the size they asked for.
fn market_amounts(book: &WeiBook, side: Side, quantity_wei: U256) -> Result<MarketAmounts> {
    let quantity = retain_significant_digits(quantity_wei, 5);
    if quantity < U256::from(MIN_QUANTITY_WEI) {
        bail!("order below the 0.01 share minimum");
    }
    let levels = match side {
        Side::Buy => &book.asks,
        Side::Sell => &book.bids,
    };

    let mut remaining = quantity;
    let mut cost = U256::ZERO;
    let mut last_price = U256::ZERO;
    for (price, size) in levels {
        if remaining.is_zero() {
            break;
        }
        let take = remaining.min(*size);
        cost += *price * take;
        remaining -= take;
        last_price = *price;
    }
    if !remaining.is_zero() {
        bail!(
            "book too thin: {} of {} shares available",
            wei_to_f64(quantity - remaining),
            wei_to_f64(quantity)
        );
    }

    let worst_value = last_price * quantity / U256::from(WEI);
    let (maker_amount, taker_amount) = match side {
        Side::Buy => (worst_value, quantity),
        Side::Sell => (quantity, worst_value),
    };
    Ok(MarketAmounts {
        maker_amount,
        taker_amount,
        price_per_share: cost / quantity,
        last_price,
        quantity,
    })
}

/// Refuse a fill worse than `limit_price`. The book is a snapshot, but the signed
/// ratio caps the price regardless of what it does before matching.
fn check_limit(amounts: &MarketAmounts, side: Side, limit_price: f64) -> Result<()> {
    let limit = f64_to_wei(limit_price)?;
    let beyond = match side {
        Side::Buy => amounts.last_price > limit,
        Side::Sell => amounts.last_price < limit,
    };
    if beyond {
        bail!(
            "book moved past the limit: {side:?} needs {} against limit {limit_price}",
            wei_to_f64(amounts.last_price)
        );
    }
    Ok(())
}

// --- Market data client ------------------------------------------------------

/// Read side of the adapter. Needs no wallet.
pub struct PredictFunClient {
    http: Client,
    deployment: &'static Deployment,
    network: PredictNetwork,
    api_key: Option<String>,
    /// Outcome token id → market.
    markets: RwLock<HashMap<String, ApiMarket>>,
    /// Category slug → (category, tags).
    categories: RwLock<HashMap<String, (Option<String>, Vec<String>)>>,
}

impl PredictFunClient {
    pub fn new(cfg: &AppConfig) -> Result<Arc<Self>> {
        let network = cfg.predict_fun.network;
        let api_key = cfg
            .credentials
            .predict_api_key
            .clone()
            .filter(|k| !k.trim().is_empty());
        if network == PredictNetwork::Mainnet && api_key.is_none() {
            bail!(
                "Predict.fun mainnet needs an API key: set predict_api_key in config.yaml \
                 or PM_PREDICT_API_KEY"
            );
        }
        Ok(Arc::new(Self {
            http: Client::builder()
                .user_agent("polymarket-toolkits/0.1")
                .timeout(Duration::from_secs(15))
                .build()?,
            deployment: deployment(network),
            network,
            api_key,
            markets: RwLock::new(HashMap::new()),
            categories: RwLock::new(HashMap::new()),
        }))
    }

    async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        bearer: Option<&str>,
        body: Option<&Value>,
    ) -> Result<Envelope<T>> {
        let url = format!("{}{}", self.deployment.api_base, path);
        let mut req = self.http.request(method.clone(), &url);
        if let Some(key) = &self.api_key {
            req = req.header("x-api-key", key);
        }
        if let Some(token) = bearer {
            req = req.bearer_auth(token);
        }
        if let Some(body) = body {
            req = req.json(body);
        }
        let resp = req.send().await.with_context(|| format!("{method} {path}"))?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(ApiError { status, body: text }).with_context(|| format!("{method} {path}"));
        }
        serde_json::from_str(&text).with_context(|| format!("parsing {method} {path} response"))
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<Envelope<T>> {
        self.request(Method::GET, path, None, None).await
    }

    async fn book(&self, market_id: u64) -> Result<ApiBook> {
        Ok(self
            .get::<ApiBook>(&format!("/markets/{market_id}/orderbook"))
            .await?
            .data)
    }

    fn market_for_token(&self, token_id: &str) -> Result<ApiMarket> {
        self.markets
            .read()
            .get(token_id)
            .cloned()
            .ok_or_else(|| anyhow!("no Predict.fun market known for token {token_id}"))
    }

    fn remember_market(&self, market: &ApiMarket) {
        let mut markets = self.markets.write();
        for outcome in &market.outcomes {
            markets.insert(outcome.on_chain_id.clone(), market.clone());
        }
    }

    /// The API quotes for the first outcome, so anything else reads it mirrored.
    fn is_mirrored(market: &ApiMarket, token_id: &str) -> bool {
        market
            .outcomes
            .first()
            .map(|yes| yes.on_chain_id != token_id)
            .unwrap_or(false)
    }

    async fn category_of(&self, slug: &str) -> (Option<String>, Vec<String>) {
        if let Some(hit) = self.categories.read().get(slug) {
            return hit.clone();
        }
        match self.get::<ApiCategory>(&format!("/categories/{slug}")).await {
            Ok(env) => {
                let tags = env.data.tags;
                let category = tags.iter().find(|t| t.level == 1).map(|t| t.name.clone());
                let names: Vec<String> = tags.into_iter().map(|t| t.name).collect();
                let entry = (category, names);
                self.categories
                    .write()
                    .insert(slug.to_string(), entry.clone());
                entry
            }
            Err(e) => {
                // Not cached: a strict allowlist skips this trade and the next
                // one tries again.
                warn!(slug, error = ?e, "category lookup failed");
                (None, Vec::new())
            }
        }
    }
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    body: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP {}: {}", self.status.as_u16(), self.body)
    }
}

impl std::error::Error for ApiError {}

#[async_trait]
impl MarketSource for PredictFunClient {
    async fn by_token_id(&self, token_id: &str) -> Result<MarketInfo> {
        // The API has no token lookup. Markets are learned from the trade feed,
        // which is the only way a token reaches the pipeline.
        let market = self.market_for_token(token_id)?;
        let outcome = |i: usize| {
            market
                .outcomes
                .get(i)
                .map(|o| o.on_chain_id.clone())
                .ok_or_else(|| anyhow!("market {} is missing outcome {i}", market.id))
        };
        let slug = market
            .category_slug
            .clone()
            .unwrap_or_else(|| market.id.to_string());
        let (category, tags) = self.category_of(&slug).await;
        Ok(MarketInfo {
            slug,
            question: market.question.clone(),
            yes_token_id: outcome(0)?,
            no_token_id: outcome(1)?,
            category,
            tags,
            closed: market.trading_status != "OPEN",
        })
    }
}

#[async_trait]
impl MidpriceSource for PredictFunClient {
    async fn midprice(&self, token_id: &str) -> Result<f64> {
        let market = self.market_for_token(token_id)?;
        let book = self.book(market.id).await?;
        let (best_bid, best_ask) = match (book.bids.first(), book.asks.first()) {
            (Some(b), Some(a)) => (b.0, a.0),
            _ => bail!("market {} has a one-sided book", market.id),
        };
        let yes_mid = (best_bid + best_ask) / 2.0;
        Ok(if Self::is_mirrored(&market, token_id) {
            1.0 - yes_mid
        } else {
            yes_mid
        })
    }
}

// --- Whale feed --------------------------------------------------------------

/// Project a match onto the legs `whale` took part in, keyed for dedupe.
fn whale_legs(m: &ApiMatch, whale: &Address) -> Result<Vec<(String, WhaleTrade)>> {
    let tx = m.transaction_hash.clone();
    let mut out = Vec::new();
    for leg in std::iter::once(&m.taker).chain(m.makers.iter()) {
        if !Address::from_str(&leg.signer).is_ok_and(|a| a == *whale) {
            continue;
        }
        let side = match leg.quote_type.as_str() {
            "Bid" => Side::Buy,
            "Ask" => Side::Sell,
            other => bail!("unknown quote type {other:?}"),
        };
        let shares = wei_to_f64(decimal_to_wei_exact(&leg.amount)?);
        let price = wei_to_f64(decimal_to_wei_exact(&leg.price)?);
        let key = format!("{}:{}", tx.as_deref().unwrap_or(&m.executed_at), leg.hash);
        out.push((
            key,
            WhaleTrade {
                venue: VenueId::PredictFun,
                maker: leg.signer.to_lowercase(),
                side,
                token_id: leg.outcome.on_chain_id.clone(),
                shares,
                price,
                usd_notional: shares * price,
                tx_hash: tx.clone(),
                block_number: None,
                observed_at: chrono::Utc::now(),
            },
        ));
    }
    Ok(out)
}

/// Amounts in API responses are already integer wei.
fn decimal_to_wei_exact(raw: &str) -> Result<U256> {
    U256::from_str_radix(raw, 10).map_err(|e| anyhow!("not a wei amount {raw:?}: {e}"))
}

/// Polls `GET /orders/matches` for a wallet's fills. Only trades executed after
/// the feed was created are reported: history is not something to copy.
pub struct WhaleFeed {
    client: Arc<PredictFunClient>,
    whale: Address,
    started_ms: i64,
    high_water_ms: i64,
    seen: HashSet<String>,
    seen_order: VecDeque<String>,
}

impl WhaleFeed {
    pub fn new(client: Arc<PredictFunClient>, whale: &str) -> Result<Self> {
        let whale = Address::from_str(whale.trim())
            .with_context(|| format!("wallet to track is not an address: {whale}"))?;
        let now = chrono::Utc::now().timestamp_millis();
        Ok(Self {
            client,
            whale,
            started_ms: now,
            high_water_ms: now,
            seen: HashSet::new(),
            seen_order: VecDeque::new(),
        })
    }

    /// New trades, oldest first.
    pub async fn poll(&mut self) -> Result<Vec<WhaleTrade>> {
        let after = (self.high_water_ms - FEED_OVERLAP_MS).max(self.started_ms - 1);
        let mut matches = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..FEED_MAX_PAGES {
            let mut path = format!(
                "/orders/matches?first={FEED_PAGE_SIZE}&signerAddress={}&executedAfter={after}",
                self.whale.to_checksum(None)
            );
            if let Some(c) = &cursor {
                path.push_str(&format!("&after={c}"));
            }
            let env = self.client.get::<Vec<ApiMatch>>(&path).await?;
            let page_len = env.data.len();
            matches.extend(env.data);
            cursor = env.cursor;
            if cursor.is_none() || page_len < FEED_PAGE_SIZE as usize {
                break;
            }
        }
        // The API sorts newest first.
        matches.reverse();

        // A row we can't read is skipped, not fatal: failing the poll here would
        // lose the trades already marked as seen.
        let mut trades = Vec::new();
        for m in &matches {
            let executed_ms = match chrono::DateTime::parse_from_rfc3339(&m.executed_at) {
                Ok(at) => at.timestamp_millis(),
                Err(e) => {
                    warn!(error = %e, executed_at = %m.executed_at, "skipping unreadable match");
                    continue;
                }
            };
            if executed_ms <= self.started_ms {
                continue;
            }
            self.high_water_ms = self.high_water_ms.max(executed_ms);
            self.client.remember_market(&m.market);
            let legs = match whale_legs(m, &self.whale) {
                Ok(legs) => legs,
                Err(e) => {
                    warn!(error = ?e, tx = ?m.transaction_hash, "skipping unreadable match");
                    continue;
                }
            };
            for (key, trade) in legs {
                if self.mark_seen(key) {
                    trades.push(trade);
                }
            }
        }
        Ok(trades)
    }

    fn mark_seen(&mut self, key: String) -> bool {
        if !self.seen.insert(key.clone()) {
            return false;
        }
        self.seen_order.push_back(key);
        if self.seen_order.len() > FEED_SEEN_CAP {
            if let Some(old) = self.seen_order.pop_front() {
                self.seen.remove(&old);
            }
        }
        true
    }
}

// --- Trading -----------------------------------------------------------------

/// Write side of the adapter: signs orders and, when live, submits them.
pub struct PredictFunTrader {
    client: Arc<PredictFunClient>,
    signer: PrivateKeySigner,
    /// Predict Account (Kernel smart wallet) the signer owns. `None` trades from
    /// the signer's own address.
    account: Option<Address>,
    jwt: tokio::sync::RwLock<Option<String>>,
}

impl PredictFunTrader {
    pub fn new(client: Arc<PredictFunClient>, cfg: &AppConfig) -> Result<Self> {
        let key = cfg.credentials.private_key.trim();
        if key.is_empty() {
            bail!("no private key configured");
        }
        let signer = PrivateKeySigner::from_str(key.trim_start_matches("0x"))
            .context("loading private key")?;
        let account = cfg
            .credentials
            .predict_account
            .as_deref()
            .map(|a| Address::from_str(a.trim()).context("parsing predict_account"))
            .transpose()?;
        Ok(Self {
            client,
            signer,
            account,
            jwt: tokio::sync::RwLock::new(None),
        })
    }

    /// The address orders trade from, and the identity the API authenticates.
    pub fn trading_address(&self) -> Address {
        self.account.unwrap_or_else(|| self.signer.address())
    }

    fn build_order(
        &self,
        market: &ApiMarket,
        token_id: &str,
        side: Side,
        amounts: &MarketAmounts,
        expiration: i64,
    ) -> Result<Order> {
        let maker = self.trading_address();
        Ok(Order {
            salt: U256::from(rand::random::<u32>() % MAX_SALT),
            maker,
            signer: maker,
            taker: Address::ZERO,
            tokenId: U256::from_str_radix(token_id, 10)
                .map_err(|_| anyhow!("token id must be a decimal U256"))?,
            makerAmount: amounts.maker_amount,
            takerAmount: amounts.taker_amount,
            expiration: U256::from(expiration),
            nonce: U256::ZERO,
            feeRateBps: U256::from(market.fee_rate_bps),
            side: side.as_u8(),
            signatureType: 0,
        })
    }

    async fn sign_order(&self, order: &Order, market: &ApiMarket) -> Result<(B256, String)> {
        let digest = order_digest(
            order,
            self.client.deployment,
            market.is_neg_risk,
            market.is_yield_bearing,
        );
        let signature = match self.account {
            Some(account) => {
                sign_for_account(&self.signer, digest, account, self.client.deployment.chain_id)
                    .await?
            }
            None => {
                let sig = self
                    .signer
                    .sign_hash(&digest)
                    .await
                    .context("signing order digest")?;
                format!("0x{}", hex::encode(sig.as_bytes()))
            }
        };
        Ok((digest, signature))
    }

    /// Exchange a signed message for a JWT. The message includes acceptance of
    /// Predict's Terms of Service, which is why nothing signs it except a live
    /// run or an explicit `check-venue --sign-in`.
    async fn sign_in(&self) -> Result<String> {
        let message = self
            .client
            .get::<ApiAuthMessage>("/auth/message")
            .await?
            .data
            .message;
        let signature = match self.account {
            Some(account) => {
                sign_for_account(
                    &self.signer,
                    eip191_hash_message(message.as_bytes()),
                    account,
                    self.client.deployment.chain_id,
                )
                .await?
            }
            None => {
                let sig = self
                    .signer
                    .sign_message(message.as_bytes())
                    .await
                    .context("signing auth message")?;
                format!("0x{}", hex::encode(sig.as_bytes()))
            }
        };
        let body = json!({
            "signer": self.trading_address().to_checksum(None),
            "signature": signature,
            "message": message,
        });
        let token = self
            .client
            .request::<ApiAuthToken>(Method::POST, "/auth", None, Some(&body))
            .await?
            .data
            .token;
        Ok(token)
    }

    async fn token(&self) -> Result<String> {
        if let Some(token) = self.jwt.read().await.as_ref() {
            return Ok(token.clone());
        }
        let mut slot = self.jwt.write().await;
        if let Some(token) = slot.as_ref() {
            return Ok(token.clone());
        }
        let token = self.sign_in().await?;
        *slot = Some(token.clone());
        Ok(token)
    }

    /// An authenticated request, signing in again once if the JWT has expired.
    async fn authed<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Envelope<T>> {
        let token = self.token().await?;
        match self
            .client
            .request::<T>(method.clone(), path, Some(&token), body)
            .await
        {
            Err(e)
                if e.downcast_ref::<ApiError>()
                    .is_some_and(|a| a.status == StatusCode::UNAUTHORIZED) =>
            {
                debug!("JWT rejected; signing in again");
                *self.jwt.write().await = None;
                let token = self.token().await?;
                self.client
                    .request::<T>(method, path, Some(&token), body)
                    .await
            }
            other => other,
        }
    }

    /// Wei held of an outcome. Buy fees are taken in shares, so this is less
    /// than the size bought and is the only safe size to sell.
    async fn held_wei(&self, market_id: u64, token_id: &str) -> Result<U256> {
        let positions = self
            .authed::<Vec<ApiPosition>>(
                Method::GET,
                &format!("/positions?marketId={market_id}&first=100"),
                None,
            )
            .await?
            .data;
        for pos in positions {
            if pos.outcome.on_chain_id == token_id {
                return decimal_to_wei_exact(&pos.amount);
            }
        }
        Ok(U256::ZERO)
    }

    /// Wait for a MARKET order to settle. `None` means the API couldn't say.
    async fn confirmed_fill(&self, order_hash: &str) -> Option<f64> {
        let mut last = None;
        for _ in 0..FILL_POLL_ATTEMPTS {
            tokio::time::sleep(FILL_POLL_DELAY).await;
            match self
                .authed::<ApiOrderState>(Method::GET, &format!("/orders/{order_hash}"), None)
                .await
            {
                Ok(env) => {
                    let filled = decimal_to_wei_exact(&env.data.amount_filled)
                        .map(wei_to_f64)
                        .ok();
                    last = filled;
                    if env.data.status != "OPEN" {
                        return filled;
                    }
                }
                Err(e) => warn!(error = ?e, order_hash, "fill check failed"),
            }
        }
        last.filter(|f| *f > 0.0)
    }

    /// Prove the connection without placing anything. `sign_in` also signs the
    /// auth message, which accepts Predict's Terms of Service.
    pub async fn check_sign_in(&self) -> Result<String> {
        let account = self
            .authed::<ApiAccount>(Method::GET, "/account", None)
            .await?
            .data;
        Ok(account.address)
    }
}

#[async_trait]
impl OrderVenue for PredictFunTrader {
    fn id(&self) -> VenueId {
        VenueId::PredictFun
    }

    async fn place(
        &self,
        planned: &PlannedOrder,
        _expiration_secs: u64,
        live: bool,
    ) -> Result<OrderTicket> {
        let market = self.client.market_for_token(&planned.token_id)?;

        let mut quantity = f64_to_wei(planned.shares)?;
        if live && planned.side == Side::Sell {
            let held = self.held_wei(market.id, &planned.token_id).await?;
            if held.is_zero() {
                info!(token = %planned.token_id, "nothing held to sell");
                return Ok(OrderTicket {
                    token_id: planned.token_id.clone(),
                    maker_amount: "0".into(),
                    taker_amount: "0".into(),
                    order_id: None,
                    filled_shares: Some(0.0),
                });
            }
            quantity = quantity.min(held);
        }

        let mirrored = PredictFunClient::is_mirrored(&market, &planned.token_id);
        let book = WeiBook::from_api(&self.client.book(market.id).await?, mirrored)?;
        let amounts = market_amounts(&book, planned.side, quantity)?;
        check_limit(&amounts, planned.side, planned.limit_price)?;

        let expiration = chrono::Utc::now().timestamp() + MARKET_ORDER_TTL_SECS;
        let order = self.build_order(&market, &planned.token_id, planned.side, &amounts, expiration)?;
        let (digest, signature) = self.sign_order(&order, &market).await?;
        let order_hash = format!("0x{}", hex::encode(digest));

        let mut ticket = OrderTicket {
            token_id: planned.token_id.clone(),
            maker_amount: amounts.maker_amount.to_string(),
            taker_amount: amounts.taker_amount.to_string(),
            order_id: None,
            filled_shares: None,
        };
        if !live {
            return Ok(ticket);
        }

        let body = order_body(&order, &order_hash, &signature, &amounts, expiration);
        let created = self
            .authed::<ApiCreatedOrder>(Method::POST, "/orders", Some(&body))
            .await?
            .data;
        ticket.order_id = Some(created.order_id);
        ticket.filled_shares = self.confirmed_fill(&order_hash).await;
        Ok(ticket)
    }
}

/// EIP-712 digest of an order against the exchange its market settles on.
fn order_digest(order: &Order, dep: &Deployment, is_neg_risk: bool, is_yield_bearing: bool) -> B256 {
    let domain: Eip712Domain = eip712_domain! {
        name: DOMAIN_NAME,
        version: DOMAIN_VERSION,
        chain_id: dep.chain_id,
        verifying_contract: dep.exchange(is_neg_risk, is_yield_bearing),
    };
    order.eip712_signing_hash(&domain)
}

/// Sign `hash` for a Predict Account. Kernel validates EIP-1271 signatures over a
/// wrapped hash, and expects them prefixed with its mode byte and validator.
async fn sign_for_account(
    signer: &PrivateKeySigner,
    hash: B256,
    account: Address,
    chain_id: u64,
) -> Result<String> {
    let domain: Eip712Domain = eip712_domain! {
        name: KERNEL_DOMAIN_NAME,
        version: KERNEL_DOMAIN_VERSION,
        chain_id: chain_id,
        verifying_contract: account,
    };
    let mut wrapped = Vec::with_capacity(64);
    wrapped.extend_from_slice(keccak256("Kernel(bytes32 hash)").as_slice());
    wrapped.extend_from_slice(hash.as_slice());
    let mut preimage = Vec::with_capacity(66);
    preimage.extend_from_slice(&[0x19, 0x01]);
    preimage.extend_from_slice(domain.hash_struct().as_slice());
    preimage.extend_from_slice(keccak256(&wrapped).as_slice());
    let digest = keccak256(&preimage);

    let sig = signer
        .sign_message(digest.as_slice())
        .await
        .context("signing Predict Account digest")?;
    Ok(format!(
        "0x01{}{}",
        hex::encode(ECDSA_VALIDATOR),
        hex::encode(sig.as_bytes())
    ))
}

/// `POST /orders` body. Expiration, side and signature type are JSON integers
/// here even though the other numeric fields are strings.
fn order_body(
    order: &Order,
    order_hash: &str,
    signature: &str,
    amounts: &MarketAmounts,
    expiration: i64,
) -> Value {
    json!({
        "data": {
            "pricePerShare": amounts.price_per_share.to_string(),
            "strategy": "MARKET",
            "order": {
                "hash": order_hash,
                "salt": order.salt.to_string(),
                "maker": order.maker.to_checksum(None),
                "signer": order.signer.to_checksum(None),
                "taker": order.taker.to_checksum(None),
                "tokenId": order.tokenId.to_string(),
                "makerAmount": order.makerAmount.to_string(),
                "takerAmount": order.takerAmount.to_string(),
                "expiration": expiration,
                "nonce": order.nonce.to_string(),
                "feeRateBps": order.feeRateBps.to_string(),
                "side": order.side,
                "signatureType": order.signatureType,
                "signature": signature,
            }
        }
    })
}

// --- check-venue -------------------------------------------------------------

/// `check-venue`: report what works on the configured network without placing
/// an order. Signs in only when asked to.
pub async fn check(cfg: &AppConfig, sign_in: bool) -> Result<()> {
    let client = PredictFunClient::new(cfg)?;
    println!(
        "network        {:?} (chain {}, {})",
        client.network, client.deployment.chain_id, client.deployment.api_base
    );

    let markets = client
        .get::<Vec<ApiMarket>>("/markets?first=25&status=OPEN")
        .await?
        .data;
    println!("markets        {} open markets read", markets.len());

    let mut sample = None;
    for market in markets.iter().filter(|m| m.trading_status == "OPEN") {
        let book = client.book(market.id).await?;
        if !book.asks.is_empty() && !book.bids.is_empty() {
            sample = Some((market.clone(), book));
            break;
        }
    }
    let Some((market, book)) = sample else {
        bail!("no open market with a two-sided book in the first page");
    };
    client.remember_market(&market);
    println!(
        "order book     market {}: bid {} / ask {} ({})",
        market.id, book.bids[0].0, book.asks[0].0, market.question
    );

    let trader = match PredictFunTrader::new(Arc::clone(&client), cfg) {
        Ok(trader) => trader,
        Err(e) => {
            println!("signer         none ({e:#}); orders can't be signed");
            return Ok(());
        }
    };
    println!("trading from   {}", trader.trading_address());

    let token_id = market
        .outcomes
        .first()
        .ok_or_else(|| anyhow!("market {} has no outcomes", market.id))?
        .on_chain_id
        .clone();
    let wei_book = WeiBook::from_api(&book, false)?;
    let amounts = market_amounts(&wei_book, Side::Buy, U256::from(WEI))?;
    let planned = PlannedOrder {
        venue: VenueId::PredictFun,
        token_id,
        side: Side::Buy,
        shares: 1.0,
        limit_price: wei_to_f64(amounts.last_price),
        usd_notional: wei_to_f64(amounts.maker_amount),
        order_type: OrderType::Fak,
        source_trade_hash: None,
    };
    let ticket = trader.place(&planned, 0, false).await?;
    println!(
        "signing        ok: 1 share would cost {} USDT (not submitted)",
        wei_to_f64(ticket.maker_amount.parse()?)
    );

    if sign_in {
        println!("sign-in        ok: API identifies you as {}", trader.check_sign_in().await?);
    } else {
        println!("sign-in        skipped; --sign-in accepts Predict's Terms of Service");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Vectors come from the official SDK (`@predictdotfun/sdk` 1.3.8) with the
    //! public test key 0x11…11, which others use too: never fund it or sign in
    //! with it. The domain separators they imply were compared with
    //! `domainSeparator()` on all eight deployed exchanges.
    use super::*;

    const DUMMY_KEY: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const TOKEN: &str =
        "52942861133159413416948390056898167822481351386047208865817464542237738901784";
    const ACCOUNT: Address = address!("1234567890123456789012345678901234567890");

    fn signer() -> PrivateKeySigner {
        PrivateKeySigner::from_str(DUMMY_KEY).unwrap()
    }

    fn vector_order() -> Order {
        let maker = signer().address();
        Order {
            salt: U256::from(1234567u64),
            maker,
            signer: maker,
            taker: Address::ZERO,
            tokenId: U256::from_str_radix(TOKEN, 10).unwrap(),
            makerAmount: U256::from(3_700_000_000_000_000_000u128),
            takerAmount: U256::from(10_000_000_000_000_000_000u128),
            expiration: U256::from(4102444800u64),
            nonce: U256::ZERO,
            feeRateBps: U256::from(200u32),
            side: 0,
            signatureType: 0,
        }
    }

    fn wei(v: &str) -> U256 {
        decimal_to_wei(v).unwrap()
    }

    #[test]
    fn signer_matches_sdk_address() {
        assert_eq!(
            signer().address(),
            address!("19E7E376E7C213B7E7e7e46cc70A5dD086DAff2A")
        );
    }

    #[tokio::test]
    async fn order_hash_and_signature_match_sdk_for_every_exchange() {
        let cases = [
            (
                false,
                false,
                "0x82a533b4bc32c5e9caf889f0a1fbcb31b999c44e71df1d8cc63e4d704d179823",
                "0xc7c780b9b7d9770e899e899f84426e350a35fc4e8f0769d43a0808556af22d0f4581b673dd83c137ca1b08ecf73cfd854cdd9d76e845c590d42cd5458f47ed171b",
            ),
            (
                true,
                false,
                "0xfe2d1b9cae7176c5934deb7d0238f42957a6f0caddb7da05a5a96814d84b8a9d",
                "0xc2b306ea192c735899cca384a5a6fb739c9c590a9648f2622efbdc6ca7c717820409912f884b836b0b262d07d9602997f8053e76a0ad700c711118db57dfa7741c",
            ),
            (
                false,
                true,
                "0x71d00ba8946e12512429ce17cc5873304cbb7fbd71dd01411f6d451ba792af40",
                "0x4431d78893375abb39b2cc4f8621a3b099a3a60a065ce8c5dc99dbed80a63ea17a28f67f36a9f9afa4a1ff5086f9e8033728e923267f55ad72da655410b83bba1b",
            ),
            (
                true,
                true,
                "0x2819f671fbb96ea6ad4c375a7ba788938bf14e6f7b190947154f98cdc0742e17",
                "0xf6216a361ffaeaa5126e2866994122d0d36a8d7fcd8048e404680e463d0dc2ae73fcf85fb90a38a6c5cf3aaea5eba7d025be01f4716478164c269bf9b95ab36b1c",
            ),
        ];
        for (neg, yld, hash, signature) in cases {
            let digest = order_digest(&vector_order(), &TESTNET, neg, yld);
            assert_eq!(format!("0x{}", hex::encode(digest)), hash);
            let sig = signer().sign_hash(&digest).await.unwrap();
            assert_eq!(format!("0x{}", hex::encode(sig.as_bytes())), signature);
        }
    }

    #[tokio::test]
    async fn auth_message_signature_matches_ethers() {
        let sig = signer().sign_message(b"hello predict").await.unwrap();
        assert_eq!(
            format!("0x{}", hex::encode(sig.as_bytes())),
            "0xf6b6e84f0896e1c4319f81a716f30c1ca25d4cb8ae657592347e3e22d41ad37778fba72be838524a4f7340abd0bc102207b04f28f9a7265ca8b1ee841941d7301b"
        );
    }

    #[tokio::test]
    async fn predict_account_signatures_match_sdk_formula() {
        let order_hash: B256 = "0x82a533b4bc32c5e9caf889f0a1fbcb31b999c44e71df1d8cc63e4d704d179823"
            .parse()
            .unwrap();
        assert_eq!(
            sign_for_account(&signer(), order_hash, ACCOUNT, 97).await.unwrap(),
            "0x01845adb2c711129d4f3966735ed98a9f09fc4ce571c19cf14981465427fda41189155d493833eb39feefa8a556603013825300d68399ca8b7003e46c8bdaca05036a64790313833fbd63ff4fafe653f76339b57181b"
        );
        assert_eq!(
            sign_for_account(&signer(), eip191_hash_message("hello predict"), ACCOUNT, 97)
                .await
                .unwrap(),
            "0x01845adb2c711129d4f3966735ed98a9f09fc4ce57fd7e20d4f2b8351eb7328ecda02795dac00321786fd9d6e1fcefe7c3775bce5410230894231dd63be456e96f97ba5766f392962feaff2e7d27750a09cb6549ba1b"
        );
    }

    fn sdk_book() -> WeiBook {
        WeiBook::from_api(
            &ApiBook {
                asks: vec![(0.37, 5.0), (0.4, 10.0), (0.45, 100.0)],
                bids: vec![(0.33, 4.0), (0.3, 8.0), (0.1, 100.0)],
            },
            false,
        )
        .unwrap()
    }

    #[test]
    fn market_buy_matches_sdk() {
        let a = market_amounts(&sdk_book(), Side::Buy, wei("10")).unwrap();
        assert_eq!(a.maker_amount, wei("4"));
        assert_eq!(a.taker_amount, wei("10"));
        assert_eq!(a.price_per_share, wei("0.385"));
        assert_eq!(a.last_price, wei("0.4"));
    }

    #[test]
    fn market_sell_matches_sdk() {
        let a = market_amounts(&sdk_book(), Side::Sell, wei("6")).unwrap();
        assert_eq!(a.maker_amount, wei("6"));
        assert_eq!(a.taker_amount, wei("1.8"));
        assert_eq!(a.price_per_share, wei("0.32"));
        assert_eq!(a.last_price, wei("0.3"));
    }

    #[test]
    fn market_buy_truncates_quantity_like_sdk() {
        let a = market_amounts(&sdk_book(), Side::Buy, wei("12.34567890123456789")).unwrap();
        assert_eq!(a.quantity, wei("12.345"));
        assert_eq!(a.maker_amount, wei("4.938"));
        assert_eq!(a.taker_amount, wei("12.345"));
        assert_eq!(a.price_per_share, U256::from(387849331713244228u128));
    }

    #[test]
    fn thin_book_is_refused_rather_than_part_filled() {
        let err = market_amounts(&sdk_book(), Side::Sell, wei("1000")).unwrap_err();
        assert!(err.to_string().contains("too thin"), "{err}");
    }

    #[test]
    fn dust_is_refused() {
        assert!(market_amounts(&sdk_book(), Side::Buy, wei("0.001")).is_err());
    }

    #[test]
    fn no_outcome_sees_the_book_mirrored() {
        // Matches the API's own per-outcome quotes on a live market: Yes 0.33/0.37
        // reads as No 0.63/0.67.
        let raw = ApiBook {
            asks: vec![(0.37, 519869.6282), (0.45, 7.5211)],
            bids: vec![(0.33, 123050.4076), (0.17, 6.0764)],
        };
        let no = WeiBook::from_api(&raw, true).unwrap();
        assert_eq!(no.asks[0], (wei("0.67"), wei("123050.4076")));
        assert_eq!(no.asks[1].0, wei("0.83"));
        assert_eq!(no.bids[0], (wei("0.63"), wei("519869.6282")));
        assert_eq!(no.bids[1].0, wei("0.55"));
    }

    #[test]
    fn prices_parse_without_float_drift() {
        assert_eq!(f64_to_wei(0.37).unwrap(), U256::from(370_000_000_000_000_000u128));
        assert_eq!(f64_to_wei(1.0).unwrap(), U256::from(WEI));
        assert_eq!(f64_to_wei(519869.6282).unwrap(), wei("519869.6282"));
        assert!(f64_to_wei(-1.0).is_err());
        assert!(f64_to_wei(f64::NAN).is_err());
    }

    #[test]
    fn limit_blocks_worse_fills_only() {
        let buy = market_amounts(&sdk_book(), Side::Buy, wei("10")).unwrap();
        assert!(check_limit(&buy, Side::Buy, 0.4).is_ok());
        assert!(check_limit(&buy, Side::Buy, 0.39).is_err());
        let sell = market_amounts(&sdk_book(), Side::Sell, wei("6")).unwrap();
        assert!(check_limit(&sell, Side::Sell, 0.3).is_ok());
        assert!(check_limit(&sell, Side::Sell, 0.31).is_err());
    }

    #[test]
    fn exchanges_are_routed_by_market_flags() {
        assert_eq!(MAINNET.exchange(false, false), MAINNET.ctf_exchange);
        assert_eq!(MAINNET.exchange(true, false), MAINNET.neg_risk_exchange);
        assert_eq!(MAINNET.exchange(false, true), MAINNET.yield_ctf_exchange);
        assert_eq!(MAINNET.exchange(true, true), MAINNET.yield_neg_risk_exchange);
    }

    /// A real match from the testnet feed, trimmed to the fields we read.
    const MATCH: &str = r#"{
      "market": {
        "id": 437, "question": "Q", "tradingStatus": "OPEN", "isNegRisk": true,
        "isYieldBearing": false, "feeRateBps": 200,
        "outcomes": [{"onChainId": "111"}, {"onChainId": "222"}],
        "categorySlug": "who-will-trump-nominate-as-fed-chair"
      },
      "taker": {
        "quoteType": "Bid", "amount": "3000000000000000000", "price": "370000000000000000",
        "outcome": {"onChainId": "111"},
        "signer": "0x19E7E376E7C213B7E7e7e46cc70A5dD086DAff2A", "hash": "0xaa"
      },
      "makers": [{
        "quoteType": "Ask", "amount": "3000000000000000000", "price": "630000000000000000",
        "outcome": {"onChainId": "222"},
        "signer": "0x44bBBea76D2593EaC52f17eCfdBFd88E7569B6fC", "hash": "0xbb"
      }],
      "transactionHash": "0x7099",
      "executedAt": "2026-11-03T19:34:33.000Z"
    }"#;

    #[test]
    fn taker_leg_becomes_a_trade() {
        let m: ApiMatch = serde_json::from_str(MATCH).unwrap();
        let legs = whale_legs(&m, &address!("19E7E376E7C213B7E7e7e46cc70A5dD086DAff2A")).unwrap();
        assert_eq!(legs.len(), 1);
        let (key, t) = &legs[0];
        assert_eq!(key, "0x7099:0xaa");
        assert_eq!(t.venue, VenueId::PredictFun);
        assert_eq!(t.side, Side::Buy);
        assert_eq!(t.token_id, "111");
        assert!((t.shares - 3.0).abs() < 1e-9);
        assert!((t.price - 0.37).abs() < 1e-9);
        assert!((t.usd_notional - 1.11).abs() < 1e-9);
    }

    #[test]
    fn maker_leg_uses_its_own_side_and_outcome() {
        let m: ApiMatch = serde_json::from_str(MATCH).unwrap();
        let legs = whale_legs(&m, &address!("44bBBea76D2593EaC52f17eCfdBFd88E7569B6fC")).unwrap();
        assert_eq!(legs.len(), 1);
        assert_eq!(legs[0].1.side, Side::Sell);
        assert_eq!(legs[0].1.token_id, "222");
    }

    #[test]
    fn unrelated_wallet_yields_nothing() {
        let m: ApiMatch = serde_json::from_str(MATCH).unwrap();
        assert!(whale_legs(&m, &ACCOUNT).unwrap().is_empty());
    }

    #[test]
    fn unknown_quote_type_is_an_error_for_that_match_only() {
        let mut m: ApiMatch = serde_json::from_str(MATCH).unwrap();
        m.taker.quote_type = "Mystery".into();
        assert!(whale_legs(&m, &address!("19E7E376E7C213B7E7e7e46cc70A5dD086DAff2A")).is_err());
    }

    /// Real testnet API, dry-run, throwaway wallet, no sign-in. Run with
    /// `cargo test -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "talks to the Predict.fun testnet API"]
    async fn live_testnet_dry_run_pipeline() {
        use crate::models::Side;
        use crate::service::order_executor::{ExecutionOutcome, OrderExecutor};
        use crate::service::position_store::PositionStore;
        use crate::service::risk_guard::RiskGuard;

        let mut cfg: AppConfig = serde_json::from_str(include_str!("../../config.json")).unwrap();
        cfg.filters = Default::default();
        cfg.credentials.private_key = hex::encode(rand::random::<[u8; 32]>());
        assert!(!cfg.live_trading_allowed(), "this test must never go live");

        let client = PredictFunClient::new(&cfg).unwrap();

        // Find a wallet with recent fills and read them through the real feed,
        // back-dated so history counts. Newest first, so this is the live end.
        let recent = client
            .get::<Vec<ApiMatch>>("/orders/matches?first=20")
            .await
            .unwrap()
            .data;
        let whale = recent[0].taker.signer.clone();
        let mut feed = WhaleFeed::new(Arc::clone(&client), &whale).unwrap();
        let week_ago = feed.started_ms - 7 * 24 * 3_600_000;
        feed.started_ms = week_ago;
        feed.high_water_ms = week_ago + FEED_OVERLAP_MS;
        let trades = feed.poll().await.unwrap();
        println!("feed: {} fills for {whale}", trades.len());
        assert!(!trades.is_empty());
        assert!(trades.iter().all(|t| t.maker == whale.to_lowercase()));
        assert!(feed.poll().await.unwrap().is_empty(), "dedupe failed");

        // Drive both outcomes of open markets through the shared pipeline, with
        // the whale's price set to the current best ask so the limit check passes.
        let markets = client
            .get::<Vec<ApiMarket>>("/markets?first=25&status=OPEN")
            .await
            .unwrap()
            .data;
        let trader = PredictFunTrader::new(Arc::clone(&client), &cfg).unwrap();
        let positions = PositionStore::new();
        let executor = OrderExecutor::new(
            cfg.clone(),
            Ok(Arc::new(trader)),
            RiskGuard::new(cfg.risk.clone()),
            client.clone(),
            Arc::clone(&positions),
        )
        .unwrap();

        let mut signed = 0;
        for market in markets.iter().filter(|m| m.outcomes.len() == 2) {
            client.remember_market(market);
            let raw = client.book(market.id).await.unwrap();
            if raw.asks.is_empty() || raw.bids.is_empty() {
                continue;
            }
            for (i, outcome) in market.outcomes.iter().enumerate() {
                let book = WeiBook::from_api(&raw, i == 1).unwrap();
                let (ask, depth) = book.asks[0];
                let (ask, depth) = (wei_to_f64(ask), wei_to_f64(depth));
                // Stay within the top level, above the sizing minimum.
                let usd = (depth * ask).min(50.0);
                if usd < cfg.strategy.min_order_size_usd * 2.0 || !(0.05..=0.95).contains(&ask) {
                    continue;
                }
                let whale_usd = usd / (cfg.strategy.copy_size / 100.0);
                let trade = WhaleTrade {
                    venue: VenueId::PredictFun,
                    maker: whale.to_lowercase(),
                    side: Side::Buy,
                    token_id: outcome.on_chain_id.clone(),
                    shares: whale_usd / ask,
                    price: ask,
                    usd_notional: whale_usd,
                    tx_hash: None,
                    block_number: None,
                    observed_at: chrono::Utc::now(),
                };
                match executor.execute(&trade).await.unwrap() {
                    ExecutionOutcome::DryRun(ticket) => {
                        let mid = client.midprice(&outcome.on_chain_id).await.unwrap();
                        println!(
                            "market {} outcome {i}: ask {ask} mid {mid:.3} maker {} taker {}",
                            market.id, ticket.maker_amount, ticket.taker_amount
                        );
                        // Cost per share must sit near this outcome's own price,
                        // which is what catches an unmirrored No book.
                        let maker: f64 = wei_to_f64(ticket.maker_amount.parse().unwrap());
                        let taker: f64 = wei_to_f64(ticket.taker_amount.parse().unwrap());
                        assert!((maker / taker - ask).abs() < 0.011, "priced off the wrong book");
                        assert!(ticket.order_id.is_none(), "dry run must not submit");
                        signed += 1;
                    }
                    other => println!("market {} outcome {i}: {other:?}", market.id),
                }
            }
        }
        println!("signed {signed} dry-run orders, {} positions open", positions.len());
        assert!(signed > 0);
    }

    #[test]
    fn order_body_has_the_shape_the_api_declares() {
        let amounts = market_amounts(&sdk_book(), Side::Buy, wei("10")).unwrap();
        let body = order_body(&vector_order(), "0xhash", "0xsig", &amounts, 4102444800);
        let data = &body["data"];
        assert_eq!(data["strategy"], "MARKET");
        assert_eq!(data["pricePerShare"], "385000000000000000");
        let order = &data["order"];
        assert_eq!(order["maker"], "0x19E7E376E7C213B7E7e7e46cc70A5dD086DAff2A");
        assert_eq!(order["expiration"], 4102444800i64);
        assert_eq!(order["side"], 0);
        assert_eq!(order["signatureType"], 0);
        assert_eq!(order["feeRateBps"], "200");
        assert_eq!(order["signature"], "0xsig");
    }
}
