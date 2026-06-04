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
- `src/llm.rs`: optional Vertex AI one-window directional forecaster for D1 research; reads ignored `llm.json` service-account credentials from project root.
- `src/trader.rs`: `Portfolio`, `WindowState`, paper buy/sell/redeem, equity, CSV history.
- `src/strategy.rs`: `TradeStrategy` trait, `OrderSignal`, strategy router.
- `src/strategy/strategy_a.rs`: Simple Both.
- `src/strategy/strategy_b.rs`: Asymmetric Ladder.
- `src/strategy/strategy_c.rs`: Dynamic Break-Even.
- `src/strategy/strategy_d.rs`: Dynamic Grid / TVDS / WeakScalp.
- `src/strategy/strategy_d1.rs`: D1 one-leg pair-builder experiment.
- `src/strategy/strategy_dx.rs`: Dx current-window fair-value directional strategy with optional hedge/PTB unwind.
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
- `execute_buy` takes USD amount and ask price; runtime BUYs below `$1.00` are rejected.
- `execute_sell` takes share amount and bid price; SELL can be any USD value so dust/tails can be cleared.
- If a current window was promoted as `SKIPPED` but later receives a live BUY, `execute_buy` converts it to `LIVE`, decrements `skipped_windows`, and increments `entered_windows`. This supports post-open D1 conviction entries without corrupting terminal stats.
- `close_window` redeems ITM residual shares at 1.00 if spot/PTB are known, otherwise sells all at cached bids.

## Strategy Interface

All strategies implement:

- `check_pre_start_entry(config, prices, window_number, secs_to_start, current_btc_atr) -> Option<EntrySignal>`
- `process_live_tick(config, prices, spot_price, market, win_state, secs_to_end, current_atr, spot_signal) -> Vec<OrderSignal>`
- `get_strategy_state(window_number)`

Strategies do not directly mutate the portfolio. They emit `EntrySignal` / `OrderSignal`; `main.rs` applies them through `Portfolio`.

`EntrySignal` carries pre-start ask prices plus `budget_multiplier`, `cheaper_side_ratio`, `mode`, and a reason string. `mode = Both` uses the paired-core allocator; `mode = OneSide("UP"/"DOWN")` spends the full entry budget on only that side. Strategy D uses `Both`; D1 uses `OneSide`.

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
- Strong grid now sells smaller layers and is gated by window-progress percentage plus probability edge. The bid target alone is not enough: the market price must be better than the model's win probability, unless the edge is unusually large early. This prevents 15m windows from dumping most inventory in the first few minutes.
- Paired-floor protection applies to strong-grid step3/rest: surplus shares can still sell, and paired-core shares can sell with a bounded floor-sacrifice budget instead of waiting only for a high 0.90 release. The guard limits damage to `cash_returned + min(up_shares, down_shares)` while still allowing partial risk reduction below 0.90. The marker is `_paired_floor_protected` when the proposed sell is cut.
- Weak side exits through TVDS target matrix plus late break-even fallback, but targets are only a minimum price floor; probability decides whether the sale is worth taking. Weak-exit separates surplus from paired core. Surplus can be sold when bid overpays win probability; paired core is preserved unless the edge is clearly favorable or the side is very unlikely late. The crossover weak-exit block must not dump the second side if that side has become current live-strong or ITM after a PTB cross; it now uses projected shares/cash after earlier same-tick signals. Weak-exit reasons include `_p_..._edge_..._sell_..._reserve_..._insurance_...`; strong-grid reasons include `_p_..._edge_...` and `_insurance_tail_kept` when the tail blocks a sell.
- Emergency 15%-time salvage sells only OTM surplus above the paired core. It must not liquidate `min(up_shares, down_shares)`, because that paired core has guaranteed redeem value on one side. OTM surplus/tail selling is probability-based: estimate the held side's win probability from PTB distance, ATR, seconds left, and spot velocity, then sell only when current bid is better than that probability. Example: if holding DOWN, `UP +300 with 50s left` implies low DOWN probability and is a sell; `UP +9 with 30s left` implies meaningful DOWN probability and should be kept if bid underpays it. Emergency reasons include `_otm_surplus_..._p_..._keep_paired_...`.
- SELL-grid targets are spot-velocity-aware: favorable velocity slightly raises targets, adverse velocity slightly lowers them. The adjustment is intentionally small so ATR/PTB/time decay still dominate.

