# J Endgame v5 — Concept (no code)

**Status:** design only · v4 remains implementation baseline  
**Date:** 2026-06-18  
**Verdict:** **да, задумка имеет смысл**, но только как *profit-target control loop*, а не «лить бабло вслепую». Ниже — формулировка и границы применимости.

---

## One-line idea

На протяжении окна — **мелкие динамические доливы (~$1)** по мере появления edge; в конце — **расчётный rescue-sweep** в нужную ногу, чтобы закрыть окно с **целевым профитом** (например **+$1**), а не с фиксированным числом клипов.

---

## Why this is not crazy (math sketch)

Бинарное окно Polymarket: выигравшая нога редемпится @ **$1/share** (минус fee).

Для окна с уже потраченным `spent_total` и текущим портфелем `(up_shares, down_shares)`:

```
if winner = UP:
  payout = up_shares × 1.0 − redeem_fees
else:
  payout = down_shares × 1.0 − redeem_fees

window_pnl = payout − spent_total
```

**Цель v5:** к экспирации `window_pnl ≥ target_profit` (default **$1**).

Если за `T` секунд до конца видим, что `window_pnl < target` при текущем winner (spot vs PTB + tape/mid):

```
need_payout � target_profit − window_pnl + fee_buffer
rescue_usd  ≈ need_payout × winner_ask / (1 − effective_fee)
rescue_usd  = min(rescue_usd, cash_left, max_rescue_per_window)
```

→ **«бешеный долив»** — не эмоция, а *минимальный USD в winner leg*, чтобы дотянуть до +$1.  
Если при текущем ask rescue дороже `max_rescue` или банк не тянет — **не спасаем**, фиксируем controlled loss (v5 must say NO).

---

## Operating modes (3 phases, all dynamic)

| Phase | When | Size | Logic |
|-------|------|------|--------|
| **Probe** | Whole window (after warmup), when edge exists | ~**$1**/clip, rate-limited | Докуп winner (or cheap side) only if `edge_per_$ > ε` and PTB/gap/tape gates pass |
| **Accumulate** | 2nd half + endgame zone | $1–$3 scaled by **confidence** | Больше size при высоком `\|gap_z\|`, hot tape, stable mid — меньше при шуме |
| **Rescue** | Last **10–25s** | **Computed sweep** (not fixed clips) | Solve for `rescue_usd` to hit `target_profit`; optional hedge on flip |

**v4 → v5 shift:** budgets не «3 cheap + 4 late», а **continuous controller** с одной целью на окно.

---

## Dynamic knobs (everything derived, nothing sacred)

- `clip_usd(t)` — base $1, scales up with edge (cap e.g. $3)
- `max_usd_per_window` — hard ceiling (e.g. $21–$30 on $100 bank)
- `target_profit_usd` — **$1 default** (config per asset/session)
- `max_rescue_usd` — cap end dump (e.g. 60% of window budget)
- `min_edge_after_fees` — skip probe if buy@ask can't ever contribute to +$1
- Flip hedge (from v4) — special case of rescue when **primary leg wrong** and chaos detected

---

## What v5 fixes vs v4 pain (from logs)

| v4 problem | v5 response |
|------------|-------------|
| Fixed tiers / clip counts | Size & timing from **PnL gap to target** |
| Early impulse on weak gap | Probes only if edge + elapsed time gates |
| Late add on tiny PTB gap | Rescue math includes **current mtm** — won't chase if can't reach +$1 |
| Flip with no hedge | Rescue explicitly buys **winner leg** to close at target |

---

## When v5 makes sense ✅

1. **Profit-target framing** matches how we evaluate windows (`window_summary.pnl` vs +$1).
2. **Early $1 probes** are cheap optionality if entry ≤88¢ and winner stable.
3. **End rescue is bounded optimization** — same family as paired-floor / redeem math we already use.
4. Bank **$100**, target **$1/window** → 1% ROI/window is realistic *if* win-rate on rescue decisions high enough.
5. Works best on **5m BTC/ETH** with liquid books and Chainlink PTB.

---

## When v5 does NOT make sense ❌

1. **Rescue at 97–99¢ after large wrong-leg spend** — need ~1:1 extra capital to break even; +$1 may be impossible within `max_rescue`. Must abort.
2. **Both legs accumulated without arb** — can lock negative floor; v5 must track **paired vs directional** exposure.
3. **Low bank / high target** — target $1 with $5 window budget and 3 bad probes leaves no rescue room.
4. **Illiquid alt books (SOL/XRP/DOGE)** — rescue slippage breaks the formula; per-asset `target_profit` lower or disabled.
5. **Chasing target when winner unclear** (spot ≈ PTB, cross storm) — rescue becomes gamble, not control.

**Rule:** if `rescue_usd > max_rescue` OR `rescue_usd > cash` OR `expected_pnl_after_rescue < target − tolerance` → **do not rescue**.

---

## Decision flow (conceptual)

```
each tick:
  update window_state(spent, shares, mtm, winner, gap_z, tape, mid_cross)

  if secs_to_end > rescue_zone:
    if probe_allowed(edge, gates):
      buy ~$1 on winner (or value side)
  else:
    pnl_if_hold = mark_to_redeem(window_state, predicted_winner)
    if pnl_if_hold >= target_profit:
      HOLD (no more buys)
    elif flip_detected:
      rescue_side = new_winner
      rescue_usd = solve_for_target(target_profit, rescue_side_ask)
      SWEEP rescue_side @ ask up to rescue_usd
    else:
      rescue_usd = solve_for_target(target_profit, winner_ask)
      if rescue_usd affordable: SWEEP
      else: accept best-effort exit (log j_rescue_impossible)
```

---

## Config sketch (future `jEndgameV5` or extend `jEndgame`)

| Key | Example | Meaning |
|-----|---------|---------|
| `targetProfitUsd` | 1.0 | Window close goal |
| `probeClipUsd` | 1.0 | Base probe size |
| `maxClipUsd` | 3.0 | Scale cap |
| `maxUsdPerWindow` | 21.0 | Hard budget |
| `maxRescueUsd` | 12.0 | Max end dump |
| `maxRescuePctOfBudget` | 0.6 | Rescue ≤60% of window budget |
| `rescueZoneSecs` | 20 | Start solve-for-target |
| `minEdgeBps` | 50 | Min edge after fees to probe |
| `abortRescueIfAskAbove` | 0.97 | Don't rescue into 97¢+ if math fails |

---

## Implementation path (after v4 paper validates)

1. **PnL solver module** — given `(shares, spent, ask, fees)` → `usd_needed_for_target`.
2. **Window controller** — replaces tier planner; v4 tiers become *fallback presets*.
3. **Paper gate** — log `j_rescue_solve`, `j_rescue_execute`, `j_rescue_skip_impossible`; KPI: `% windows ≥ target`, not clip count.
4. **Per-asset profiles** — BTC/ETH full v5; alts conservative targets.

---

## Relation to v4

- **v4** = discrete tiers (value / late / flip hedge) — good for first paper, interpretable logs.
- **v5** = same instincts, **unified objective**: `close window at +$target`.
- v4 flip hedge ≈ v5 rescue special case.
- Do **not** implement v5 until v4 paper shows: PTB correct, $3 clips, hedge fires on flips.

---

## Open questions (TBD in paper)

- Target $1 fixed or `bank × 1%`?
- Probe on loser leg ever (true hedge) or winner-only?
- Rescue once per window or iterative each tick in last 20s?
- Include **sell** (scalp loser) in solver or buy-only?

---

*Concept only. No code until v4 overnight logs reviewed.*
