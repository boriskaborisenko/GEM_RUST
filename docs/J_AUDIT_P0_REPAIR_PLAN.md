# Strategy J Audit P0 Repair Plan

Status: implemented
Date: 2026-06-21
Scope: Strategy J runtime, paper execution, and planner behavior only.

## Must Fix

- [x] Collapse J decisions to one runtime contour.
- [x] Ensure every J decision goes through maintenance and CLOB freshness gates.
- [x] Add CLOB stale-data guard.
- [x] Remove book/top inconsistency after top-only WS updates.
- [x] Make CEX micro asset-specific instead of BTC-only.
- [x] Normalize recovered J budget after SELL without corrupting realized PnL accounting.
- [x] Move sell-rescue out of the early return path so it cannot starve hedge/recovery.
- [x] Stop global window budget / clip caps from choking defensive recovery tiers.
- [x] Validate with `cargo test -q`.

## Validation

- `cargo test -q` — 90 passed, 1 ignored on 2026-06-21.

## Non-Goals

- No real CLOB live submit path.
- No changes to non-J strategy behavior unless required for shared types.
- Do not reduce `WindowState.spent` after SELL; it is the gross cost basis used for realized PnL. J must use net risk (`spent - cash_returned`) for recovery budget instead.
