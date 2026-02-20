# Polymarket 15m Arbitrage Bot (Rust Core + Local Executor)

This repo now supports the architecture you asked for:

- **Rust core bot = strategy only** (orderbook ingest, arbitrage detection, risk/sizing).
- **Local executor (Python) = execution only** (owns keys, signs, submits orders via official SDK).

## Why this fixes many 401 issues

A frequent 401 root cause is bad/mismatched CLOB auth header generation. In this design:

- Rust no longer needs to own or generate CLOB signing logic in `EXECUTION_MODE=executor`.
- Executor uses `py-clob-client` helpers (`derive_api_key` / `create_api_key`) and official order posting flow.

## Quick start

## WSL prerequisites (recommended)

On Ubuntu WSL, install the common build/runtime dependencies first:

```bash
sudo apt update
sudo apt install -y \
  build-essential pkg-config libssl-dev curl git ca-certificates \
  python3 python3-venv python3-pip ripgrep
```

Then ensure Rust is usable in the current shell:

```bash
# if rustup is already installed, this is enough
source "$HOME/.cargo/env"

# optional: install rustup only if missing
command -v rustup >/dev/null 2>&1 || curl https://sh.rustup.rs -sSf | sh -s -- -y
rustup default stable
```

> Note: if distro Rust already exists under `/usr/bin`, rustup may warn during install. That warning is safe as long as `cargo --version` and `rustc --version` work after `source ~/.cargo/env`.

Environment quick-check (without ripgrep dependency):

```bash
grep -E 'TELEGRAM_BOT_TOKEN|TELEGRAM_CHAT_ID|PROXY_WALLET|PRIVATE_KEY|TELEGRAM_CONTROL_RPC_URL|POLYGON_RPC_URL' .env
```

1. Configure `.env` with at least:

```env
RPC_URL=...
PRIVATE_KEY=...
PROXY_WALLET=0x...
POLY_API_KEY=...
POLY_API_SECRET=...
POLY_API_PASSPHRASE=...
EXECUTION_MODE=executor
EXECUTOR_URL=http://127.0.0.1:8787
# optional override: 0 (EOA) / 2 (proxy wallet)
POLY_SIGNATURE_TYPE=2
# optional: Fill-Or-Kill execution in executor mode
FOK=false
# pair selection (enable exactly one)
PAIR_BTC_ETH=true
PAIR_BTC_SOL=false
PAIR_BTC_XRP=false
# optional: per market-window cap, per direction (0 = unlimited)
MAX_TRADES_PER_DIRECTION_PER_WINDOW=2
# optional: max share exposure per market condition (0/unset = unlimited)
MAX_TOTAL_SHARES_PER_MARKET=50
# optional: max sum tolerance above ARBITRAGE_MAX_SUM for detection/rebalance
ARBITRAGE_SUM_TOLERANCE=0.02
# optional: fee-aware pricing used in arb checks/rebalance simulation
TRADE_FEE_BPS=100
# optional: slippage safety buffer in bps
SLIPPAGE_BPS=15
# optional: dedupe identical opportunities for this many milliseconds
OPPORTUNITY_COOLDOWN_MS=5000
# optional: fingerprint rounding precision for dedupe (max 6)
OPPORTUNITY_PRICE_ROUND_DP=3
```

2. Run migration/setup script:

```bash
bash scripts/migrate_to_executor.sh
```

3. Start executor:

```bash
source .venv-executor/bin/activate
uvicorn executor.app:app --host 127.0.0.1 --port 8787
```

4. Start Rust bot:

```bash
cargo run
```

Or run all three together with one command (executor API + telegram bot + Rust bot):

```bash
bash scripts/start_bot.sh
```

## Modes

- `EXECUTION_MODE=executor` (**recommended**): Rust sends order intents to local Python executor.
- `EXECUTION_MODE=direct`: Rust signs and submits directly to CLOB (legacy path).

## Credential generation helper

Attempt to auto-generate/derive API credentials from wallet:

```bash
source .venv-executor/bin/activate
python scripts/generate_poly_api_creds.py
```

If Polymarket account permissions are missing, helper prints warning and you must set `POLY_API_*` manually.


### Signature mismatch (`invalid signature`) troubleshooting

If executor returns `invalid signature`, your signer/funder/signature type combination is mismatched.

- If `PROXY_WALLET` is different from the address derived from `PRIVATE_KEY`, use `POLY_SIGNATURE_TYPE=2`.
- If you trade directly from the same EOA address, use `POLY_SIGNATURE_TYPE=0`.
- Re-generate API credentials after changing signer/funder/signature type:

```bash
source .venv-executor/bin/activate
python scripts/generate_poly_api_creds.py
```

Then restart executor and run bot again.

## Polymarket limit-order minimums enforced

For executor-mode limit orders, the bot now enforces:

- minimum **5 shares** per leg.

If an opportunity does not satisfy the share threshold, it is skipped before order submission.

## Real-funds smoke test ($1 target)

You can run a standalone real-funds test order (smallest practical size) to validate account wiring:

```bash
source .venv-executor/bin/activate
python scripts/test_one_dollar_trade.py
```

