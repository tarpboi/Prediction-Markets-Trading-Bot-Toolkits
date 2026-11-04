//! Polymarket CLOB V2 client: EIP-712 order signing, L1/L2 auth and order POST.
//!
//! CLOB V2 went live on 2026-04-28 and replaced the V1 order format. An order is
//! EIP-712 typed data signed against the exchange that matches its order book:
//!
//! | Book                       | Domain version | Verifying contract |
//! |----------------------------|----------------|--------------------|
//! | `neg_risk: false`          | `"2"`          | CTF Exchange       |
//! | `neg_risk: true`           | `"2"`          | Neg Risk Exchange  |
//! | `version: "v2"` (position) | `"3"`          | Exchange V3        |
//!
//! The signed struct dropped `taker`, `expiration`, `nonce` and `feeRateBps`, and
//! added `timestamp`, `metadata` and `builder`. `expiration` still travels in the
//! POST body, unsigned. Fees are charged by the protocol at match time.
//!
//! Signatures, API-key auth and the wire body are checked against vectors produced
//! by Polymarket's official `rs-clob-client-v2` (see the tests below). **No order
//! from this client has been accepted by the live CLOB yet.**
//!
//! Safety: nothing is transmitted unless the caller passes `live`, and callers
//! derive that from [`AppConfig::live_trading_allowed`].

use crate::config::{AppConfig, ExchangeConfig};
use crate::models::{OrderType, PlannedOrder, Side, VenueId};
use crate::service::venue::{OrderTicket, OrderVenue};
use alloy_primitives::{keccak256, Address, B256, U256};
use alloy_signer::Signer;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{eip712_domain, sol, Eip712Domain, SolStruct, SolValue};
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use reqwest::{Client, Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::str::FromStr;
use std::time::Duration;
use tokio::sync::OnceCell;
use tracing::{info, warn};

/// The protocol version this client signs for, as `GET /version` reports it.
const CLOB_PROTOCOL_VERSION: u64 = 2;
const GEOBLOCK_URL: &str = "https://polymarket.com/api/geoblock";
const CLOB_AUTH_MESSAGE: &str = "This message attests that I control the given wallet";
/// `salt` is sent as a JSON number and read with JavaScript's integer range, so
/// it has to stay within 2^53 - 1.
const MAX_SALT: u64 = (1 << 53) - 1;
/// How long a `delayed` order is polled for its real fill.
const DELAYED_POLLS: u32 = 5;
const DELAYED_POLL_INTERVAL: Duration = Duration::from_secs(2);

const ORDER_TYPE_STRING: &str = concat!(
    "Order(uint256 salt,address maker,address signer,uint256 tokenId,",
    "uint256 makerAmount,uint256 takerAmount,uint8 side,uint8 signatureType,",
    "uint256 timestamp,bytes32 metadata,bytes32 builder)"
);
const TYPED_DATA_SIGN_TYPE_STRING: &str = concat!(
    "TypedDataSign(Order contents,string name,string version,uint256 chainId,",
    "address verifyingContract,bytes32 salt)",
    "Order(uint256 salt,address maker,address signer,uint256 tokenId,",
    "uint256 makerAmount,uint256 takerAmount,uint8 side,uint8 signatureType,",
    "uint256 timestamp,bytes32 metadata,bytes32 builder)"
);
const DEPOSIT_WALLET_NAME: &str = "DepositWallet";
const DEPOSIT_WALLET_VERSION: &str = "1";

sol! {
    /// CLOB V2 order. Field order and types feed the EIP-712 type hash, so they
    /// have to match the deployed exchange exactly.
    struct Order {
        uint256 salt;
        address maker;
        address signer;
        uint256 tokenId;
        uint256 makerAmount;
        uint256 takerAmount;
        uint8 side;
        uint8 signatureType;
        uint256 timestamp;
        bytes32 metadata;
        bytes32 builder;
    }

    /// L1 attestation used to create or derive API credentials.
    struct ClobAuth {
        address address;
        string timestamp;
        uint256 nonce;
        string message;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SignatureType {
    Eoa = 0,
    PolyProxy = 1,
    PolyGnosisSafe = 2,
    /// Deposit wallet. The signature is wrapped for EIP-1271 validation.
    DepositWallet = 3,
}

impl SignatureType {
    fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "eoa" | "0" => Ok(Self::Eoa),
            "proxy" | "1" => Ok(Self::PolyProxy),
            "safe" | "gnosis_safe" | "2" => Ok(Self::PolyGnosisSafe),
            "deposit_wallet" | "deposit" | "3" => Ok(Self::DepositWallet),
            other => bail!(
                "unknown signature_type `{other}`: use eoa, proxy, safe or deposit_wallet"
            ),
        }
    }
}

/// Works out who holds the funds and how orders prove it. A funder that isn't the
/// signer can be one of three wallet types, and a wrong guess gets every order
/// rejected, so that case has to be stated rather than inferred.
fn resolve_wallet(
    signer: Address,
    funder: &str,
    configured: Option<&str>,
) -> Result<(Address, SignatureType)> {
    let funder = match funder.trim() {
        "" => Address::ZERO,
        raw => Address::from_str(raw).context("parsing funder address")?,
    };
    let configured = configured.map(SignatureType::parse).transpose()?;
    if funder == Address::ZERO || funder == signer {
        return match configured {
            None | Some(SignatureType::Eoa) => Ok((signer, SignatureType::Eoa)),
            Some(other) => bail!("signature_type {other:?} needs a funder_address"),
        };
    }
    match configured {
        Some(SignatureType::Eoa) => {
            bail!("signature_type eoa can't be used with a separate funder_address")
        }
        Some(kind) => Ok((funder, kind)),
        None => bail!(
            "funder_address differs from the signer, so set signature_type to proxy \
             (email login), safe (browser wallet) or deposit_wallet"
        ),
    }
}

/// The `order` object of a `POST /order` body.
#[derive(Debug, Clone, Serialize)]
pub struct SignedOrder {
    pub salt: u64,
    pub maker: String,
    pub signer: String,
    #[serde(rename = "tokenId")]
    pub token_id: String,
    #[serde(rename = "makerAmount")]
    pub maker_amount: String,
    #[serde(rename = "takerAmount")]
    pub taker_amount: String,
    pub side: String,
    pub expiration: String,
    #[serde(rename = "signatureType")]
    pub signature_type: u8,
    pub timestamp: String,
    pub metadata: String,
    pub builder: String,
    pub signature: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct OrderPostBody {
    pub order: SignedOrder,
    #[serde(rename = "orderType")]
    pub order_type: String,
    /// The CLOB API key, not a wallet address.
    pub owner: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct OrderResponse {
    #[serde(default, rename = "orderID")]
    pub order_id: Option<String>,
    #[serde(default)]
    pub success: bool,
    #[serde(default, rename = "errorMsg")]
    pub error_msg: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default, rename = "makingAmount")]
    pub making_amount: Option<String>,
    #[serde(default, rename = "takingAmount")]
    pub taking_amount: Option<String>,
}

/// What the order book says about how a token has to be signed and sized.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BookParams {
    pub tick_size: f64,
    pub min_order_size: f64,
    pub neg_risk: bool,
    /// A `version: "v2"` book holds position orders, which go to Exchange V3.
    pub position_book: bool,
}

