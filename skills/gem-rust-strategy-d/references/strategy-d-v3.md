# Strategy D v3 Reference

## Objective

Improve expectancy for 15m BTC UP/DOWN windows. The old run shown by the user had positive winrate but negative realized PnL, so v3 prioritizes loss size, controlled add-ons, PTB-aware exits, and winner runner retention over raw winrate.

Current status: first logs showed the old Dynamic BUY was too frequent and usually fired after the strong side was fully sold. It has been replaced by experimental WeakScalp. The next useful work is observation, not theoretical optimization.

## Coupled-System Principle

Strategy D is a coupled control system, not a pile of independent thresholds. Every rule must be evaluated against the full lifecycle:

- ATR regime changes entry frequency, budget size, split, grid targets, WeakScalp reach, and weak-side patience.
- Time decay changes whether a low bid is a temporary mark or a real loss that should be salvaged.
- PTB deviation changes winner conviction, weak-side reversal probability, WeakScalp safety, and redeem-hold confidence.
- Spot velocity changes whether a PTB deviation is likely still expanding or already mean-reverting.
- Strong-side grid sells reduce risk but can destroy expected value if they sell a clear ITM winner before redeem.
- WeakScalp can harvest weak-side rebounds, but it must not become averaging down or naked re-entry after the strong side is gone.
- Emergency exits preserve cash on losers, but must not liquidate a clear ITM redeem runner too cheaply.

When changing one parameter, explicitly check the effects on entry count, average position size, average win, average loss, weak-side exposure, runner retention, and redeem probability.

## Entry Model

Entry still requires both sides near parity through `config.preStartEntry`.

Additional v3 checks in `src/strategy/strategy_d.rs`:

- ATR does not hard-skip by itself. It sets an entry regime and budget multiplier.
- Ultra-low ATR uses micro sizing and demands perfect combined ask.
- Extreme ATR uses micro neutral sizing and a tighter combined-ask cap.
- Require ask spread <= `ENTRY_MAX_ASK_SPREAD`.
- Require combined ask below the ATR-regime cap.

ATR regimes:

- Low/scout ATR: smaller budget multiplier, almost neutral split.
- Normal ATR: full budget, mild cheaper-side tilt.
- High ATR: reduced budget, delta-neutral split.
- Extreme ATR: micro neutral split, no directional concentration.

## Sizing Semantics

`EntrySignal` carries:

- `budget_multiplier`
- `cheaper_side_ratio`
- `reason`

`main.rs` applies the entry plan to the normal equity-based budget.

Important: `OrderSignal.amount` has mixed historical semantics:

- Buy: USD amount passed to `Portfolio::execute_buy`.
- Sell: share amount passed to `Portfolio::execute_sell`.

WeakScalp buy signals must use USD amount. Sell signals still use share amount.

## Live Management

Strong side:

- Use live bid leadership with a small tie band.
- Fall back to initial share dominance only when UP/DOWN bids are close.

Strong grid:

- Uses live bid leadership for strong side.
- First step de-risks only 30%.
- Second step normally takes another 30%, but can be blocked by redeem-hold.
- Final step sells the remainder only when redeem-hold is not active.
- Targets adapt to time, ATR, PTB deviation zone, and whether the strong side was the cheaper/heavier side.
- Spot velocity softly adjusts targets: with-trend velocity raises strong-side targets a few cents; against-trend velocity lowers them a few cents.

PTB deviation:

- Use both absolute USD distance and percentage distance.
- BTC zones: near <= $25, moderate <= $100, far <= $250, runaway above that, with percent fallbacks.
- Use zones in strong-grid target expansion, WeakScalp reach, and weak-side TVDS exits.

Redeem hold:

- If the strong side is clearly ITM, PTB deviation has conviction, and the window is close enough to expiry, do not dump the winner at 0.66-0.75.
- While redeem-hold is active, mid-grid exits are blocked below `REDEEM_HOLD_RELEASE_BID`.
- If release bid is reached, sell only a small partial and leave the runner for close-window redeem.
- Emergency time stop is blocked for the held ITM winner while redeem-hold conditions remain true.

WeakScalp:

- Replaces the old Dynamic BUY after logs showed Dynamic BUY was usually not a hedge.
- Only before 55% elapsed.
- Max two tranches per side/window, but only one active tranche per side at a time.
- Requires known PTB deviation, near/moderate zone, and ATR-derived deviation cap.
- Requires live strong-side inventory: at least 20% of initial strong-side shares remain.
- Requires weak ask below ATR-derived cap and spot velocity in favor of the weak side.
- Uses tiny USD sizing: 3.5% of window spent per tranche.
- Sells only the active scalp shares at an ATR-derived target above average entry.

Weak exit:

- Uses TVDS matrix: time elapsed, ATR, PTB deviation.
- Spot velocity softly adjusts weak exits: wait a little more for an actual reversal, exit faster when the weak side remains under pressure.
- Late phase has an exact break-even fallback when available.

## Logging

Each run writes to:

`logs/runs/<run_id>_<asset>_<interval>_<strategy>/`

Files:

- `lifecycle_events.csv`: one row per window promotion, including skipped/entered state, ATR, prices, spot/PTB.
- `entry_events.csv`: accepted pre-start entries and ATR regime reasons.
- `strategy_signals.csv`: live strategy signals, execution status, time, prices, PTB distance, position, and mark-to-market PnL.
- `strategy_signals.csv` also includes raw/smoothed spot velocity and acceleration so WeakScalp / redeem-hold decisions can be audited.
- `trade_events.csv`: executed paper buys/sells.
- `window_summary.csv`: closed entered windows, realized PnL, close spot, PTB, and winner.

## Next Test Protocol

Run from `GEM_RUST` in two terminals:

```bash
cargo run -- BTC 5m
cargo run -- BTC 15m
```

Recommended sample before tuning:

- 5m: 40-50 windows.
- 15m: 15-25 windows.
- No parameter tuning during the first 5-10 windows unless there is a clear bug.
- Compare WeakScalp runs against the old `20260601_103616` logs where Dynamic BUY was too frequent.

Review order:

1. `window_summary.csv`: final PnL, winner, close spot/PTB, redeem behavior.
2. `entry_events.csv`: entry regimes, ATR, split, accepted windows.
3. `strategy_signals.csv`: WeakScalp, SELL-grid, weak exits, velocity bias, execution status.
4. `trade_events.csv`: accounting trail.
5. `lifecycle_events.csv`: skipped/entered lifecycle sanity.

Key review questions:

- Is WeakScalp rare enough and profitable enough to justify added exposure?
- Are strong winners sold too early, or is redeem-hold capturing enough upside?
- Are weak exits saving cash before decay kills the side?
- Does spot velocity help by soft-adjusting decisions, or does it overfilter good opportunities?
- Is average loss shrinking relative to the old negative-PnL baseline?
