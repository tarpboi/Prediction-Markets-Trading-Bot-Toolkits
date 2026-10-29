# Prediction Market Toolkits

<div align="center">

<img width="820" alt="Polymarket Toolkits TUI" src="https://github.com/user-attachments/assets/b6c51ba1-14c6-4582-858c-e9441516dd1d" />
<img width="820" alt="Prediction Market Toolkits dashboard" src="https://github.com/user-attachments/assets/2ae5783d-be8e-458d-8da4-1ff82aada3db" />

### Venue-agnostic prediction-market trading infrastructure — any market with an order book

[![Rust](https://img.shields.io/badge/rust-1.70+-orange.svg?style=flat-square&logo=rust)](https://www.rust-lang.org/)
[![Rust CI](https://github.com/tarpboi/Prediction-Markets-Trading-Bot-Toolkits/actions/workflows/rust.yml/badge.svg)](https://github.com/tarpboi/Prediction-Markets-Trading-Bot-Toolkits/actions/workflows/rust.yml)
[![Stars](https://img.shields.io/github/stars/tarpboi/Prediction-Markets-Trading-Bot-Toolkits?style=flat-square&color=6e40c9)](https://github.com/tarpboi/Prediction-Markets-Trading-Bot-Toolkits/stargazers)
[![License](https://img.shields.io/badge/license-MIT-blue.svg?style=flat-square)](LICENSE)
[![Tokio](https://img.shields.io/badge/async-tokio-blue.svg?style=flat-square)](https://tokio.rs/)
[![Venues](https://img.shields.io/badge/venues-20_repos-6e40c9.svg?style=flat-square)](#venue-coverage)

> **One execution core. One risk layer. Every venue.**
> Ten strategy bots share a single engine and a venue-agnostic adapter stack — adding a market means writing **one adapter**, not rebuilding a bot. Twenty venues are mapped to that interface, each with its own repo.

<br/>

[![Chat on Telegram](https://img.shields.io/badge/💬_Chat_on_Telegram-@tarpboi-229ED9?style=for-the-badge&logo=telegram&logoColor=white)](https://t.me/tarpboi)
&nbsp;
[![PnL Profit — live](https://img.shields.io/badge/🚀_PnL_Profit-Live_at_pnlpro.fit-16a34a?style=for-the-badge)](https://pnlpro.fit)

**[Quick Start](#-quick-start) • [Strategies](#strategies) • [Managed Service](#-managed--copy-trading--currently-closed) • [Venue Coverage](#venue-coverage) • [Engine](#engine) • [Safety](#safety) • [Contact](#contact)**

**🌐 Language / 语言 / Язык:** [English](#prediction-market-toolkits) • [简体中文](README.zh-CN.md) • [Русский](README.ru.md)

</div>

---

## 🚀 Quick Start

**No Rust toolchain needed.** Grab a prebuilt binary from the [latest release](https://github.com/tarpboi/Prediction-Markets-Trading-Bot-Toolkits/releases/latest) — Linux (x86_64 / aarch64), macOS (Intel / Apple Silicon), Windows — verify the SHA256, and you're running in about a minute:

```bash
tar -xzf polymarket-toolkits-<tag>-<target>.tar.gz
cd polymarket-toolkits-<tag>-<target>

cp config.yaml.example config.yaml   # credentials; config.json ships with the archive
./polymarket-toolkits run copy-trading   # dry-run: enable_trading is false by default
```

The archive ships `config.json` (public settings) alongside `config.yaml.example` (credentials). Run `./polymarket-toolkits --help` for the full command list, or launch with no subcommand for the interactive TUI.

Prefer to build it? `cargo install --git https://github.com/tarpboi/Prediction-Markets-Trading-Bot-Toolkits`

<table>
<tr>
<td width="50%" valign="top">

### 🛠️ Run the bots yourself

Open-source engine, your keys, your wallet.

```bash
# 1. Clone the engine (the trading code lives here)
git clone https://github.com/tarpboi/Prediction-Markets-Trading-Bot-Toolkits
cd Prediction-Markets-Trading-Bot-Toolkits

# 2. Configure — credentials from the example
cp config.yaml.example config.yaml

# 3. Dry-run first (no real orders)
cargo run --release -- run copy-trading
```

Every bot ships with `enable_trading: false` by default — the full execution path runs in dry-run until *you* flip it. The [per-venue repos](#venue-coverage) currently hold venue metadata, not a working bot; clone this one.

</td>
<td width="50%" valign="top">

### 💬 Talk it through first

Not sure which strategy fits your venue, capital size, or risk budget? Ask.

- What Copy Trading actually does on a live book, and where it stops
- How the dry-run path works before you ever flip `enable_trading`
- Which strategy suits your venue and capital size — and how the risk layer caps it

> ⏸️ **The hosted service is closed right now** — it ran as a paper-trading beta and never touched real funds. Running it yourself is the supported path today.

**[→ Message on Telegram](https://t.me/tarpboi)**

</td>
</tr>
</table>

---

## By the numbers

<div align="center">

[![Stars](https://img.shields.io/github/stars/tarpboi/Prediction-Markets-Trading-Bot-Toolkits?style=for-the-badge&logo=github&label=Stars&color=1f6feb)](https://github.com/tarpboi/Prediction-Markets-Trading-Bot-Toolkits/stargazers)
[![Forks](https://img.shields.io/github/forks/tarpboi/Prediction-Markets-Trading-Bot-Toolkits?style=for-the-badge&logo=github&label=Forks&color=1f6feb)](https://github.com/tarpboi/Prediction-Markets-Trading-Bot-Toolkits/forks)
[![CI](https://img.shields.io/github/actions/workflow/status/tarpboi/Prediction-Markets-Trading-Bot-Toolkits/rust.yml?style=for-the-badge&logo=githubactions&logoColor=white&label=Build)](https://github.com/tarpboi/Prediction-Markets-Trading-Bot-Toolkits/actions/workflows/rust.yml)
[![License](https://img.shields.io/github/license/tarpboi/Prediction-Markets-Trading-Bot-Toolkits?style=for-the-badge&color=1f6feb)](LICENSE)

| 🎯 Strategies | ⚙️ Engine | 🧪 Dry-run |
|:---:|:---:|:---:|
| **10** | **Rust** | **Every path** |

*Star and fork counts are live badges, not numbers typed into a README — they track the repo and can't drift. No fake testimonials, no cherry-picked P&L.*

</div>

---

## Strategies

Ten strategies, one execution core. Each targets a distinct, well-defined market edge, and every one of them runs on the same engine: a Polymarket CLOB client, an order executor with EIP-712 signing and a real dry-run path, a risk guard, position monitor and store, market cache, and on-chain ingestion off Polygon logs. Pick the edge that fits your thesis — the infrastructure underneath is shared.

> 📦 Every bot runs on the shared engine and [safety layer](#safety), with full dry-run support. The [per-venue repos](#venue-coverage) hold venue metadata — the trading code is here.

| # | Strategy | Edge in one line | Key spec |
|---|----------|------------------|----------|
| 1 | 🎯 **Copy Trading** | Mirror wallets that already proved they have alpha | Multi-wallet · FAK/GTD · circuit breaker |
| 2 | ⚡ **BTC 5m / 15m / 1hr Arbitrage** | Speed on short-window BTC Up/Down | FAK · short-window |
| 3 | 💰 **Cross-Market Arbitrage** | Lock the spread, not the direction | Polymarket ↔ Kalshi ↔ PredictIt · hedged legs |
| 4 | 🎯 **Directional Arbitrage** | Arb base (Up + Down < $1), then tilt toward the side with more edge | Hedged base · limit-only |
| 5 | 📈 **Spread Farming** | A thousand 0.5¢ wins compound into one number | Bid-ask capture · per-trade P&L |
| 6 | 🏆 **Sports Execution** | Click. Filled. Done | NBA / NFL / Soccer · FAK |
| 7 | 🎯 **Resolution Sniper** | 95¢ near-certainty → guaranteed $1.00 payout | Certainty scan · hold to resolution |
| 8 | 📊 **Orderbook Imbalance** | The signal *is* the order book — no external feeds | Live OBI · 500ms refresh |
| 9 | 💰 **Market Making** | Be the house, not the gambler | Two-sided GTD · inventory skew |
| 10 | ⚡ **On-Chain Whale Signal** | Ahead of the public positions API | Polygon block sub · ABI calldata decode |

<details>
<summary><b>How the flagship edges actually work</b> (click to expand)</summary>

<br/>

**🎯 Copy Trading —** Point the bot at one or more wallets with a proven on-chain record. It mirrors their fills at your chosen scale, with per-wallet caps, FAK/GTD order types, and a circuit breaker that halts on abnormal bursts. Pick who to follow from any wallet with a verifiable on-chain record.

**💰 Cross-Market Arbitrage —** The same real-world question is often listed on Polymarket, Kalshi *and* PredictIt at slightly different prices. The engine matches the same contract across venues (strict matching — no fuzzy false pairs), and captures the gap **only when it beats round-trip fees**. Cross-listed markets are mostly efficient, so this is a patience game: it waits for a real dislocation instead of forcing trades.

**🎯 Directional Arbitrage —** Buy the Yes + No basket while it costs under \$1 (a structural arb base), then tilt extra size onto the side with more upside. Limit-only, hedged base — structure improves expected value instead of betting on a hunch.

**🎯 Resolution Sniper —** Scan for near-certainty contracts (e.g. 95¢+) where the market has effectively resolved but hasn't paid out, and hold to \$1.00. High win-rate, low per-trade return — it compounds on volume, not on swings.

**📊 Orderbook Imbalance —** No external feeds, no oracle: the signal *is* the book. Near-touch bid/ask depth skew becomes a short-term directional read, refreshed every 500ms.

</details>

<div align="center">

💬 **Want a strategy explained for your venue or capital size?** → **[t.me/tarpboi](https://t.me/tarpboi)**

</div>

---

## 💼 Managed & Copy-Trading — Currently Closed

> ⏸️ **The hosted service is not currently open.** It ran as a paper-trading beta — simulated funds, no real custody — and is offline while the infrastructure moves. It never handled real money, and live trading was always gated behind custody, security audit and licensing. **Sign-ups and pricing are closed for now**, so the way to use this project today is to run it yourself with your own keys (see [Run the bots yourself](#-run-the-bots-yourself)).

If you want to talk about a managed setup when it reopens — or about running the engine at size yourself — that conversation happens on Telegram:

<div align="center">

[![Talk on Telegram](https://img.shields.io/badge/Discuss_the_bots-Telegram-229ED9?style=for-the-badge&logo=telegram&logoColor=white)](https://t.me/tarpboi)

</div>

---

## Venue Coverage

The engine is venue-agnostic **by design**: any platform exposing an order book or
position feed plugs in through a single adapter, so adding a market means writing
one adapter rather than rebuilding a bot.

Twenty venues are mapped to that interface, each with a repo of its own. **Polymarket
is the reference implementation** the engine is built and tested against; the
per-venue repos hold venue metadata, so clone this repo for the trading code.

| Venue | Type | Strategies |
|---|---|---|
| [**Polymarket**](https://github.com/tarpboi/Polymarket) | Decentralized (Polygon / USDC) | Reference implementation — all ten |

### Traditional / Regulated

| Venue | Type |
|---|---|
| [**Kalshi**](https://github.com/tarpboi/Kalshi) | CFTC-regulated (US) |
| [**PredictIt**](https://github.com/tarpboi/PredictIt) | Academic / US politics |
| [**Robinhood Predictions**](https://github.com/tarpboi/Robinhood-Predictions) | Brokerage-integrated |
| [**Crypto.com Predictions**](https://github.com/tarpboi/Crypto.com-Predictions) | Crypto-integrated |
| [**OG.com**](https://github.com/tarpboi/OG.com) | Social / multi-outcome |
| [**DraftKings Predictions**](https://github.com/tarpboi/DraftKings-Predictions) | Sports |
| [**FanDuel Predicts**](https://github.com/tarpboi/FanDuel-Predicts) | Sports |
| [**Fanatics Markets**](https://github.com/tarpboi/Fanatics-Markets) | Sports / entertainment |
| [**Interactive Brokers ForecastTrader**](https://github.com/tarpboi/Interactive-Brokers-ForecastTrader) | Financial events |

### Crypto / Decentralized

| Venue | Chain / Type |
|---|---|
| [**Limitless**](https://github.com/tarpboi/Limitless-Exchange) | On-chain order book |
| [**Drift BET**](https://github.com/tarpboi/Drift-BET) | Solana |
| [**Azuro**](https://github.com/tarpboi/Azuro) | Decentralized protocol |
| [**Augur**](https://github.com/tarpboi/Augur) | Ethereum |
| [**Myriad Markets**](https://github.com/tarpboi/Myriad-Markets) | Crypto |
| [**Hedgehog Markets**](https://github.com/tarpboi/Hedgehog-Markets) | Solana / social |
| [**Zeitgeist**](https://github.com/tarpboi/Zeitgeist) | Polkadot |
| [**Projection Finance**](https://github.com/tarpboi/Projection-Finance) | Volatility / sims |
| [**Better Fan**](https://github.com/tarpboi/Better-Fan) | Sports / esports |
| [**Manifold Markets**](https://github.com/tarpboi/Manifold-Markets) | Play-money · consensus signal |

> **Want a venue prioritized?** Adapter work is demand-driven — if you trade a
> platform you'd like wired up next, [reach out on Telegram](https://t.me/tarpboi)
> and it can move up the queue.

---

## Engine

Rust, async on Tokio, one execution core behind every strategy and venue. The adapter stack means a new market is one adapter — not a new bot.

### Performance

| | |
|---|---|
| **Event processing** | < 1ms per event |
| **Order execution** | < 100ms end-to-end |
| **Position polling** | ~200ms per wallet |
| **Memory** | ~50MB baseline |
| **CPU** | < 5% on modern hardware |
| **Concurrency** | Semaphore-based rate limiting (default: 25 req / 10s) |

---

## Safety

| | |
|---|---|
| **Circuit Breaker** | Auto-halts after N consecutive large trades inside a configurable window |
| **Depth Guard** | Validates orderbook liquidity before every order |
| **Dry Run** | Full execution path runs without placing real orders |
| **Trade Floor** | Minimum size enforcement against negative-EV micro-trades |

The circuit breaker fires when consecutive large trades exceed the configured threshold, or when orderbook depth falls below the minimum. Once tripped, execution is blocked for the cooldown duration. Trip state and cooldown are logged and visible in the TUI.

**Recommendations:**

| Stage | Action |
|-------|--------|
| Initial setup | Run with `enable_trading: false` for a full session |
| First real trades | Keep `copy_percentage` at 5–10% until you trust the signal |
| Ongoing | Watch circuit breaker trips — they surface execution anomalies |
| Production | Dedicated wallet with only the capital you intend to deploy |

---

## Contact

Built and maintained actively. Whether you want to **run the bots**, request a **new venue adapter**, or just talk Polymarket tooling and algorithmic strategies — reach out.

<div align="center">

[![Chat on Telegram](https://img.shields.io/badge/💬_Telegram-@tarpboi-229ED9?style=for-the-badge&logo=telegram&logoColor=white)](https://t.me/tarpboi)

| Platform | Link |
|----------|------|
| **Telegram** | [t.me/tarpboi](https://t.me/tarpboi) |
| **Discussions** | [GitHub Discussions](../../discussions) |

*Response time is typically within a few hours. Open to questions, feedback, venue requests, and serious collaborations.*

</div>

---

## Disclaimer

> Trading prediction markets involves real financial risk. This software is provided as-is, without warranty or guarantee of any outcome. It is not financial advice. Always test with `enable_trading: false` before deploying real capital. Check the source for the current state of each strategy before running it with real capital. The **managed / copy-trading service is currently closed** — it ran in paper mode (simulated funds), never custodied real money, and any future live-trading rollout will follow proper custody, audit, and licensing. Ensure compliance with each venue's terms of service and applicable regulations in your jurisdiction.

---

<div align="center">

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg?style=flat-square)](LICENSE)
[![Telegram](https://img.shields.io/badge/💬_Telegram-@tarpboi-229ED9?style=flat-square&logo=telegram&logoColor=white)](https://t.me/tarpboi)

**Built for the Prediction Markets including Polymarket, Kalshi, Limitless etc**

[Back to top](#prediction-market-toolkits)

</div>

[Power of Bot](http://x.com/theparuchh/status/2053766299281416621)