impl BookParams {
    fn from_book(book: &Value) -> Result<Self> {
        let number = |key: &str| {
            let v = book.get(key)?;
            v.as_str()
                .and_then(|s| s.parse::<f64>().ok())
                .or_else(|| v.as_f64())
        };
        let tick_size = number("tick_size").ok_or_else(|| anyhow!("book has no tick_size"))?;
        if tick_size <= 0.0 || tick_size >= 1.0 {
            bail!("book has an invalid tick_size {tick_size}");
        }
        Ok(Self {
            tick_size,
            min_order_size: number("min_order_size").unwrap_or(0.0),
            neg_risk: book.get("neg_risk").and_then(Value::as_bool).unwrap_or(false),
            position_book: book.get("version").and_then(Value::as_str) == Some("v2"),
        })
    }
}

/// CLOB amounts for an order. Prices move onto the tick in the order's favour: a
/// buy never pays more than its limit, and a sell never takes less.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrderAmounts {
    pub maker: u128,
    pub taker: u128,
    /// Size in 1/100ths of a share, the finest the CLOB accepts.
    pub size_centi: u128,
    pub price_micro: u128,
}

pub fn order_amounts(shares: f64, price: f64, side: Side, tick: f64) -> Result<OrderAmounts> {
    if shares <= 0.0 || price <= 0.0 || shares.is_nan() || price.is_nan() {
        bail!("order needs a positive size and price (size {shares}, price {price})");
    }
    let tick_micro = (tick * 1_000_000.0).round() as u128;
    if tick_micro == 0 || tick_micro >= 1_000_000 {
        bail!("unsupported tick size {tick}");
    }
    let raw_ticks = price / tick;
    let ticks = match side {
        Side::Buy => (raw_ticks + 1e-9).floor(),
        Side::Sell => (raw_ticks - 1e-9).ceil(),
    };
    let max_ticks = 1_000_000 / tick_micro - 1;
    let ticks = (ticks.max(1.0) as u128).min(max_ticks);
    let price_micro = ticks * tick_micro;

    let size_centi = (shares * 100.0 + 1e-9).floor() as u128;
    if size_centi == 0 {
        bail!("size {shares} rounds down to zero at 2 decimals");
    }
    let share_units = size_centi * 10_000;
    let usd_units = size_centi * price_micro / 100;
    let (maker, taker) = match side {
        Side::Buy => (usd_units, share_units),
        Side::Sell => (share_units, usd_units),
    };
    Ok(OrderAmounts { maker, taker, size_centi, price_micro })
}

/// GTD orders live one minute less than stated and have to be at least three
/// minutes out. Every other type must carry zero.
fn expiration_for(order_type: OrderType, ttl_secs: u64, now_secs: u64) -> u64 {
    match order_type {
        OrderType::Gtd => now_secs + (ttl_secs + 60).max(185),
        OrderType::Fak | OrderType::Gtc => 0,
    }
}

/// Everything that goes into a signature, so tests can fix the salt and clock.
#[derive(Debug, Clone)]
struct OrderFields {
    salt: u64,
    token_id: U256,
    maker_amount: u128,
    taker_amount: u128,
    side: Side,
    timestamp_ms: u64,
    expiration: u64,
}

/// Shares filled, as far as the POST response can say. `None` means unknown.
fn filled_from_response(resp: &OrderResponse, side: Side, requested: f64) -> Option<f64> {
    let amount = match side {
        Side::Buy => resp.taking_amount.as_deref(),
        Side::Sell => resp.making_amount.as_deref(),
    }
    .and_then(|raw| raw.trim().parse::<f64>().ok())
    .unwrap_or(0.0);
    let status = resp.status.as_deref().unwrap_or("").to_ascii_lowercase();
    if amount > 0.0 {
        // A FAK buy can get more shares than planned by filling below its limit,
        // but not this many. Far above means the units aren't what we expect.
        return (amount <= requested * 2.0).then_some(amount);
    }
    matches!(status.as_str(), "live" | "unmatched").then_some(0.0)
}

struct ApiCreds {
    key: String,
    secret: String,
    passphrase: String,
}

pub struct ClobClient {
    http: Client,
    clob_base: String,
    signer: PrivateKeySigner,
    funder: Address,
    exchange: ExchangeConfig,
    signature_type: SignatureType,
    configured_creds: Option<(String, String, String)>,
    creds: OnceCell<ApiCreds>,
    version_checked: OnceCell<()>,
}

impl ClobClient {
    pub fn new(cfg: &AppConfig) -> Result<Self> {
        cfg.exchange.ensure_clob_v2()?;
        let pk = parse_private_key(&cfg.credentials.private_key)
            .context("loading private key")?;
        let signer = PrivateKeySigner::from_bytes(&B256::from(pk))
            .context("creating EOA signer from private key")?;
        let (funder, signature_type) = resolve_wallet(
            signer.address(),
            &cfg.credentials.funder_address,
            cfg.credentials.signature_type.as_deref(),
        )?;

        let creds = &cfg.credentials;
        let configured_creds = match (&creds.api_key, &creds.api_secret, &creds.api_passphrase) {
            (Some(k), Some(s), Some(p)) => Some((k.clone(), s.clone(), p.clone())),
            (None, None, None) => None,
            _ => bail!("set api_key, api_secret and api_passphrase together, or none of them"),
        };

        Ok(Self {
            http: Client::builder()
                .user_agent("polymarket-toolkits/0.2")
                .timeout(Duration::from_secs(15))
                .build()?,
            clob_base: cfg.site.clob_api_base.trim_end_matches('/').to_string(),
            signer,
            funder,
            exchange: cfg.exchange.clone(),
            signature_type,
            configured_creds,
            creds: OnceCell::new(),
            version_checked: OnceCell::new(),
        })
    }

    pub fn signer_address(&self) -> Address {
        self.signer.address()
    }

