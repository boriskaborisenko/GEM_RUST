# GEM_RUST

`GEM_RUST` is a Rust paper-trading engine for short Polymarket crypto UP/DOWN windows.

Current focus: `dynamic_grid_d1` for BTC `5m` and `15m` windows. The 15m interval is the priority because it gives more time for PTB crossings and post-open management; 5m is mostly used to collect fast behavioral statistics.

This project is experimental research code. It is designed for paper execution, log analysis, and strategy iteration.

## What It Watches

- Polymarket Gamma REST for market discovery.
- Polymarket CLOB REST/WebSocket for UP/DOWN bid and ask prices.
- Polymarket Chainlink WebSocket for live crypto spot.
- Bybit REST/WebSocket for BTC 1m ATR, with Binance REST fallback for warmup.
- Optional Google Vertex AI for one-window LLM directional forecasts.

## Current Strategy

`config.json` currently selects:

```json
{
  "strategy": "dynamic_grid_d1",
  "llm": {
    "enabled": false,
    "model": "gemini-3.5-flash",
    "location": "global"
  }
}
```

### Dynamic Grid D1

D1 is a one-leg pair-builder strategy:

1. Before the next window starts, D1 buys one side, `UP` or `DOWN`, near parity.
2. After the window opens, it protects the runner only when the first side is genuinely deteriorating or the opposite side becomes cheap enough.
3. It tries not to over-hedge correctly guessed windows, because an unhedged correct side can redeem at `1.00`.
4. It sells winners in small surplus layers and preserves clear near-expiry winners for redeem when probability supports that.
5. It sells weak/OTM tails only late or when the bid overpays the modeled probability.

Important semantics:

- BUY signal amount is USD.
- SELL signal amount is shares.
- BUY orders below `$1.00` are rejected.
- SELL can be any size, including dust/tails.
- PTB distance must be interpreted as percent and ATR/time-adjusted z-score, not raw dollars only.

### D1 Pre-Start Side Selection

D1 chooses the first side by priority:

1. Strong local directional/fair signal when PTB and spot context are available.
2. Momentum signal from spot velocity/acceleration.
3. First-window bootstrap fallback only when there is no signal history.

LLM is currently disabled and D1 ignores LLM forecasts for BUY side selection. Recent logs showed that `llm_prior` entries had negative expectancy and weak directional accuracy, especially on the priority 15m interval.

Current ask range:

```json
"preStartEntry": {
  "minSideAsk": 0.42,
  "maxSideAsk": 0.58
}
```

So normal future-window prices around `0.47-0.54` are allowed.

## LLM Forecasting

LLM is optional and configured under `llm` in `config.json`, but it is currently disabled.

Credentials:

- Put the Google service account JSON at `GEM_RUST/llm.json`.
- `llm.json` is ignored by git.
- The code reads `llm.json` at runtime and does not copy or hardcode its contents.

Startup behavior:

- If `llm.enabled = true`, the bot immediately runs a real Vertex `hello` health check.
- If the check passes, `SYSTEM EVENT LOG` shows `[LLM] OK ...`.
- If the check fails, LLM is disabled for that process and the bot continues without it.

Runtime behavior:

- When enabled for research, one forecast is requested per upcoming window.
- The forecast prompt includes rolling context from the last 10 closed windows in the same process: avg/median PnL, max drawdown, entry-side accuracy, LLM accuracy, runner redeem rate, hedge cost/rescue PnL, tail liquidation value, slippage sensitivity, winner counts, and compact per-window rows.
- Terminal shows LLM status, model, location, right/wrong count, and accuracy.
- Forecast rows and result rows are written to `llm_forecasts.csv`.
- Entry rows can include LLM side/confidence next to the actual entry side, but D1 does not use LLM to choose BUY side.

## Project Map

```text
src/
├── main.rs                  # Runtime loop, dashboard, market lifecycle, logging.
├── client.rs                # Polymarket market discovery, CLOB WS/REST, Chainlink spot.
├── config.rs                # config.json parsing.
├── llm.rs                   # Vertex AI directional forecaster.
├── trader.rs                # Paper portfolio, buys, sells, redeem, CSV trade logs.
├── volatility.rs            # BTC ATR warmup/tracking.
├── analytics.rs             # Optional offline Vertex helper; not wired into main.rs.
├── strategy.rs              # Strategy trait, signals, router.
└── strategy/
    ├── strategy_a.rs        # Simple Both baseline.
    ├── strategy_b.rs        # Asymmetric Ladder.
    ├── strategy_c.rs        # Dynamic Break-Even.
    ├── strategy_d.rs        # Older Dynamic Grid / TVDS / WeakScalp.
    └── strategy_d1.rs       # Current one-leg D1 strategy.
```

