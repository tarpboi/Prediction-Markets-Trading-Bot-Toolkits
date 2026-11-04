use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use polymarket_toolkits::{
    bot::{self, BotKind},
    config::AppConfig,
    models::VenueId,
    service::{clob, predict_fun},
    ui,
};
use std::path::PathBuf;
use tracing::{info, warn};
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
    /// Check a venue's connection and signing without placing an order.
    CheckVenue {
        /// Also sign in, proving the wallet and API key work. On Predict.fun this
        /// accepts its Terms of Service; on Polymarket it creates or derives API
        /// credentials and reads the balance. Either way it's opt-in.
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

    if cfg.live_trading_allowed() {
        warn!(
            venue = ?cfg.venue,
            "LIVE TRADING IS ON: orders use real funds. See docs/LIVE-TRADING.md"
        );
    }

    match cli.command {
        Some(Command::Run { bot: kind }) => bot::run(kind.into(), cfg).await,
        Some(Command::CheckVenue { sign_in }) => match cfg.venue {
            VenueId::Polymarket => clob::check(&cfg, sign_in).await,
            _ => predict_fun::check(&cfg, sign_in).await,
        },
        Some(Command::Tui) | None => ui::run(cfg).await,
    }
}