    /// Run before going live, so a problem stops the bot at startup instead of
    /// surfacing on the first copied trade.
    pub async fn preflight(&self) -> Result<()> {
        if let Some(geo) = geoblock(&self.http).await {
            if geo.blocked {
                bail!(
                    "Polymarket reports this connection as restricted ({}), so it would reject \
                     these orders. See Polymarket's geographic restrictions before going further.",
                    geo.location()
                );
            }
        }
        self.ensure_version().await?;
        self.creds().await?;
        let funds = self.balance_allowance().await?;
        if funds.balance_usd <= 0.0 {
            warn!("no pUSD in the trading wallet; orders will be rejected until it's funded");
        }
        for (name, address) in self.exchange_spenders()? {
            if !funds.allowed(address) {
                warn!(contract = name, "no pUSD allowance; orders on it will be rejected");
            }
        }
        info!(
            balance_usd = funds.balance_usd,
            signature_type = ?self.signature_type,
            "Polymarket preflight passed"
        );
        Ok(())
    }

    fn exchange_spenders(&self) -> Result<[(&'static str, Address); 2]> {
        Ok([
            ("CTF Exchange", Address::from_str(&self.exchange.ctf_exchange_address)?),
            ("Neg Risk Exchange", Address::from_str(&self.exchange.neg_risk_exchange_address)?),
        ])
    }

    async fn ensure_version(&self) -> Result<()> {
        self.version_checked
            .get_or_try_init(|| async {
                let url = format!("{}/version", self.clob_base);
                let body: Value = self
                    .http
                    .get(&url)
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await
                    .context("parsing /version")?;
                match body.get("version").and_then(Value::as_u64) {
                    Some(CLOB_PROTOCOL_VERSION) => Ok(()),
                    other => Err(anyhow!(
                        "CLOB reports protocol version {other:?}, but this client signs \
                         version {CLOB_PROTOCOL_VERSION} orders"
                    )),
                }
            })
            .await
            .map(|_| ())
    }

    /// Read how `token_id` has to be signed and sized from its current book.
    pub async fn book_params(&self, token_id: &str) -> Result<BookParams> {
        let url = format!("{}/book?token_id={token_id}", self.clob_base);
        let book: Value = self
            .http
            .get(&url)
            .send()
            .await?
            .error_for_status()
            .with_context(|| format!("reading the book for token {token_id}"))?
            .json()
            .await
            .context("parsing /book")?;
        BookParams::from_book(&book)
    }

    fn domain_for(&self, params: &BookParams) -> Result<Eip712Domain> {
        let (contract, version) = if params.position_book {
            (&self.exchange.exchange_v3_address, "3".to_string())
        } else if params.neg_risk {
            (&self.exchange.neg_risk_exchange_address, self.exchange.domain_version.clone())
        } else {
            (&self.exchange.ctf_exchange_address, self.exchange.domain_version.clone())
        };
        let verifying_contract = Address::from_str(contract)
            .with_context(|| format!("parsing exchange address {contract}"))?;
        Ok(eip712_domain! {
            name: self.exchange.domain_name.clone(),
            version: version,
            chain_id: self.exchange.chain_id,
            verifying_contract: verifying_contract,
        })
    }

    async fn sign_fields(&self, f: &OrderFields, params: &BookParams) -> Result<SignedOrder> {
        // A deposit wallet validates its own orders, so it's named as the signer.
        let order_signer = match self.signature_type {
            SignatureType::DepositWallet => self.funder,
            _ => self.signer.address(),
        };
        let order = Order {
            salt: U256::from(f.salt),
            maker: self.funder,
            signer: order_signer,
            tokenId: f.token_id,
            makerAmount: U256::from(f.maker_amount),
            takerAmount: U256::from(f.taker_amount),
            side: f.side.as_u8(),
            signatureType: self.signature_type as u8,
            timestamp: U256::from(f.timestamp_ms),
            metadata: B256::ZERO,
            builder: B256::ZERO,
        };
        let domain = self.domain_for(params)?;
        let signature = match self.signature_type {
            SignatureType::DepositWallet => self.sign_for_deposit_wallet(&order, &domain).await?,
            _ => {
                let sig = self
                    .signer
                    .sign_hash(&order.eip712_signing_hash(&domain))
                    .await
                    .context("signing order digest")?;
                format!("0x{}", hex::encode(sig.as_bytes()))
            }
        };

        Ok(SignedOrder {
            salt: f.salt,
            maker: format!("0x{:x}", order.maker),
            signer: format!("0x{:x}", order.signer),
            token_id: order.tokenId.to_string(),
            maker_amount: order.makerAmount.to_string(),
            taker_amount: order.takerAmount.to_string(),
            side: side_str(f.side).to_string(),
            expiration: f.expiration.to_string(),
            signature_type: self.signature_type as u8,
            timestamp: order.timestamp.to_string(),
            metadata: format!("0x{}", hex::encode(order.metadata)),
            builder: format!("0x{}", hex::encode(order.builder)),
            signature,
        })
    }

    /// The nested typed-data signature a deposit wallet checks through EIP-1271.
    async fn sign_for_deposit_wallet(&self, order: &Order, domain: &Eip712Domain) -> Result<String> {
        let contents_hash = order.eip712_hash_struct();
        let domain_separator = domain.hash_struct();
        let typed_data_sign_hash = keccak256(
            (
                keccak256(TYPED_DATA_SIGN_TYPE_STRING.as_bytes()),
                contents_hash,
                keccak256(DEPOSIT_WALLET_NAME.as_bytes()),
                keccak256(DEPOSIT_WALLET_VERSION.as_bytes()),
                U256::from(self.exchange.chain_id),
                order.signer,
                B256::ZERO,
            )
                .abi_encode(),
        );
        let mut digest_input = [0u8; 66];
        digest_input[0] = 0x19;
        digest_input[1] = 0x01;
        digest_input[2..34].copy_from_slice(domain_separator.as_slice());
        digest_input[34..66].copy_from_slice(typed_data_sign_hash.as_slice());
        let inner = self
            .signer
            .sign_hash(&keccak256(digest_input))
            .await
            .context("signing deposit wallet digest")?;

        let type_len = u16::try_from(ORDER_TYPE_STRING.len())?;
        Ok(format!(
            "0x{}{}{}{}{}",
            hex::encode(inner.as_bytes()),
            hex::encode(domain_separator),
            hex::encode(contents_hash),
            hex::encode(ORDER_TYPE_STRING.as_bytes()),
            hex::encode(type_len.to_be_bytes()),
        ))
    }

    /// Build and sign an order without sending it.
    pub async fn build_signed_order(
        &self,
        planned: &PlannedOrder,
        order_type: OrderType,
        expiration_secs: u64,
        params: &BookParams,
    ) -> Result<SignedOrder> {
        let token_id = U256::from_str(&planned.token_id)
            .map_err(|_| anyhow!("token_id must be a U256 decimal"))?;
        let amounts =
            order_amounts(planned.shares, planned.limit_price, planned.side, params.tick_size)?;
        let size = amounts.size_centi as f64 / 100.0;
        if size < params.min_order_size {
            bail!(
                "{size} shares is below this market's minimum of {}",
                params.min_order_size
            );
        }
        let now = chrono::Utc::now();
        self.sign_fields(
            &OrderFields {
                salt: rand::random::<u64>() & MAX_SALT,
                token_id,
                maker_amount: amounts.maker,
                taker_amount: amounts.taker,
                side: planned.side,
                timestamp_ms: now.timestamp_millis() as u64,
                expiration: expiration_for(order_type, expiration_secs, now.timestamp() as u64),
            },
            params,
        )
        .await
    }

    /// Post a signed order. A response with `success: false` is an error, so a
    /// rejected order can never be recorded as an open position.
    pub async fn post_order(
        &self,
        signed: SignedOrder,
        order_type: OrderType,
    ) -> Result<OrderResponse> {
        let creds = self.creds().await?;
        let body = OrderPostBody {
            order: signed,
            order_type: order_type_str(order_type).to_string(),
            owner: creds.key.clone(),
        };
        let body_json = serde_json::to_string(&body)?;
        let (status, text) = self
            .authed_request(Method::POST, "/order", "", Some(body_json))
            .await?;
        if !status.is_success() {
            bail!("CLOB rejected order (HTTP {}): {text}", status.as_u16());
        }
        let parsed: OrderResponse =
            serde_json::from_str(&text).context("parsing CLOB response")?;
        if !parsed.success {
            bail!(
                "CLOB rejected order: {}",
                parsed.error_msg.as_deref().unwrap_or("no reason given")
            );
        }
        Ok(parsed)
    }

    /// Shares filled on an order the CLOB accepted as `delayed`. `None` if it
    /// still hasn't settled after a few polls.
    async fn poll_delayed_fill(&self, order_id: &str) -> Option<f64> {
        let path = format!("/data/order/{order_id}");
        for _ in 0..DELAYED_POLLS {
            tokio::time::sleep(DELAYED_POLL_INTERVAL).await;
            let Ok((status, text)) = self.authed_request(Method::GET, &path, "", None).await else {
                continue;
            };
            if !status.is_success() {
                continue;
            }
            let Ok(order) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let state = order.get("status").and_then(Value::as_str).unwrap_or("");
            if state.eq_ignore_ascii_case("delayed") {
                continue;
            }
            return order
                .get("size_matched")
                .and_then(Value::as_str)
                .and_then(|s| s.parse::<f64>().ok());
        }
        None
    }

    async fn balance_allowance(&self) -> Result<Funds> {
        let query = format!(
            "?asset_type=COLLATERAL&signature_type={}",
            self.signature_type as u8
        );
        let (status, text) = self
            .authed_request(Method::GET, "/balance-allowance", &query, None)
            .await?;
        if !status.is_success() {
            bail!("reading balance failed (HTTP {}): {text}", status.as_u16());
        }
        Funds::from_json(&serde_json::from_str(&text).context("parsing /balance-allowance")?)
    }

    /// API credentials from the config, or else created (or derived, if they
    /// already exist) with an L1 signature.
    async fn creds(&self) -> Result<&ApiCreds> {
        self.creds
            .get_or_try_init(|| async {
                if let Some((key, secret, passphrase)) = self.configured_creds.clone() {
                    return Ok(ApiCreds { key, secret, passphrase });
                }
                let created = self.l1_request(Method::POST, "/auth/api-key").await?;
                let body = if created.0.is_success() {
                    created.1
                } else {
                    let derived = self.l1_request(Method::GET, "/auth/derive-api-key").await?;
                    if !derived.0.is_success() {
                        bail!(
                            "couldn't create or derive CLOB API credentials (HTTP {})",
                            derived.0.as_u16()
                        );
                    }
                    derived.1
                };
                #[derive(Deserialize)]
                struct Shape {
                    #[serde(rename = "apiKey")]
                    key: String,
                    secret: String,
                    passphrase: String,
                }
                // Deliberately no context from the body: it holds the secret.
                let shape: Shape = serde_json::from_str(&body)
                    .map_err(|_| anyhow!("unexpected shape in the API credentials response"))?;
                Ok(ApiCreds {
                    key: shape.key,
                    secret: shape.secret,
                    passphrase: shape.passphrase,
                })
            })
            .await
    }

    async fn l1_request(&self, method: Method, path: &str) -> Result<(StatusCode, String)> {
        let headers = l1_headers(
            &self.signer,
            self.exchange.chain_id,
            chrono::Utc::now().timestamp() as u64,
            0,
        )
        .await?;
        let url = format!("{}{path}", self.clob_base);
        let mut req = self.http.request(method, &url);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        let resp = req.send().await?;
        let status = resp.status();
        Ok((status, resp.text().await?))
    }

    /// `query` isn't covered by the signature, only the path is.
    async fn authed_request(
        &self,
        method: Method,
        path: &str,
        query: &str,
        body: Option<String>,
    ) -> Result<(StatusCode, String)> {
        let creds = self.creds().await?;
        let ts = chrono::Utc::now().timestamp().to_string();
        let body_text = body.as_deref().unwrap_or("");
        let signature = l2_signature(&creds.secret, &ts, method.as_str(), path, body_text)?;

        let url = format!("{}{path}{query}", self.clob_base);
        let mut req = self
            .http
            .request(method, &url)
            .header("POLY_ADDRESS", self.signer.address().to_checksum(None))
            .header("POLY_SIGNATURE", signature)
            .header("POLY_TIMESTAMP", ts)
            .header("POLY_API_KEY", &creds.key)
            .header("POLY_PASSPHRASE", &creds.passphrase);
        if let Some(body) = body {
            req = req.header("Content-Type", "application/json").body(body);
        }
        let resp = req.send().await?;
        let status = resp.status();
        Ok((status, resp.text().await?))
    }
}

#[async_trait]
impl OrderVenue for ClobClient {
    fn id(&self) -> VenueId {
        VenueId::Polymarket
    }

