use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use polymarket_toolkits::{
    bot::{self, BotKind},
    config::AppConfig,
    models::VenueId,
    service::predict_fun,
    ui,
};
use std::path::PathBuf;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "polymarket-toolkits")]
#[command(about = "Multi-venue prediction-market trading toolkit.", long_about = None)]
struct Cli {
    /// Path to public config (JSON).
    #[arg(long, default_value = "config.json")]
    config: PathBuf,

    /// Path to credentials file (YAML).
    #[arg(long, default_value = "config.yaml")]
    credentials: PathBuf,

    /// Venue to trade. Overrides `venue` in the config.
    #[arg(long, value_enum)]
    venue: Option<VenueArg>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Launch the interactive TUI to select a bot. (Default if no subcommand.)
    Tui,
    /// Run a specific bot headlessly (no TUI).
    Run {
        /// Which bot to run.
        #[arg(value_enum)]
        bot: BotKindArg,
    },
    /// Check the Predict.fun connection without placing an order.
    CheckVenue {
        /// Also sign in, proving the wallet and API key work. Signing accepts
        /// Predict's Terms of Service, so it's opt-in.
        #[arg(long)]
        sign_in: bool,
    },
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum VenueArg {
    Polymarket,
    PredictFun,
}

impl From<VenueArg> for VenueId {
    fn from(v: VenueArg) -> Self {
        match v {
            VenueArg::Polymarket => VenueId::Polymarket,
            VenueArg::PredictFun => VenueId::PredictFun,
        }
    }
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum BotKindArg {
    CopyTrading,
    BtcArb,
    CrossArb,
    DirectionalArb,
    SpreadFarming,
    Sports,
    ResolutionSniper,
    OrderbookImbalance,
    MarketMaking,
    WhaleSignal,
}

impl From<BotKindArg> for BotKind {
    fn from(b: BotKindArg) -> Self {
        match b {
            BotKindArg::CopyTrading => BotKind::CopyTrading,
            BotKindArg::BtcArb => BotKind::BtcArb,
            BotKindArg::CrossArb => BotKind::CrossArb,
            BotKindArg::DirectionalArb => BotKind::DirectionalArb,
            BotKindArg::SpreadFarming => BotKind::SpreadFarming,
            BotKindArg::Sports => BotKind::Sports,
            BotKindArg::ResolutionSniper => BotKind::ResolutionSniper,
            BotKindArg::OrderbookImbalance => BotKind::OrderbookImbalance,
            BotKindArg::MarketMaking => BotKind::MarketMaking,
            BotKindArg::WhaleSignal => BotKind::WhaleSignal,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("polymarket_toolkits=info,info")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let mut cfg = AppConfig::load(&cli.config, &cli.credentials)
        .context("loading configuration")?;
    if let Some(venue) = cli.venue {
        cfg.venue = venue.into();
    }

    info!(
        venue = ?cfg.venue,
        wallets = cfg.bot.wallets_to_track.len(),
        enable_trading = cfg.bot.enable_trading,
        mock_trading = cfg.bot.mock_trading,
        "configuration loaded"
    );

    match cli.command {
        Some(Command::Run { bot: kind }) => bot::run(kind.into(), cfg).await,
        Some(Command::CheckVenue { sign_in }) => predict_fun::check(&cfg, sign_in).await,
        Some(Command::Tui) | None => ui::run(cfg).await,
    }
}