## Configuration

Main fields in `config.json`:

```json
{
  "strategy": "dynamic_grid_d1",
  "llm": {
    "enabled": true,
    "model": "gemini-3.5-flash",
    "location": "global"
  },
  "minBtcAtr": 0.0,
  "session": {
    "startingBank": 100,
    "minWindowBudget": 30.0,
    "maxWindowBudget": 150.0,
    "windowBudgetPct": 10.0
  },
  "preStartEntry": {
    "enabled": true,
    "minSecondsBeforeStart": 5,
    "maxSecondsBeforeStart": 120,
    "minSideAsk": 0.42,
    "maxSideAsk": 0.58
  }
}
```

Budget logic:

- Raw budget = current equity * `windowBudgetPct / 100`.
- It is clamped to `minWindowBudget..maxWindowBudget`.
- D1 then applies its own ATR-regime multiplier.
- In storm/high-volatility regimes D1 currently scouts smaller instead of hard-skipping.

## Running

From the project root:

```bash
cd GEM_RUST
cargo check
```

Runtime examples:

```bash
cargo run -- BTC 5m
cargo run -- BTC 15m
```

Common test setup:

- Run `BTC 5m` in one terminal.
- Run `BTC 15m` in another terminal.
- Let 5m collect faster statistics.
- Let 15m collect the priority signal sample.

## Logs

Each run creates a separate directory:

```text
logs/runs/<YYYYMMDD_HHMMSS>_<asset>_<interval>_<strategy>/
```

Files:

- `window_summary.csv`: closed-window PnL, winner, PTB, close spot.
- `entry_events.csv`: accepted pre-start entries, actual side, LLM side, confidence, ATR, ask prices, budget, shares.
- `llm_forecasts.csv`: LLM forecast rows plus result rows with `result_winner` and `result_correct`.
- `strategy_signals.csv`: every strategy BUY/SELL signal and reason, with spot velocity, PTB delta, shares, MTM, and probability context.
- `trade_events.csv`: executed paper BUY/SELL/REDEEM/EXPIRED rows.
- `lifecycle_events.csv`: market discovery, promotion, skip/entry lifecycle, WS status.

Recommended review order:

1. `window_summary.csv`
2. `entry_events.csv`
3. `llm_forecasts.csv`
4. `strategy_signals.csv`
5. `trade_events.csv`
6. `lifecycle_events.csv`

## Terminal Dashboard

The dashboard shows:

- strategy, asset, interval;
- LLM enabled/disabled, model, location, right/wrong/accuracy;
- started time, runtime, ATR;
- total/entered/closed/open/skipped windows;
- closed-only win/loss percentages;
- bank, cash, equity, realized PnL;
- current and next window details;
- bid/ask, combined ask, PTB, live spot deviation;
- spent, returned, estimated value, paired floor, break-even gap;
- trade log with side, shares, price, USD value, cash, and reason;
- system event log, including LLM startup status.

## Strategy Review Principles

Do not judge a run only by winrate.

Look at:

- avg PnL per window;
- median PnL;
- max drawdown;
- expectancy: average win vs average loss;
- entry side accuracy;
- runner held-to-redeem rate;
- hedge cost vs hedge rescue value;
- tail liquidation loss;
- slippage sensitivity at `+/-0.01` and `+/-0.02`;
- whether the chosen first side matches the eventual winner;
- whether any future LLM observer mode provides useful signal before re-enabling it;
- whether correct runners are left alone long enough to redeem;
- whether hedge buys are timely and not too early;
- whether weak tails are sold only when bid overpays probability;
- whether 15m entry count is high enough.

## Known Sharp Edges

- `volatility.rs` tracks BTC ATR even when the CLI asset is ETH/SOL.
- BUY amount is USD while SELL amount is shares; this interface is easy to misuse.
- `analytics.rs` is an offline helper and is not part of the live runtime path.
- `CLOB price_change` handling approximates ask as `price + 0.01` when needed.
- `static mut TIME_OFFSET_MS` exists for clock offset and should be treated carefully.

## Verification

Allowed quick checks:

```bash
cargo fmt
cargo check
```

Runtime tests are started manually by the operator.
