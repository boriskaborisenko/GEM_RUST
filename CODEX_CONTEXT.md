# CODEX_CONTEXT

Short project map for future Codex work. Keep this file compact and update it when architecture or strategy behavior changes.

## Scope

Work only inside `GEM_RUST`.

`GEM_RUST` is a Tokio Rust paper-trading engine for short Polymarket UP/DOWN crypto windows. It watches:

- Polymarket Gamma REST for market discovery.
- Polymarket CLOB REST/WS for UP/DOWN bid/ask prices.
- Polymarket Chainlink WS for live spot.
- Bybit REST/WS for BTC 1m ATR.

The current project builds with `cargo check`.

## Runtime Flow

Entry point: `src/main.rs`.

1. CLI defaults to `BTC 5m`; usage is `cargo run -- <asset> <interval>`.
2. Loads `config.json` from the process working directory, so run from `GEM_RUST`.
3. Syncs local time against Polymarket Gamma `Date` header.
4. Warms BTC ATR through Bybit REST, then starts Bybit WS ATR tracking.
5. Spawns Chainlink spot WS.
6. Discovers active CURRENT window and upcoming NEXT window.
7. Subscribes each window to CLOB price streams.
8. Event loop renders terminal dashboard, monitors time/promotion, and processes market/spot events.

CURRENT/NEXT lifecycle:

- NEXT starts as `WAITING_ENTRY`.
- Pre-start buys happen while `secs_to_start` is within `config.preStartEntry`.
- At start time, NEXT is promoted to CURRENT.
- Entered NEXT becomes `LIVE`; unentered NEXT becomes `SKIPPED`.
- Old CURRENT is closed during overlap or after expiry safety checks.

## Core Modules

- `src/config.rs`: serde structs for `config.json`.
- `src/client.rs`: market discovery, CLOB orderbook snapshots/WS, Chainlink spot WS, PTB parsing/deviation.
- `src/trader.rs`: `Portfolio`, `WindowState`, paper buy/sell/redeem, equity, CSV history.
- `src/strategy.rs`: `TradeStrategy` trait, `OrderSignal`, strategy router.
- `src/strategy/strategy_a.rs`: Simple Both.
- `src/strategy/strategy_b.rs`: Asymmetric Ladder.
- `src/strategy/strategy_c.rs`: Dynamic Break-Even.
- `src/strategy/strategy_d.rs`: Dynamic Grid / TVDS / WeakScalp.
- `src/volatility.rs`: Bybit BTC ATR(14), currently BTC-only even when asset CLI is ETH/SOL.
- `src/analytics.rs`: optional Vertex AI log analysis helper; not wired into `main.rs` yet.

## Important Data Types

`MarketWindow` in `client.rs`:

- `slug`, `start_time`, `end_time`, optional `price_to_beat`.
- UP/DOWN CLOB token ids.
- `get_ptb_deviation(spot_price)` returns `(delta, percent)`.

`PricesState`:

- `up.bid/ask`, `down.bid/ask`.

`WindowState` in `trader.rs`:

- lifecycle status, market, spent/cash_returned, current shares, initial shares, trade list, cached prices.

`Portfolio`:

- cash, equity, realized PnL, wins/losses/skips.
- `execute_buy` takes USD amount and ask price.
- `execute_sell` takes share amount and bid price.
- `close_window` redeems ITM residual shares at 1.00 if spot/PTB are known, otherwise sells all at cached bids.

## Strategy Interface

All strategies implement:

- `check_pre_start_entry(config, prices, window_number, secs_to_start, current_btc_atr) -> Option<EntrySignal>`
- `process_live_tick(config, prices, spot_price, market, win_state, secs_to_end, current_atr, spot_signal) -> Vec<OrderSignal>`
- `get_strategy_state(window_number)`

Strategies do not directly mutate the portfolio. They emit `EntrySignal` / `OrderSignal`; `main.rs` applies them through `Portfolio`.

`EntrySignal` carries pre-start ask prices plus `budget_multiplier`, `cheaper_side_ratio`, and a reason string. Strategy D uses this for ATR-regime entry sizing.

`SpotSignalSnapshot` carries raw 20s spot velocity, smoothed spot velocity, and acceleration. `main.rs` computes it from Chainlink spot ticks. A/B/C ignore it; Strategy D uses it as a guardrail for WeakScalp, SELL-grid adjustment, and redeem-hold.

## Current Config

`config.json` currently selects:

- `strategy`: `dynamic_grid`
- `startingBank`: `100`
- `minWindowBudget`: `30`
- `maxWindowBudget`: `150`
- `windowBudgetPct`: `10`
- `preStartEntry`: enabled, 5 to 120 seconds before start, asks in `[0.42, 0.58]`
- `minBtcAtr`: `0.0`

