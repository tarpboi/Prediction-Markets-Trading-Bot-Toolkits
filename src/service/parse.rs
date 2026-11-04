//! ABI log decoding for Polymarket CTF Exchange events.
//!
//! Decodes the CLOB V2 `OrderFilled` event into the [`WhaleTrade`] canonical
//! view. V2 changed this event: it states the side and token directly, instead
//! of the V1 pair of asset ids, so its topic-0 and layout both differ from V1.

use crate::models::{Side, VenueId, WhaleTrade};
use alloy_primitives::{B256, U256};
use alloy_sol_types::{sol, SolEvent};
use anyhow::{anyhow, Result};

use super::onchain::RawLog;

sol! {
    /// CTF Exchange V2: fired once per filled order.
    event OrderFilled(
        bytes32 indexed orderHash,
        address indexed maker,
        address indexed taker,
        uint8 side,
        uint256 tokenId,
        uint256 makerAmountFilled,
        uint256 takerAmountFilled,
        uint256 fee,
        bytes32 builder,
        bytes32 metadata
    );
}

/// Topic-0 selector for `OrderFilled`, derived once at compile time via the
/// `sol!` macro.
pub fn order_filled_topic() -> B256 {
    OrderFilled::SIGNATURE_HASH
}

/// Returns `Ok(Some(trade))` if the log was an `OrderFilled` involving the
/// given whale address (as maker), `Ok(None)` if it was not relevant, or `Err`
/// on a malformed log.
pub fn decode_whale_trade(log: &RawLog, whale_address: &str) -> Result<Option<WhaleTrade>> {
    let whale = whale_address.to_lowercase();
    let target_topic = order_filled_topic();

    let topic0 = log
        .topics
        .first()
        .ok_or_else(|| anyhow!("log has no topic0"))?;
    let topic0_b256: B256 = topic0
        .parse()
        .map_err(|e| anyhow!("topic0 not hex bytes32: {e}"))?;
    if topic0_b256 != target_topic {
        return Ok(None);
    }

    // topic0 + the three indexed fields: orderHash, maker, taker.
    if log.topics.len() < 4 {
        return Ok(None);
    }
    let maker_topic = &log.topics[2];
    let maker = topic_to_address(maker_topic)?;
    if maker.to_lowercase() != whale {
        return Ok(None);
    }

    // Non-indexed body: side, tokenId, makerAmountFilled, takerAmountFilled, fee,
    // builder, metadata.
    let data_bytes =
        hex::decode(log.data.trim_start_matches("0x")).map_err(|e| anyhow!(e))?;
    if data_bytes.len() < 7 * 32 {
        return Err(anyhow!("OrderFilled data shorter than 7*32 bytes"));
    }

    let side_word = U256::from_be_slice(&data_bytes[0..32]);
    let token_id = U256::from_be_slice(&data_bytes[32..64]).to_string();
    let maker_amount = to_f64_with_decimals(U256::from_be_slice(&data_bytes[64..96]), 6);
    let taker_amount = to_f64_with_decimals(U256::from_be_slice(&data_bytes[96..128]), 6);

    // A buy gives collateral for shares and a sell gives shares for collateral,
    // both with 6 decimals. Side::Buy = 0 and Side::Sell = 1 on chain.
    let (side, shares, usd_notional) = if side_word == U256::ZERO {
        (Side::Buy, taker_amount, maker_amount)
    } else if side_word == U256::from(1) {
        (Side::Sell, maker_amount, taker_amount)
    } else {
        return Err(anyhow!("OrderFilled has an unknown side {side_word}"));
    };
    let price = if shares > 0.0 { usd_notional / shares } else { 0.0 };

    Ok(Some(WhaleTrade {
        venue: VenueId::Polymarket,
        maker,
        side,
        token_id,
        shares,
        price,
        usd_notional,
        tx_hash: Some(log.tx_hash.clone()),
        block_number: Some(log.block_number),
        observed_at: chrono::Utc::now(),
    }))
}

