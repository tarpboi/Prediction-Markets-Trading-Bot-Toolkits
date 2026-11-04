//! Plumbing the book-watching strategies share: tracking known markets, feeding
//! fresh books to the broker, and picking which markets to quote.

use crate::bot::runtime::{is_allowed, is_quotable, pick_quotable, reconcile, Ctx, Quote};
use crate::config::FiltersConfig;
use crate::models::BookSnapshot;
use crate::service::predict_fun::PredictFunClient;
use crate::service::venue::{MarketData, MarketQuery, TradableMarket};
use anyhow::Result;
use chrono::Utc;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{info, warn};

/// Pages of the market list read per scan, by lifetime volume.
pub const SCAN_PAGES: u32 = 5;
/// Spreads wider than this are dead books, not opportunities.
const MAX_QUOTABLE_SPREAD: f64 = 0.15;
/// Rounds follow an outside price, so quoting one around its own book means being
/// picked off by anyone watching that price.
const UNQUOTABLE_VARIANTS: [&str; 2] = ["CRYPTO_UP_DOWN", "UP_DOWN"];

/// Every market a strategy has seen, so a position outlives the market list.
#[derive(Default)]
pub struct Known {
    markets: HashMap<u64, TradableMarket>,
}

impl Known {
    pub fn remember(&mut self, markets: &[TradableMarket]) {
        for market in markets {
            self.markets.insert(market.id, market.clone());
        }
    }

    pub fn get(&self, id: u64) -> Option<&TradableMarket> {
        self.markets.get(&id)
    }

    /// Fetch `ids` plus every market the broker is tracking, and show the broker
    /// those books.
    pub async fn observe(&mut self, ctx: &Ctx, ids: &[u64]) -> Result<HashMap<u64, BookSnapshot>> {
        let mut all: Vec<u64> = ids.to_vec();
        for id in ctx.broker.tracked_markets() {
            if !all.contains(&id) {
                all.push(id);
            }
        }
        for id in &all {
            if !self.markets.contains_key(id) {
                self.markets.insert(*id, ctx.data.market(*id).await?);
            }
        }
        let books = ctx.data.books(&all).await?;
        for (id, book) in &books {
            if let Some(market) = self.markets.get(id) {
                ctx.broker.observe(market, book);
            }
        }
        Ok(books)
    }
}

#[derive(Clone)]
struct ScanSpec {
    filters: FiltersConfig,
    min_spread: f64,
    min_secs_left: i64,
    count: usize,
}

/// Read the whole market list and choose. Takes a while on a venue with thousands
/// of markets, which is why it doesn't run on the quoting loop: quotes would sit
/// unrefreshed for the length of every scan.
async fn scan(
    client: &Arc<PredictFunClient>,
    spec: &ScanSpec,
) -> Result<(Vec<TradableMarket>, Vec<u64>)> {
    let markets = client
        .markets(&MarketQuery {
            variants: Vec::new(),
            max_pages: SCAN_PAGES,
        })
        .await?;
    let candidates: Vec<TradableMarket> = markets
        .into_iter()
        .filter(|m| !UNQUOTABLE_VARIANTS.contains(&m.variant.as_str()))
        .filter(|m| is_quotable(m, spec.min_secs_left, Utc::now()))
        .collect();
    let ids: Vec<u64> = candidates.iter().map(|m| m.id).collect();
    let books = client.books(&ids).await?;
    let ranked = pick_quotable(
        &candidates,
        &books,
        spec.min_spread,
        MAX_QUOTABLE_SPREAD,
        spec.min_secs_left,
        usize::MAX,
        Utc::now(),
    );

    // Schedules and tags cost a request each, so only for what might get chosen.
    let mut chosen = Vec::new();
    for id in &ranked {
        if chosen.len() >= spec.count {
            break;
        }
        let Some(market) = candidates.iter().find(|m| m.id == *id) else {
            continue;
        };
        let market = match client.with_schedule(market).await {
            Ok(market) => market,
            Err(e) => {
                warn!(market = id, error = ?e, "skipping market with no readable schedule");
                continue;
            }
        };
        if is_quotable(&market, spec.min_secs_left, Utc::now())
            && is_allowed(client, &spec.filters, &market).await
        {
            chosen.push(market);
        }
    }
    let selected: Vec<u64> = chosen.iter().map(|m| m.id).collect();
    info!(
        scanned = ids.len(),
        in_range = ranked.len(),
        selected = ?selected,
        "markets chosen"
    );
    Ok((chosen, selected))
}

/// Holds the current choice of markets to quote, refreshed in the background.
pub struct Picker {
    latest: Arc<Mutex<Option<(Vec<TradableMarket>, Vec<u64>)>>>,
    scanner: JoinHandle<()>,
    known: Known,
    selected: Vec<u64>,
    min_secs_left: i64,
}

impl Drop for Picker {
    fn drop(&mut self) {
        self.scanner.abort();
    }
}

impl Picker {
    pub fn new(ctx: &Ctx, min_spread: f64, min_secs_left: i64, count: usize) -> Self {
        let latest = Arc::new(Mutex::new(None));
        let every = Duration::from_secs(ctx.cfg.strategies.rescan_secs.max(1));
        let spec = ScanSpec {
            filters: ctx.cfg.filters.clone(),
            min_spread,
            min_secs_left,
            count,
        };
        let client = ctx.client.clone();
        let slot = Arc::clone(&latest);
        let scanner = tokio::spawn(async move {
            loop {
                match scan(&client, &spec).await {
                    Ok(result) => *slot.lock() = Some(result),
                    // Quoting carries on with the last choice.
                    Err(e) => warn!(error = ?e, "market scan failed"),
                }
                tokio::time::sleep(every).await;
            }
        });
        Self {
            latest,
            scanner,
            known: Known::default(),
            selected: Vec::new(),
            min_secs_left,
        }
    }

    pub fn known(&self) -> &Known {
        &self.known
    }

    /// Take up the latest choice, if a scan has finished since the last tick, and
    /// read books for the chosen markets and anything still open from an earlier
    /// choice.
    pub async fn step(&mut self, ctx: &Ctx) -> Result<HashMap<u64, BookSnapshot>> {
        if let Some((markets, selected)) = self.latest.lock().take() {
            self.known.remember(&markets);
            self.selected = selected;
        }
        let selected = self.selected.clone();
        self.known.observe(ctx, &selected).await
    }

    /// Quote every chosen market as `wanted` says, and pull orders anywhere the
    /// strategy no longer wants to be.
    pub async fn quote<F>(
        &mut self,
        ctx: &Ctx,
        books: &HashMap<u64, BookSnapshot>,
        tolerance: f64,
        mut wanted: F,
    ) -> Result<()>
    where
        F: FnMut(&TradableMarket, &BookSnapshot) -> Vec<Quote> + Send,
    {
        if ctx.broker.is_halted() {
            return Ok(());
        }
        let now = Utc::now();
        let mut ids = self.selected.clone();
        for id in ctx.broker.tracked_markets() {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        for id in ids {
            let (Some(market), Some(book)) = (self.known.get(id).cloned(), books.get(&id)) else {
                continue;
            };
            let chosen = self.selected.contains(&id) && is_quotable(&market, self.min_secs_left, now);
            let quotes = if chosen { wanted(&market, book) } else { Vec::new() };
            reconcile(ctx, &market, book, &quotes, tolerance).await?;
        }
        Ok(())
    }
}