Budget logic in `main.rs`:

- Budget = equity * `windowBudgetPct` / 100.
- Clamped to `[minWindowBudget, maxWindowBudget]`.
- If cash is below target but at least min, budget is clipped to available cash.
- If cash is below min, entry is rejected.
- Entry allocation builds an equal-shares paired core first, then spends only a bounded surplus on the cheaper side according to `EntrySignal.cheaper_side_ratio`. This allows asymmetric exposure without destroying the terminal paired floor. Strategy D sets the ratio by ATR regime; A/B/C use the internal legacy default `0.60`.

## Strategy Notes

Common pre-start behavior:

- Strategies require enabled pre-start entry, not already entered, positive asks, and asks within config bounds.
- A/B/C still hardcode a minimum `secs_to_start >= 5` instead of using both config bounds. D uses the config range.

Common emergency behavior:

- All strategies contain a 15% remaining-time emergency sell rule.
- They sell remaining side only if bid is at least `0.20`.
- `config.exitBeforeEndSeconds` and `config.forceCloseAtEnd` exist but are not used by the strategy code.

Strategy A:

- Sells 100% of either side when bid reaches `config.sellStrategy.exitBid`.

Strategy B:

- Determines strong side by higher initial shares.
- Strong/weak ladders default to `[0.62, 0.72]` / `[0.70, 0.85]`, overridden by `config.asymmetricLadder`.
- Optional decay multiplies targets by 0.90 after 50% elapsed and 0.80 after 80% elapsed.

Strategy C:

- First side hitting `exitBid` is sold fully.
- Second side waits for spot/PTB crossover.
- Then sells only if bid >= `(spent - cash_returned) / remaining_shares + slippageBuffer`.

Strategy D:

- Treat Strategy D as a coupled control system. ATR, time decay, PTB deviation, bid/ask spread, sizing, grid exits, WeakScalp, emergency stop, and redeem-hold must be changed together consciously.
- Pre-start entry uses internal ATR regimes in `src/strategy/strategy_d.rs`: ultra-low ATR micro sizing, low ATR scout sizing, full-size normal ATR, reduced neutral high ATR, and micro neutral extreme ATR. ATR does not hard-skip by itself.
- Strategy D now allows directional pre-start entries up to a wider ask spread, while still rejecting too-wide or overpriced pairs through spread and combined-ask caps. Directional entries are skipped in low ATR and use reduced, neutral sizing by spread.
- Strong grid uses live bid leadership with a small tie band instead of only initial share dominance.
- PTB deviation is classified using both USD distance and percentage distance, so `$5` and `$300` around BTC are not treated the same.
- Old Dynamic BUY was too frequent in the first logs and often fired after the strong side was already fully sold. It has been replaced by experimental WeakScalp.
- WeakScalp can buy a tiny weak-side tranche only after the window has recovered at least 40% of spent cash, while the strong side still has at least 20% of its initial shares, PTB is near/moderate, weak ask is capped, and spot velocity favors the weak side.
- WeakScalp uses one active tranche per side at a time, max two tranches per side/window. A new buy is allowed only after the previous scalp tranche has been sold or cleared.
- WeakScalp amount is USD, currently 3.5% of window spent with a small minimum.
- Strategy D has a capital-protected mode once returned cash reaches 70% of window spent: new WeakScalp buys are blocked and weak-side exit targets are capped lower to prioritize keeping recovered capital.
- Clear ITM winners can enter redeem-hold: mid-grid exits are blocked below `0.90`, a small partial can be released at high bid, and the remaining runner is left for `close_window` redeem if conditions persist. A sharp counter-velocity move disables the hold.
- Strong grid now sells smaller early tranches and keeps a baseline runner unless the strong side reaches the high release bid.
- Paired-floor protection applies to strong-grid step3/rest: surplus shares can still sell, and paired-core shares can sell with a bounded floor-sacrifice budget instead of waiting only for a high 0.90 release. The guard limits damage to `cash_returned + min(up_shares, down_shares)` while still allowing partial risk reduction below 0.90. The marker is `_paired_floor_protected` when the proposed sell is cut.
- Weak side exits through TVDS target matrix plus late break-even fallback, but they are no longer full-liquidation by default. They sell the cash-preservation portion and keep an insurance tail based on initial shares, PTB zone, time decay, and capital-protected mode. The crossover weak-exit block must not dump the second side if that side has become current live-strong or ITM after a PTB cross; it now uses projected shares after earlier same-tick strong-grid signals. Weak-exit reasons include `_sell_..._reserve_..._insurance_...`.
- SELL-grid targets are spot-velocity-aware: favorable velocity slightly raises targets, adverse velocity slightly lowers them. The adjustment is intentionally small so ATR/PTB/time decay still dominate.

