//! Copy-trading bot — production-ready.
//!
//! Architecture (matches §2 of the technical brief):
//!
//! 1. **Ingestion**: Polygon WebSocket `eth_subscribe`/`logs` filtered to the
//!    configured CTF Exchange contracts, topic-0 = `OrderFilled`, and the
//!    watched whale address packed as topic-2 (maker). Server-side filtering
//!    means the bot is woken only when the whale actually transacts.
//! 2. **Parse**: decode raw logs into [`WhaleTrade`] via [`service::parse`].
//! 3. **Eligibility**: resolve market metadata via Gamma, then check against
//!    the operator's allow/block lists ([`service::eligibility`]).
//! 4. **Sizing**: apply the configured copy strategy ([`service::strategy`]).
//! 5. **Exposure caps**: per-category and per-tag open-USD limits enforced
//!    via [`service::position_store`].
//! 6. **Risk**: fast in-memory check, optional book/depth check
//!    ([`service::risk_guard`]).
//! 7. **Execute**: build EIP-712 signed CTF order, post via L2 auth
//!    ([`service::clob`], [`service::order_executor`]).
//! 8. **TP/SL**: background monitor polls midprice for every open position
//!    and submits an exit FAK when P&L crosses the configured thresholds
//!    ([`service::position_monitor`]).
//!
//! Safety: `enable_trading=false` OR `mock_trading=true` keeps the executor
//! in dry-run mode — the full pipeline runs but signed orders are logged,
//! not submitted. Dry-run also records positions locally so the TP/SL
//! monitor exercises against real midprices.
//!
//! On Predict.fun, ingestion is the venue's trade feed rather than chain logs
//! (see [`service::predict_fun`]); steps 3 onward are the same pipeline.

use crate::config::AppConfig;
use crate::models::{VenueId, WhaleTrade};
use crate::service::{
    clob::ClobClient,
    market_cache::MarketCache,
    midprice::{ClobMidpriceSource, MidpriceSource},
    onchain::{spawn_subscription, LogFilter, RawLog},
    order_executor::{ExecutionOutcome, OrderExecutor},
    parse::{decode_whale_trade, order_filled_topic},
    position_monitor,
    position_store::PositionStore,
    predict_fun::{PredictFunClient, PredictFunTrader, WhaleFeed},
    risk_guard::RiskGuard,
    venue::OrderVenue,
};
use anyhow::{anyhow, bail, Result};
use reqwest::Client;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

const LOG_CHANNEL_CAPACITY: usize = 256;

pub async fn run(cfg: AppConfig) -> Result<()> {
    match cfg.venue {
        VenueId::Polymarket => run_polymarket(cfg).await,
        VenueId::PredictFun => run_predict_fun(cfg).await,
        other => bail!("copy trading has no {other:?} adapter yet"),
    }
}

async fn run_polymarket(cfg: AppConfig) -> Result<()> {
    let whale = cfg
        .bot
        .wallets_to_track
        .first()
        .cloned()
        .ok_or_else(|| anyhow!("config.bot.wallets_to_track is empty"))?;

    info!(
        whale = %whale,
        enable_trading = cfg.bot.enable_trading,
        mock_trading = cfg.bot.mock_trading,
        strict_allowlist = cfg.filters.is_strict(),
        tp_sl_enabled = cfg.tp_sl.enabled,
        "starting copy-trading bot"
    );

    let http = Client::builder()
        .user_agent("polymarket-toolkits/0.1")
        .build()?;
    let risk = RiskGuard::new(cfg.risk.clone());
    let markets = MarketCache::new(http.clone(), cfg.site.gamma_api_base.clone());
    let positions = PositionStore::new();

    let venue = ClobClient::new(&cfg).map(|c| Arc::new(c) as Arc<dyn OrderVenue>);
    let executor = OrderExecutor::new(
        cfg.clone(),
        venue,
        Arc::clone(&risk),
        markets,
        Arc::clone(&positions),
    )?;

    let midprice: Arc<dyn MidpriceSource> = Arc::new(ClobMidpriceSource::new(
        http.clone(),
        cfg.site.clob_api_base.clone(),
    ));
    spawn_tp_sl(&cfg, &executor, &positions, midprice);

    let filter = build_filter(&cfg, &whale)?;
    let (tx, mut rx) = mpsc::channel::<RawLog>(LOG_CHANNEL_CAPACITY);
    let _sub = spawn_subscription(cfg.site.polygon_ws_url.clone(), filter, tx);

    let mut shutdown = Box::pin(tokio::signal::ctrl_c());

    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => {
                info!(open_positions = positions.len(), "shutdown signal received");
                return Ok(());
            }
            maybe_log = rx.recv() => {
                let Some(log) = maybe_log else {
                    warn!("on-chain subscription channel closed");
                    return Ok(());
                };
                if let Err(e) = handle_log(&executor, &whale, &log).await {
                    error!(error = ?e, tx = %log.tx_hash, "handle_log failed");
                }
            }
        }
    }
}

