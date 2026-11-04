# Going live

Every bot starts in dry run: it reads real markets, builds and signs orders, and sends nothing. This guide covers switching that off.

> ⚠️ **No live order has been sent through this engine on Predict.fun or on Polymarket's CLOB V2.** Signing, sign-in and order amounts are checked against each venue's official SDK, and dry runs have used real books and real on-chain fills. Sending an order, cancelling one and reading a real fill have not been run, so expect to find problems, and no strategy here has a track record. Start with money you can afford to lose.

## Before either venue

1. **Check that you're allowed to trade there.** Read the venue's terms. Don't try to get around a restriction.
2. **Use a new wallet** that holds only what you're prepared to lose. Never use your main wallet: its private key sits in `config.yaml` or an environment variable.
3. **Run a dry run first** for a full session, and read what it would have done.
4. **Set small limits** (see [Limits](#limits)).

## Predict.fun

**Terms:** [Predict's Terms of Service](https://docs.predict.fun/terms-of-service) bar several jurisdictions, including the US and UK. Signing in accepts those terms.

**You need:**

| Item | Where it goes |
|---|---|
| Wallet private key | `private_key` in `config.yaml`, or `PM_PRIVATE_KEY` |
| API key from Predict (mainnet only) | `predict_api_key`, or `PM_PREDICT_API_KEY` |
| Predict Account address, if you trade through the web app's smart wallet | `predict_account`, or `PM_PREDICT_ACCOUNT`. Set `private_key` to the Privy wallet exported from your account settings |
| USDT on BNB Chain, plus BNB for gas | The trading wallet |
| Approvals, set once | `setApprovals` in the official [`PredictDotFun/sdk`](https://github.com/PredictDotFun/sdk). The engine doesn't send on-chain transactions |

**Check the connection.** The first command signs nothing with Predict. The second signs in, which accepts the terms.

```bash
./polymarket-toolkits --venue predict-fun check-venue
./polymarket-toolkits --venue predict-fun check-venue --sign-in
```

**Testnet or mainnet:** `predict_fun.network` in `config.json` is `testnet` by default. Change it to `mainnet` only after a testnet order has worked.

**Good to know:**

- Removing an order on Predict doesn't cancel it on-chain. A removed order can stay fillable for a short time, and the bots keep watching it until the venue closes it.
- Cross-Market Arbitrage only reports gaps. It refuses to run live.
- BTC rounds settle on Binance's top of book. Where Binance isn't reachable, that bot uses Coinbase's price and refuses to go live.
- The bots don't redeem winnings. Redeem them on Predict.

## Polymarket

Copy Trading is the only strategy on Polymarket for now.

**Terms:** Polymarket [rejects orders from restricted regions](https://docs.polymarket.com/api-reference/geoblock), including the US and UK. `check-venue` shows what Polymarket reports for your connection, and a live run stops at startup if it's blocked.

**You need:**

| Item | Where it goes |
|---|---|
| Wallet private key | `private_key` in `config.yaml`, or `PM_PRIVATE_KEY` |
| The address holding your funds, if it isn't that key's own address | `funder_address`, or `PM_FUNDER_ADDRESS` |
| Your wallet type, if you set a funder: `proxy` (email login), `safe` (browser wallet) or `deposit_wallet` | `signature_type`, or `PM_SIGNATURE_TYPE` |
| pUSD on Polygon. It's Polymarket's collateral, wrapped from USDC.e | The funder wallet |
| POL for gas, if you trade from your own key | The same wallet |
| Approvals, set once | See below |
| A WebSocket RPC you trust | `site.polygon_ws_url` in `config.json` |

API credentials are created or derived for you on the first live run. To use your own, set `api_key`, `api_secret` and `api_passphrase` together.

**Approvals:** the Polymarket site sets these for accounts made there. If you trade from your own key, approve the CTF Exchange (`0xE111180000d2663C0091e4f400237545B87B996B`) and the Neg Risk Exchange (`0xe2222d279d744050d28e00520010520000310F59`) to spend your pUSD, and to move your outcome tokens on the Conditional Tokens contract. Neg-risk markets also need the Neg Risk Adapter. Polymarket's own `approvals.rs` example still lists older contracts and USDC.e, so don't run it unchanged.

**Check it.** The first command signs a sample order locally and sends nothing. The second creates or derives your API credentials, then shows your balance and whether each exchange is approved.

```bash
./polymarket-toolkits --venue polymarket check-venue
./polymarket-toolkits --venue polymarket check-venue --sign-in
```

**Good to know:**

- The shipped public RPC can miss events under load, and a missed event is a missed copy. Use your own for anything real.
- Prices snap to the market's tick in your favour, so a buy limit of 0.505 on a 0.01 tick becomes 0.50 and may not fill.
- Orders below the market's minimum, usually 5 shares, are refused.
- Copy Trading watches the CTF and Neg Risk exchanges. It doesn't yet see fills on Exchange V3, which handles position-order markets.
- Config files from before v0.2.0 are refused. Copy the `exchange` block from the current `config.json`.

## Limits

Set these before you switch anything on.

**Copy Trading**, in `config.json`:

| Setting | Shipped | For a first run |
|---|---|---|
| `strategy.max_order_size_usd` | 500 | 10 |
| `strategy.copy_size` | 20 (percent of the whale's trade) | 5 |
| `filters.per_category_max_open_usd` | 300 to 500 | 25 |

**The other nine**, under `strategies.limits` in `config.json`, hard caps in USD:

| Setting | Shipped |
|---|---|
| `max_order_usd` | 5 |
| `max_market_usd` | 25 |
| `max_total_usd` | 100 |
| `max_loss_usd` (halts new entries) | 25 |

Positions already in the wallet count toward these caps, and no strategy will sell them.

## Switch it on

Both flags in `config.json` have to change. Either one left alone keeps you in dry run.

```json
"bot": {
  "enable_trading": true,
  "mock_trading": false
}
```

Then run one strategy, and watch the first order on the venue's own site:

```bash
./polymarket-toolkits --venue predict-fun run market-making
./polymarket-toolkits --venue polymarket run copy-trading
```

The log shows `LIVE TRADING IS ON` at startup. If you don't see it, you're still in dry run.

## Stopping

Press **Ctrl-C** once.

- **Predict.fun strategies** finish the current step and stop. Check the venue afterwards for resting orders.
- **Copy Trading** stops following and stops its take-profit and stop-loss checks. Open positions stay in your wallet. They're tracked in memory, so a restart won't resume watching them: manage them on the venue.

To go back to dry run, set the flags back and restart.

## If something goes wrong

- **Orders rejected:** the venue's reason is in the log. Common causes are missing approvals, no balance, the wrong `signature_type`, or a restricted region.
- **A fill looks wrong:** trust the venue's site over the bot's log. If the bot can't confirm a fill, it says so and assumes the planned size.
- **Found a bug in the live path?** [Open an issue](https://github.com/tarpboi/Prediction-Markets-Trading-Bot-Toolkits/issues) with the log, with keys removed. Reports from real runs are especially useful, as none exist yet.