    async fn place(
        &self,
        planned: &PlannedOrder,
        expiration_secs: u64,
        live: bool,
    ) -> Result<OrderTicket> {
        self.ensure_version().await?;
        let params = self.book_params(&planned.token_id).await?;
        let signed = self
            .build_signed_order(planned, planned.order_type, expiration_secs, &params)
            .await?;
        let mut ticket = OrderTicket {
            token_id: signed.token_id.clone(),
            maker_amount: signed.maker_amount.clone(),
            taker_amount: signed.taker_amount.clone(),
            order_id: None,
            filled_shares: None,
        };
        if live {
            let resp = self.post_order(signed, planned.order_type).await?;
            let delayed = resp
                .status
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case("delayed"));
            ticket.filled_shares = filled_from_response(&resp, planned.side, planned.shares);
            if delayed && ticket.filled_shares.is_none() {
                if let Some(id) = resp.order_id.as_deref() {
                    ticket.filled_shares = self.poll_delayed_fill(id).await;
                }
            }
            if ticket.filled_shares.is_none() {
                warn!(
                    order_id = ?resp.order_id,
                    status = ?resp.status,
                    "couldn't confirm this order's fill; assuming the planned size"
                );
            }
            ticket.order_id = resp.order_id;
        }
        Ok(ticket)
    }
}

