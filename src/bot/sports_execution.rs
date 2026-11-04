//! Sports execution: list live fixtures and hit them from the terminal.
//!
//! A manual tool rather than an automatic strategy. It lists open sports markets
//! with their books, and sends all-or-nothing orders at the best price when told
//! to, under the same limits as the bots. Prices are Predict's own book: there's no
//! external odds feed, because those are paid services.
//!
//! ```text
//! list [text]              open fixtures, optionally filtered
//! book <n>                 a listed market's book
//! buy <n> yes|no <usd>     cross the spread for that much
//! sell <n> yes|no          sell everything held of that outcome
//! pos                      holdings and P&L
//! quit
//! ```

use crate::bot::runtime::{shares_for, token_book, Ctx, TakeResult};
use crate::config::AppConfig;
use crate::models::{BookSnapshot, Side};
use crate::service::venue::{MarketQuery, TradableMarket};
use anyhow::{bail, Result};
use std::str::FromStr;
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing::info;

const SCAN_PAGES: u32 = 10;
const FIXTURE_VARIANTS: [&str; 10] = [
    "SPORTS_MATCH",
    "SPORTS_TEAM_MATCH",
    "SPORTS_NBA",
    "SPORTS_FIFA_WORLD_CUP",
    "SPORTS_FIFA_FRIENDLIES",
    "SPORTS_EXACT_SCORE",
    "SPORTS_HALFTIME_RESULT",
    "SPORTS_FIRST_TO_SCORE",
    "SPORTS_TOTAL_CORNERS",
    "SPORTS_SECOND_HALF_RESULT",
];

pub async fn run(cfg: AppConfig) -> Result<()> {
    let ctx = Ctx::connect(cfg, "Sports execution")?;
    if ctx.broker.is_live() {
        ctx.broker.seed_from_venue().await?;
    }
    println!(
        "Sports execution ({}). Commands: list [text], book <n>, buy <n> yes|no <usd>, sell <n> yes|no, pos, quit",
        if ctx.broker.is_live() { "LIVE ORDERS" } else { "dry run" }
    );

    let mut listed: Vec<TradableMarket> = Vec::new();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        match parse_command(&line) {
            Ok(Command::Quit) => break,
            Ok(Command::Nothing) => {}
            Ok(command) => {
                if let Err(e) = execute(&ctx, command, &mut listed).await {
                    println!("error: {e:#}");
                }
            }
            Err(e) => println!("{e}"),
        }
    }
    info!("sports execution finished");
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
enum Outcome {
    Yes,
    No,
}

#[derive(Debug, Clone, PartialEq)]
enum Command {
    List(Option<String>),
    Book(usize),
    Buy(usize, Outcome, f64),
    Sell(usize, Outcome),
    Positions,
    Quit,
    Nothing,
}

fn parse_command(line: &str) -> Result<Command> {
    let words: Vec<&str> = line.split_whitespace().collect();
    let index = |raw: &str| {
        usize::from_str(raw).map_err(|_| anyhow::anyhow!("not a listed number: {raw}"))
    };
    let outcome = |raw: &str| match raw.to_ascii_lowercase().as_str() {
        "yes" => Ok(Outcome::Yes),
        "no" => Ok(Outcome::No),
        other => bail!("expected yes or no, got {other}"),
    };
    Ok(match words.as_slice() {
        [] => Command::Nothing,
        ["list"] => Command::List(None),
        ["list", rest @ ..] => Command::List(Some(rest.join(" ").to_lowercase())),
        ["book", n] => Command::Book(index(n)?),
        ["buy", n, side, usd] => {
            let usd = f64::from_str(usd).map_err(|_| anyhow::anyhow!("not an amount: {usd}"))?;
            if !usd.is_finite() || usd <= 0.0 {
                bail!("amount must be positive");
            }
            Command::Buy(index(n)?, outcome(side)?, usd)
        }
        ["sell", n, side] => Command::Sell(index(n)?, outcome(side)?),
        ["pos"] => Command::Positions,
        ["quit"] | ["exit"] => Command::Quit,
        _ => bail!("commands: list [text], book <n>, buy <n> yes|no <usd>, sell <n> yes|no, pos, quit"),
    })
}

fn is_listable(market: &TradableMarket, include_props: bool, filter: Option<&str>) -> bool {
    market.open
        && !market.contested
        && market.variant.starts_with("SPORTS")
        && (include_props || !market.variant.contains("PROP"))
        && filter.is_none_or(|f| market.question.to_lowercase().contains(f))
}