Notes:

- The script auto-discovers an active market from Gamma,
- submits a marketable LIMIT BUY,
- auto-retries with detected taker fee bps when needed,
- and ensures at least 5 shares to satisfy Polymarket limit-order constraints.


### Strategy defaults (example `.env` values)

```env
ARBITRAGE_MAX_SUM=0.985
MIN_SHARES=5
MAX_SHARES=25
STRICT_SHARE_BOUNDS=true
```


Strategy pair logic (only these two combinations are considered):
- ETH_UP + BTC_DOWN
- ETH_DOWN + BTC_UP

Each candidate must satisfy:
- `sum(ask_prices) < ARBITRAGE_MAX_SUM`
- `max_shares_at_ask >= MIN_SHARES` where `max_shares_at_ask = min(ETH_ask_size, BTC_ask_size)`
- execution buys equal shares on both legs, additionally capped by optional `MAX_SHARES`.

- strict fixed share mode is enforced when `STRICT_SHARE_BOUNDS=true` and `MIN_SHARES == MAX_SHARES`:
  - the bot will submit exactly that share size or skip the trade (never more / never less)

- pair toggles in env (enable exactly one):
  - `PAIR_BTC_ETH=true` trades BTC/ETH 15m pair
  - `PAIR_BTC_SOL=true` trades BTC/SOL 15m pair
  - `PAIR_BTC_XRP=true` trades BTC/XRP 15m pair
  - bot rejects startup if none or multiple toggles are enabled

- one-leg fail-safe unwind in executor mode:
  - if one leg is placed and the other leg fails, bot waits briefly then calls executor `cashout` (GTC market order) on ~99% of the filled leg to avoid FOK $1-min failures

- Telegram manual control bot (`executor/telegram_control_bot.py`):
  - `/start` pins keyboard with buttons: `TRACK`, `KILL`, `CLAIM`
  - all Telegram control actions run in proxy/Safe mode and require `PROXY_WALLET`
  - `TRACK` fetches current open positions + held shares from Polymarket data API for `PROXY_WALLET`
  - `KILL` cashes out all open positions immediately using `PositionGuard` for `PROXY_WALLET`
  - `CLAIM` submits onchain `redeemPositions` txs for settled positions
  - if `PROXY_WALLET` is a contract wallet, CLAIM executes via Safe `execTransaction` (threshold=1 supported)
  - uses `TELEGRAM_CONTROL_RPC_URL` (or falls back to `POLYGON_RPC_URL`) for CLAIM tx execution

- optional Telegram notifications in executor mode:
  - set `TELEGRAM_ENABLED=true`, `TELEGRAM_BOT_TOKEN`, and `TELEGRAM_CHAT_ID`
  - on executor startup, bot sends a "BOT STARTED" Telegram heartbeat
  - `/cashout` sends unwind initiated/completed/failed alerts
  - `/notify` endpoint accepts structured events (`success`, `partial`, `unwind_start`, `unwind_complete`) so Rust can push trade notifications


- per-direction trade cap per 15m market window via `MAX_TRADES_PER_DIRECTION_PER_WINDOW`:
  - cap is tracked separately for each pair direction (`ETH_UP + BTC_DOWN` and `ETH_DOWN + BTC_UP`)
  - when a direction reaches its cap, further opportunities in that same direction are skipped until the next market window

- in-memory share tracking per active ETH/BTC market condition for cleaner balancing visibility in logs:
  - bot logs current tracked ETH/BTC shares before each trade attempt
  - bot records only fully completed paired executions toward direction caps

- fee-aware arbitrage detection/rebalance simulation now uses:
  - effective_buy_price = ask_price * (1 + TRADE_FEE_BPS/10000) * (1 + SLIPPAGE_BPS/10000)
  - effective_total_cost is checked against `ARBITRAGE_MAX_SUM + ARBITRAGE_SUM_TOLERANCE`

- rebalance-only mode (single-leg buy on weaker side) is attempted when imbalance exists and all of these are true:
  - imbalance shares >= `MIN_SHARES`
  - projected combined avg (fee-aware) <= `ARBITRAGE_MAX_SUM + ARBITRAGE_SUM_TOLERANCE`
  - projected combined avg is not worse than current combined avg
  - leg liquidity supports full rebalance size
  - optional exposure cap (`MAX_TOTAL_SHARES_PER_MARKET`) is not exceeded


### Telegram manual control bot

```bash
# from repo root
python -m executor.telegram_control_bot
```

Requires env vars: `TELEGRAM_BOT_TOKEN`, `TELEGRAM_CHAT_ID`, `PROXY_WALLET`, `PRIVATE_KEY`.
`CLAIM` also requires `TELEGRAM_CONTROL_RPC_URL` (or `POLYGON_RPC_URL`).

If you see `CLAIM failed: failed connecting to TELEGRAM_CONTROL_RPC_URL`:
- ensure the value is a full HTTPS endpoint (include `https://`),
- avoid surrounding quotes/spaces in env values,
- verify it is Polygon mainnet (`chainId=137`),
- optionally set `POLYGON_RPC_URL` as an additional fallback.
