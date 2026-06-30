# J Live/Dry Repair Plan

## Current Findings

- Live CLOB execution is basically alive: POLY_1271 auth works, FOK BUYs reach CLOB, fills and rejects are visible in `strategy_signals.csv`.
- The loss window `btc-updown-5m-1782766800` bought UP twice, then tried a late DOWN hedge. Four DOWN FOK attempts were killed before one fill. Final winner was DOWN, so the hedge reduced but could not remove the tail.
- Local live accounting is wrong/incomplete: `strategy_signals.csv` shows `executed=true`, but `spent`, `up_shares`, `down_shares` remain zero and `trade_events.csv` is missing.
- Polymarket UI/chain transfer math is broadly consistent: visible transfers imply a balance near the UI balance after rounding/missing hidden rows.

## Work Order

1. Fix live fill accounting first.
   - On every `executed=true`, local portfolio must record a BUY/SELL trade.
   - If SDK fill amounts are unusable, fallback to the strategy intent/signaled USD and price.
   - Log failures explicitly instead of silently dropping them.

2. Make live/dry terminal balance honest.
   - Live/dry starts from real CLOB balance, not `session.startingBank`.
   - At each promoted/current window, refresh CLOB balance.
   - Trade cash is capped by real CLOB cash.
   - Pending auto-redeem is shown separately and is not counted as spendable cash.

3. Verify sizing uses live bank.
   - `startingBank` remains for paper only.
   - Live and live dry use latest CLOB balance as the bank base.
   - Min order remains `$1`.

4. Add minimal SQLite journal for live/dry.
   - Persist run id, CLOB balance snapshots, live order attempts/fills/rejects.
   - Keep CSVs; SQLite is an audit layer, not a replacement.

5. Validate.
   - `cargo fmt`
   - `cargo check`
   - live executor tests
   - a short `--live --dry-run` smoke after code compiles.
