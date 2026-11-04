//! Take-profit / stop-loss monitor.
//!
//! Periodically polls the midprice of every open position. When the P&L
//! relative to the recorded entry crosses the take-profit or stop-loss
//! threshold, the monitor synthesises an exit `PlannedOrder` and posts it
//! through the existing executor — same signing path, same risk gates.

use crate::config::TpSlConfig;
use crate::models::{OrderType, PlannedOrder, Side};
use crate::service::midprice::MidpriceSource;
use crate::service::position_store::{OpenPosition, PositionStore};
use crate::service::venue::OrderVenue;
use crate::utils;
use anyhow::Result;
use std::sync::Arc;
use tokio::time::{interval, Duration};
use tracing::{debug, error, info, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    TakeProfit,
    StopLoss,
}

pub fn check_exit(pos: &OpenPosition, midprice: f64) -> Option<ExitReason> {
    let pnl = pos.pnl_pct(midprice);
    if pnl >= pos.take_profit_pct {
        Some(ExitReason::TakeProfit)
    } else if pnl <= -pos.stop_loss_pct {
        Some(ExitReason::StopLoss)
    } else {
        None
    }
}

/// Spawns the monitor task. The handle lets the caller observe shutdown
/// independently of the main bot loop.
pub fn spawn(
    cfg: TpSlConfig,
    positions: Arc<PositionStore>,
    venue: Arc<dyn OrderVenue>,
    midprice: Arc<dyn MidpriceSource>,
    live_trading: bool,
    price_buffer: f64,
    order_expiration_secs: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if !cfg.enabled {
            info!("TP/SL monitor disabled in config — exiting task");
            return;
        }
        let mut ticker = interval(Duration::from_secs(cfg.poll_interval_secs.max(1)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let snapshot = positions.snapshot();
            for pos in snapshot {
                if let Err(e) = monitor_once(
                    &pos,
                    &positions,
                    venue.as_ref(),
                    midprice.as_ref(),
                    live_trading,
                    price_buffer,
                    order_expiration_secs,
                )
                .await
                {
                    error!(
                        token = %pos.token_id,
                        error = ?e,
                        "tp/sl tick failed for position"
                    );
                }
            }
        }
    })
}