## Strategy D Skill

Project-local skill lives at `skills/gem-rust-strategy-d/SKILL.md`.

Use it for future Strategy D design, tuning, logging, or 5m/15m run analysis. Detailed v3 notes live in `skills/gem-rust-strategy-d/references/strategy-d-v3.md`.

## Current Test Handoff

Important user workflow rule: do not start runtime tests or trading runs unless the user explicitly asks for that exact run. The user starts 5m/15m tests themselves.

As of the latest Strategy D work, old Dynamic BUY has been replaced by WeakScalp after log review. The next run is an A/B-style behavioral comparison against `logs/runs/20260601_103616_btc_5m_dynamic_grid` and `logs/runs/20260601_103616_btc_15m_dynamic_grid`.

Current intended user-run test:

- User starts `cargo run -- BTC 5m` in one terminal.
- User starts `cargo run -- BTC 15m` in a second terminal.
- 15m is the priority signal; 5m is mainly a faster behavioral smoke test.
- Let 5m collect roughly 40-50 windows if possible.
- Let 15m collect roughly 15-25 windows if possible.
- Do not tune parameters during the first 5-10 windows unless the behavior is clearly broken.
- Expect `weak_scalp_buy_*` to be much rarer than old `dynamic_buy_*`.

Primary questions for the first log review:

- Entry: are too many windows skipped, or is the ATR/parity regime allowing enough entries?
- ATR hypothesis to test, not yet a rule: 5m may perform better in the 30-40/30-45 ATR band and degrade at 40+ ATR; avoid hard-filtering until a fresh wider-entry run confirms whether this is causal or sample noise.
- WeakScalp: is it rare and useful, or does it still add loss size? Check velocity, PTB distance, weak ask, strong remaining shares, and later window result.
- Strong grid: does velocity-aware target adjustment avoid selling good winners too early, or does it wait too long?
- Weak exit: does it preserve cash when the weak side remains under pressure?
- Redeem-hold: does it hold real ITM winners into redeem, and does counter-velocity disable it when the move reverses?
- Expectancy: prioritize average win vs average loss and final equity over raw winrate.

## Run Logs

Each process creates a separated run directory:

`logs/runs/<YYYYMMDD_HHMMSS>_<asset>_<interval>_<strategy>/`

Files:

- `lifecycle_events.csv`
- `entry_events.csv` includes accepted pre-start entries, ATR regime reasons, USD allocation, and resulting shares per side.
- `strategy_signals.csv` includes spot velocity, smoothed velocity, acceleration, normalized `amount_kind`, `signal_shares`, and `signal_usd_value` columns for impulse-aware and BUY/SELL audit. Buy signal `amount` is USD; sell signal `amount` is shares.
- `trade_events.csv` includes executed paper BUY/SELL plus terminal REDEEM/EXPIRED records, with price, shares, USD value, and cash-after.
- `window_summary.csv`

Terminal display notes:

- Window result percentages are calculated from closed/settled windows only; open entered windows are shown separately.
- Trade log lines show side, shares, price, USD value, cash-after, and reason.
- CURRENT window display shows the paired terminal floor (`returned + min(up_shares, down_shares)`), break-even gap, live bid leader, ITM side, and whether crossover weak exit is armed or blocked because the second side has become live-strong/ITM.

Recommended first-pass log order:

1. `window_summary.csv` for equity/PnL, winners, close spot, PTB.
2. `entry_events.csv` for entry count, ATR regimes, budget multipliers, split reasons.
3. `strategy_signals.csv` for why each BUY/SELL happened and whether velocity influenced it correctly.
4. `trade_events.csv` only after signal reasons are understood.
5. `lifecycle_events.csv` for skipped-vs-entered sanity and PTB/spot capture.

## Known Sharp Edges

- `analytics.rs` mentions `config.toml` in its prompt, but the project uses `config.json`.
- `volatility.rs` tracks BTC ATR only; CLI asset does not change the ATR symbol.
- `static mut TIME_OFFSET_MS` is globally mutable and unsafe; OK for now but not ideal.
- CLOB `price_change` handling treats update price as bid and approximates ask as `price + 0.01` when needed; check API semantics before relying on precision.
- `OrderSignal.amount` is still semantically mixed: buy amount is USD, sell amount is shares. Strategy D now accounts for this, but this interface remains easy to misuse.
- `process_event` clones `win_state` before writing newest prices, so strategy sees current `prices` argument fresh but `win_state.prices` from the previous tick.
- Git metadata is not present at `/Users/boriskaborisenko/Desktop/poly`, so use local file diffs rather than git status unless repo root is elsewhere.

## Verification Commands

From `GEM_RUST`:

```bash
cargo check
```

Do not run `cargo run` yourself unless the user explicitly asks for that exact runtime run.
