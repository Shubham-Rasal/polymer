# simple-bot

Consolidated Polymarket trading bot. Single binary with 3 strategies (Bayesian, Sniper, Arbitrage), paper + live modes, and utility commands for approvals, redemptions, and balance checks.

Targets **BTC 5-minute and 15-minute Up/Down markets** on Polymarket.

## Quick Start

```bash
# Paper mode (no key needed, simulated 100 USDC wallet)
cargo run -- run --strategy bayesian --paper

# Live mode
export POLYMARKET_PRIVATE_KEY=your_hex_key
cargo run -- run --strategy bayesian
```

## Commands

### `run` — Execute a trading strategy

```
simple-bot run --strategy bayesian|sniper|arbitrage [--paper] [--size 5] [--event slug] [--dry-run]
```

| Flag | Description | Default |
|------|-------------|---------|
| `--strategy` | `bayesian`, `sniper`, or `arbitrage` | `bayesian` |
| `--paper` | Paper mode — real market data, simulated execution | off (live if key set) |
| `--size` | Order size in USDC (sniper only) | 5 |
| `--event` | Target event slug, e.g. `btc-updown-5m-1770978300` (sniper only) | auto-scan |
| `--dry-run` | Log signals only, no orders | off |

**Paper mode** activates when `--paper` is passed OR `POLYMARKET_PRIVATE_KEY` is not set. Uses real market data and price feeds but simulates order execution against a virtual 100 USDC balance.

**Live mode** authenticates with the CLOB and submits real FOK/FAK orders.

### `approve` — Set token approvals

```bash
cargo run -- approve
```

Checks and sets USDC + CTF (ERC-1155) approvals for the CTF Exchange, Neg Risk CTF Exchange, and Neg Risk Adapter contracts. Required once before live trading.

```
$ cargo run -- approve
Wallet: 0x0A7c56744ed6fd786931E11E40F462CF213654b0

=== Current Approval Status ===

CTF Exchange:
  USDC allowance: MAX (unlimited) (OK)
  CTF approved:   true (OK)
Neg Risk CTF Exchange:
  USDC allowance: MAX (unlimited) (OK)
  CTF approved:   true (OK)
Neg Risk Adapter:
  USDC allowance: MAX (unlimited) (OK)
  CTF approved:   true (OK)

All approvals are already set. You're ready to trade!
```

### `redeem` — Redeem resolved positions

```bash
cargo run -- redeem --condition-id 0xabc123...
```

Redeems winning tokens from a resolved market and prints the resulting USDC balance.

### `balance` — Check wallet USDC balance

```bash
cargo run -- balance
```

```
USDC balance: 4.5418
```

## Strategies

### Bayesian

Continuous loop that scans for BTC 5/15-min Up/Down markets, collects price evidence from 4 real-time feeds, and trades when the Bayesian posterior diverges from the market price.

```bash
# Paper mode
cargo run -- run --strategy bayesian --paper

# Live
cargo run -- run --strategy bayesian
```

**How it works:**

1. Subscribes to 4 price feeds (Binance BTC/ETH, Chainlink BTC/ETH via RTDS)
2. Scans Gamma API for open `btc-updown-5m` and `btc-updown-15m` markets
3. For each market: uses the market price as Bayesian prior, updates with feed evidence
4. Generates BUY UP or BUY DOWN signals when edge > 3%
5. Sizes positions with quarter-Kelly criterion (max 10% of bankroll per market, max 2 trades per iteration)
6. Monitors open positions: sells at 2% profit, resolves closed markets

```
[Live] Wallet balance: 4.5418 USDC (synced)
Bayesian posterior P(UP|evidence) = 55.0% (prior/market was 50.5%, 1 feeds)
  → BUY UP: posterior=67.7% vs market=60.0% (edge=7.7%), kelly=4.84%, amount=4.84 USDC
  ★★★ TRADE SIGNAL ★★★ Market: Bitcoin Up or Down - February 16, 10:45AM-11:00AM ET (4.84 USDC)
```

**Parameters:**

| Constant | Value | Description |
|----------|-------|-------------|
| `MIN_DISCREPANCY` | 3% | Minimum edge to trade |
| `KELLY_FRACTION` | 0.25 | Quarter-Kelly sizing |
| `MAX_POSITION_FRACTION` | 10% | Max bankroll per market |
| `MAX_SIGNALS_PER_ITERATION` | 2 | Cap trades per loop |
| `SCAN_INTERVAL_SECS` | 10 | Seconds between iterations |
| `START_PRICE_RESET_SECS` | 300 | Reference price reset interval |
| `PROFIT_THRESHOLD` | 2% | Sell when price >= buy_price * 1.02 |

### Sniper

Finds the current BTC 5-minute Up/Down market by computing the active window slug (`btc-updown-5m-{timestamp}`), monitors the WebSocket orderbook, and buys the winning side in the last second before resolution.

```bash
# Auto-discover current 5-min market
cargo run -- run --strategy sniper --paper

# Target a specific event by slug
cargo run -- run --strategy sniper --event btc-updown-5m-1771176300 --size 5

# Dry run (log only)
cargo run -- run --strategy sniper --dry-run
```

