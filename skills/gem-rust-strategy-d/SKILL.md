---
name: gem-rust-strategy-d
description: Work on the GEM_RUST Dynamic Grid Strategy D sub-project. Use when Codex is asked to analyze, tune, redesign, debug, or implement ATR-aware pre-start entry, UP/DOWN capital split, strong-side sell ladders, weak-side exits, WeakScalp, per-run logging, or 5m/15m behavior inside /Users/boriskaborisenko/Desktop/poly/GEM_RUST.
---

# GEM_RUST Strategy D

## Scope

Work inside `/Users/boriskaborisenko/Desktop/poly/GEM_RUST`.

This skill is mandatory for every GEM_RUST / Strategy D turn: log analysis, tuning, code review, strategy redesign, terminal display, CSV logging, probability/redeem math, and any 5m/15m behavior discussion.

Prefer keeping strategy math in `src/strategy/strategy_d.rs`. Touch other files when the strategy interface, execution semantics, config, logging, or asset-aware ATR requires it.

## First Reads

Read these files before changing behavior:

- `CODEX_CONTEXT.md` for project map and known sharp edges.
- `src/strategy/strategy_d.rs` for Strategy D math.
- `src/strategy.rs` for signal semantics.
- `src/main.rs` for pre-start budgeting, lifecycle, and signal execution.
- `src/trader.rs` for paper accounting and log files.

For current Strategy D v3 invariants, read `references/strategy-d-v3.md`.

## Core Invariants

- Pre-start buy must happen only before the window starts. Strategy D buys both sides near parity; Strategy D1 intentionally buys one side near parity and later tries to build/repair a pair.
- Buy signal `amount` means USD. Sell signal `amount` means shares. Do not send shares as buy amount.
- Treat Strategy D as a coupled control system. ATR, time decay, PTB deviation, bid/ask spread, entry sizing, strong-side grid, weak-side exit, WeakScalp, emergency stop, and redeem-hold all affect each other.
- Do not add a local rule that fixes one symptom without checking its second-order effects on the full window lifecycle.
- ATR should tune aggression, not hard-cut entries by itself. Low ATR should micro/scout small; normal ATR may use full size; high ATR should avoid directional over-concentration.
- Spot velocity is a guardrail, not a standalone signal. Use it to allow WeakScalp only on a real weak-side reversal, release/avoid redeem-hold when the winner starts reversing sharply, and softly adjust SELL-grid targets.
- PTB deviation must use both absolute USD distance and percentage distance.
- Clear ITM winners can be held as runners for close-window redeem instead of being dumped too early.
- Strong side should be based on live bid leadership when possible, not only initial shares.
- WeakScalp is experimental micro-scalping, not revenge sizing or averaging down. It must require known PTB deviation, a tight deviation cap, an ask cap, favorable velocity, live strong-side inventory, and a small USD cap.
- 15m is the priority timeframe, but 5m must still compile and run with the same code path.
- Keep per-run logs separated under `logs/runs/<run_id>_<asset>_<interval>_<strategy>/`.

## Workflow

1. Inspect current code and any run logs relevant to the request.
2. Identify whether the change is pure Strategy D math or needs interface/logging/config support.
3. Run a coupling audit before changing thresholds: entry frequency, position size, sell timing, add-on risk, weak-side salvage, emergency behavior, redeem probability, and log observability.
4. Preserve paper accounting semantics in `Portfolio`.
5. Add or update concise strategy reasons in emitted signals; logs rely on them.
6. Run `cargo check` from `GEM_RUST`.
7. Update `CODEX_CONTEXT.md` or `references/strategy-d-v3.md` when behavior changes.

Never start runtime trading/test processes yourself. The user runs `cargo run -- BTC 5m` and `cargo run -- BTC 15m`; Codex reads their logs afterward. `cargo check` and `cargo fmt` are allowed.

## Current Handoff

The current Strategy D contour should be reviewed from user-generated paper-test logs. Before proposing more math, inspect fresh logs unless the user reports a concrete bug.

Target sample:

- 5m: 40-50 windows for quick behavior discovery.
- 15m: 15-25 windows; this is the priority timeframe.
- Avoid tuning the first 5-10 windows unless something is clearly broken.

First review should focus on expectancy, not winrate:

- avg PnL per window,
- median PnL,
- max drawdown,
- entry side accuracy,
- runner held-to-redeem rate,
- hedge cost vs hedge rescue value,
- tail liquidation loss,
- slippage sensitivity at +/-0.01 and +/-0.02,
- if LLM observer mode was enabled, whether it provided useful directional signal before considering re-enabling it,
- entry frequency and skipped-window reasons,
- WeakScalp rarity/usefulness and loss-size impact compared with the old `dynamic_buy_*` logs,
- whether velocity-aware SELL-grid changes improved exits,
- whether weak exits preserve cash,
- whether redeem-hold captures real 1.00 redeems without ignoring reversals.

## Coupling Audit

Before accepting a strategy change, answer:

- Does it reduce entry count too much on 15m?
- Does it increase average loss size when wrong?
- Does it accidentally sell an ITM winner that should be held for redeem?
- Does it block weak-side exits that are needed to preserve cash?
- Does it allow WeakScalp during a far/runaway PTB move?
- Does it allow WeakScalp without favorable current spot impulse?
- Does it keep redeem-hold active while spot velocity is sharply reversing?
- Does the SELL-grid adjust only softly, without overriding ATR/PTB/time decay?
- Does it make 5m behavior pathological even though 15m is priority?
- Are the emitted `reason` strings enough to diagnose the decision later?

## Validation

Use:

```bash
cargo check
```

For runtime comparisons, the user runs two terminal sessions from `GEM_RUST`:

```bash
cargo run -- BTC 5m
cargo run -- BTC 15m
```

Compare per-run:

- `lifecycle_events.csv`
- `entry_events.csv`
- `strategy_signals.csv`
- `trade_events.csv`
- `window_summary.csv`

Read logs in this order for first-pass diagnosis:

1. `window_summary.csv`
2. `entry_events.csv`
3. `strategy_signals.csv`
4. `trade_events.csv`
5. `lifecycle_events.csv`
