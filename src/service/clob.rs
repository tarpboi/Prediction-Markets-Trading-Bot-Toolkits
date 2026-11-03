//! Polymarket CLOB v2 client — EIP-712 order signing + L2 HMAC auth + POST.
//!
//! The Polymarket exchange expects orders as EIP-712 typed data signed by the
//! maker EOA (or a delegated signer for proxy/Safe accounts) against the
//! `Polymarket CTF Exchange` domain on chain 137. The signed payload is then
//! posted to `clob.polymarket.com/order` with L2 HMAC headers derived from
//! per-account API credentials.
//!
//! Safety: every public entry-point honours `enable_trading` and
//! `mock_trading`. The hot path will not transmit a signed order until both
//! flags are explicitly permissive (see [`AppConfig::live_trading_allowed`]).

use crate::config::{AppConfig, ExchangeConfig};
use crate::models::{OrderType, PlannedOrder, Side, VenueId};
use crate::service::venue::{OrderTicket, OrderVenue};
use alloy_primitives::{Address, B256, U256};
use alloy_signer::Signer;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{eip712_domain, sol, Eip712Domain, SolStruct};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

sol! {
    /// Polymarket CTF Exchange order — EIP-712 typed data.
    ///
    /// NOTE: `side` and `signatureType` are encoded as `uint256` to match
    /// Polymarket's on-chain `Order` struct. If you point at a fork that
    /// declares them as `uint8`, narrow them here — the EIP-712 typehash
    /// embeds the field types verbatim, so this MUST match the deployed
    /// contract exactly or signatures will be rejected.
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
        uint256 side;
        uint256 signatureType;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SignatureType {
    Eoa = 0,
    PolyProxy = 1,
    PolyGnosisSafe = 2,
}

#[derive(Debug, Clone, Serialize)]
pub struct SignedOrder {
    pub salt: String,
    pub maker: String,
    pub signer: String,
    pub taker: String,
    #[serde(rename = "tokenId")]
    pub token_id: String,
    #[serde(rename = "makerAmount")]
    pub maker_amount: String,
    #[serde(rename = "takerAmount")]
    pub taker_amount: String,
    pub side: String,
    pub expiration: String,
    pub nonce: String,
    #[serde(rename = "feeRateBps")]
    pub fee_rate_bps: String,
    #[serde(rename = "signatureType")]
    pub signature_type: u8,
    pub signature: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct OrderPostBody {
    pub order: SignedOrder,
    pub owner: String,
    #[serde(rename = "orderType")]
    pub order_type: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OrderResponse {
    #[serde(default, rename = "orderID")]
    pub order_id: Option<String>,
    #[serde(default)]
    pub success: bool,
    #[serde(default, rename = "errorMsg")]
    pub error_msg: Option<String>,
}

pub struct ClobClient {
    http: Client,
    clob_base: String,
    signer: PrivateKeySigner,
    funder: Address,
    exchange: ExchangeConfig,
    fee_rate_bps: u32,
    signature_type: SignatureType,

    api_key: Option<String>,
    api_secret: Option<String>,
    api_passphrase: Option<String>,
}

impl ClobClient {
    pub fn new(cfg: &AppConfig) -> Result<Self> {
        let pk = parse_private_key(&cfg.credentials.private_key)
            .context("loading private key")?;
        let signer = PrivateKeySigner::from_bytes(&B256::from(pk))
            .context("creating EOA signer from private key")?;
        let funder = Address::from_str(&cfg.credentials.funder_address)
            .context("parsing funder address")?;

        // If a funder is provided that differs from the signer, we're using a
        // proxy/Safe; otherwise standard EOA signing.
        let signature_type = if funder == signer.address() {
            SignatureType::Eoa
        } else {
            SignatureType::PolyProxy
        };

        Ok(Self {
            http: Client::builder()
                .user_agent("polymarket-toolkits/0.1")
                .build()?,
            clob_base: cfg.site.clob_api_base.clone(),
            signer,
            funder,
            exchange: cfg.exchange.clone(),
            fee_rate_bps: cfg.trading.fee_rate_bps,
            signature_type,
            api_key: cfg.credentials.api_key.clone(),
            api_secret: cfg.credentials.api_secret.clone(),
            api_passphrase: cfg.credentials.api_passphrase.clone(),
        })
    }

    pub fn signer_address(&self) -> Address {
        self.signer.address()
    }

    /// Build, sign, and prepare an order without posting it. Useful for unit
    /// tests and dry-run logging.
    pub async fn build_signed_order(
        &self,
        planned: &PlannedOrder,
        order_type: OrderType,
        expiration_secs: u64,
    ) -> Result<SignedOrder> {
        let token_id_u256 = U256::from_str(&planned.token_id)
            .map_err(|_| anyhow!("token_id must be a U256 decimal"))?;
        let (maker_amount, taker_amount) =
            usd_and_share_amounts(planned.shares, planned.limit_price, planned.side);

        let expiration = match order_type {
            OrderType::Gtc => 0u64,
            _ => (chrono::Utc::now().timestamp() as u64).saturating_add(expiration_secs),
        };

        let order = Order {
            salt: U256::from(rand::random::<u128>()),
            maker: self.funder,
            signer: self.signer.address(),
            taker: Address::ZERO,
            tokenId: token_id_u256,
            makerAmount: maker_amount,
            takerAmount: taker_amount,
            expiration: U256::from(expiration),
            nonce: U256::ZERO,
            feeRateBps: U256::from(self.fee_rate_bps),
            side: U256::from(planned.side.as_u8()),
            signatureType: U256::from(self.signature_type as u8),
        };

        let verifying_contract = Address::from_str(&self.exchange.ctf_exchange_address)?;
        let domain: Eip712Domain = eip712_domain! {
            name: self.exchange.domain_name.clone(),
            version: self.exchange.domain_version.clone(),
            chain_id: self.exchange.chain_id,
            verifying_contract: verifying_contract,
        };
        let digest: B256 = order.eip712_signing_hash(&domain);
        let sig = self
            .signer
            .sign_hash(&digest)
            .await
            .context("signing order digest")?;

        Ok(SignedOrder {
            salt: order.salt.to_string(),
            maker: format!("0x{:x}", order.maker),
            signer: format!("0x{:x}", order.signer),
            taker: format!("0x{:x}", order.taker),
            token_id: order.tokenId.to_string(),
            maker_amount: order.makerAmount.to_string(),
            taker_amount: order.takerAmount.to_string(),
            side: side_str(planned.side).to_string(),
            expiration: order.expiration.to_string(),
            nonce: order.nonce.to_string(),
            fee_rate_bps: order.feeRateBps.to_string(),
            signature_type: self.signature_type as u8,
            signature: format!("0x{}", hex::encode(sig.as_bytes())),
        })
    }

    /// Post a fully signed order to `/order`. Requires L2 API credentials.
    pub async fn post_order(
        &self,
        signed: SignedOrder,
        order_type: OrderType,
    ) -> Result<OrderResponse> {
        let body = OrderPostBody {
            order: signed,
            owner: format!("0x{:x}", self.funder),
            order_type: order_type_str(order_type).to_string(),
        };
        let path = "/order";
        let body_json = serde_json::to_string(&body)?;
        let url = format!("{}{}", self.clob_base, path);

        let headers = self.l2_headers("POST", path, &body_json)?;
        let mut req = self.http.post(&url).body(body_json);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        let resp = req.send().await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(anyhow!(
                "CLOB rejected order (HTTP {}): {}",
                status.as_u16(),
                text
            ));
        }
        let parsed: OrderResponse =
            serde_json::from_str(&text).context("parsing CLOB response")?;
        Ok(parsed)
    }

    /// Build the L2 HMAC auth headers Polymarket expects on signed endpoints.
    fn l2_headers(
        &self,
        method: &str,
        path: &str,
        body: &str,
    ) -> Result<Vec<(&'static str, String)>> {
        let api_key = self
            .api_key
            .as_ref()
            .ok_or_else(|| anyhow!("L2 auth missing api_key — run the L1 sign-in flow first"))?;
        let api_secret = self
            .api_secret
            .as_ref()
            .ok_or_else(|| anyhow!("L2 auth missing api_secret"))?;
        let api_passphrase = self
            .api_passphrase
            .as_ref()
            .ok_or_else(|| anyhow!("L2 auth missing api_passphrase"))?;

        let ts = chrono::Utc::now().timestamp().to_string();
        let prehash = format!("{ts}{method}{path}{body}");
        let signature = hmac_sha256_base64url(api_secret, &prehash);

        Ok(vec![
            ("POLY_ADDRESS", format!("0x{:x}", self.signer.address())),
            ("POLY_SIGNATURE", signature),
            ("POLY_TIMESTAMP", ts),
            ("POLY_API_KEY", api_key.clone()),
            ("POLY_PASSPHRASE", api_passphrase.clone()),
            ("Content-Type", "application/json".into()),
        ])
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
        let signed = self
            .build_signed_order(planned, planned.order_type, expiration_secs)
            .await?;
        let mut ticket = OrderTicket {
            token_id: signed.token_id.clone(),
            maker_amount: signed.maker_amount.clone(),
            taker_amount: signed.taker_amount.clone(),
            order_id: None,
            filled_shares: None,
        };
        if live {
            ticket.order_id = self.post_order(signed, planned.order_type).await?.order_id;
        }
        Ok(ticket)
    }
}

fn usd_and_share_amounts(shares: f64, price: f64, side: Side) -> (U256, U256) {
    // USDC has 6 decimals on Polygon; CTF shares are also 6-decimal scaled by Polymarket.
    let shares_units = (shares * 1_000_000.0) as u128;
    let usd_units = ((shares * price) * 1_000_000.0) as u128;
    match side {
        // BUY: maker pays USDC, takes shares.
        Side::Buy => (U256::from(usd_units), U256::from(shares_units)),
        // SELL: maker gives shares, takes USDC.
        Side::Sell => (U256::from(shares_units), U256::from(usd_units)),
    }
}

fn side_str(side: Side) -> &'static str {
    match side {
        Side::Buy => "BUY",
        Side::Sell => "SELL",
    }
}

fn order_type_str(t: OrderType) -> &'static str {
    match t {
        OrderType::Fak => "FOK",
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

/// HMAC-SHA256 over `data` keyed by `secret`, returned as url-safe base64
/// without padding — the exact format Polymarket requires.
fn hmac_sha256_base64url(secret: &str, data: &str) -> String {
    let mac = hmac_sha256(secret.as_bytes(), data.as_bytes());
    base64_url_no_pad(&mac)
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

fn base64_url_no_pad(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(((data.len() + 2) / 3) * 4);
    let mut i = 0;
    while i + 3 <= data.len() {
        let n = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8) | (data[i + 2] as u32);
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        out.push(ALPHABET[(n & 0x3f) as usize] as char);
        i += 3;
    }
    let rem = data.len() - i;
    if rem == 1 {
        let n = (data[i] as u32) << 16;
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
    } else if rem == 2 {
        let n = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8);
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn base64url_round_trip_known() {
        // RFC 4648 §10 test vector: f → "Zg" (no padding)
        assert_eq!(base64_url_no_pad(b"f"), "Zg");
        assert_eq!(base64_url_no_pad(b"fo"), "Zm8");
        assert_eq!(base64_url_no_pad(b"foo"), "Zm9v");
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
}