fn topic_to_address(topic: &str) -> Result<String> {
    let bytes = hex::decode(topic.trim_start_matches("0x")).map_err(|e| anyhow!(e))?;
    if bytes.len() != 32 {
        return Err(anyhow!("address topic not 32 bytes"));
    }
    Ok(format!("0x{}", hex::encode(&bytes[12..32])))
}

fn to_f64_with_decimals(v: U256, decimals: u32) -> f64 {
    // Safe for typical USDC amounts; clamps if overflowing f64 mantissa.
    let s = v.to_string();
    s.parse::<f64>().unwrap_or(0.0) / 10f64.powi(decimals as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str =
        "110467134401642646227270093729063729697052084387543922254706197246736803930085";

    fn log(maker: &str, taker: &str, data: &str) -> RawLog {
        RawLog {
            address: "0xe111180000d2663c0091e4f400237545b87b996b".into(),
            topics: vec![
                "0xd543adfd945773f1a62f74f0ee55a5e3b9b1a28262980ba90b1a89f2ea84d8ee".into(),
                "0xe4a2cd5f9f206235e69afa05525ff6f9ba7a2c99df680c304a270e3c4df84069".into(),
                format!("0x000000000000000000000000{maker}"),
                format!("0x000000000000000000000000{taker}"),
            ],
            data: data.into(),
            tx_hash: "0x57356b51e4b2c9986df6ca64f73f7005936f79aeb7c8d9aaf2c297e9e95de79d".into(),
            block_number: 96741671,
        }
    }

    #[test]
    fn topic_matches_the_event_on_chain() {
        assert_eq!(
            format!("0x{}", hex::encode(order_filled_topic())),
            "0xd543adfd945773f1a62f74f0ee55a5e3b9b1a28262980ba90b1a89f2ea84d8ee"
        );
    }

    // Both logs are real fills from one transaction on the CTF Exchange.
    #[test]
    fn decodes_a_real_buy() {
        let maker = "6915daeb116f88027413d3ce74c99e7e98c89a47";
        let data = "0x0000000000000000000000000000000000000000000000000000000000000000f43a2eafb6c025ce90ae4231523f73c119f01e4627c74195f8f817cfe6d59fe500000000000000000000000000000000000000000000000000000000000f424000000000000000000000000000000000000000000000000000000000000f75b4000000000000000000000000000000000000000000000000000000000000038e9fedc0b0702ca6cb294e5321d9491b1e38b8bd2b463a7f7b06df8e6d7553cd180000000000000000000000000000000000000000000000000000000000000000";
        let trade = decode_whale_trade(
            &log(maker, "e111180000d2663c0091e4f400237545b87b996b", data),
            &format!("0x{maker}"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(trade.side, Side::Buy);
        assert_eq!(trade.token_id, TOKEN);
        assert_eq!(trade.usd_notional, 1.0);
        assert_eq!(trade.shares, 1.013172);
        assert!(trade.price > 0.98 && trade.price < 0.99);
    }

    #[test]
    fn decodes_a_real_sell() {
        let maker = "eee92f1cc6d6e0ad0b4ffda20b01cf3678e27ecb";
        let data = "0x0000000000000000000000000000000000000000000000000000000000000001f43a2eafb6c025ce90ae4231523f73c119f01e4627c74195f8f817cfe6d59fe500000000000000000000000000000000000000000000000000000000000f75b400000000000000000000000000000000000000000000000000000000000f4240000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";
        let trade = decode_whale_trade(
            &log(maker, "6915daeb116f88027413d3ce74c99e7e98c89a47", data),
            &format!("0x{maker}"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(trade.side, Side::Sell);
        assert_eq!(trade.token_id, TOKEN);
        assert_eq!(trade.shares, 1.013172);
        assert_eq!(trade.usd_notional, 1.0);
    }

    #[test]
    fn ignores_other_makers_and_rejects_v1_sized_data() {
        let short = format!("0x{}", "00".repeat(5 * 32));
        let maker = "6915daeb116f88027413d3ce74c99e7e98c89a47";
        let other = "0x0000000000000000000000000000000000000001";
        assert!(decode_whale_trade(&log(maker, maker, &short), other).unwrap().is_none());
        assert!(decode_whale_trade(&log(maker, maker, &short), &format!("0x{maker}")).is_err());
    }
}