**How it works:**

1. Computes the current 5-min window timestamp (`now - now % 300`) and looks up `btc-updown-5m-{ts}` via the Gamma events API. Also tries the next window (`+300s`) if the current one isn't found.
2. Connects to CLOB WebSocket for real-time bid/ask updates on both YES/NO tokens
3. Infers the winning side from bid/ask midpoints
4. **Trigger:** time remaining <= 1 second AND winning side price > 0.50 AND best ask < 0.99
5. Executes a single FOK buy order

```
[Scanner] Trying slug: btc-updown-5m-1771291800
[Scanner] Found BTC 5-min market: Bitcoin Up or Down - February 16, 8:30PM-8:35PM ET (ends Some(2026-02-17T01:35:00Z))
[Sniper] Target: Bitcoin Up or Down - February 16, 8:30PM-8:35PM ET (ends Some(2026-02-17T01:35:00Z))
[Sniper] Monitoring until 2026-02-17T01:35:00Z. Trigger: t <= 1s, price > 0.5, best_ask < 0.99
[Sniper] TRIGGER: t=1s, winning_price=0.65, best_ask=0.67
```

### Arbitrage

Monitors WebSocket orderbooks across binary markets looking for YES + NO price deviations from $1. Executes both legs in parallel with FOK orders.

```bash
cargo run -- run --strategy arbitrage --paper
cargo run -- run --strategy arbitrage
```

**How it works:**

1. Fetches open binary markets from Gamma
2. Subscribes to WebSocket orderbook updates for all token pairs
3. Detects arbitrage when:
   - **BuyBoth:** `ask_yes + ask_no < $0.98` (buy both for < $1, redeem for $1)
   - **SellBoth:** `bid_yes + bid_no > $1.02` (sell both for > $1)
4. Submits both legs in parallel via `tokio::join!`

```
[Arbitrage] Monitoring 100 markets via WebSocket
[Signal] Buy both arbitrage: market=abc, ask_yes=0.47, ask_no=0.50, sum=0.97, profit=0.03
[Execute] BUY both legs: 5 USDC each, profit/unit=0.0300
```

**Parameters:**

| Constant | Value | Description |
|----------|-------|-------------|
| `ARB_THRESHOLD` | 2% | Minimum deviation from $1 |
| `MIN_POSITION_USDC` | $5 | Minimum position |
| `MAX_POSITION_USDC` | $100 | Maximum position |
| `LIQUIDITY_CAP_FRAC` | 50% | Max fraction of available depth |

## Environment Variables

| Variable | Required | Description |
|----------|----------|-------------|
| `POLYMARKET_PRIVATE_KEY` | For live mode | Polygon wallet private key (hex, with or without 0x) |
| `POLYMARKET_WALLET_TYPE` | No | Set to `proxy` for Magic/email wallets |
| `POLYMARKET_FUNDER_ADDRESS` | No | Explicit funder address for proxy wallets |
| `LOG_FILE` | No | Log file path (default: `bot.log`) |
| `RESULTS_FILE` | No | Trade log path (default: `trades.json`) |

## Wallet Support

- **EOA (default):** Direct private key signing
- **Proxy (Magic/email):** Set `POLYMARKET_WALLET_TYPE=proxy`. Uses `derive_proxy_wallet()` or explicit `POLYMARKET_FUNDER_ADDRESS`

## Output Files

- **`bot.log`** — Full session log (also printed to stdout)
- **`trades.json`** — Append-only JSON-lines trade log:

```json
{"timestamp":"2026-02-16T15:51:05Z","mode":"bayesian","market":"Bitcoin Up or Down - Feb 16","side":"BUY","amount_usdc":"4.836180","price":0.6,"executed":false}
```

## Project Structure

```
src/
├── main.rs              # CLI dispatch, tracing setup
├── cli.rs               # clap subcommands (Run, Approve, Redeem, Balance)
├── auth.rs              # Wallet auth, CLOB client setup, paper mode fallback
├── trading.rs           # Unified order execution (paper vs live)
├── tracker.rs           # Position tracking, P&L, trade persistence
├── feeds.rs             # 4 RTDS price feeds (Binance + Chainlink, BTC + ETH)
├── orderbook.rs         # WebSocket orderbook state (DashMap best bid/ask)
├── approvals.rs         # USDC + CTF token approval management
├── redeem.rs            # Resolved position redemption
└── strategies/
    ├── mod.rs           # TradeSignal type
    ├── bayesian.rs      # Bayesian updater + Kelly sizing + market discovery
    ├── sniper.rs        # Scanner + last-second trigger
    └── arbitrage.rs     # YES+NO deviation detector + parallel executor
```

## Building

```bash
cargo build           # debug
cargo build --release # optimized
```

## Dependencies

- `polymarket-client-sdk` — CLOB, Gamma, CTF, RTDS, WebSocket APIs
- `alloy` — Ethereum signing, contract interaction, Polygon RPC
- `clap` — CLI parsing
- `tokio` — Async runtime
- `rust_decimal` — Precise USDC arithmetic
- `dashmap` — Concurrent orderbook state
- `tracing` — Structured logging