/// Wallet funds the CLOB can see, in pUSD.
#[derive(Debug, Clone, Default)]
struct Funds {
    balance_usd: f64,
    allowances: Vec<(Address, f64)>,
}

impl Funds {
    fn from_json(v: &Value) -> Result<Self> {
        let amount = |v: &Value| {
            v.as_str()
                .and_then(|s| s.parse::<f64>().ok())
                .or_else(|| v.as_f64())
        };
        let balance_usd = v
            .get("balance")
            .and_then(amount)
            .ok_or_else(|| anyhow!("balance response has no balance"))?
            / 1_000_000.0;
        let allowances = v
            .get("allowances")
            .and_then(Value::as_object)
            .map(|map| {
                map.iter()
                    .filter_map(|(k, v)| Some((Address::from_str(k).ok()?, amount(v)?)))
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self { balance_usd, allowances })
    }

    fn allowed(&self, spender: Address) -> bool {
        self.allowances.iter().any(|(a, v)| *a == spender && *v > 0.0)
    }
}

#[derive(Debug, Clone)]
struct Geoblock {
    blocked: bool,
    country: String,
    region: String,
}

impl Geoblock {
    fn location(&self) -> String {
        match (self.country.is_empty(), self.region.is_empty()) {
            (false, false) => format!("{}, {}", self.country, self.region),
            (false, true) => self.country.clone(),
            _ => "unknown location".to_string(),
        }
    }
}

/// `None` when the check can't be reached. The CLOB still makes its own decision.
async fn geoblock(http: &Client) -> Option<Geoblock> {
    let result: Result<Value> = async {
        Ok(http.get(GEOBLOCK_URL).send().await?.error_for_status()?.json().await?)
    }
    .await;
    match result {
        Ok(v) => {
            let text = |key: &str| {
                v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
            };
            Some(Geoblock {
                blocked: v.get("blocked").and_then(Value::as_bool).unwrap_or(false),
                country: text("country"),
                region: text("region"),
            })
        }
        Err(e) => {
            warn!(error = %e, "couldn't reach Polymarket's geoblock check");
            None
        }
    }
}

/// `check-venue` for Polymarket. Reads public data and signs a sample order
/// locally. Nothing is posted. `sign_in` also creates or derives API credentials
/// and reads the wallet's balance.
pub async fn check(cfg: &AppConfig, sign_in: bool) -> Result<()> {
    cfg.exchange.ensure_clob_v2()?;
    let http = Client::builder()
        .user_agent("polymarket-toolkits/0.2")
        .timeout(Duration::from_secs(15))
        .build()?;
    let clob = cfg.site.clob_api_base.trim_end_matches('/');

    let version: Value = http.get(format!("{clob}/version")).send().await?.json().await?;
    println!("clob           {clob}, protocol version {}", version["version"]);
    match geoblock(&http).await {
        Some(geo) if geo.blocked => println!(
            "geoblock       BLOCKED from {}: Polymarket would reject live orders here",
            geo.location()
        ),
        Some(geo) => println!("geoblock       not blocked ({})", geo.location()),
        None => println!("geoblock       check unreachable"),
    }

    let url = format!(
        "{}/markets?limit=1&active=true&closed=false&order=volume24hr&ascending=false",
        cfg.site.gamma_api_base.trim_end_matches('/')
    );
    let markets: Value = http.get(url).send().await?.error_for_status()?.json().await?;
    let market = markets
        .get(0)
        .ok_or_else(|| anyhow!("Gamma returned no open market"))?;
    let tokens: Vec<String> = serde_json::from_str(
        market["clobTokenIds"]
            .as_str()
            .ok_or_else(|| anyhow!("market has no clobTokenIds"))?,
    )?;
    let token_id = tokens
        .first()
        .ok_or_else(|| anyhow!("market has no tokens"))?
        .clone();
    println!("market         {}", market["question"].as_str().unwrap_or("?"));

    let client = match ClobClient::new(cfg) {
        Ok(client) => client,
        Err(e) => {
            println!("signer         none ({e:#}); orders can't be signed");
            return Ok(());
        }
    };
    let params = client.book_params(&token_id).await?;
    println!(
        "book           tick {}, minimum {} shares, neg_risk {}",
        params.tick_size, params.min_order_size, params.neg_risk
    );
    println!(
        "wallet         signer {}, funder 0x{:x}, signature type {:?}",
        client.signer_address(),
        client.funder,
        client.signature_type
    );

    let shares = params.min_order_size.max(5.0);
    let planned = PlannedOrder {
        venue: VenueId::Polymarket,
        token_id,
        side: Side::Buy,
        shares,
        limit_price: 0.5,
        usd_notional: shares * 0.5,
        order_type: OrderType::Gtc,
        source_trade_hash: None,
    };
    let signed = client
        .build_signed_order(&planned, OrderType::Gtc, 0, &params)
        .await?;
    let domain = client.domain_for(&params)?;
    println!(
        "signing        OK: sample order signed for {:?} (domain version {}), not sent",
        domain.verifying_contract.unwrap_or_default(),
        domain.version.as_deref().unwrap_or("?"),
    );
    println!("               maker {}, taker {}", signed.maker_amount, signed.taker_amount);

    if !sign_in {
        println!("auth           skipped (--sign-in to create or derive API credentials)");
        return Ok(());
    }
    let funds = client.balance_allowance().await?;
    println!("balance        {:.2} pUSD", funds.balance_usd);
    for (name, address) in client.exchange_spenders()? {
        println!(
            "allowance      {name}: {}",
            if funds.allowed(address) { "set" } else { "MISSING" }
        );
    }
    Ok(())
}

async fn l1_headers(
    signer: &PrivateKeySigner,
    chain_id: u64,
    timestamp: u64,
    nonce: u32,
) -> Result<Vec<(&'static str, String)>> {
    let auth = ClobAuth {
        address: signer.address(),
        timestamp: timestamp.to_string(),
        nonce: U256::from(nonce),
        message: CLOB_AUTH_MESSAGE.to_string(),
    };
    let domain = eip712_domain! {
        name: "ClobAuthDomain",
        version: "1",
        chain_id: chain_id,
    };
    let sig = signer
        .sign_hash(&auth.eip712_signing_hash(&domain))
        .await
        .context("signing CLOB auth message")?;
    Ok(vec![
        ("POLY_ADDRESS", format!("0x{:x}", signer.address())),
        ("POLY_SIGNATURE", format!("0x{}", hex::encode(sig.as_bytes()))),
        ("POLY_TIMESTAMP", timestamp.to_string()),
        ("POLY_NONCE", nonce.to_string()),
    ])
}

/// HMAC-SHA256 over timestamp + method + path + body, keyed by the base64-decoded
/// secret and returned as URL-safe base64 with padding.
fn l2_signature(secret: &str, ts: &str, method: &str, path: &str, body: &str) -> Result<String> {
    let key = base64_decode(secret).context("decoding the API secret")?;
    let message = format!("{ts}{method}{path}{body}");
    Ok(base64_url(&hmac_sha256(&key, message.as_bytes())))
}

fn side_str(side: Side) -> &'static str {
    match side {
        Side::Buy => "BUY",
        Side::Sell => "SELL",
    }
}

