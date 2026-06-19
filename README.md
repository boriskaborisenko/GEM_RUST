# GEM_RUST — Strategy J (Endgame)

Paper-trading bot for Polymarket crypto UP/DOWN windows (BTC/ETH, 5m).

Active strategy: **`j_endgame`** — one leg on the winner, redeem @ $1, no sells.

Per-window goal: **+$1 redeem PnL** with controlled risk. Not “pick a side at the open”, but **enter late when spot and the book already point at the winner**, then scale exposure dynamically.

### Track record (paper, real windows)

After sizing + flip-hedge fixes (probe first clip, expensive-ask gate, working composite flip hedge):

| Metric | Range |
|--------|--------|
| **Windows** | **400+** (BTC/ETH 5m, live paper against real PM windows) |
| **Winrate** | **97–100%** |
| **Typical win** | +$1–6 / window |
| **Typical loss** | small vs bank — **one loss no longer wipes a session** |

Losses still happen (late PTB cross, chop). They are **rare and bounded** by probe sizing, entry gates, and flip hedge — not the old pattern of −$70 on a single wrong-side window.

---

## The idea

A 5m window is a race to PTB (price to beat). In the last ~2 minutes:

1. **Spot vs PTB** tells you who is ITM (in the money).
2. **The book** (mid-cross lead) shows where flow is leaning.
3. **Tape** (Polymarket buys) and **CEX micro** confirm direction.
4. **gap_z** normalizes “how far spot is from PTB” vs remaining time and ATR.

J does **not** use a fixed number of BUYs. It computes **composite confidence** `C ∈ [0,1]` and derives a **target exposure** (USD we want on the winner). Each SpotTick buys only the **delta** to that target. Hence emergent N buys: sometimes 3, sometimes 12 — driven by the signal, not a schedule.

**Profit:** buy the winner cheap (88–99¢), hold to $1 redeem.  
**Risk:** wrong side near PTB can still lose, but **loss size is capped** by first-clip probe, ramp rules, and flip hedge — not a full-window dump on tick one.

---

## Window timeline (5m)

```
0–8%     WARMUP      — mid-cross tracker arms, no BUYs
8–50%    MID         — wait, no BUYs
50–120s  ACCUMULATE  — composite endgame: probe → ramp clips on winner
≤25s     LATE        — legacy tier; composite dominates
≤20s     RESCUE      — profit-gap sizing; flip-hedge has priority
≤5s      FINAL SEAL  — last seconds
```

Insurance (early $1 on the underdog) is **off** (`insuranceEnabled: false`).

---

## Three engines (every SpotTick)

Planner: `src/j_controller.rs` → `plan_j_window()`.

| Priority | Engine | When | What it does |
|----------|--------|------|--------------|
| 1 | **Flip hedge** | Primary exposure + thesis broken | Buys the **opposite** side up to `flipTierUsd` |
| 2 | **Composite** | Confidence ≥ threshold | Target-exposure on the **current winner** |
| — | Insurance | `insuranceEnabled` | Off today |

Flip hedge is checked **before** composite. If spot crosses PTB against our side, hedge can fire before we add more to the loser.

---

## Composite confidence

Function: `endgame_confidence()` in `j_controller.rs`.

Weighted blend (defaults):

| Signal | Weight | Meaning |
|--------|--------|---------|
| **gap_z** | 55% | `(spot − PTB) / expected_move(ATR, secs_left)` |
| **book** | 20% | mid-cross lead on winner, chop penalty |
| **momentum** | 10% | smoothed spot velocity toward winner |
| **flow** | 15% | tape imbalance + CEX buy/sell imbalance |

**Hard veto (C = 0):**

- `|gap_z| < finalSealMinGapZ` (~0.8) — coin flip, skip
- book **firmly** leads the **opposite** side (`bookContradictGap`)

**Boost:** strong gap_z lifts C even when book/flow lag — so we can buy @88¢ before the book reprices to 99¢.

---

## Target exposure & sizing

Function: `plan_endgame_composite()`.

```
enter         = effective_conf_enter(ask, gap_z)   // lower for cheap ask / safe gap
eff           = ramp(confidence, enter, 1.0)
conf_target   = eff × maxRescueUsd                 // up to $75
profit_target = USD needed for redeem PnL ≥ targetProfitUsd
                (only when exposure already exists — not on a blank window)
target        = min(max(conf_target, profit_target), maxRescueUsd)
increment     = target − rescue_spent_usd
clip          = increment capped by effective_max_clip
```

### Clip ramp (not $35 on tick one)

| Stage | Rule |
|-------|------|
| **First BUY** | capped at `firstClipUsd` ($8) |
| **Follow-up** | ramp on gap_z + window % + cheap ask |
| **Ceiling** | `maxClipUsd` ($35) |
| **Anti-spam** | `minIncrementUsd` ($5), `minBuyIntervalMs` (3000 ms) |

### Expensive ask + weak gap

Fresh entry is **blocked** when:

- ask > `expensiveAskThreshold` (0.94) **and**
- `|gap_z| < expensiveMinGapZ` (1.35)

Protects against coin-flip entries @ 95–99¢.

---

## Flip hedge

Function: `flip_hedge_triggered()` in `strategy_j.rs`.

Arms when:

- `has_primary_exposure()` — `primary_side` set **and** USD deployed (composite → `rescue_spent_usd`, not only cheap/late clip counters)
- Spot **or** mid lead is **against** our side
- Enough evidence: significant crosses / cross count / `|gap_z| ≥ flipMinGapZ`

Taker buy on the opposite side up to **`flipTierUsd`** ($12), max ask **`flipMaxAsk`** (0.99).

Flip hedge is partial cover on reversal — it cuts tail loss; it does not need to fully offset primary exposure to keep session PnL healthy at 97%+ winrate.