async fn monitor_once(
    pos: &OpenPosition,
    positions: &PositionStore,
    venue: &dyn OrderVenue,
    midprice: &dyn MidpriceSource,
    live_trading: bool,
    price_buffer: f64,
    order_expiration_secs: u64,
) -> Result<()> {
    let mp = midprice.midprice(&pos.token_id).await?;
    let pnl = pos.pnl_pct(mp);
    debug!(
        token = %pos.token_id,
        mid = mp,
        entry = pos.entry_price,
        pnl_pct = pnl,
        "tp/sl tick"
    );

    let Some(reason) = check_exit(pos, mp) else {
        return Ok(());
    };

    info!(
        token = %pos.token_id,
        slug = %pos.slug,
        ?reason,
        pnl_pct = pnl,
        midprice = mp,
        "TP/SL triggered — submitting exit"
    );

    let exit_side = pos.side.flip();
    let limit_price = match exit_side {
        // Selling out — accept a buffer below the mid to make sure we fill.
        Side::Sell => utils::clamp_price(mp - price_buffer),
        // Buying to cover — pay up by the buffer.
        Side::Buy => utils::clamp_price(mp + price_buffer),
    };

    let planned = PlannedOrder {
        venue: venue.id(),
        token_id: pos.token_id.clone(),
        side: exit_side,
        shares: pos.shares,
        limit_price,
        usd_notional: pos.shares * limit_price,
        order_type: OrderType::Fak,
        source_trade_hash: None,
    };

    if live_trading {
        match venue.place(&planned, order_expiration_secs, true).await {
            // Accepted but unfilled: the shares are still held, so keep watching.
            Ok(ticket) if ticket.order_id.is_some() && ticket.filled_shares == Some(0.0) => {
                warn!(
                    order_id = ?ticket.order_id,
                    token = %pos.token_id,
                    "exit order filled nothing; will retry next tick"
                );
            }
            // Part of it sold: keep watching what's left.
            Ok(ticket) if ticket.filled_shares.is_some_and(|f| f > 0.0 && f < pos.shares - 1e-6) => {
                let filled = ticket.filled_shares.unwrap_or(0.0);
                warn!(
                    order_id = ?ticket.order_id,
                    token = %pos.token_id,
                    filled,
                    held = pos.shares,
                    "exit filled in part; will retry the rest next tick"
                );
                positions.reduce(&pos.token_id, filled);
            }
            Ok(ticket) => {
                info!(
                    order_id = ?ticket.order_id,
                    token = %pos.token_id,
                    "exit order submitted"
                );
                positions.close(&pos.token_id);
            }
            Err(e) => {
                warn!(error = ?e, token = %pos.token_id, "exit POST failed; will retry next tick");
            }
        }
    } else {
        venue.place(&planned, order_expiration_secs, false).await?;
        info!(
            token = %pos.token_id,
            shares = planned.shares,
            limit = planned.limit_price,
            "dry-run exit signed (not submitted) — closing position locally"
        );
        positions.close(&pos.token_id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::VenueId;
    use crate::service::venue::OrderTicket;
    use async_trait::async_trait;
    use chrono::Utc;

    struct FakeVenue {
        order_id: Option<&'static str>,
        filled: Option<f64>,
    }

    #[async_trait]
    impl OrderVenue for FakeVenue {
        fn id(&self) -> VenueId {
            VenueId::PredictFun
        }
        async fn place(&self, planned: &PlannedOrder, _: u64, _: bool) -> Result<OrderTicket> {
            assert_eq!(planned.venue, VenueId::PredictFun);
            assert_eq!(planned.side, Side::Sell);
            Ok(OrderTicket {
                token_id: planned.token_id.clone(),
                maker_amount: "0".into(),
                taker_amount: "0".into(),
                order_id: self.order_id.map(String::from),
                filled_shares: self.filled,
            })
        }
    }

    struct FixedMid(f64);

    #[async_trait]
    impl MidpriceSource for FixedMid {
        async fn midprice(&self, _: &str) -> Result<f64> {
            Ok(self.0)
        }
    }

    /// Trigger take-profit against `venue` and report if the position survived.
    async fn still_open_after_exit(venue: FakeVenue) -> bool {
        let store = PositionStore::new();
        let p = pos(0.50, 30.0, 20.0, Side::Buy);
        store.open(p.clone());
        monitor_once(&p, &store, &venue, &FixedMid(0.80), true, 0.005, 60)
            .await
            .unwrap();
        store.get(&p.token_id).is_some()
    }

    #[tokio::test]
    async fn filled_exit_closes_the_position() {
        let venue = FakeVenue { order_id: Some("1"), filled: Some(100.0) };
        assert!(!still_open_after_exit(venue).await);
    }

    #[tokio::test]
    async fn exit_with_unknown_fill_closes_the_position() {
        // A delayed Polymarket order that hasn't settled by the last poll.
        let venue = FakeVenue { order_id: Some("1"), filled: None };
        assert!(!still_open_after_exit(venue).await);
    }

    #[tokio::test]
    async fn unfilled_exit_keeps_watching_the_position() {
        let venue = FakeVenue { order_id: Some("1"), filled: Some(0.0) };
        assert!(still_open_after_exit(venue).await);
    }

    #[tokio::test]
    async fn exit_with_nothing_held_closes_the_position() {
        let venue = FakeVenue { order_id: None, filled: Some(0.0) };
        assert!(!still_open_after_exit(venue).await);
    }

    #[tokio::test]
    async fn partial_exit_keeps_watching_what_is_left() {
        let store = PositionStore::new();
        let p = pos(0.50, 30.0, 20.0, Side::Buy);
        store.open(p.clone());
        let venue = FakeVenue { order_id: Some("1"), filled: Some(40.0) };
        monitor_once(&p, &store, &venue, &FixedMid(0.80), true, 0.005, 60)
            .await
            .unwrap();
        let left = store.get(&p.token_id).expect("60 shares are still held");
        assert_eq!(left.shares, 60.0);
        assert_eq!(left.usd_notional, 30.0);
    }

    fn pos(entry: f64, tp: f64, sl: f64, side: Side) -> OpenPosition {
        OpenPosition {
            token_id: "t".into(),
            slug: "s".into(),
            category: None,
            tags: vec![],
            side,
            entry_price: entry,
            shares: 100.0,
            usd_notional: 100.0 * entry,
            take_profit_pct: tp,
            stop_loss_pct: sl,
            opened_at: Utc::now(),
        }
    }

    #[test]
    fn take_profit_triggers() {
        let p = pos(0.50, 30.0, 20.0, Side::Buy);
        assert_eq!(check_exit(&p, 0.70), Some(ExitReason::TakeProfit));
    }

    #[test]
    fn stop_loss_triggers() {
        let p = pos(0.50, 30.0, 20.0, Side::Buy);
        assert_eq!(check_exit(&p, 0.39), Some(ExitReason::StopLoss));
    }

    #[test]
    fn flat_range_does_not_trigger() {
        let p = pos(0.50, 30.0, 20.0, Side::Buy);
        assert_eq!(check_exit(&p, 0.55), None);
    }

    #[test]
    fn sell_inverted_pnl_logic() {
        let p = pos(0.50, 30.0, 20.0, Side::Sell);
        // Price went DOWN — short position is in profit.
        assert_eq!(check_exit(&p, 0.30), Some(ExitReason::TakeProfit));
        // Price went UP — short position hit stop.
        assert_eq!(check_exit(&p, 0.65), Some(ExitReason::StopLoss));
    }
}
