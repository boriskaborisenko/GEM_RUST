# Strategy J log audit: 100+ window runs

Date: 2026-07-01

Scope:
- Primary request: last-week BTC/ETH `j_endgame` runs with more than 100 windows.
- Extra safety check: all available `j_endgame` runs with more than 100 windows, because the best reference run is just outside the strict 7-day window.

## Strict Last-Week 100+ Window Runs

| Run | Windows | Traded | Closed | W/L | PnL | Spent | ROI | Entry | Avg Spent | Max Loss | BUYs |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `20260630_165545_btc_5m_j_endgame` | 179 | 52 | 52 | 49/3 | -32.46 | 1194.97 | -2.7% | 29.1% | 22.98 | -35.00 | 149 |

## All Available 100+ Window Runs

| Run | Windows | Traded | Closed | W/L | PnL | Spent | ROI | Entry | Avg Spent | Max Loss | BUYs |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `20260623_070842_btc_5m_j_endgame` | 1320 | 193 | 193 | 184/9 | +82.57 | 581.30 | +14.2% | 14.6% | 3.01 | -12.25 | 284 |
| `20260622_112333_btc_5m_j_endgame` | 234 | 64 | 64 | 60/4 | -6.67 | 135.98 | -4.9% | 27.4% | 2.12 | -9.55 | 80 |
| `20260621_204356_eth_5m_j_endgame` | 128 | 26 | 26 | 25/1 | -6.02 | 54.87 | -11.0% | 20.3% | 2.11 | -8.60 | 29 |
| `20260621_203319_btc_5m_j_endgame` | 130 | 31 | 31 | 29/2 | -1.96 | 93.65 | -2.1% | 23.8% | 3.02 | -11.00 | 45 |
| `20260621_113540_btc_5m_j_endgame` | 108 | 0 | 0 | 0/0 | +0.00 | 0.00 | 0.0% | 0.0% | 0.00 | +0.00 | 0 |
| `20260621_113538_eth_5m_j_endgame` | 108 | 0 | 0 | 0/0 | +0.00 | 0.00 | 0.0% | 0.0% | 0.0% | +0.00 | 0 |

## Best Reference Run

Best usable run: `20260623_070842_btc_5m_j_endgame`.

Why:
- Large sample: 1320 windows.
- Positive result: +82.57 PnL.
- Good ROI: +14.2% on 581.30 spent.
- High but not fake winrate: 184W / 9L.
- Low average risk: 3.01 spent per closed window.
- Trade-size distribution: min 1.00, p50 1.60, p75 2.00, p90 3.13, max 7.00.

Important loss profile in this run:
- Worst loss: -12.25 on 12.25 spent.
- Most losses were small or partially hedged/rescued.
- The system made money because winners could be frequent while losers were not allowed to become very large.

## Current Problem Run

Problem run: `20260630_165545_btc_5m_j_endgame`.

Headline:
- 49W / 3L looks strong.
- Total PnL is negative because the tail cost is too high.
- Average spent per closed window is 22.98 vs 3.01 in the best reference run.

Losses:
- Window #146: -35.00, spent 35.00, wrong UP, expired zero.
- Window #63: -25.00, spent 25.00, wrong DOWN, expired zero.
- Window #77: -3.09, spent 3.09, wrong DOWN, expired zero.

Top wins:
- Best win was +4.61 on 35.00 spent.
- Several wins spent 25-35 to earn roughly +1 to +4.6.

Conclusion:
- Current sizing creates poor asymmetry: a few wrong windows erase many correct windows.
- The entry logic is not the main issue. Winrate remained high.
- Parameter target should move toward the 20260623 profile: smaller first clips, smaller expensive-tail exposure, lower per-window cap, but no global entry tightening.

## Parameter Direction

Use the 20260623 run as sizing reference:
- Keep entries alive.
- Restore cheap/probe-like sizing for first entries.
- Cap expensive ask zones much harder.
- Allow multiple small buys instead of a few large buys.
- Keep hard caps for large bankrolls.

## Applied Parameter Mapping

Important nuance: the 20260623 run is the behavior reference, not a raw
percentage reference. The current config keeps the same gate shape and scales the
effective USD exposure by about 6.25x so the average traded window moves from
about 3.01 spent to roughly 18-20 spent.

| Parameter family | 20260623 effective | Current target at bank 100 | Note |
|---|---:|---:|---|
| First clip | 1.60 | 10.00 | Earlier probe, but meaningful size |
| Max clip | 7.00 | 43.75 | Only reachable on stronger/cheaper confirmations |
| Discount reload clip | 2.00 | 12.50 | Main way to add size at better prices |
| Discount reload max | 6.00 | 37.50 | Caps cheap reloads |
| Tail cap ask <= 0.97 | 3.00 | 18.75 | Expensive tail stays capped |
| Tail cap ask <= 0.94 | 6.50 | 40.625 | Medium-expensive cap |
| Tail cap ask <= 0.88 | 11.00 | 68.75 | Good-price zone gets room |
| Tail cap ask <= 0.70 | 15.00 | 93.75 | Deep discount / strong value zone |
| Window cap | 16.00 | 100.00 | Scaled from old effective cap |

This intentionally does not loosen entry gates beyond the 20260623 profile. The
main change is size: spend more when the old profile would already trade, while
avoiding the recent failure mode where 0.94-0.97 entries ballooned without enough
price edge.

## Logic Repair From Reference Run

The 20260623 behavior was not only config. The old controller used a
profit-gap add-on: after a primary entry, if the same side became cheaper, it
calculated how many dollars were needed at the current ask to bring the whole
window back to `targetProfitUsd`. That is why the run could buy several times at
better prices instead of only opening once at 0.96-0.99.

Current repair:
- restored the same-side profit-gap add-on in `plan_endgame_composite`;
- included `cash_returned` in the projected redeem PnL calculation;
- disabled chop exposure caps for value prices (`ask <= cheapMaxAsk`) so tasty
  prices are not skipped just because crosses are noisy;
- kept expensive-tail caps and the early expensive probe limiter, so 0.94-0.99
  does not balloon the way it did in the latest bad run.