Strategy D1:

- Activate with `strategy: "dynamic_grid_d1"` in `config.json`.
- D1 is a separate one-leg pair-builder experiment in `src/strategy/strategy_d1.rs`; it does not reuse D's sell grid.
- Pre-start entry buys only one side near parity, currently ask in `[0.48, 0.52]`. Full D1 one-leg size requires directional fair probability at least `0.54`; otherwise D1 can use a two-confirmation momentum side if it is not meaningfully negative edge. When the process starts from a zero/current window and has no useful signal history, only `window_number == 1` may use a deterministic bootstrap random side from the market slug; later windows without directional/momentum evidence are skipped. `ATR=0` is treated as uninitialized/warmup data and D1 pre-start entry is skipped until ATR is valid. Other strategies keep two-sided entry through `EntryMode::Both`.
- D1 chooses the first side by signal order, not only cheapest ask: directional PTB/ATR fair edge first, momentum confirmations second, and the one-time bootstrap random fallback only for the first post-zero window. Fair probability uses PTB distance, ATR-scaled expected move over the upcoming full window horizon, and spot velocity drift, following the proto_v08 idea of gap/z-score plus velocity confirmation using only data currently available in GEM_RUST.
- Optional LLM forecasting is configured under `llm` in `config.json`, for example `{ "enabled": false, "model": "gemini-3.5-flash", "location": "global" }`, and requires Vertex service-account credentials at `GEM_RUST/llm.json` (`.gitignore` excludes it) only when enabled. LLM is currently disabled because recent `llm_prior` entries had negative expectancy and weak directional accuracy, especially on the priority 15m interval. D1 ignores LLM forecasts for BUY side selection. If LLM is re-enabled for research, `src/llm.rs` runs a real startup `hello` health check, sends one pre-window prompt per `window_number`, and includes rolling context from the last 10 closed windows in the same process: avg/median PnL, max drawdown, entry-side accuracy, LLM accuracy, runner redeem rate, hedge cost/rescue PnL, tail liquidation value, adverse slippage at 0.01/0.02, winner counts, and compact per-window rows. The terminal can show `LLM-forecast: enabled/disabled`, model, location, right/wrong counts, and accuracy; `llm_forecasts.csv` can audit hit-rate independently of PnL.
- If pre-start scout is skipped, D1 can still enter after the window opens via `d1_live_conviction_entry_*`. This is a small, careful port of proto_v08 EDGE directionality: once PTB is known, side comes from `gap_z = (spot - PTB) / ATR_expected_move` (`UP` for positive, `DOWN` for negative), while `fair - ask`, entry price, elapsed/window phase, and velocity/acceleration confirmations are quality gates. It does not import proto_v08 Markov/confluence/depth yet because GEM_RUST does not currently collect those streams.
- D1 has an initial sleep mode: for `max(25 seconds, 5% of window duration)` after window start it does not buy the opposite side. This preserves the one-leg x2 upside and lets early z-score/velocity noise settle before paying for protection.
- D1 divides the live window into time phases: `opening` (`0-25%`), `mid` (`25-60%`), `late` (`60-85%`), and `final` (`85-100%`). ATR regimes are `calm` (`<20`), `normal` (`20-45`), `volatile` (`45-90`), and `storm` (`>=90`). Phase plus ATR adjusts how patient D1 is before buying the opposite side.
- Live D1 management is now one unified opposite-side hedge plan, not separate target/insurance/repair branches. `opposite_hedge_plan(phase, ATR regime, pair_cost, first_p, first_otm_pct, first_otm_z)` returns a target opposite/first share ratio and max pair cost; the live loop buys only the missing ratio. `first_otm_pct` is the first side's distance against PTB as a percent of PTB, so BTC/SOL/ETH use the same scale. `first_otm_z` is the same distance divided by ATR-scaled expected move to window end, so time decay and volatility are still included. D1 also tracks PTB baseline/cross per window; after an adverse PTB cross it may buy a small opposite hedge only while `first_price + opposite_ask <= 1.04`.
- Correct or near-PTB runners are protected: if the first leg is near PTB (`first_otm_pct`/`first_otm_z` small), D1 does nothing unless `pair_cost <= 0.90`, in which case it can buy only a small cheap-pair lock. Example: holding DOWN with UP only `+2` near the end should stay mostly unhedged; holding DOWN with UP `+300` can become `clear_wrong` or `severe_wrong` depending on percent/z/time.
- D1 opposite-side BUY signals smaller than `$1.00` are suppressed. This prevents repeated micro-locking from slowly eating the one-leg runner while still allowing the hedge target to increase later if the first leg genuinely deteriorates or the pair becomes very cheap.
- D1 now has a simple SELL contour. `d1_strong_runner_sell_*` sells only surplus shares of the ITM side in 3 small steps (`0.65/0.75/0.85` bid zones, gated by `itm_pct`, `itm_z`, phase, and side probability), and it stops selling close to window end so a clear winner can redeem at `1.00`. `d1_weak_salvage_sell_*` sells only OTM surplus above the paired core when bid overpays the modeled probability, and remains late/close-window only; an early adverse PTB cross may trigger hedge logic, but must not liquidate a naked first-leg runner in opening/mid because short-window reversals are common.