fn order_type_str(t: OrderType) -> &'static str {
    match t {
        OrderType::Fak => "FAK",
        OrderType::Gtd => "GTD",
        OrderType::Gtc => "GTC",
    }
}

fn parse_private_key(raw: &str) -> Result<[u8; 32]> {
    let trimmed = raw.trim().trim_start_matches("0x");
    let bytes = hex::decode(trimmed).map_err(|_| anyhow!("private key not valid hex"))?;
    if bytes.len() != 32 {
        return Err(anyhow!(
            "private key must be 32 bytes (64 hex chars), got {} bytes",
            bytes.len()
        ));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

// --- minimal HMAC-SHA256 ----------------------------------------------------
// Vendored to avoid pulling another crate solely for this. SHA-256 itself
// comes from the std-compatible `sha2`-style impl below.

fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        let h = sha256(key);
        k[..32].copy_from_slice(&h);
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let inner = {
        let mut buf = Vec::with_capacity(BLOCK + msg.len());
        buf.extend_from_slice(&ipad);
        buf.extend_from_slice(msg);
        sha256(&buf)
    };
    let mut outer = Vec::with_capacity(BLOCK + 32);
    outer.extend_from_slice(&opad);
    outer.extend_from_slice(&inner);
    sha256(&outer)
}

fn sha256(input: &[u8]) -> [u8; 32] {
    // Minimal SHA-256 (FIPS-180-4) — small and dependency-free.
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1,
        0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3,
        0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786,
        0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147,
        0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
        0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a,
        0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
        0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h = [
        0x6a09e667u32, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c,
        0x1f83d9ab, 0x5be0cd19,
    ];
    // Pre-processing
    let bit_len = (input.len() as u64) * 8;
    let mut padded = Vec::with_capacity(input.len() + 72);
    padded.extend_from_slice(input);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in padded.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes(chunk[i * 4..i * 4 + 4].try_into().unwrap());
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut a = h[0]; let mut b = h[1]; let mut c = h[2]; let mut d = h[3];
        let mut e = h[4]; let mut f = h[5]; let mut g = h[6]; let mut hh = h[7];
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            hh = g; g = f; f = e; e = d.wrapping_add(temp1);
            d = c; c = b; b = a; a = temp1.wrapping_add(temp2);
        }
        h[0] = h[0].wrapping_add(a); h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c); h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e); h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g); h[7] = h[7].wrapping_add(hh);
    }
    let mut out = [0u8; 32];
    for i in 0..8 {
        out[i * 4..i * 4 + 4].copy_from_slice(&h[i].to_be_bytes());
    }
    out
}

