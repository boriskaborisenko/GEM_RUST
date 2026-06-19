# J Endgame — Log Analysis (2026-06-18)

**Runs analyzed:** 17 sessions, 35 entered windows, 256 BUY events  
**Bank context:** $100 start, mixed eras ($1 clips → $3 → v4 tiers)

---

## Headline numbers

| Metric | Value |
|--------|-------|
| Total PnL | **-$37.24** |
| Win rate (pnl > 0) | 80% (28/35) — **misleading** |
| Avg PnL / window | -$1.06 |
| Windows ≥ +$1 target | **13/35 (37%)** |
| Disasters (≤ -$5) | **7 windows (-$62 total)** |
| Flip hedge BUYs | **0** (in entire dataset) |

**Conclusion:** стратегия зарабатывает часто мало и редко теряет много. Одно bad window ≈ 10 good ones.

---

## By era

| Era | Windows | Total PnL | Avg | WR |
|-----|---------|-----------|-----|-----|
| pre_fix ($1 clips) | 24 | **+$1.04** | +$0.04 | 88% |
| post_$3 pre-v4 (impulse) | 4 | -$21.63 | -$5.41 | 50% |
| v4 (value/late/flip) | 7 | -$16.65 | -$2.38 | 71% |

$3 clips сами по себе не убили — **impulse + wrong-side + no rescue** убили.

---

## The real killer: chaos windows

| | sig_mid_cross < 4 | sig_mid_cross ≥ 4 |
|--|-------------------|-------------------|
| Windows | 22 | 13 |
| Total PnL | **+$21.40** | **-$58.64** |
| Avg | +$0.97 | **-$4.51** |

Entry side ≠ final winner: **7/35** windows → **-$62.10** combined.

**Rule candidate:** если `significant_mid_cross ≥ 4` до endgame — не наращивать directional, только hedge/rescue или skip.

---

## Tier performance (first entry reason)

| Tier | N | Total PnL | Avg |
|------|---|-----------|-----|
| late | 16 | -$16.29 | -$1.02 |
| impulse | 10 | -$20.61 | -$2.06 |
| cheap (legacy) | 7 | +$7.20 | +$1.03 |
| value (v4) | 2 | -$7.53 | -$3.77 |

- **Impulse** — net negative, disabled in v4 ✓  
- **Legacy cheap** — единственный стабильно зелёный tier в старых логах  
- **Value v4** — мало данных (2 окна), оба проблемные при flip в последние секунды  

---

## Latest session (231203 ETH + 231205 BTC)

### BTC $100 → $93.90

| Window | PnL | Spent | Entry | Winner | Notes |
|--------|-----|-------|-------|--------|-------|
| #2 | +$0.24 | $12 | UP | UP | late @ 98¢ ✓ |
| #3 | **+$1.47** | $21 | UP | UP | value 87¢ + late 99¢ ✓ **hits target** |
| #7 | **-$9.00** | $9 | DOWN | UP | value DOWN @ 88¢, 33 crosses in last 6s |
| #8 | +$1.19 | $12 | UP | UP | late @ 91¢ ✓ |

### ETH

| Window | PnL | Notes |
|--------|-----|-------|
| #4 | +$0.12 | late 99¢ |
| #5 | +$1.33 | late 90¢ ✓ |
| #7 | **-$12.00** | late DOWN @ 96¢, winner UP, xc=10/5 |

---

## Case study: BTC window #7 (why -$9)

**Timeline from `mid_cross_events.csv`:**

1. **8% elapsed (armed_init):** UP underdog **28.5¢**, DOWN leader 71.5¢, spot **$28 below PTB**
2. **66% elapsed:** bot buys **3× DOWN @ 87¢** (value tier, gap_z -1.07) — primary = DOWN
3. **98% elapsed (last 6 seconds):** **33 mid-crosses**, spot still below PTB until final seconds
4. **Close:** spot **above PTB** → UP wins → **$9 on DOWN = total loss**

**Flip hedge:** 0 trades — either not deployed in this build, or spot crossed UP only in final ~4s (after last tick).

**Insurance counterfactual:** 2×$1 on UP @ ~28¢ at 8% → ~7 shares @ $1 redeem ≈ **+$5 gross** vs actual -$9. **Insurance idea validated on this window.**

---

## Flip hedge: CRITICAL GAP

**Zero `j_flip_hedge_*` in all logs.**

Expected on: 225154 BTC w#2, 231205 BTC w#7, 231203 ETH w#7.

**Action:** verify binary rebuilt after v4, add `j_flip_hedge_armed` / `j_flip_hedge_skip` signal logs, test on replay.

---

## Insurance @ 25¢ (early probe)

| Signal | Count |
|--------|-------|
| armed_init total | 92 |
| First 30% of window | 77 |
| Underdog mid ≤ 28¢ | 9 |
| Underdog mid ≤ 35¢ | 24 |
| PTB delta logged at armed_init | **0/92** ← logging gap |

Cannot run full PTB-dist gate offline. Visual: **w#7 had UP @ 28¢ at 8%** — exactly the insurance scenario.

**Defer decision** until `mid_cross_events` logs `ptb_delta_pct` at armed_init + counterfactual script on 10+ windows.

---

## What to do (priority)

### P0 — Before next paper night

1. **Fix / verify flip hedge** — must fire when primary ≠ winner + chaos; log every skip reason  
2. **Chaos circuit breaker** — no new value/late if `sig_mid_cross ≥ 3` and PTB dist shrinking  
3. **Log PTB at armed_init** — unlock insurance backtest  

### P1 — v4 tuning (keep structure)

4. Value tier: require `elapsed ≥ 50%` **and** `|gap_z| ≥ 1.0` **and** tape on winner side (not loser)  
5. Late tier: raise `lateMinGapZ` to 0.9+ unless `secs_to_end ≤ 10`  
6. Cap value at 3 clips — OK; don't buy loser leg when underdog mid < 35¢ (that's insurance job)  

### P2 — v5 (after P0 validated)

7. Profit-target rescue solver (+$1/window)  
8. Insurance bucket (2×$1 @ ≤28¢, first 30%, small PTB dist) — **promising on w#7, need N≥10**  
9. Dynamic $1 probes full window  

### Stop doing

- Impulse tier (confirmed -EV in logs)  
- Buying winner @ 96¢+ when crosses already ≥ 5  
- Trusting 80% win rate without max-loss cap  

---

## KPI for next paper run (success criteria)

- [ ] ≥ 1 flip_hedge trade logged (proves protection works)  
- [ ] Zero windows ≤ -$9 without hedge attempt  
- [ ] ≥ 50% windows ≥ +$1 (up from 37%)  
- [ ] Chaos windows (sig_xc≥4) avg PnL > -$2  
- [ ] Session PnL > $0 on 4h BTC+ETH paper  

---

*Generated from `logs/runs/*_j_endgame/`. See also `J_V5_CONCEPT.md`.*