---

## Data J consumes

| Source | Used for |
|--------|----------|
| Polymarket CLOB WS/REST | UP/DOWN bid/ask, book depth, paper fills |
| Chainlink spot WS | spot vs PTB, gap_z, winner |
| Bybit/Binance | ATR, CEX micro imbalance |
| Trade tape tracker | $ BUY flow on winner over `tapeWindowMs` |
| Mid-cross tracker | lead side, chop (significant crosses) |

Stale PM data → no trade. BUY intent ≠ fill: depth, budget, and gates can block.

---

## Config (`config.json` → `jEndgame`)

Key fields (current defaults):

```json
{
  "strategy": "j_endgame",
  "jEndgame": {
    "endgameSecs": 120,
    "cheapMinElapsedPct": 50.0,
    "targetProfitUsd": 1.0,
    "maxRescueUsd": 75.0,
    "maxUsdPerWindow": 80.0,
    "firstClipUsd": 8.0,
    "maxClipUsd": 35.0,
    "minIncrementUsd": 5.0,
    "minBuyIntervalMs": 3000,
    "expensiveAskThreshold": 0.94,
    "expensiveMinGapZ": 1.35,
    "confEnter": 0.58,
    "fullSizeGapZ": 1.8,
    "finalSealMinGapZ": 0.8,
    "flipHedgeEnabled": true,
    "flipTierUsd": 12.0,
    "insuranceEnabled": false,
    "takerMode": true,
    "takerMaxAsk": 0.99,
    "maxSigCrossesDirectional": 3,
    "minPtbDistPct": 0.05
  }
}
```

Full field list: `src/config.rs` (`JEndgameConfig`).

---

## Running

```bash
cd GEM_RUST
cargo build --release
cargo run --release -- BTC 5m
cargo run --release -- ETH 5m
```

`config.json` must have `"strategy": "j_endgame"`.

Session budget: clamped by `session.maxWindowBudget` — currently up to **$80/window** on a $500 bank.

---

## Logs

Each run:

```text
logs/runs/<YYYYMMDD_HHMMSS>_<asset>_<interval>_j_endgame/
```

| File | Contents |
|------|----------|
| `window_summary.csv` | PnL, winner, PTB, close spot, entry_side, mid-cross counts |
| `strategy_signals.csv` | Every BUY signal: reason, gap_z, phase, tape, executed |
| `trade_events.csv` | Executed BUY / EXPIRED / REDEEM |
| `mid_cross_events.csv` | Book lead flips |
| `lifecycle_events.csv` | promote/skip, WS events |

### Reason string (decode)

Example:

```text
j_final_seal_taker_down_fill_0.89_ask_0.90_gap_z_-1.71_phase_accumulate_pnl_proj_+1.25_tape_$466/39_xc0
```

| Part | Meaning |
|------|---------|
| `j_final_seal` | tier = composite endgame |
| `taker` | buy @ ask |
| `down` | side |
| `gap_z_-1.71` | spot below PTB, ~1.7 expected moves |
| `phase_accumulate` | window phase |
| `pnl_proj_+1.25` | projected redeem PnL if DOWN wins |
| `tape_$466/39` | $466 buy flow / 39 prints on winner in 5s |
| `xc0` | mid-cross count at signal time |

Flip hedge: `j_flip_hedge_taker_...`

---

## How to read a run

Winrate on **400+ windows** is **97–100%** — useful as a sanity check, not the only metric.

| Metric | Healthy (current J) | Warning |
|--------|---------------------|---------|
| winrate (400+ windows) | 97–100% | sustained drop below ~95% |
| avg PnL / window | ~$1–4 | ≪ $0 over 50+ windows |
| spent / window | stable, gated | unbounded ramp on weak gap |
| first clip | ~$8 (`firstClipUsd`) | $35 on first tick |
| single loss vs bank | small, survivable | loss ≈ full `maxUsdPerWindow` |
| `j_flip_hedge_*` on losses | hedge fired when thesis broke | 0 hedge on clear reversal |
| sig mid-crosses at entry | 0–2 | heavy chop ignored at entry |

Typical **win**: modest deploy, PnL +$1–6.  
Typical **loss** (rare): bounded — not the pre-fix −$70 blow-up on one window.

---

## Code map (J)

```text
src/
├── main.rs                 # runtime loop, dashboard, logging
├── j_controller.rs         # phases, confidence, composite planner, flip plan
├── strategy/strategy_j.rs  # TradeStrategy impl, flip_hedge_triggered, fills
├── config.rs               # JEndgameConfig from config.json
├── mid_cross_tracker.rs    # book lead / chop
├── trade_tape.rs           # Polymarket tape window
├── cex_micro.rs            # CEX imbalance
├── orderbook.rs            # paper taker fill simulation
├── j_fees.rs               # crypto fee model
└── trader.rs               # portfolio, redeem, CSV logs
```

---

## Known sharp edges

- **Paper ≠ live** — real CLOB depth, fees, and latency can differ; 97–100% is on paper over 400+ real-time windows.
- **Cheap-ask trap** — winner @86¢ still loses if spot crosses PTB late; gates reduce frequency, they do not eliminate it.
- **Chop filter is entry-time only** — `maxSigCrossesDirectional` uses crosses **at entry**; end-of-window panic chop can still hurt an open position (flip hedge is the backstop).
- **Flip hedge cap $12** — partial, not delta-neutral; enough to keep losses small at current winrate, not to zero them every time.
- BUY amount is **USD**, not shares. Min notional ~$1.
- `cargo test` — 70 unit tests on planner/sizing/flip.

---

## Verify

```bash
cargo fmt
cargo check
cargo test
```

Runtime paper runs are started manually; review `logs/runs/` afterward.