async fn run_predict_fun(cfg: AppConfig) -> Result<()> {
    let whale = cfg
        .predict_fun
        .wallets_to_track
        .first()
        .cloned()
        .ok_or_else(|| anyhow!("config.predict_fun.wallets_to_track is empty"))?;

    info!(
        whale = %whale,
        network = ?cfg.predict_fun.network,
        enable_trading = cfg.bot.enable_trading,
        mock_trading = cfg.bot.mock_trading,
        strict_allowlist = cfg.filters.is_strict(),
        tp_sl_enabled = cfg.tp_sl.enabled,
        "starting copy-trading bot on Predict.fun"
    );

    let client = PredictFunClient::new(&cfg)?;
    let risk = RiskGuard::new(cfg.risk.clone());
    let positions = PositionStore::new();

    let venue = PredictFunTrader::new(Arc::clone(&client), &cfg)
        .map(|t| Arc::new(t) as Arc<dyn OrderVenue>);
    let executor = OrderExecutor::new(
        cfg.clone(),
        venue,
        risk,
        client.clone(),
        Arc::clone(&positions),
    )?;
    spawn_tp_sl(&cfg, &executor, &positions, client.clone());

    let mut feed = WhaleFeed::new(client, &whale)?;
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(
        cfg.trading.poll_interval_secs.max(1),
    ));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut shutdown = Box::pin(tokio::signal::ctrl_c());

    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => {
                info!(open_positions = positions.len(), "shutdown signal received");
                return Ok(());
            }
            _ = ticker.tick() => {
                let trades = match feed.poll().await {
                    Ok(trades) => trades,
                    Err(e) => {
                        warn!(error = ?e, "trade feed poll failed; retrying next tick");
                        continue;
                    }
                };
                for trade in trades {
                    if let Err(e) = handle_trade(&executor, &trade).await {
                        error!(error = ?e, tx = ?trade.tx_hash, "handle_trade failed");
                    }
                }
            }
        }
    }
}

/// TP/SL monitor — only spawn if we have a venue client (cfg permitted it).
fn spawn_tp_sl(
    cfg: &AppConfig,
    executor: &OrderExecutor,
    positions: &Arc<PositionStore>,
    midprice: Arc<dyn MidpriceSource>,
) {
    if !cfg.tp_sl.enabled {
        return;
    }
    if let Some(venue) = executor.venue() {
        position_monitor::spawn(
            cfg.tp_sl.clone(),
            Arc::clone(positions),
            venue,
            midprice,
            cfg.live_trading_allowed(),
            cfg.trading.price_buffer,
            cfg.trading.order_expiration_secs,
        );
        info!(
            poll_interval_secs = cfg.tp_sl.poll_interval_secs,
            "TP/SL monitor spawned"
        );
    } else {
        warn!(
            "TP/SL enabled but no venue client (missing credentials?) — \
             entries will still record positions, but exits won't fire"
        );
    }
}

async fn handle_log(executor: &OrderExecutor, whale: &str, log: &RawLog) -> Result<()> {
    let Some(trade) = decode_whale_trade(log, whale)? else {
        return Ok(());
    };
    handle_trade(executor, &trade).await
}

async fn handle_trade(executor: &OrderExecutor, trade: &WhaleTrade) -> Result<()> {
    info!(
        token = %trade.token_id,
        side = ?trade.side,
        shares = trade.shares,
        usd = trade.usd_notional,
        tx = ?trade.tx_hash,
        "whale trade detected"
    );
    match executor.execute(trade).await? {
        ExecutionOutcome::Skipped(r) => info!(?r, "execution skipped"),
        ExecutionOutcome::DryRun(ticket) => info!(
            token = %ticket.token_id,
            maker_amount = %ticket.maker_amount,
            taker_amount = %ticket.taker_amount,
            "dry-run order signed (not submitted)"
        ),
        ExecutionOutcome::Submitted { order_id, ticket } => info!(
            order_id = ?order_id,
            filled_shares = ?ticket.filled_shares,
            venue = ?trade.venue,
            "order submitted"
        ),
    }
    Ok(())
}

fn build_filter(cfg: &AppConfig, whale: &str) -> Result<LogFilter> {
    let exchanges = vec![
        cfg.exchange.ctf_exchange_address.to_lowercase(),
        cfg.exchange.neg_risk_exchange_address.to_lowercase(),
    ];
    let topic0 = format!("0x{}", hex::encode(order_filled_topic().as_slice()));
    let whale_topic = pad_address_to_topic(whale)?;
    Ok(LogFilter {
        address: exchanges,
        topics: vec![
            Some(vec![topic0]),
            None,
            Some(vec![whale_topic]),
        ],
    })
}

fn pad_address_to_topic(addr: &str) -> Result<String> {
    let trimmed = addr.trim().trim_start_matches("0x").to_lowercase();
    if trimmed.len() != 40 {
        return Err(anyhow!("address must be 20 bytes / 40 hex chars"));
    }
    Ok(format!("0x{}{}", "0".repeat(24), trimmed))
}
