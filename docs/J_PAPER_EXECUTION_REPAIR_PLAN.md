# Strategy J Paper Execution Repair Plan

Status: implemented
Scope: Strategy J signal/execution architecture in `GEM_RUST` paper runtime only.
Date: 2026-06-21

## Goal

Make Strategy J execution semantics explicit and testable before any live CLOB work.

The strategy must emit clear intent:

- operation: `BUY` or `SELL`
- order type: `Market` or `Limit`
- amount semantics:
  - `BUY`: amount is USD budget
  - `SELL`: amount is shares

Current live CLOB execution is out of scope. This repair only hardens the paper path that consumes real market data.

## Invariants

- Strategy J internal state changes only after a confirmed paper fill.
- A generated signal is not automatically a fill.
- Paper execution must respect `Market` vs `Limit` intent.
- J paper executor must handle both `BUY` and `SELL`.
- Polymarket minimum notional is modeled as `$1` for J paper orders.
- Future live executor can reuse the same `OrderSignal` operation/order-type semantics, but no live submit path is changed here.

## Work Plan

- [x] Write this plan before code changes.
- [x] Add explicit operation semantics around `OrderSignal` (`BUY` / `SELL`) without rewriting non-J strategies.
- [x] Add a dedicated J paper executor that handles:
  - [x] `BUY + Market`
  - [x] `BUY + Limit`
  - [x] `SELL + Market`
  - [x] `SELL + Limit`
- [x] Route `j_*` signals through the J paper executor.
- [x] Keep non-J strategy execution behavior unchanged.
- [x] Update Strategy J signal construction to use explicit BUY/SELL constructors.
- [x] Add targeted tests for J paper Market/Limit behavior.
- [x] Run `cargo test -q`.
- [x] Mark this plan complete with validation result.

## Validation

- `cargo test -q` — 90 passed, 1 ignored on 2026-06-21 after the audit P0 repair pass.

## Later, Not Now

- Real CLOB live executor.
- Resting limit order tracking.
- Partial live fills and cancel/replace.
- Live redeem worker.
- Replacing legacy non-J `is_buy` signal construction.