/// URL-safe base64 with padding, the format the CLOB expects.
fn base64_url(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(((data.len() + 2) / 3) * 4);
    for chunk in data.chunks(3) {
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = ((chunk[0] as u32) << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 { ALPHABET[((n >> 6) & 0x3f) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[(n & 0x3f) as usize] as char } else { '=' });
    }
    out
}

/// Accepts standard or URL-safe base64, padded or not.
fn base64_decode(raw: &str) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(raw.len() * 3 / 4);
    let (mut buffer, mut bits) = (0u32, 0u32);
    for c in raw.trim().trim_end_matches('=').bytes() {
        let value = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => bail!("not valid base64"),
        };
        buffer = (buffer << 6) | value as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Anvil's first key, public test material. The expected values below were
    // produced by Polymarket's official rs-clob-client-v2 (commit 981fc8f).
    const KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
    const FUNDER: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";
    const TOKEN: &str =
        "15871154585880608648532107628464183779895785213830018178010423617714102767076";

    fn client(funder: &str, signature_type: Option<&str>) -> ClobClient {
        let mut cfg: AppConfig =
            serde_json::from_str(include_str!("../../config.json")).unwrap();
        cfg.credentials.private_key = KEY.to_string();
        cfg.credentials.funder_address = funder.to_string();
        cfg.credentials.signature_type = signature_type.map(str::to_string);
        ClobClient::new(&cfg).unwrap()
    }

    fn params(neg_risk: bool, position_book: bool) -> BookParams {
        BookParams { tick_size: 0.01, min_order_size: 5.0, neg_risk, position_book }
    }

    fn fields(side: Side, expiration: u64) -> OrderFields {
        OrderFields {
            salt: 123_456_789,
            token_id: U256::from_str(TOKEN).unwrap(),
            maker_amount: 4_900_000,
            taker_amount: 10_000_000,
            side,
            timestamp_ms: 1_793_816_097_424,
            expiration,
        }
    }

    #[tokio::test]
    async fn eoa_order_matches_the_official_sdk() {
        let signed = client("", None)
            .sign_fields(&fields(Side::Buy, 0), &params(false, false))
            .await
            .unwrap();
        let body = OrderPostBody {
            order: signed,
            order_type: "FAK".to_string(),
            owner: "00000000-0000-0000-0000-000000000000".to_string(),
        };
        assert_eq!(serde_json::to_string(&body).unwrap(), SDK_EOA_BODY);
    }

    #[tokio::test]
    async fn neg_risk_order_matches_the_official_sdk() {
        let signed = client("", None)
            .sign_fields(&fields(Side::Sell, 1_793_900_000), &params(true, false))
            .await
            .unwrap();
        assert_eq!(signed.side, "SELL");
        assert_eq!(signed.expiration, "1793900000");
        assert_eq!(signed.signature, SDK_NEG_RISK_SIGNATURE);
    }

    #[tokio::test]
    async fn safe_order_matches_the_official_sdk() {
        let signed = client(FUNDER, Some("safe"))
            .sign_fields(&fields(Side::Buy, 0), &params(false, false))
            .await
            .unwrap();
        assert_eq!(signed.maker, "0x70997970c51812dc3a010c7d01b50e0d17dc79c8");
        assert_eq!(signed.signer, "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266");
        assert_eq!(signed.signature_type, 2);
        assert_eq!(signed.signature, SDK_SAFE_SIGNATURE);
    }

    #[tokio::test]
    async fn deposit_wallet_order_matches_the_official_sdk() {
        let signed = client(FUNDER, Some("deposit_wallet"))
            .sign_fields(&fields(Side::Buy, 0), &params(false, false))
            .await
            .unwrap();
        assert_eq!(signed.signer, "0x70997970c51812dc3a010c7d01b50e0d17dc79c8");
        assert_eq!(signed.signature_type, 3);
        assert_eq!(signed.signature, SDK_DEPOSIT_SIGNATURE);
    }

    #[tokio::test]
    async fn position_book_order_matches_the_official_sdk() {
        let signed = client("", None)
            .sign_fields(&fields(Side::Buy, 0), &params(false, true))
            .await
            .unwrap();
        assert_eq!(signed.signature, SDK_V3_SIGNATURE);
    }

    #[tokio::test]
    async fn l1_auth_matches_the_official_sdk() {
        let signer = PrivateKeySigner::from_bytes(&B256::from(parse_private_key(KEY).unwrap()))
            .unwrap();
        let headers = l1_headers(&signer, 80002, 10_000_000, 23).await.unwrap();
        let get = |k: &str| headers.iter().find(|(h, _)| *h == k).unwrap().1.clone();
        assert_eq!(get("POLY_ADDRESS"), "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266");
        assert_eq!(get("POLY_NONCE"), "23");
        assert_eq!(get("POLY_TIMESTAMP"), "10000000");
        assert_eq!(
            get("POLY_SIGNATURE"),
            "0xf62319a987514da40e57e2f4d7529f7bac38f0355bd88bb5adbb3768d80de6c1682518e0af677d5260366425f4361e7b70c25ae232aff0ab2331e2b164a1aedc1b"
        );
    }

    #[test]
    fn l2_signature_matches_the_official_sdk() {
        let signature = l2_signature(
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            "1000000",
            "test-sign",
            "/orders",
            r#"{"hash":"0x123"}"#,
        )
        .unwrap();
        assert_eq!(signature, "4gJVbox-R6XlDK4nlaicig0_ANVL1qdcahiL8CXfXLM=");
    }

    #[test]
    fn amounts_follow_the_tick_in_the_orders_favour() {
        // 0.505 isn't on a 0.01 tick: a buy drops to 0.50, a sell rises to 0.51.
        let buy = order_amounts(10.0, 0.505, Side::Buy, 0.01).unwrap();
        assert_eq!((buy.maker, buy.taker), (5_000_000, 10_000_000));
        let sell = order_amounts(10.0, 0.505, Side::Sell, 0.01).unwrap();
        assert_eq!((sell.maker, sell.taker), (10_000_000, 5_100_000));
    }

    #[test]
    fn amounts_cope_with_float_noise_and_fine_ticks() {
        let a = order_amounts(10.0, 0.29, Side::Buy, 0.01).unwrap();
        assert_eq!(a.maker, 2_900_000, "0.29 / 0.01 must not floor to 28 ticks");
        let b = order_amounts(5.55, 0.5025, Side::Buy, 0.0025).unwrap();
        assert_eq!((b.maker, b.taker), (2_788_875, 5_550_000));
    }

    #[test]
    fn amounts_round_size_down_to_two_decimals() {
        let a = order_amounts(12.349, 0.5, Side::Sell, 0.01).unwrap();
        assert_eq!((a.maker, a.taker), (12_340_000, 6_170_000));
        assert!(order_amounts(0.001, 0.5, Side::Buy, 0.01).is_err());
    }

    #[test]
    fn prices_stay_inside_the_book() {
        let low = order_amounts(10.0, 0.001, Side::Buy, 0.01).unwrap();
        assert_eq!(low.price_micro, 10_000);
        let high = order_amounts(10.0, 0.999, Side::Sell, 0.01).unwrap();
        assert_eq!(high.price_micro, 990_000);
    }

    #[test]
    fn only_gtd_orders_expire() {
        assert_eq!(expiration_for(OrderType::Fak, 60, 1000), 0);
        assert_eq!(expiration_for(OrderType::Gtc, 60, 1000), 0);
        // 60s would be rejected: three minutes is the minimum.
        assert_eq!(expiration_for(OrderType::Gtd, 60, 1000), 1185);
        assert_eq!(expiration_for(OrderType::Gtd, 600, 1000), 1660);
    }

    #[test]
    fn real_book_response_gives_signing_params() {
        let book: Value = serde_json::json!({
            "min_order_size": "5", "tick_size": "0.001", "neg_risk": true, "last_trade_price": "0.078"
        });
        assert_eq!(
            BookParams::from_book(&book).unwrap(),
            BookParams { tick_size: 0.001, min_order_size: 5.0, neg_risk: true, position_book: false }
        );
        let position: Value = serde_json::json!({"tick_size": "0.01", "version": "v2"});
        assert!(BookParams::from_book(&position).unwrap().position_book);
        assert!(BookParams::from_book(&serde_json::json!({})).is_err());
    }

    #[test]
    fn position_books_sign_against_exchange_v3() {
        let c = client("", None);
        let domain = c.domain_for(&params(false, true)).unwrap();
        assert_eq!(domain.version.as_deref(), Some("3"));
        let neg = c.domain_for(&params(true, false)).unwrap();
        assert_eq!(neg.version.as_deref(), Some("2"));
        assert_eq!(
            neg.verifying_contract.unwrap(),
            Address::from_str("0xe2222d279d744050d28e00520010520000310F59").unwrap()
        );
    }

    fn response(status: &str, making: &str, taking: &str) -> OrderResponse {
        OrderResponse {
            success: true,
            status: Some(status.to_string()),
            making_amount: Some(making.to_string()),
            taking_amount: Some(taking.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn fills_read_from_the_side_that_holds_shares() {
        assert_eq!(filled_from_response(&response("matched", "2.5", "5"), Side::Buy, 10.0), Some(5.0));
        assert_eq!(filled_from_response(&response("matched", "5", "2.5"), Side::Sell, 10.0), Some(5.0));
    }

    #[test]
    fn unfilled_and_unknown_fills_stay_distinct() {
        assert_eq!(filled_from_response(&response("live", "", ""), Side::Buy, 10.0), Some(0.0));
        assert_eq!(filled_from_response(&response("unmatched", "", ""), Side::Buy, 10.0), Some(0.0));
        assert_eq!(filled_from_response(&response("delayed", "", ""), Side::Buy, 10.0), None);
        assert_eq!(filled_from_response(&response("matched", "", ""), Side::Buy, 10.0), None);
        // Micro-units would be a million times too large: don't believe them.
        assert_eq!(filled_from_response(&response("matched", "2500000", "5000000"), Side::Buy, 10.0), None);
    }

    #[test]
    fn a_separate_funder_needs_a_stated_wallet_type() {
        let signer = Address::from_str("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266").unwrap();
        assert_eq!(resolve_wallet(signer, "", None).unwrap(), (signer, SignatureType::Eoa));
        assert_eq!(
            resolve_wallet(signer, "0x0000000000000000000000000000000000000000", None).unwrap(),
            (signer, SignatureType::Eoa)
        );
        assert!(resolve_wallet(signer, FUNDER, None).is_err());
        assert!(resolve_wallet(signer, FUNDER, Some("eoa")).is_err());
        assert!(resolve_wallet(signer, "", Some("safe")).is_err());
        assert_eq!(resolve_wallet(signer, FUNDER, Some("proxy")).unwrap().1, SignatureType::PolyProxy);
    }

    #[test]
    fn v1_settings_are_refused() {
        let mut cfg: AppConfig = serde_json::from_str(include_str!("../../config.json")).unwrap();
        cfg.credentials.private_key = KEY.to_string();
        cfg.exchange.domain_version = "1".to_string();
        assert!(ClobClient::new(&cfg).is_err());
    }

    #[test]
    fn partial_api_credentials_are_refused() {
        let mut cfg: AppConfig = serde_json::from_str(include_str!("../../config.json")).unwrap();
        cfg.credentials.private_key = KEY.to_string();
        cfg.credentials.api_key = Some("k".to_string());
        assert!(ClobClient::new(&cfg).is_err());
    }

    #[test]
    fn funds_read_balance_and_allowances() {
        let spender = "0xE111180000d2663C0091e4f400237545B87B996B";
        let funds = Funds::from_json(&serde_json::json!({
            "balance": "2500000",
            "allowances": { spender: "1000", "0xe2222d279d744050d28e00520010520000310F59": "0" }
        }))
        .unwrap();
        assert_eq!(funds.balance_usd, 2.5);
        assert!(funds.allowed(Address::from_str(spender).unwrap()));
        assert!(!funds.allowed(Address::from_str("0xe2222d279d744050d28e00520010520000310F59").unwrap()));
    }

    #[test]
    fn sha256_known_vectors() {
        // "abc" → ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
        let h = sha256(b"abc");
        assert_eq!(
            hex::encode(h),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn base64_round_trip_known() {
        // RFC 4648 §10 test vectors, padded.
        assert_eq!(base64_url(b"f"), "Zg==");
        assert_eq!(base64_url(b"fo"), "Zm8=");
        assert_eq!(base64_url(b"foo"), "Zm9v");
        assert_eq!(base64_decode("Zg==").unwrap(), b"f");
        assert_eq!(base64_decode("Zm8").unwrap(), b"fo");
        assert_eq!(base64_decode("Zm9v").unwrap(), b"foo");
        assert_eq!(base64_decode("-_-_").unwrap(), base64_decode("+/+/").unwrap());
        assert!(base64_decode("not base64!").is_err());
    }

    #[test]
    fn hmac_rfc4231_test_1() {
        // RFC 4231 §4.2 — HMAC-SHA-256 of "Hi There" with 20×0x0b key.
        let key = vec![0x0bu8; 20];
        let mac = hmac_sha256(&key, b"Hi There");
        assert_eq!(
            hex::encode(mac),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    const SDK_EOA_BODY: &str = r#"{"order":{"salt":123456789,"maker":"0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266","signer":"0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266","tokenId":"15871154585880608648532107628464183779895785213830018178010423617714102767076","makerAmount":"4900000","takerAmount":"10000000","side":"BUY","expiration":"0","signatureType":0,"timestamp":"1793816097424","metadata":"0x0000000000000000000000000000000000000000000000000000000000000000","builder":"0x0000000000000000000000000000000000000000000000000000000000000000","signature":"0x7e929f5f9c3b4487af5aac308b74d0ec2f5f7c9f6bfda3eeaa0ba69dc95e42ca1d08155cf5649ca91c37cc6b8616873474fdd30eec2e1f8056f82681d1a46b2f1b"},"orderType":"FAK","owner":"00000000-0000-0000-0000-000000000000"}"#;
    const SDK_NEG_RISK_SIGNATURE: &str = "0xb37c5c158504f91d78e8f5e20b7fc5d044727f5379e8c3d7d99913a4a3ca69982a947eba0bdebb85fccaa97337daaf0dc3a304f9efc0e772cc54dd42ec88c8f91b";
    const SDK_SAFE_SIGNATURE: &str = "0xfcb29144953fa607e66f192b181451f4163aea863763e41e5c988ac5ce9ea4b5498ad0ce81ada6023451f49c258dc34947b741a7014757641d3bc6e614d43edd1b";
    const SDK_V3_SIGNATURE: &str = "0x258ecf3d22585506379ca38f362e59389a185a04856b85d8525324619488d8824418b19fc594cc8194d33c7460e498edc00e618e649c5e57fd7da1f8279d52b01b";
    const SDK_DEPOSIT_SIGNATURE: &str = "0xaf8148755e125d3e499bdc8585fb72a6825c29b30a5ec0443fe9a87455bb1a1f1f79df90d6b31c9ea34700431e7c221adfd2401bd9978545857e8a219c80a2dc1c3264e159346253e26a64e00b69032db0e7d32f94628de3e6eecb50304d7af3d2542fd979ca20df0c777cfdbb0d7a41bccf21472fd14403439d73939bff5b6d2a4f726465722875696e743235362073616c742c61646472657373206d616b65722c61646472657373207369676e65722c75696e7432353620746f6b656e49642c75696e74323536206d616b6572416d6f756e742c75696e743235362074616b6572416d6f756e742c75696e743820736964652c75696e7438207369676e6174757265547970652c75696e743235362074696d657374616d702c62797465733332206d657461646174612c62797465733332206275696c6465722900ba";
}