Strategy Dx:

- Activate with `strategy: "dynamic_grid_dx"` in `config.json`.
- Dx deliberately does not buy NEXT/future windows. `check_pre_start_entry` always returns `None`; the runtime promotes an unentered window to `SKIPPED`, and Dx can then use the existing live BUY path to convert the active CURRENT window to `LIVE`.
- Dx is a hybrid of D1 directionality and Strategy D PTB management: direction comes from current PTB gap/z-score, entry quality comes from fair probability vs live ask, and management uses optional hedge plus PTB sells.
- Fair probability follows the existing Normal CDF model: `(spot - PTB + velocity_drift) / (ATR * sqrt(seconds_left / 60))`. Dx requires the fair edge side to match the PTB gap side before entering.
- Dx keeps an internal per-window YES-mid tick buffer to approximate Polymarket probability velocity over 30s/60s. This is a microstructure guardrail: favorable probability velocity can confirm entry; strongly adverse probability velocity blocks entry or can justify a small hedge.
- Entry phases are current-window only: first 10% observe, then entry/manage, strict late mode after roughly 76%, and final salvage after 90%. Late entries require higher edge, higher fair probability, tighter spread, and stronger PTB z-score.
- Hedge buys are optional and capped. Dx buys the opposite side only when it is cheap versus fair value, pair cost is bounded, and the primary leg shows adverse cross/counter-velocity. Hedge PTB sells unload the hedge when bid exceeds its average entry by the target and overpays modeled probability.
- Primary PTB sells lock profit in small steps when bid beats average entry and overpays fair probability, while clear close-window winners can remain for redeem.

ATR:

- ATR warmup first tries Bybit REST `BTCUSDT` 1m candles. Bybit can return HTTP `403` for restricted/blocked IPs, so warmup now falls back to Binance REST 1m candles before giving up. Live ATR tracking still uses Bybit WebSocket.

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
- Core metrics to report: avg PnL per window, median PnL, max drawdown, entry side accuracy, runner held-to-redeem rate, hedge cost vs hedge rescue value, tail liquidation loss, and slippage sensitivity at +/-0.01 and +/-0.02.
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
- `llm_forecasts.csv` includes optional Vertex AI per-window `UP`/`DOWN` forecast, confidence, strength, prompt context, and errors; join by `window_id` against `window_summary.csv` to measure LLM directional hit-rate.
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
