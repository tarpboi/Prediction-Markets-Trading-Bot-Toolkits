//! What the strategy bots share: hard limits, paper or live fills, the run loop.
//!
//! Strategies decide; [`Broker`] is the only way an order leaves. It enforces
//! [`StrategyLimits`] on every order and halts entries once the loss limit is
//! reached, so a bug in a strategy can't spend past them.
//!
//! On a dry run nothing is transmitted and fills are simulated against live
//! books: takers walk the book, and a resting order fills only once the book
//! trades through its price. Queue position isn't modelled, so paper fills miss
//! trades a real quote would get, and they don't show adverse selection the way
//! real ones do. Treat paper P&L as a check of the plumbing, not a forecast.

use crate::config::{AppConfig, FiltersConfig, StrategyLimits};
use crate::models::{BookLevel, BookSnapshot, OrderType, PlannedOrder, Side, VenueId};
use crate::service::eligibility::{self, Eligibility};
use crate::service::market_cache::MarketInfo;
use crate::service::predict_fun::{PredictFunClient, PredictFunTrader};
use crate::service::venue::{
    LimitOrder, LimitVenue, MarketData, MarketQuery, MarketSource, OrderVenue, TradableMarket,
};
use anyhow::{bail, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

/// The venue's minimum order value.
pub const MIN_ORDER_USD: f64 = 1.0;
const SETTLE_CHECK_EVERY: Duration = Duration::from_secs(60);
const SUMMARY_EVERY: Duration = Duration::from_secs(60);
/// Failed ticks in a row before resting orders are pulled: their prices were set
/// from data that's now stale.
const MAX_FAILED_TICKS: u32 = 3;
const EPS: f64 = 1e-9;

fn clean(price: f64) -> f64 {
    (price * 1e6).round() / 1e6
}

pub fn snap_down(price: f64, tick: f64) -> f64 {
    clean((price / tick + EPS).floor() * tick)
}

pub fn snap_up(price: f64, tick: f64) -> f64 {
    clean((price / tick - EPS).ceil() * tick)
}

/// Shares `usd` buys at `price`, rounded down to the hundredth.
pub fn shares_for(usd: f64, price: f64) -> f64 {
    if price <= 0.0 {
        return 0.0;
    }
    (usd / price * 100.0 + EPS).floor() / 100.0
}

/// Why the broker didn't send an order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocked {
    Halted,
    OrderCap,
    MarketCap,
    TotalCap,
    BelowMinimum,
    NothingHeld,
    WouldCross,
    ThinBook,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TakeResult {
    Filled { shares: f64, price: f64 },
    Blocked(Blocked),
}

#[derive(Debug, Clone)]
pub struct Holding {
    pub market_id: u64,
    pub token: String,
    pub shares: f64,
    pub cost: f64,
    pub opened_at: DateTime<Utc>,
}

impl Holding {
    pub fn avg_price(&self) -> f64 {
        if self.shares <= 0.0 {
            0.0
        } else {
            self.cost / self.shares
        }
    }
}

#[derive(Debug, Clone)]
pub struct Resting {
    pub id: String,
    pub hash: String,
    pub market_id: u64,
    pub token: String,
    pub side: Side,
    pub price: f64,
    pub shares: f64,
    pub filled: f64,
    pub expires_at: DateTime<Utc>,
    /// Removal was accepted. Kept until the venue says it's closed, since the
    /// venue says removal doesn't cancel an order on-chain.
    pub removing: bool,
}

impl Resting {
    fn remaining(&self) -> f64 {
        (self.shares - self.filled).max(0.0)
    }

    fn committed_usd(&self) -> f64 {
        match self.side {
            Side::Buy => self.remaining() * self.price,
            Side::Sell => 0.0,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Summary {
    pub realized: f64,
    pub fees: f64,
    pub unrealized: f64,
    pub open_cost: f64,
    pub holdings: usize,
    pub resting: usize,
}

impl Summary {
    pub fn equity(&self) -> f64 {
        self.realized - self.fees + self.unrealized
    }
}

#[derive(Debug, Default)]
struct State {
    holdings: HashMap<String, Holding>,
    /// Cost of what the wallet held at startup, by market. It counts toward the
    /// limits, but it isn't this run's to sell: it may be a position the operator
    /// took by hand.
    inherited: HashMap<u64, f64>,
    resting: HashMap<String, Resting>,
    marks: HashMap<String, f64>,
    realized: f64,
    fees: f64,
    halted: bool,
    next_paper_id: u64,
}

impl State {
    fn market_exposure(&self, market_id: u64) -> f64 {
        let held: f64 = self
            .holdings
            .values()
            .filter(|h| h.market_id == market_id)
            .map(|h| h.cost)
            .sum();
        let resting: f64 = self
            .resting
            .values()
            .filter(|r| r.market_id == market_id)
            .map(Resting::committed_usd)
            .sum();
        held + resting + self.inherited.get(&market_id).copied().unwrap_or(0.0)
    }

    fn total_exposure(&self) -> f64 {
        self.holdings.values().map(|h| h.cost).sum::<f64>()
            + self.resting.values().map(Resting::committed_usd).sum::<f64>()
            + self.inherited.values().sum::<f64>()
    }

    fn apply_fill(
        &mut self,
        market_id: u64,
        token: &str,
        side: Side,
        shares: f64,
        price: f64,
        fee: f64,
    ) {
        self.fees += fee;
        match side {
            Side::Buy => {
                let holding = self.holdings.entry(token.to_string()).or_insert_with(|| Holding {
                    market_id,
                    token: token.to_string(),
                    shares: 0.0,
                    cost: 0.0,
                    opened_at: Utc::now(),
                });
                holding.shares += shares;
                holding.cost += shares * price;
            }
            Side::Sell => {
                let Some(holding) = self.holdings.get_mut(token) else {
                    return;
                };
                let sold = shares.min(holding.shares);
                let basis = holding.avg_price() * sold;
                self.realized += sold * price - basis;
                holding.shares -= sold;
                holding.cost -= basis;
                if holding.shares <= EPS {
                    self.holdings.remove(token);
                }
            }
        }
    }

    fn summary(&self) -> Summary {
        let mut unrealized = 0.0;
        let mut open_cost = 0.0;
        for h in self.holdings.values() {
            let mark = self.marks.get(&h.token).copied().unwrap_or_else(|| h.avg_price());
            unrealized += h.shares * mark - h.cost;
            open_cost += h.cost;
        }
        Summary {
            realized: self.realized,
            fees: self.fees,
            unrealized,
            open_cost,
            holdings: self.holdings.len(),
            resting: self.resting.len(),
        }
    }
}

/// Walk `levels` for `shares`, refusing a partial fill or one past `limit`.
/// Returns the average price.
fn walk(levels: &[BookLevel], side: Side, shares: f64, limit: f64) -> Option<f64> {
    let mut remaining = shares;
    let mut cost = 0.0;
    for level in levels {
        let beyond = match side {
            Side::Buy => level.price > limit + EPS,
            Side::Sell => level.price < limit - EPS,
        };
        if beyond || remaining <= EPS {
            break;
        }
        let take = remaining.min(level.size);
        cost += take * level.price;
        remaining -= take;
    }
    (remaining <= EPS).then(|| clean(cost / shares))
}

pub struct Broker {
    limits: StrategyLimits,
    live: bool,
    taker: Option<Arc<dyn OrderVenue>>,
    limit: Option<Arc<dyn LimitVenue>>,
    state: Mutex<State>,
}

impl Broker {
    pub fn new(
        limits: StrategyLimits,
        live: bool,
        taker: Option<Arc<dyn OrderVenue>>,
        limit: Option<Arc<dyn LimitVenue>>,
    ) -> Result<Self> {
        if live && (taker.is_none() || limit.is_none()) {
            bail!("live trading needs a configured wallet");
        }
        Ok(Self {
            limits,
            live,
            taker,
            limit,
            state: Mutex::new(State::default()),
        })
    }

    pub fn is_live(&self) -> bool {
        self.live
    }

    pub fn is_halted(&self) -> bool {
        self.state.lock().halted
    }

    pub fn summary(&self) -> Summary {
        self.state.lock().summary()
    }

    pub fn holding(&self, token: &str) -> Option<Holding> {
        self.state.lock().holdings.get(token).cloned()
    }

    pub fn holdings(&self) -> Vec<Holding> {
        self.state.lock().holdings.values().cloned().collect()
    }

    /// Orders still working in a market, excluding ones being removed.
    pub fn resting_in(&self, market_id: u64) -> Vec<Resting> {
        self.state
            .lock()
            .resting
            .values()
            .filter(|r| r.market_id == market_id && !r.removing)
            .cloned()
            .collect()
    }

    /// Every market with a holding or a resting order, which must keep being
    /// observed even after a strategy loses interest in it.
    pub fn tracked_markets(&self) -> Vec<u64> {
        let state = self.state.lock();
        let mut ids: HashSet<u64> = state.holdings.values().map(|h| h.market_id).collect();
        ids.extend(state.resting.values().map(|r| r.market_id));
        ids.into_iter().collect()
    }

    /// Count what the wallet already holds toward the limits, so a restart can't
    /// double them. Those positions are never sold by a strategy, including ones
    /// from an earlier run. Live runs must not start without this.
    pub async fn seed_from_venue(&self) -> Result<()> {
        let Some(limit) = self.limit.as_ref() else {
            bail!("no venue to read positions from");
        };
        let positions = limit.positions().await?;
        let mut state = self.state.lock();
        for pos in &positions {
            *state.inherited.entry(pos.market.id).or_default() += pos.shares * pos.avg_price;
        }
        if !positions.is_empty() {
            warn!(
                positions = positions.len(),
                "wallet already holds positions: they count toward the limits, and no strategy \
                 will sell them"
            );
        }
        Ok(())
    }

    fn check_entry(&self, state: &State, market_id: u64, usd: f64) -> Option<Blocked> {
        if state.halted {
            Some(Blocked::Halted)
        } else if usd < MIN_ORDER_USD {
            Some(Blocked::BelowMinimum)
        } else if usd > self.limits.max_order_usd + EPS {
            Some(Blocked::OrderCap)
        } else if state.market_exposure(market_id) + usd > self.limits.max_market_usd + EPS {
            Some(Blocked::MarketCap)
        } else if state.total_exposure() + usd > self.limits.max_total_usd + EPS {
            Some(Blocked::TotalCap)
        } else {
            None
        }
    }

    /// Cross the spread for `shares`, no worse than `limit`, all or nothing.
    /// `yes_book` is the market's Yes-quoted book.
    pub async fn take(
        &self,
        market: &TradableMarket,
        token: &str,
        side: Side,
        shares: f64,
        limit: f64,
        yes_book: &BookSnapshot,
    ) -> Result<TakeResult> {
        let limit = clean(limit);
        let shares = {
            let state = self.state.lock();
            match side {
                Side::Buy => {
                    if let Some(blocked) = self.check_entry(&state, market.id, shares * limit) {
                        return Ok(TakeResult::Blocked(blocked));
                    }
                    shares
                }
                Side::Sell => {
                    let held = state.holdings.get(token).map_or(0.0, |h| h.shares);
                    if held <= EPS {
                        return Ok(TakeResult::Blocked(Blocked::NothingHeld));
                    }
                    let shares = shares.min(held);
                    if shares * limit < MIN_ORDER_USD {
                        return Ok(TakeResult::Blocked(Blocked::BelowMinimum));
                    }
                    shares
                }
            }
        };

        let book = token_book(market, token, yes_book);
        let levels = match side {
            Side::Buy => &book.asks,
            Side::Sell => &book.bids,
        };
        let Some(avg_price) = walk(levels, side, shares, limit) else {
            return Ok(TakeResult::Blocked(Blocked::ThinBook));
        };

        let (filled, price) = match self.taker.as_ref() {
            Some(venue) => {
                let planned = PlannedOrder {
                    venue: venue.id(),
                    token_id: token.to_string(),
                    side,
                    shares,
                    limit_price: limit,
                    usd_notional: shares * limit,
                    order_type: OrderType::Fak,
                    source_trade_hash: None,
                };
                let ticket = venue.place(&planned, 0, self.live).await?;
                if self.live {
                    // The venue reports size, not price. The limit is the worst
                    // the signed order allows, so costs aren't understated.
                    (ticket.filled_shares.unwrap_or(shares), limit)
                } else {
                    (shares, avg_price)
                }
            }
            None => (shares, avg_price),
        };
        if filled <= EPS {
            warn!(market = market.id, token, "order accepted but nothing filled");
            return Ok(TakeResult::Filled { shares: 0.0, price });
        }

        let mut state = self.state.lock();
        state.apply_fill(market.id, token, side, filled, price, market.taker_fee(price) * filled);
        info!(
            market = market.id,
            ?side,
            shares = filled,
            price,
            live = self.live,
            realized = state.realized,
            "taker fill"
        );
        Ok(TakeResult::Filled {
            shares: filled,
            price,
        })
    }

    /// Rest a limit order. Returns its id, or why it wasn't sent.
    pub async fn rest(
        &self,
        market: &TradableMarket,
        token: &str,
        side: Side,
        shares: f64,
        price: f64,
        ttl_secs: u64,
        yes_book: &BookSnapshot,
    ) -> Result<std::result::Result<String, Blocked>> {
        let price = clean(price);
        {
            let state = self.state.lock();
            match side {
                Side::Buy => {
                    if let Some(blocked) = self.check_entry(&state, market.id, shares * price) {
                        return Ok(Err(blocked));
                    }
                }
                Side::Sell => {
                    if state.halted {
                        return Ok(Err(Blocked::Halted));
                    }
                    let held = state.holdings.get(token).map_or(0.0, |h| h.shares);
                    let offered: f64 = state
                        .resting
                        .values()
                        .filter(|r| r.token == token && r.side == Side::Sell)
                        .map(Resting::remaining)
                        .sum();
                    if shares > held - offered + EPS {
                        return Ok(Err(Blocked::NothingHeld));
                    }
                    if shares * price < MIN_ORDER_USD {
                        return Ok(Err(Blocked::BelowMinimum));
                    }
                }
            }
        }

        let book = token_book(market, token, yes_book);
        let crosses = match side {
            Side::Buy => book.best_ask().is_some_and(|a| price >= a.price - EPS),
            Side::Sell => book.best_bid().is_some_and(|b| price <= b.price + EPS),
        };
        if crosses {
            return Ok(Err(Blocked::WouldCross));
        }

        let order = LimitOrder {
            market_id: market.id,
            token_id: token.to_string(),
            side,
            shares,
            price,
            ttl_secs,
            post_only: true,
        };
        let posted = match self.limit.as_ref() {
            Some(venue) => Some(venue.post_limit(&order, self.live).await?),
            None => None,
        };

        let mut state = self.state.lock();
        let id = match posted.as_ref().and_then(|p| p.order_id.clone()) {
            Some(id) => id,
            None => {
                state.next_paper_id += 1;
                format!("paper-{}", state.next_paper_id)
            }
        };
        state.resting.insert(
            id.clone(),
            Resting {
                id: id.clone(),
                hash: posted.map(|p| p.order_hash).unwrap_or_default(),
                market_id: market.id,
                token: token.to_string(),
                side,
                price,
                shares,
                filled: 0.0,
                expires_at: Utc::now() + chrono::Duration::seconds(ttl_secs as i64),
                removing: false,
            },
        );
        debug!(market = market.id, ?side, shares, price, id, "quote resting");
        Ok(Ok(id))
    }

    /// Ask to remove orders. Returns the ones refused for now, which stay working.
    pub async fn cancel(&self, ids: &[String]) -> Result<Vec<String>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        if !self.live {
            let mut state = self.state.lock();
            for id in ids {
                state.resting.remove(id);
            }
            return Ok(Vec::new());
        }
        let Some(venue) = self.limit.as_ref() else {
            bail!("live trading needs a configured wallet");
        };
        let mut rejected = Vec::new();
        for batch in ids.chunks(100) {
            let result = venue.remove(batch).await?;
            let mut state = self.state.lock();
            for id in result.removed.iter().chain(result.noop.iter()) {
                if let Some(order) = state.resting.get_mut(id) {
                    order.removing = true;
                }
            }
            rejected.extend(result.rejected);
        }
        Ok(rejected)
    }

    pub async fn cancel_all(&self) -> Result<()> {
        let ids: Vec<String> = self
            .state
            .lock()
            .resting
            .values()
            .filter(|r| !r.removing)
            .map(|r| r.id.clone())
            .collect();
        let rejected = self.cancel(&ids).await?;
        if !rejected.is_empty() {
            warn!(
                count = rejected.len(),
                "orders inside the venue's removal lock stay open until they expire"
            );
        }
        Ok(())
    }

    /// Feed a fresh book: updates marks and the loss limit, and on a dry run
    /// simulates fills and expiry.
    pub fn observe(&self, market: &TradableMarket, yes_book: &BookSnapshot) {
        let mut state = self.state.lock();
        if let Some(mid) = yes_book.mid() {
            state.marks.insert(market.yes_token.clone(), mid);
            state.marks.insert(market.no_token.clone(), 1.0 - mid);
        }
        if !self.live {
            self.paper_fills(&mut state, market, yes_book);
        }
        self.check_loss_limit(&mut state);
    }

    fn paper_fills(&self, state: &mut State, market: &TradableMarket, yes_book: &BookSnapshot) {
        let no_book = yes_book.mirrored();
        let now = Utc::now();
        let mut done = Vec::new();
        for order in state.resting.values().filter(|r| r.market_id == market.id) {
            let book = if market.is_yes(&order.token) { yes_book } else { &no_book };
            let traded_through = match order.side {
                Side::Buy => book.best_ask().is_some_and(|a| a.price <= order.price + EPS),
                Side::Sell => book.best_bid().is_some_and(|b| b.price >= order.price - EPS),
            };
            if traded_through {
                done.push((order.id.clone(), true));
            } else if order.expires_at <= now {
                done.push((order.id.clone(), false));
            }
        }
        for (id, filled) in done {
            let Some(order) = state.resting.remove(&id) else {
                continue;
            };
            if filled {
                state.apply_fill(
                    order.market_id,
                    &order.token,
                    order.side,
                    order.remaining(),
                    order.price,
                    0.0,
                );
                info!(
                    market = order.market_id,
                    side = ?order.side,
                    shares = order.remaining(),
                    price = order.price,
                    realized = state.realized,
                    "paper maker fill"
                );
            }
        }
    }

    fn check_loss_limit(&self, state: &mut State) {
        if state.halted {
            return;
        }
        let summary = state.summary();
        if summary.equity() <= -self.limits.max_loss_usd {
            state.halted = true;
            error!(
                equity = summary.equity(),
                limit = self.limits.max_loss_usd,
                "loss limit reached: no new entries until restart"
            );
        }
    }

    /// Live runs: read each working order's state and book what filled.
    pub async fn sync_live(&self) -> Result<()> {
        if !self.live {
            return Ok(());
        }
        let Some(venue) = self.limit.as_ref() else {
            return Ok(());
        };
        let orders: Vec<Resting> = self.state.lock().resting.values().cloned().collect();
        for order in orders {
            let venue_state = venue.order_state(&order.hash).await?;
            let mut state = self.state.lock();
            let new_fill = venue_state.filled_shares - order.filled;
            if new_fill > EPS {
                state.apply_fill(order.market_id, &order.token, order.side, new_fill, order.price, 0.0);
                info!(
                    market = order.market_id,
                    side = ?order.side,
                    shares = new_fill,
                    price = order.price,
                    realized = state.realized,
                    "maker fill"
                );
                if let Some(entry) = state.resting.get_mut(&order.id) {
                    entry.filled = venue_state.filled_shares;
                }
            }
            if !venue_state.open {
                state.resting.remove(&order.id);
            }
        }
        Ok(())
    }

    /// Close out holdings in markets whose result is known.
    pub async fn settle_resolved(&self, data: &dyn MarketData) -> Result<()> {
        let held: HashSet<u64> = self.holdings().iter().map(|h| h.market_id).collect();
        for id in held {
            let market = match data.market(id).await {
                Ok(market) => market,
                Err(e) => {
                    debug!(market = id, error = ?e, "settlement check failed");
                    continue;
                }
            };
            if let Some(payout_yes) = market.payout_yes {
                self.settle(&market, payout_yes);
            }
        }
        Ok(())
    }

    fn settle(&self, market: &TradableMarket, payout_yes: f64) {
        let mut state = self.state.lock();
        let tokens: Vec<String> = state
            .holdings
            .values()
            .filter(|h| h.market_id == market.id)
            .map(|h| h.token.clone())
            .collect();
        for token in tokens {
            let Some(holding) = state.holdings.remove(&token) else {
                continue;
            };
            let payout = if market.is_yes(&token) { payout_yes } else { 1.0 - payout_yes };
            let pnl = holding.shares * payout - holding.cost;
            state.realized += pnl;
            info!(
                market = market.id,
                shares = holding.shares,
                payout,
                pnl,
                realized = state.realized,
                "settled"
            );
            if self.live {
                warn!(
                    market = market.id,
                    "winnings aren't redeemed by this bot: redeem them on the venue"
                );
            }
        }
    }
}

/// `yes_book` as seen by `token`.
pub fn token_book(market: &TradableMarket, token: &str, yes_book: &BookSnapshot) -> BookSnapshot {
    if market.is_yes(token) {
        yes_book.clone()
    } else {
        yes_book.mirrored()
    }
}

// --- Quotes ------------------------------------------------------------------

/// An order a strategy wants resting.
#[derive(Debug, Clone, PartialEq)]
pub struct Quote {
    pub token: String,
    pub side: Side,
    pub price: f64,
    pub shares: f64,
}

/// Which working orders to remove and which wanted ones to send. An order within
/// `tolerance` of a wanted quote on the same token and side stays, so a quote
/// keeps its queue position when the market barely moved.
pub fn plan_quotes(
    working: &[Resting],
    wanted: &[Quote],
    tolerance: f64,
) -> (Vec<String>, Vec<Quote>) {
    let mut matched = vec![false; wanted.len()];
    let mut cancel = Vec::new();
    for order in working {
        let hit = wanted.iter().enumerate().position(|(i, q)| {
            !matched[i]
                && q.token == order.token
                && q.side == order.side
                && (q.price - order.price).abs() <= tolerance + EPS
        });
        match hit {
            Some(i) => matched[i] = true,
            None => cancel.push(order.id.clone()),
        }
    }
    let place = wanted
        .iter()
        .zip(&matched)
        .filter(|(_, hit)| !**hit)
        .map(|(q, _)| q.clone())
        .collect();
    (cancel, place)
}

/// Bring a market's working orders in line with `wanted`.
pub async fn reconcile(
    ctx: &Ctx,
    market: &TradableMarket,
    yes_book: &BookSnapshot,
    wanted: &[Quote],
    tolerance: f64,
) -> Result<()> {
    let working = ctx.broker.resting_in(market.id);
    let (cancel, place) = plan_quotes(&working, wanted, tolerance);
    let rejected = ctx.broker.cancel(&cancel).await?;
    for quote in place {
        // An order inside the removal lock is still live, so quoting beside it
        // would double the size on that side.
        let locked = working.iter().any(|o| {
            rejected.contains(&o.id) && o.token == quote.token && o.side == quote.side
        });
        if locked {
            continue;
        }
        let sent = ctx
            .broker
            .rest(
                market,
                &quote.token,
                quote.side,
                quote.shares,
                quote.price,
                ctx.cfg.strategies.quote_ttl_secs,
                yes_book,
            )
            .await?;
        if let Err(blocked) = sent {
            debug!(market = market.id, ?blocked, price = quote.price, "quote not sent");
        }
    }
    Ok(())
}

// --- Running a strategy ------------------------------------------------------

pub struct Ctx {
    pub cfg: AppConfig,
    pub client: Arc<PredictFunClient>,
    pub data: Arc<dyn MarketData>,
    pub broker: Arc<Broker>,
}

impl Ctx {
    /// Connect to the configured venue. The strategy bots need market listing and
    /// resting orders, which only the Predict.fun adapter has so far.
    pub fn connect(cfg: AppConfig, strategy: &str) -> Result<Self> {
        if cfg.venue != VenueId::PredictFun {
            bail!(
                "{strategy} runs on Predict.fun only so far: use --venue predict-fun. The \
                 {:?} adapter has no market listing or resting orders yet.",
                cfg.venue
            );
        }
        let client = PredictFunClient::new(&cfg)?;
        let live = cfg.live_trading_allowed();
        let trader = match PredictFunTrader::new(Arc::clone(&client), &cfg) {
            Ok(trader) => Some(Arc::new(trader)),
            Err(e) if !live => {
                info!(reason = %e, "no wallet: simulating fills without signing orders");
                None
            }
            Err(e) => return Err(e),
        };
        let taker = trader.clone().map(|t| t as Arc<dyn OrderVenue>);
        let limit = trader.map(|t| t as Arc<dyn LimitVenue>);
        let broker = Broker::new(cfg.strategies.limits.clone(), live, taker, limit)?;
        Ok(Self {
            cfg,
            data: client.clone(),
            client,
            broker: Arc::new(broker),
        })
    }

    /// Does the operator's allow/block list let this market through?
    pub async fn allowed(&self, market: &TradableMarket) -> bool {
        is_allowed(&self.client, &self.cfg.filters, market).await
    }
}

pub async fn is_allowed(
    client: &PredictFunClient,
    filters: &FiltersConfig,
    market: &TradableMarket,
) -> bool {
    let info = match &market.labels {
        // Scans cover thousands of markets, so no lookups when the list said.
        Some((category, tags)) => MarketInfo {
            slug: market.category_slug.clone(),
            question: market.question.clone(),
            yes_token_id: market.yes_token.clone(),
            no_token_id: market.no_token.clone(),
            category: category.clone(),
            tags: tags.clone(),
            closed: !market.open,
        },
        None => match client.by_token_id(&market.yes_token).await {
            Ok(info) => info,
            Err(e) => {
                warn!(market = market.id, error = ?e, "market metadata lookup failed");
                return false;
            }
        },
    };
    eligibility::check(filters, &info) == Eligibility::Allowed
}

/// The market list, read again every so often.
pub struct Universe {
    query: MarketQuery,
    every: Duration,
    last: Option<Instant>,
    markets: Vec<TradableMarket>,
}

impl Universe {
    pub fn new(query: MarketQuery, every_secs: u64) -> Self {
        Self {
            query,
            every: Duration::from_secs(every_secs),
            last: None,
            markets: Vec::new(),
        }
    }

    /// True when the list was just re-read.
    pub async fn refresh(&mut self, data: &dyn MarketData) -> Result<bool> {
        if self.last.is_some_and(|t| t.elapsed() < self.every) {
            return Ok(false);
        }
        self.markets = data.markets(&self.query).await?;
        self.last = Some(Instant::now());
        debug!(markets = self.markets.len(), "market list refreshed");
        Ok(true)
    }

    pub fn markets(&self) -> &[TradableMarket] {
        &self.markets
    }
}

/// The markets most worth quoting: open, not contested, with a two-sided book whose
/// spread is at least `min_spread` and no more than `max_spread`, and at least
/// `min_secs_left` to run, deepest first. Beyond `max_spread` the mid means nothing,
/// so there's nothing to quote around.
pub fn pick_quotable(
    markets: &[TradableMarket],
    books: &HashMap<u64, BookSnapshot>,
    min_spread: f64,
    max_spread: f64,
    min_secs_left: i64,
    count: usize,
    now: DateTime<Utc>,
) -> Vec<u64> {
    let mut ranked: Vec<(u64, f64)> = markets
        .iter()
        .filter(|m| is_quotable(m, min_secs_left, now))
        .filter_map(|m| {
            let book = books.get(&m.id)?;
            let spread = book.spread()?;
            (spread >= min_spread - EPS && spread <= max_spread + EPS)
                .then(|| (m.id, book.depth_usd(Side::Buy, 3) + book.depth_usd(Side::Sell, 3)))
        })
        .collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    ranked.into_iter().take(count).map(|(id, _)| id).collect()
}

/// Whether a market may carry quotes now. A market with no known end stays in.
pub fn is_quotable(market: &TradableMarket, min_secs_left: i64, now: DateTime<Utc>) -> bool {
    market.open
        && !market.contested
        && market.payout_yes.is_none()
        && market.secs_left(now).is_none_or(|left| left >= min_secs_left)
}

#[async_trait]
pub trait Strategy: Send {
    fn name(&self) -> &'static str;
    async fn tick(&mut self, ctx: &Ctx) -> Result<()>;
}

/// Run `strategy` until Ctrl-C, keeping the broker's books straight around it.
pub async fn drive(strategy: &mut dyn Strategy, ctx: &Ctx) -> Result<()> {
    let name = strategy.name();
    let limits = &ctx.cfg.strategies.limits;
    info!(
        strategy = name,
        live = ctx.broker.is_live(),
        network = ?ctx.cfg.predict_fun.network,
        max_order_usd = limits.max_order_usd,
        max_market_usd = limits.max_market_usd,
        max_total_usd = limits.max_total_usd,
        max_loss_usd = limits.max_loss_usd,
        "strategy starting"
    );
    if ctx.broker.is_live() {
        ctx.broker.seed_from_venue().await?;
    }

    let mut ticker =
        tokio::time::interval(Duration::from_secs(ctx.cfg.strategies.poll_secs.max(1)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut shutdown = Box::pin(tokio::signal::ctrl_c());
    let mut failed_ticks = 0u32;
    let mut last_settle = Instant::now();
    let mut last_summary = Instant::now();

    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => {
                info!(strategy = name, "shutdown signal received; pulling quotes");
                if let Err(e) = ctx.broker.cancel_all().await {
                    error!(error = ?e, "couldn't pull quotes: they stay open until they expire");
                }
                log_summary(name, ctx);
                return Ok(());
            }
            _ = ticker.tick() => {
                if let Err(e) = ctx.broker.sync_live().await {
                    warn!(error = ?e, "order state check failed");
                }
                if ctx.broker.is_halted() {
                    if let Err(e) = ctx.broker.cancel_all().await {
                        warn!(error = ?e, "couldn't pull quotes while halted");
                    }
                }
                let result = if ctx.broker.is_live() {
                    // Dropping a tick midway could lose track of an order it just
                    // posted, so a live run finishes it. Every call in it is
                    // time-limited, and Ctrl-C is seen on the next loop.
                    strategy.tick(ctx).await
                } else {
                    tokio::select! {
                        biased;
                        _ = &mut shutdown => {
                            info!(strategy = name, "shutdown signal received; pulling quotes");
                            if let Err(e) = ctx.broker.cancel_all().await {
                                error!(error = ?e, "couldn't pull quotes");
                            }
                            log_summary(name, ctx);
                            return Ok(());
                        }
                        result = strategy.tick(ctx) => result,
                    }
                };
                match result {
                    Ok(()) => failed_ticks = 0,
                    Err(e) => {
                        failed_ticks += 1;
                        warn!(strategy = name, error = ?e, failed_ticks, "tick failed");
                        if failed_ticks == MAX_FAILED_TICKS {
                            warn!("no fresh data: pulling resting orders");
                            if let Err(e) = ctx.broker.cancel_all().await {
                                error!(error = ?e, "couldn't pull quotes");
                            }
                        }
                    }
                }
                if last_settle.elapsed() >= SETTLE_CHECK_EVERY {
                    last_settle = Instant::now();
                    if let Err(e) = ctx.broker.settle_resolved(ctx.data.as_ref()).await {
                        warn!(error = ?e, "settlement check failed");
                    }
                }
                if last_summary.elapsed() >= SUMMARY_EVERY {
                    last_summary = Instant::now();
                    log_summary(name, ctx);
                }
            }
        }
    }
}

fn log_summary(name: &str, ctx: &Ctx) {
    let s = ctx.broker.summary();
    info!(
        strategy = name,
        live = ctx.broker.is_live(),
        realized = s.realized,
        fees = s.fees,
        unrealized = s.unrealized,
        open_cost = s.open_cost,
        holdings = s.holdings,
        resting = s.resting,
        halted = ctx.broker.is_halted(),
        "summary"
    );
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub fn level(price: f64, size: f64) -> BookLevel {
        BookLevel { price, size }
    }

    pub fn book(bids: &[(f64, f64)], asks: &[(f64, f64)]) -> BookSnapshot {
        BookSnapshot {
            bids: bids.iter().map(|(p, s)| level(*p, *s)).collect(),
            asks: asks.iter().map(|(p, s)| level(*p, *s)).collect(),
        }
    }

    pub fn market(id: u64) -> TradableMarket {
        TradableMarket {
            id,
            yes_token: format!("yes-{id}"),
            no_token: format!("no-{id}"),
            open: true,
            tick: 0.01,
            fee_rate_bps: 200,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::service::venue::{HeldPosition, OrderTicket, PostedOrder, RemoveResult, RestingState};

    fn limits() -> StrategyLimits {
        StrategyLimits {
            max_order_usd: 10.0,
            max_market_usd: 20.0,
            max_total_usd: 30.0,
            max_loss_usd: 5.0,
        }
    }

    fn paper() -> Broker {
        Broker::new(limits(), false, None, None).unwrap()
    }

    /// Records what reached the venue, standing in for it on a live run.
    #[derive(Default)]
    struct FakeVenue {
        placed: Mutex<Vec<PlannedOrder>>,
        posted: Mutex<Vec<LimitOrder>>,
        removed: Mutex<Vec<String>>,
        remove_result: Mutex<RemoveResult>,
        states: Mutex<HashMap<String, RestingState>>,
        fill: Mutex<Option<f64>>,
        held: Mutex<Vec<HeldPosition>>,
    }

    #[async_trait]
    impl OrderVenue for FakeVenue {
        fn id(&self) -> VenueId {
            VenueId::PredictFun
        }
        async fn place(&self, p: &PlannedOrder, _: u64, _: bool) -> Result<OrderTicket> {
            self.placed.lock().push(p.clone());
            Ok(OrderTicket {
                token_id: p.token_id.clone(),
                maker_amount: "0".into(),
                taker_amount: "0".into(),
                order_id: Some("taken".into()),
                filled_shares: *self.fill.lock(),
            })
        }
    }

    #[async_trait]
    impl LimitVenue for FakeVenue {
        async fn post_limit(&self, order: &LimitOrder, _: bool) -> Result<PostedOrder> {
            let mut posted = self.posted.lock();
            posted.push(order.clone());
            Ok(PostedOrder {
                order_hash: format!("hash-{}", posted.len()),
                order_id: Some(format!("order-{}", posted.len())),
            })
        }
        async fn remove(&self, ids: &[String]) -> Result<RemoveResult> {
            self.removed.lock().extend_from_slice(ids);
            Ok(self.remove_result.lock().clone())
        }
        async fn order_state(&self, hash: &str) -> Result<RestingState> {
            Ok(self.states.lock().get(hash).cloned().unwrap_or(RestingState {
                open: true,
                filled_shares: 0.0,
            }))
        }
        async fn positions(&self) -> Result<Vec<HeldPosition>> {
            Ok(self.held.lock().clone())
        }
    }

    fn live(venue: &Arc<FakeVenue>) -> Broker {
        Broker::new(
            limits(),
            true,
            Some(venue.clone() as Arc<dyn OrderVenue>),
            Some(venue.clone() as Arc<dyn LimitVenue>),
        )
        .unwrap()
    }

    #[test]
    fn prices_snap_to_the_tick() {
        assert_eq!(snap_down(0.378, 0.01), 0.37);
        assert_eq!(snap_up(0.372, 0.01), 0.38);
        assert_eq!(snap_down(0.37, 0.01), 0.37);
        assert_eq!(snap_up(0.37, 0.01), 0.37);
        assert_eq!(snap_down(0.1234, 0.001), 0.123);
        assert_eq!(shares_for(5.0, 0.37), 13.51);
    }

    #[test]
    fn live_refuses_to_start_without_a_wallet() {
        assert!(Broker::new(limits(), true, None, None).is_err());
    }

    #[tokio::test]
    async fn paper_take_walks_the_book_and_charges_the_taker_fee() {
        let b = paper();
        let m = market(1);
        let bk = book(&[(0.4, 100.0)], &[(0.5, 2.0), (0.6, 100.0)]);
        let r = b.take(&m, "yes-1", Side::Buy, 4.0, 0.6, &bk).await.unwrap();
        assert_eq!(r, TakeResult::Filled { shares: 4.0, price: 0.55 });
        let s = b.summary();
        assert!((s.open_cost - 2.2).abs() < 1e-9);
        assert!((s.fees - 4.0 * 0.02 * 0.45).abs() < 1e-9);
    }

    #[tokio::test]
    async fn a_no_limit_read_from_the_book_is_an_exact_price() {
        let venue = Arc::new(FakeVenue::default());
        *venue.fill.lock() = Some(5.0);
        let b = live(&venue);
        let m = market(1);
        let bk = book(&[(0.07, 100.0)], &[(0.09, 100.0)]);
        let ask = token_book(&m, "no-1", &bk).best_ask().unwrap().price;
        b.take(&m, "no-1", Side::Buy, 5.0, ask, &bk).await.unwrap();
        assert_eq!(venue.placed.lock()[0].limit_price, 0.93);
    }

    #[tokio::test]
    async fn take_refuses_a_partial_fill_or_one_past_the_limit() {
        let b = paper();
        let m = market(1);
        let bk = book(&[(0.4, 100.0)], &[(0.5, 3.0), (0.6, 100.0)]);
        let r = b.take(&m, "yes-1", Side::Buy, 4.0, 0.5, &bk).await.unwrap();
        assert_eq!(r, TakeResult::Blocked(Blocked::ThinBook));
        assert!(b.holdings().is_empty());
    }

    #[tokio::test]
    async fn no_token_trades_the_mirrored_book() {
        let b = paper();
        let m = market(1);
        let bk = book(&[(0.4, 100.0)], &[(0.5, 100.0)]);
        // A Yes bid at 0.40 is a No ask at 0.60.
        let r = b.take(&m, "no-1", Side::Buy, 5.0, 0.6, &bk).await.unwrap();
        assert_eq!(r, TakeResult::Filled { shares: 5.0, price: 0.6 });
    }

    #[tokio::test]
    async fn caps_stop_orders_before_they_reach_the_venue() {
        let venue = Arc::new(FakeVenue::default());
        let b = live(&venue);
        let m = market(1);
        let bk = book(&[(0.4, 1000.0)], &[(0.5, 1000.0)]);

        let r = b.take(&m, "yes-1", Side::Buy, 21.0, 0.5, &bk).await.unwrap();
        assert_eq!(r, TakeResult::Blocked(Blocked::OrderCap));
        let r = b.take(&m, "yes-1", Side::Buy, 1.0, 0.5, &bk).await.unwrap();
        assert_eq!(r, TakeResult::Blocked(Blocked::BelowMinimum));
        assert!(venue.placed.lock().is_empty());

        // Three 10 USD buys against a 20 USD market cap: the third is refused.
        for _ in 0..2 {
            let r = b.take(&m, "yes-1", Side::Buy, 20.0, 0.5, &bk).await.unwrap();
            assert!(matches!(r, TakeResult::Filled { .. }));
        }
        let r = b.take(&m, "yes-1", Side::Buy, 20.0, 0.5, &bk).await.unwrap();
        assert_eq!(r, TakeResult::Blocked(Blocked::MarketCap));
        assert_eq!(venue.placed.lock().len(), 2);

        // A second market still fits the 30 USD total, then the total stops it.
        let m2 = market(2);
        let r = b.take(&m2, "yes-2", Side::Buy, 20.0, 0.5, &bk).await.unwrap();
        assert!(matches!(r, TakeResult::Filled { .. }));
        let m3 = market(3);
        let r = b.take(&m3, "yes-3", Side::Buy, 2.0, 0.5, &bk).await.unwrap();
        assert_eq!(r, TakeResult::Blocked(Blocked::TotalCap));
    }

    #[tokio::test]
    async fn resting_buys_count_toward_the_caps() {
        let b = paper();
        let m = market(1);
        let bk = book(&[(0.4, 100.0)], &[(0.6, 100.0)]);
        for _ in 0..2 {
            let r = b.rest(&m, "yes-1", Side::Buy, 20.0, 0.5, 60, &bk).await.unwrap();
            assert!(r.is_ok());
        }
        let r = b.rest(&m, "yes-1", Side::Buy, 20.0, 0.5, 60, &bk).await.unwrap();
        assert_eq!(r, Err(Blocked::MarketCap));
    }

    #[tokio::test]
    async fn sells_are_clamped_to_what_is_held() {
        let b = paper();
        let m = market(1);
        let bk = book(&[(0.5, 100.0)], &[(0.6, 100.0)]);
        let r = b.take(&m, "yes-1", Side::Sell, 5.0, 0.5, &bk).await.unwrap();
        assert_eq!(r, TakeResult::Blocked(Blocked::NothingHeld));

        b.take(&m, "yes-1", Side::Buy, 10.0, 0.6, &bk).await.unwrap();
        let r = b.take(&m, "yes-1", Side::Sell, 99.0, 0.5, &bk).await.unwrap();
        assert_eq!(r, TakeResult::Filled { shares: 10.0, price: 0.5 });
        assert!(b.holding("yes-1").is_none());
        assert!((b.summary().realized - (-1.0)).abs() < 1e-9);
    }

    #[tokio::test]
    async fn resting_sells_cant_offer_more_than_is_held() {
        let b = paper();
        let m = market(1);
        let bk = book(&[(0.5, 100.0)], &[(0.6, 100.0)]);
        b.take(&m, "yes-1", Side::Buy, 8.0, 0.6, &bk).await.unwrap();
        let first = b.rest(&m, "yes-1", Side::Sell, 5.0, 0.59, 60, &bk).await.unwrap();
        assert!(first.is_ok());
        let second = b.rest(&m, "yes-1", Side::Sell, 5.0, 0.59, 60, &bk).await.unwrap();
        assert_eq!(second, Err(Blocked::NothingHeld));
    }

    #[tokio::test]
    async fn a_quote_that_would_cross_is_refused() {
        let b = paper();
        let m = market(1);
        let bk = book(&[(0.4, 100.0)], &[(0.5, 100.0)]);
        let r = b.rest(&m, "yes-1", Side::Buy, 10.0, 0.5, 60, &bk).await.unwrap();
        assert_eq!(r, Err(Blocked::WouldCross));
        // Buying No at 0.61 is selling Yes at 0.39, through the 0.40 bid.
        let r = b.rest(&m, "no-1", Side::Buy, 10.0, 0.61, 60, &bk).await.unwrap();
        assert_eq!(r, Err(Blocked::WouldCross));
    }

    #[tokio::test]
    async fn paper_quote_fills_only_when_the_book_trades_through_it() {
        let b = paper();
        let m = market(1);
        let bk = book(&[(0.4, 100.0)], &[(0.5, 100.0)]);
        b.rest(&m, "yes-1", Side::Buy, 10.0, 0.45, 60, &bk).await.unwrap().unwrap();

        b.observe(&m, &book(&[(0.44, 100.0)], &[(0.46, 100.0)]));
        assert!(b.holding("yes-1").is_none());

        b.observe(&m, &book(&[(0.43, 100.0)], &[(0.45, 100.0)]));
        let held = b.holding("yes-1").expect("filled");
        assert!((held.shares - 10.0).abs() < 1e-9);
        assert!(b.resting_in(1).is_empty());
        assert_eq!(b.summary().fees, 0.0, "makers pay no fee");
    }

    #[tokio::test]
    async fn paper_no_quote_fills_on_the_mirrored_book() {
        let b = paper();
        let m = market(1);
        let bk = book(&[(0.4, 100.0)], &[(0.5, 100.0)]);
        // No bid at 0.55 is a Yes ask at 0.45: filled when Yes bids reach 0.45.
        b.rest(&m, "no-1", Side::Buy, 10.0, 0.55, 60, &bk).await.unwrap().unwrap();
        b.observe(&m, &book(&[(0.44, 100.0)], &[(0.5, 100.0)]));
        assert!(b.holding("no-1").is_none());
        b.observe(&m, &book(&[(0.45, 100.0)], &[(0.5, 100.0)]));
        assert!(b.holding("no-1").is_some());
    }

    #[tokio::test]
    async fn paper_quote_expires() {
        let b = paper();
        let m = market(1);
        let bk = book(&[(0.4, 100.0)], &[(0.5, 100.0)]);
        b.rest(&m, "yes-1", Side::Buy, 10.0, 0.45, 0, &bk).await.unwrap().unwrap();
        b.observe(&m, &bk);
        assert!(b.resting_in(1).is_empty());
        assert!(b.holding("yes-1").is_none());
    }

    #[tokio::test]
    async fn loss_limit_halts_entries_but_not_exits() {
        let b = paper();
        let m = market(1);
        let bk = book(&[(0.5, 1000.0)], &[(0.5, 1000.0)]);
        b.take(&m, "yes-1", Side::Buy, 20.0, 0.5, &bk).await.unwrap();

        b.observe(&m, &book(&[(0.2, 1000.0)], &[(0.2, 1000.0)]));
        assert!(b.is_halted());

        let r = b.take(&market(2), "yes-2", Side::Buy, 10.0, 0.5, &bk).await.unwrap();
        assert_eq!(r, TakeResult::Blocked(Blocked::Halted));
        let exit = book(&[(0.2, 1000.0)], &[(0.3, 1000.0)]);
        let r = b.take(&m, "yes-1", Side::Sell, 20.0, 0.2, &exit).await.unwrap();
        assert!(matches!(r, TakeResult::Filled { .. }));
    }

    #[tokio::test]
    async fn settlement_pays_winners_and_ties_half() {
        let b = paper();
        let m = market(1);
        let bk = book(&[(0.4, 100.0)], &[(0.5, 100.0)]);
        b.take(&m, "yes-1", Side::Buy, 10.0, 0.5, &bk).await.unwrap();
        b.settle(&m, 1.0);
        assert!(b.holding("yes-1").is_none());
        assert!((b.summary().realized - 5.0).abs() < 1e-9);

        b.take(&m, "no-1", Side::Buy, 10.0, 0.6, &bk).await.unwrap();
        b.settle(&m, 0.5);
        assert!((b.summary().realized - 4.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn live_fill_records_the_size_the_venue_reports() {
        let venue = Arc::new(FakeVenue::default());
        *venue.fill.lock() = Some(3.0);
        let b = live(&venue);
        let m = market(1);
        let bk = book(&[(0.4, 100.0)], &[(0.5, 100.0)]);
        let r = b.take(&m, "yes-1", Side::Buy, 10.0, 0.5, &bk).await.unwrap();
        assert_eq!(r, TakeResult::Filled { shares: 3.0, price: 0.5 });
        assert!((b.holding("yes-1").unwrap().shares - 3.0).abs() < 1e-9);

        *venue.fill.lock() = Some(0.0);
        let r = b.take(&m, "yes-1", Side::Buy, 10.0, 0.5, &bk).await.unwrap();
        assert_eq!(r, TakeResult::Filled { shares: 0.0, price: 0.5 });
        assert!((b.holding("yes-1").unwrap().shares - 3.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn live_partial_fills_are_booked_once_and_closed_orders_drop() {
        let venue = Arc::new(FakeVenue::default());
        let b = live(&venue);
        let m = market(1);
        let bk = book(&[(0.4, 100.0)], &[(0.5, 100.0)]);
        b.rest(&m, "yes-1", Side::Buy, 10.0, 0.45, 60, &bk).await.unwrap().unwrap();

        venue.states.lock().insert(
            "hash-1".into(),
            RestingState { open: true, filled_shares: 4.0 },
        );
        b.sync_live().await.unwrap();
        b.sync_live().await.unwrap();
        assert!((b.holding("yes-1").unwrap().shares - 4.0).abs() < 1e-9);
        assert_eq!(b.resting_in(1).len(), 1);

        venue.states.lock().insert(
            "hash-1".into(),
            RestingState { open: false, filled_shares: 10.0 },
        );
        b.sync_live().await.unwrap();
        assert!((b.holding("yes-1").unwrap().shares - 10.0).abs() < 1e-9);
        assert!(b.resting_in(1).is_empty());
        assert_eq!(b.summary().resting, 0);
    }

    #[tokio::test]
    async fn live_removed_order_is_watched_until_the_venue_closes_it() {
        let venue = Arc::new(FakeVenue::default());
        let b = live(&venue);
        let m = market(1);
        let bk = book(&[(0.4, 100.0)], &[(0.5, 100.0)]);
        let id = b.rest(&m, "yes-1", Side::Buy, 10.0, 0.45, 60, &bk).await.unwrap().unwrap();

        venue.remove_result.lock().removed = vec![id.clone()];
        let rejected = b.cancel(&[id.clone()]).await.unwrap();
        assert!(rejected.is_empty());
        assert!(b.resting_in(1).is_empty(), "not offered to the strategy");
        assert_eq!(b.summary().resting, 1, "still counted until the venue closes it");

        // A fill landing after removal must still be booked.
        venue.states.lock().insert(
            "hash-1".into(),
            RestingState { open: false, filled_shares: 2.0 },
        );
        b.sync_live().await.unwrap();
        assert!((b.holding("yes-1").unwrap().shares - 2.0).abs() < 1e-9);
        assert_eq!(b.summary().resting, 0);
    }

    #[tokio::test]
    async fn live_rejected_removal_leaves_the_order_working() {
        let venue = Arc::new(FakeVenue::default());
        let b = live(&venue);
        let m = market(1);
        let bk = book(&[(0.4, 100.0)], &[(0.5, 100.0)]);
        let id = b.rest(&m, "yes-1", Side::Buy, 10.0, 0.45, 60, &bk).await.unwrap().unwrap();

        venue.remove_result.lock().rejected = vec![id.clone()];
        let rejected = b.cancel(&[id.clone()]).await.unwrap();
        assert_eq!(rejected, vec![id]);
        assert_eq!(b.resting_in(1).len(), 1);
    }

    #[tokio::test]
    async fn positions_already_in_the_wallet_limit_entries_but_are_never_sold() {
        let venue = Arc::new(FakeVenue::default());
        venue.held.lock().push(HeldPosition {
            market: market(1),
            token_id: "yes-1".into(),
            shares: 40.0,
            avg_price: 0.5,
        });
        let b = live(&venue);
        b.seed_from_venue().await.unwrap();
        let bk = book(&[(0.4, 100.0)], &[(0.5, 100.0)]);

        let r = b.take(&market(1), "yes-1", Side::Buy, 4.0, 0.5, &bk).await.unwrap();
        assert_eq!(r, TakeResult::Blocked(Blocked::MarketCap));
        // They also use up the total: 20 held plus 10 here reaches the 30 cap.
        let r = b.take(&market(2), "yes-2", Side::Buy, 20.0, 0.5, &bk).await.unwrap();
        assert!(matches!(r, TakeResult::Filled { .. }));
        let r = b.take(&market(3), "yes-3", Side::Buy, 2.0, 0.5, &bk).await.unwrap();
        assert_eq!(r, TakeResult::Blocked(Blocked::TotalCap));

        // A strategy can't see them, sell them or offer them.
        assert!(b.holding("yes-1").is_none());
        assert_eq!(b.holdings().len(), 1, "only this run's own position");
        let r = b.take(&market(1), "yes-1", Side::Sell, 40.0, 0.4, &bk).await.unwrap();
        assert_eq!(r, TakeResult::Blocked(Blocked::NothingHeld));
        let r = b.rest(&market(1), "yes-1", Side::Sell, 40.0, 0.6, 60, &bk).await.unwrap();
        assert_eq!(r, Err(Blocked::NothingHeld));
        assert_eq!(venue.placed.lock().len(), 1, "only the buy reached the venue");
    }

    #[test]
    fn plan_keeps_near_quotes_and_replaces_the_rest() {
        let order = |id: &str, price: f64| Resting {
            id: id.into(),
            hash: String::new(),
            market_id: 1,
            token: "yes-1".into(),
            side: Side::Buy,
            price,
            shares: 10.0,
            filled: 0.0,
            expires_at: Utc::now(),
            removing: false,
        };
        let quote = |price: f64| Quote {
            token: "yes-1".into(),
            side: Side::Buy,
            price,
            shares: 10.0,
        };
        let (cancel, place) = plan_quotes(&[order("a", 0.40)], &[quote(0.405)], 0.01);
        assert!(cancel.is_empty() && place.is_empty());

        let (cancel, place) = plan_quotes(&[order("a", 0.40)], &[quote(0.45)], 0.01);
        assert_eq!(cancel, vec!["a".to_string()]);
        assert_eq!(place, vec![quote(0.45)]);

        let (cancel, place) = plan_quotes(&[order("a", 0.40)], &[], 0.01);
        assert_eq!(cancel, vec!["a".to_string()]);
        assert!(place.is_empty());
    }

    #[test]
    fn quotable_markets_exclude_contested_ending_and_tight_ones() {
        let now = Utc::now();
        let mut good = market(1);
        good.ends_at = Some(now + chrono::Duration::hours(5));
        let mut deep = market(2);
        deep.ends_at = None;
        let mut disputed = market(3);
        disputed.contested = true;
        let mut ending = market(4);
        ending.ends_at = Some(now + chrono::Duration::minutes(5));
        let mut resolved = market(5);
        resolved.payout_yes = Some(1.0);
        let tight = market(6);

        let wide = book(&[(0.4, 10.0)], &[(0.5, 10.0)]);
        let wide_deep = book(&[(0.4, 1000.0)], &[(0.5, 1000.0)]);
        let narrow = book(&[(0.49, 10.0)], &[(0.5, 10.0)]);
        let books: HashMap<u64, BookSnapshot> = [
            (1, wide.clone()),
            (2, wide_deep),
            (3, wide.clone()),
            (4, wide.clone()),
            (5, wide),
            (6, narrow),
        ]
        .into_iter()
        .collect();
        let markets = [good, deep, disputed, ending, resolved, tight];
        assert_eq!(pick_quotable(&markets, &books, 0.04, 0.5, 3600, 5, now), vec![2, 1]);
        assert_eq!(pick_quotable(&markets, &books, 0.04, 0.5, 3600, 1, now), vec![2]);
        assert!(pick_quotable(&markets, &books, 0.04, 0.05, 3600, 5, now).is_empty());
    }
}
