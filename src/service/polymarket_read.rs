//! Read-only Polymarket lookups for cross-venue checks: a market's tokens by
//! condition id, and order books. Places nothing.

use crate::models::{BookLevel, BookSnapshot};
use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedMarket {
    pub question: String,
    pub yes_token: String,
    pub no_token: String,
}

#[derive(Debug, Deserialize)]
struct ApiMarket {
    #[serde(default)]
    question: String,
    #[serde(default)]
    active: bool,
    #[serde(default)]
    closed: bool,
    #[serde(default)]
    accepting_orders: bool,
    #[serde(default)]
    tokens: Vec<ApiToken>,
}

#[derive(Debug, Deserialize)]
struct ApiToken {
    token_id: String,
    outcome: String,
}

#[derive(Debug, Deserialize)]
struct ApiBook {
    #[serde(default)]
    bids: Vec<ApiLevel>,
    #[serde(default)]
    asks: Vec<ApiLevel>,
}

#[derive(Debug, Deserialize)]
struct ApiLevel {
    price: String,
    size: String,
}

pub struct PolymarketReader {
    http: Client,
    base: String,
}

impl PolymarketReader {
    pub fn new(base: &str) -> Result<Self> {
        Ok(Self {
            http: Client::builder()
                .user_agent("polymarket-toolkits/0.1")
                .timeout(Duration::from_secs(10))
                .build()?,
            base: base.trim_end_matches('/').to_string(),
        })
    }

    /// The tradable market behind a condition id, matched to `yes_name`/`no_name`.
    /// `None` when it's closed or its outcomes aren't those two: pairing markets
    /// that only look alike is how a hedged trade turns into two losing ones.
    pub async fn linked_market(
        &self,
        condition_id: &str,
        yes_name: &str,
        no_name: &str,
    ) -> Result<Option<LinkedMarket>> {
        let market: ApiMarket = self
            .http
            .get(format!("{}/markets/{condition_id}", self.base))
            .send()
            .await
            .context("Polymarket market")?
            .error_for_status()?
            .json()
            .await?;
        Ok(link(&market, yes_name, no_name))
    }

    pub async fn book(&self, token_id: &str) -> Result<BookSnapshot> {
        let book: ApiBook = self
            .http
            .get(format!("{}/book?token_id={token_id}", self.base))
            .send()
            .await
            .context("Polymarket book")?
            .error_for_status()?
            .json()
            .await?;
        Ok(snapshot(&book))
    }
}

fn link(market: &ApiMarket, yes_name: &str, no_name: &str) -> Option<LinkedMarket> {
    if !market.active || market.closed || !market.accepting_orders || market.tokens.len() != 2 {
        return None;
    }
    let token = |name: &str| {
        market
            .tokens
            .iter()
            .find(|t| t.outcome.eq_ignore_ascii_case(name))
            .map(|t| t.token_id.clone())
    };
    Some(LinkedMarket {
        question: market.question.clone(),
        yes_token: token(yes_name)?,
        no_token: token(no_name)?,
    })
}

/// Polymarket lists its best prices last, so sort: best first, like every book here.
fn snapshot(book: &ApiBook) -> BookSnapshot {
    let levels = |raw: &[ApiLevel]| -> Vec<BookLevel> {
        raw.iter()
            .filter_map(|l| {
                Some(BookLevel {
                    price: l.price.parse().ok()?,
                    size: l.size.parse().ok()?,
                })
            })
            .collect()
    };
    let mut out = BookSnapshot {
        bids: levels(&book.bids),
        asks: levels(&book.asks),
    };
    out.bids.sort_by(|a, b| b.price.total_cmp(&a.price));
    out.asks.sort_by(|a, b| a.price.total_cmp(&b.price));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn market(json: &str) -> ApiMarket {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn links_only_an_open_market_with_the_same_outcomes() {
        let open = r#"{"question":"Q","active":true,"closed":false,"accepting_orders":true,
            "tokens":[{"token_id":"1","outcome":"No"},{"token_id":"2","outcome":"Yes"}]}"#;
        let linked = link(&market(open), "Yes", "No").unwrap();
        assert_eq!(linked.yes_token, "2", "matched by name, not position");
        assert_eq!(linked.no_token, "1");
        assert!(link(&market(open), "Up", "Down").is_none());

        let closed = r#"{"question":"Q","active":true,"closed":true,"accepting_orders":false,
            "tokens":[{"token_id":"1","outcome":"Yes"},{"token_id":"2","outcome":"No"}]}"#;
        assert!(link(&market(closed), "Yes", "No").is_none());
    }

    #[tokio::test]
    #[ignore = "talks to the Polymarket API"]
    async fn live_reads_an_open_market_and_its_books() {
        #[derive(Deserialize)]
        struct Page {
            data: Vec<Row>,
        }
        #[derive(Deserialize)]
        struct Row {
            condition_id: String,
        }
        const BASE: &str = "https://clob.polymarket.com";
        let reader = PolymarketReader::new(BASE).unwrap();
        let page: Page = reader
            .http
            .get(format!("{BASE}/sampling-markets"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let mut compared = 0;
        for row in page.data.iter().take(25) {
            let Some(link) = reader.linked_market(&row.condition_id, "Yes", "No").await.unwrap() else {
                continue;
            };
            let yes = reader.book(&link.yes_token).await.unwrap();
            let no = reader.book(&link.no_token).await.unwrap();
            let (Some(yes_ask), Some(no_ask)) = (yes.best_ask(), no.best_ask()) else {
                continue;
            };
            let (Some(yes_bid), Some(no_bid)) = (yes.best_bid(), no.best_bid()) else {
                continue;
            };
            println!(
                "{}: yes {} / {}, no {} / {}",
                link.question, yes_bid.price, yes_ask.price, no_bid.price, no_ask.price
            );
            assert!(yes_bid.price <= yes_ask.price, "book not sorted best first");
            assert!(no_bid.price <= no_ask.price, "book not sorted best first");
            // The two tokens are mirror images, so this is a check on the pairing.
            assert!(yes_ask.price + no_ask.price >= 0.99, "asks add up to {}", yes_ask.price + no_ask.price);
            assert!(yes_ask.price + no_bid.price >= 0.98 && yes_ask.price + no_bid.price <= 1.02);
            compared += 1;
        }
        assert!(compared > 0, "no open two-sided market in the sample");
        println!("{compared} markets compared");
    }

    #[test]
    fn books_come_back_best_first() {
        let raw: ApiBook = serde_json::from_str(
            r#"{"bids":[{"price":"0.1","size":"5"},{"price":"0.4","size":"6"}],
                "asks":[{"price":"0.9","size":"5"},{"price":"0.5","size":"6"}]}"#,
        )
        .unwrap();
        let book = snapshot(&raw);
        assert_eq!(book.best_bid().unwrap().price, 0.4);
        assert_eq!(book.best_ask().unwrap().price, 0.5);
    }
}
