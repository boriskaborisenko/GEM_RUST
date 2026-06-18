# J Endgame v2 — tape sweep

## What it does (like the green pluses on Polymarket)

1. **Last 120s** of 5m window
2. **Winner** from spot vs PTB + `|gap_z| ≥ 0.6`
3. **Tape gate**: ≥ **$5** BUY flow on winner in last **5s** (the bot pluses)
4. **Taker sweep**: up to **5× $1 clips per tick** @ ask (max **99¢**), max **$15/window**
5. Reacts on **every WS trade print** + book tick (not only 1Hz monitor)

## Run

```bash
cargo build --release
cp config.j_endgame.json config.json   # or set strategy manually
cargo run --release -- BTC 5m
```

Dashboard shows: `J tape HOT`, ask depth, clips filled.

## Config (`jEndgame`)

| Key | Default | Meaning |
|-----|---------|---------|
| `tapeWindowMs` | 5000 | Rolling tape window |
| `minTapeUsd` | 5 | Min $ BUY on winner to join sweep |
| `minTapeBuys` | 2 | Min buy prints in window |
| `takerMode` | true | Buy @ ask (not passive limit) |
| `takerMaxAsk` | 0.99 | Max pay price |
| `sweepClipsPerTick` | 5 | Clips per strategy tick |
| `requireTape` | true | Wait for bot flow before entry |

## KPI

Same as v1: `win_rate_clips > 90%`, `avg_net_per_clip > $0.005` after fees.

## H baseline

Still recommended: 48h `cheap_hold_h` before live J paper (see prior sections).
