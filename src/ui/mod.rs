//! Terminal UI — launcher for picking which bot to run.
//!
//! `ratatui`-driven layout: left pane lists the 10 bots and their status,
//! right pane shows a description for the highlighted bot plus a brief help
//! footer. Enter launches the selected bot in the same process; q quits.

use crate::bot::{self, BotKind};
use crate::config::AppConfig;
use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    layout::{Constraint, Direction, Layout},
    prelude::*,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Terminal,
};
use std::io;
use std::time::Duration;
use tracing::info;

const BOTS: &[BotKind] = &[
    BotKind::CopyTrading,
    BotKind::BtcArb,
    BotKind::CrossArb,
    BotKind::DirectionalArb,
    BotKind::SpreadFarming,
    BotKind::Sports,
    BotKind::ResolutionSniper,
    BotKind::OrderbookImbalance,
    BotKind::MarketMaking,
    BotKind::WhaleSignal,
];

pub async fn run(cfg: AppConfig) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend)?;

    let result = ui_loop(&mut term, cfg).await;

    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    term.show_cursor()?;
    result
}

async fn ui_loop<B: Backend + io::Write>(term: &mut Terminal<B>, cfg: AppConfig) -> Result<()> {
    let mut state = ListState::default();
    state.select(Some(0));

    loop {
        term.draw(|f| draw(f, &cfg, &mut state))?;

        if event::poll(Duration::from_millis(150))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Down | KeyCode::Char('j') => {
                        let i = state.selected().unwrap_or(0);
                        state.select(Some((i + 1).min(BOTS.len() - 1)));
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        let i = state.selected().unwrap_or(0);
                        state.select(Some(i.saturating_sub(1)));
                    }
                    KeyCode::Enter => {
                        let i = state.selected().unwrap_or(0);
                        let kind = BOTS[i];
                        info!(bot = kind.label(), "launching from TUI");

                        // Drop out of raw-mode TUI temporarily so logs render
                        // to the regular terminal while the bot is running.
                        disable_raw_mode()?;
                        execute!(
                            term.backend_mut(),
                            LeaveAlternateScreen,
                            DisableMouseCapture
                        )?;
                        let res = bot::run(kind, cfg.clone()).await;
                        enable_raw_mode()?;
                        execute!(
                            term.backend_mut(),
                            EnterAlternateScreen,
                            EnableMouseCapture
                        )?;
                        term.clear()?;
                        if let Err(e) = res {
                            tracing::error!(error = ?e, "bot exited with error");
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

fn draw(f: &mut Frame, cfg: &AppConfig, state: &mut ListState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Min(0),    // body
            Constraint::Length(2), // footer
        ])
        .split(f.area());

    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            " Polymarket Toolkits ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!(
                "{} wallet(s) tracked · trading={} · mock={}",
                cfg.bot.wallets_to_track.len(),
                cfg.bot.enable_trading,
                cfg.bot.mock_trading
            ),
            Style::default().fg(Color::DarkGray),
        ),
    ]))
    .block(Block::default().borders(Borders::BOTTOM));
    f.render_widget(header, chunks[0]);

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(40), Constraint::Min(0)])
        .split(chunks[1]);

    let items: Vec<ListItem> = BOTS
        .iter()
        .enumerate()
        .map(|(_, k)| {
            let status = if matches!(k, BotKind::CrossArb) { "👀" } else { "🧪" };
            let line = Line::from(vec![
                Span::raw(format!("{status}  ")),
                Span::styled(k.label(), Style::default().fg(Color::White)),
            ]);
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" Strategies "))
        .highlight_style(
            Style::default()
                .bg(Color::Magenta)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, body[0], state);

    let selected = BOTS[state.selected().unwrap_or(0)];
    let detail = bot_detail(selected);
    let detail_p = Paragraph::new(detail)
        .block(Block::default().borders(Borders::ALL).title(" Details "))
        .wrap(Wrap { trim: true });
    f.render_widget(detail_p, body[1]);

    let footer = Paragraph::new(Line::from(vec![
        Span::styled("↑/↓", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" navigate  "),
        Span::styled("Enter", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" launch  "),
        Span::styled("q", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" quit"),
    ]))
    .style(Style::default().fg(Color::DarkGray));
    f.render_widget(footer, chunks[2]);
}

fn bot_detail(kind: BotKind) -> Vec<Line<'static>> {
    let status_line = kind.status();
    let description = match kind {
        BotKind::CopyTrading => {
            "Mirror one wallet's fills through the sizing, eligibility and risk pipeline. Polymarket and Predict.fun."
        }
        BotKind::BtcArb => {
            "Value up/down rounds from the underlying and buy when the book lags. Held to the end of the round, so it can lose."
        }
        BotKind::CrossArb => {
            "Compare Predict.fun with its linked Polymarket markets and report gaps that clear fees. Trades nothing."
        }
        BotKind::DirectionalArb => {
            "Rest Yes and No bids totalling under $1, tilted toward the side the book favours. Limit orders only."
        }
        BotKind::SpreadFarming => {
            "Bid inside a wide spread, then offer what filled inside the ask, one round trip at a time."
        }
        BotKind::Sports => {
            "List sports fixtures and cross the spread from the terminal when you say so. Under the same limits."
        }
        BotKind::ResolutionSniper => {
            "Buy near-certain outcomes in markets about to end and hold to the payout. A surprise costs dozens of wins."
        }
        BotKind::OrderbookImbalance => {
            "Trade near-touch size imbalance, exiting on a fixed target, stop or time limit. No external feeds."
        }
        BotKind::MarketMaking => {
            "Two-sided post-only quotes with inventory skew and expiry on every quote."
        }
        BotKind::WhaleSignal => {
            "Follow many wallets, or any large taker, each wallet with its own spending bucket."
        }
    };
    vec![
        Line::from(vec![
            Span::styled(
                kind.label(),
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(status_line, Style::default().fg(Color::Yellow))),
        Line::from(""),
        Line::from(description),
    ]
}