async fn execute(ctx: &Ctx, command: Command, listed: &mut Vec<TradableMarket>) -> Result<()> {
    let settings = &ctx.cfg.strategies.sports;
    let pick = |listed: &[TradableMarket], n: usize| -> Result<TradableMarket> {
        n.checked_sub(1)
            .and_then(|i| listed.get(i))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no market {n}: run list first"))
    };
    match command {
        Command::List(filter) => {
            let all = ctx
                .data
                .markets(&MarketQuery {
                    variants: FIXTURE_VARIANTS
                        .iter()
                        .map(|v| v.to_string())
                        .chain(settings.include_props.then(|| "SPORTS_PROPS".to_string()))
                        .collect(),
                    max_pages: SCAN_PAGES,
                })
                .await?;
            let mut matching: Vec<TradableMarket> = all
                .into_iter()
                .filter(|m| is_listable(m, settings.include_props, filter.as_deref()))
                .collect();
            let ids: Vec<u64> = matching.iter().map(|m| m.id).collect();
            let books = ctx.data.books(&ids).await?;
            // Tradable ones first: a fixture with an empty book can't be hit.
            matching.sort_by_key(|m| {
                !books.get(&m.id).is_some_and(|b| b.best_bid().is_some() && b.best_ask().is_some())
            });
            matching.truncate(settings.list_limit);
            *listed = matching;
            for (i, market) in listed.iter().enumerate() {
                println!(
                    "{:>3}  {}  {}",
                    i + 1,
                    quote_line(books.get(&market.id)),
                    market.question
                );
            }
            println!("{} markets", listed.len());
        }
        Command::Book(n) => {
            let market = pick(listed, n)?;
            let book = one_book(ctx, &market).await?;
            println!("{}  ({} / {})", market.question, market.yes_name, market.no_name);
            for level in book.asks.iter().take(5).rev() {
                println!("  ask {:.3} x {:.2}", level.price, level.size);
            }
            for level in book.bids.iter().take(5) {
                println!("  bid {:.3} x {:.2}", level.price, level.size);
            }
        }
        Command::Buy(n, outcome, usd) => {
            let market = pick(listed, n)?;
            let book = one_book(ctx, &market).await?;
            let token = token_for(&market, &outcome);
            let view = token_book(&market, &token, &book);
            let Some(ask) = view.best_ask() else {
                bail!("nobody is offering that outcome");
            };
            let shares = shares_for(usd, ask.price);
            report(ctx
                .broker
                .take(&market, &token, Side::Buy, shares, ask.price, &book)
                .await?);
        }
        Command::Sell(n, outcome) => {
            let market = pick(listed, n)?;
            let book = one_book(ctx, &market).await?;
            let token = token_for(&market, &outcome);
            let view = token_book(&market, &token, &book);
            let Some(bid) = view.best_bid() else {
                bail!("nobody is bidding for that outcome");
            };
            let Some(held) = ctx.broker.holding(&token) else {
                bail!("nothing held of that outcome");
            };
            report(ctx
                .broker
                .take(&market, &token, Side::Sell, held.shares, bid.price, &book)
                .await?);
        }
        Command::Positions => {
            for holding in ctx.broker.holdings() {
                println!(
                    "market {}: {:.2} shares at {:.3}",
                    holding.market_id,
                    holding.shares,
                    holding.avg_price()
                );
            }
            let s = ctx.broker.summary();
            println!("realized {:.2}, fees {:.2}, open cost {:.2}", s.realized, s.fees, s.open_cost);
        }
        Command::Quit | Command::Nothing => {}
    }
    Ok(())
}

fn token_for(market: &TradableMarket, outcome: &Outcome) -> String {
    match outcome {
        Outcome::Yes => market.yes_token.clone(),
        Outcome::No => market.no_token.clone(),
    }
}

async fn one_book(ctx: &Ctx, market: &TradableMarket) -> Result<BookSnapshot> {
    let books = ctx.data.books(&[market.id]).await?;
    match books.get(&market.id) {
        Some(book) => {
            ctx.broker.observe(market, book);
            Ok(book.clone())
        }
        None => bail!("no order book for market {}", market.id),
    }
}

fn quote_line(book: Option<&BookSnapshot>) -> String {
    let best = |level: Option<&crate::models::BookLevel>| {
        level.map_or_else(|| "  -  ".to_string(), |l| format!("{:.3}", l.price))
    };
    match book {
        Some(b) => format!("{} / {}", best(b.best_bid()), best(b.best_ask())),
        None => "  -   /   -  ".to_string(),
    }
}

fn report(result: TakeResult) {
    match result {
        TakeResult::Filled { shares, price } => println!("filled {shares:.2} at {price:.3}"),
        TakeResult::Blocked(reason) => println!("not sent: {reason:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::runtime::test_support::market;

    #[test]
    fn parses_commands() {
        assert_eq!(parse_command("").unwrap(), Command::Nothing);
        assert_eq!(parse_command("list").unwrap(), Command::List(None));
        assert_eq!(
            parse_command("list Lakers vs").unwrap(),
            Command::List(Some("lakers vs".into()))
        );
        assert_eq!(parse_command("book 3").unwrap(), Command::Book(3));
        assert_eq!(
            parse_command("buy 2 YES 5.5").unwrap(),
            Command::Buy(2, Outcome::Yes, 5.5)
        );
        assert_eq!(parse_command("sell 2 no").unwrap(), Command::Sell(2, Outcome::No));
        assert_eq!(parse_command("quit").unwrap(), Command::Quit);
    }

    #[test]
    fn rejects_bad_input_rather_than_guessing() {
        assert!(parse_command("buy two yes 5").is_err());
        assert!(parse_command("buy 2 maybe 5").is_err());
        assert!(parse_command("buy 2 yes -5").is_err());
        assert!(parse_command("buy 2 yes NaN").is_err());
        assert!(parse_command("hello").is_err());
    }

    #[test]
    fn lists_open_fixtures_and_hides_props_unless_asked() {
        let fixture = TradableMarket {
            variant: "SPORTS_MATCH".into(),
            question: "Lakers vs Celtics".into(),
            ..market(1)
        };
        let props = TradableMarket {
            variant: "SPORTS_PROPS".into(),
            ..fixture.clone()
        };
        let crypto = TradableMarket {
            variant: "CRYPTO_UP_DOWN".into(),
            ..fixture.clone()
        };
        assert!(is_listable(&fixture, false, None));
        assert!(is_listable(&fixture, false, Some("lakers")));
        assert!(!is_listable(&fixture, false, Some("arsenal")));
        assert!(!is_listable(&props, false, None));
        assert!(is_listable(&props, true, None));
        assert!(!is_listable(&crypto, false, None));
        assert!(!is_listable(&TradableMarket { open: false, ..fixture }, false, None));
    }
}
