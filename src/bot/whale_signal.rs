//! Whale signal bot: follows many wallets at once, each in its own risk bucket.
//!
//! Copy trading follows one wallet through the full sizing pipeline. This fans out:
//! every wallet in `predict_fun.wallets_to_track` gets a feed and a spending bucket,
//! so one busy wallet can't use up the limits. With no wallets listed it follows any
//! taker trade of at least `min_trade_usd` instead. It reads Predict's trade feed, so
//! it sees a trade after it happened: the signal can arrive with the price already
//! moved, which is what `max_price` guards against at the top end.

use crate::bot::quoting::Known;
use crate::bot::runtime::{drive, shares_for, token_book, Ctx, Strategy, TakeResult};
use crate::config::{AppConfig, WhaleSignalConfig};
use crate::models::{Side, WhaleTrade};
use crate::service::predict_fun::WhaleFeed;
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use tracing::{info, warn};

pub async fn run(cfg: AppConfig) -> Result<()> {
    let ctx = Ctx::connect(cfg, "Whale signal")?;
    let wallets = ctx.cfg.predict_fun.wallets_to_track.clone();
    let settings = ctx.cfg.strategies.whale_signal.clone();
    let feeds = if wallets.is_empty() {
        info!(min_trade_usd = settings.min_trade_usd, "no wallets listed: following large takers");
        vec![WhaleFeed::large_trades(ctx.client.clone(), settings.min_trade_usd)?]
    } else {
        info!(wallets = wallets.len(), "following wallets");
        wallets
            .iter()
            .map(|w| WhaleFeed::new(ctx.client.clone(), w).with_context(|| format!("wallet {w}")))
            .collect::<Result<Vec<_>>>()?
    };
    let mut bot = WhaleBot {
        cfg: settings,
        feeds,
        buckets: Buckets::default(),
        known: Known::default(),
        follow_exits: !wallets.is_empty(),
    };
    drive(&mut bot, &ctx).await
}

/// USD opened per followed wallet.
#[derive(Default)]
struct Buckets {
    opened: HashMap<String, f64>,
}

impl Buckets {
    fn has_room(&self, wallet: &str, usd: f64, cap: f64) -> bool {
        self.opened.get(wallet).copied().unwrap_or(0.0) + usd <= cap + 1e-9
    }

    fn add(&mut self, wallet: &str, usd: f64) {
        *self.opened.entry(wallet.to_string()).or_default() += usd;
    }
}

struct WhaleBot {
    cfg: WhaleSignalConfig,
    feeds: Vec<WhaleFeed>,
    buckets: Buckets,
    known: Known,
    /// A large taker selling says little about an entry someone else made; a
    /// followed wallet selling what it bought does.
    follow_exits: bool,
}

#[async_trait]
impl Strategy for WhaleBot {
    fn name(&self) -> &'static str {
        "whale-signal"
    }

    async fn tick(&mut self, ctx: &Ctx) -> Result<()> {
        let mut trades = Vec::new();
        for feed in &mut self.feeds {
            match feed.poll().await {
                Ok(new) => trades.extend(new),
                Err(e) => warn!(error = ?e, "trade feed poll failed; retrying next tick"),
            }
        }
        self.known.observe(ctx, &[]).await?;
        for trade in trades {
            if let Err(e) = self.follow(ctx, &trade).await {
                warn!(error = ?e, tx = ?trade.tx_hash, "couldn't act on trade");
            }
        }
        Ok(())
    }
}

impl WhaleBot {
    async fn follow(&mut self, ctx: &Ctx, trade: &WhaleTrade) -> Result<()> {
        let market = ctx.client.tradable_for_token(&trade.token_id)?;
        info!(
            wallet = %trade.maker,
            market = market.id,
            side = ?trade.side,
            usd = trade.usd_notional,
            price = trade.price,
            "whale trade seen"
        );
        match trade.side {
            Side::Buy if !market.open || market.contested || !ctx.allowed(&market).await => {
                return Ok(());
            }
            Side::Sell if !self.follow_exits || ctx.broker.holding(&trade.token_id).is_none() => {
                return Ok(());
            }
            _ => {}
        }
        let books = ctx.data.books(&[market.id]).await?;
        let Some(book) = books.get(&market.id) else {
            return Ok(());
        };
        ctx.broker.observe(&market, book);
        let view = token_book(&market, &trade.token_id, book);

        match trade.side {
            Side::Buy => {
                let Some(ask) = view.best_ask() else {
                    return Ok(());
                };
                if !worth_following(ask.price, trade.price, &self.cfg) {
                    info!(market = market.id, ask = ask.price, "price already out of range");
                    return Ok(());
                }
                let shares = shares_for(self.cfg.order_usd, ask.price);
                let usd = shares * ask.price;
                if !self.buckets.has_room(&trade.maker, usd, self.cfg.per_wallet_max_usd) {
                    info!(wallet = %trade.maker, "wallet's bucket is full");
                    return Ok(());
                }
                match ctx
                    .broker
                    .take(&market, &trade.token_id, Side::Buy, shares, ask.price, book)
                    .await?
                {
                    TakeResult::Filled { shares, price } if shares > 0.0 => {
                        self.buckets.add(&trade.maker, shares * price);
                    }
                    TakeResult::Filled { .. } => {}
                    TakeResult::Blocked(reason) => {
                        info!(market = market.id, ?reason, "entry blocked");
                    }
                }
            }
            Side::Sell => {
                let Some(bid) = view.best_bid() else {
                    return Ok(());
                };
                let Some(held) = ctx.broker.holding(&trade.token_id) else {
                    return Ok(());
                };
                let result = ctx
                    .broker
                    .take(&market, &trade.token_id, Side::Sell, held.shares, bid.price, book)
                    .await?;
                info!(market = market.id, ?result, "followed the wallet out");
            }
        }
        Ok(())
    }
}

/// Following only makes sense near the whale's price and below the near-certain
/// zone, where there's little left to gain.
fn worth_following(ask: f64, whale_price: f64, cfg: &WhaleSignalConfig) -> bool {
    const MAX_CHASE: f64 = 0.03;
    ask <= cfg.max_price + 1e-9 && ask <= whale_price + MAX_CHASE + 1e-9
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_wallet_has_its_own_bucket() {
        let mut buckets = Buckets::default();
        assert!(buckets.has_room("a", 25.0, 25.0));
        buckets.add("a", 20.0);
        assert!(buckets.has_room("a", 5.0, 25.0));
        assert!(!buckets.has_room("a", 6.0, 25.0));
        assert!(buckets.has_room("b", 25.0, 25.0));
    }

    #[test]
    fn wont_chase_a_moved_price_or_buy_near_certainties() {
        let cfg = WhaleSignalConfig { max_price: 0.9, ..Default::default() };
        assert!(worth_following(0.52, 0.50, &cfg));
        assert!(!worth_following(0.54, 0.50, &cfg));
        assert!(!worth_following(0.92, 0.91, &cfg));
    }
}
