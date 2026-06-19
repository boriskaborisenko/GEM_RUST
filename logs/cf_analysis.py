#!/usr/bin/env python3
"""Counterfactual PnL replay over j_endgame run logs.

Reconstructs per-window PnL from trade_events.csv under alternative rules,
without touching strategy code. Paper redeem = winning-side shares * $1.

Rules evaluated:
  - flip_fix      : drop flip-hedge buys whose gap_z sign still favors the
                    primary side (current code fires on |gap_z|, ignores sign).
  - cross_gate(t) : skip a window if entry raw cross count (xcN) >= t.
  - ask_ceil(c)   : drop buy clips with fill price > c.
  - late_secs(T)  : drop buy clips placed earlier than T seconds before close.
  - edge_min(m)   : drop buy clips whose edge = Phi(dir gap_z) - ask < m.

Edge model: p_win = Phi(directional gap_z), edge = p_win - ask (fee=0 in config).
"""
import csv
import glob
import math
import os
import re
from collections import defaultdict

RUNS = sorted(glob.glob(os.path.join(os.path.dirname(__file__), "runs", "20260619_1159*_j_endgame")))

GZ_RE = re.compile(r"gap_z_(-?\d+\.\d+)")
XC_RE = re.compile(r"_xc(\d+)")
ASK_RE = re.compile(r"_ask_(\d+\.\d+)")
WINDOW_SECS = 300


def normal_cdf(x):
    """Same erf approximation the bot uses (strategy_e::normal_cdf)."""
    sign = -1.0 if x < 0 else 1.0
    x = abs(x) / (2.0 ** 0.5)
    t = 1.0 / (1.0 + 0.3275911 * x)
    a1, a2, a3, a4, a5 = (
        0.254829592, -0.284496736, 1.421413741, -1.453152027, 1.061405429,
    )
    erf = sign * (1.0 - (((((a5 * t + a4) * t) + a3) * t + a2) * t + a1) * t * math.exp(-x * x))
    return min(1.0, max(0.0, (1.0 + erf) * 0.5))


def parse_reason(reason):
    gz = GZ_RE.search(reason)
    xc = XC_RE.search(reason)
    ask = ASK_RE.search(reason)
    return (
        float(gz.group(1)) if gz else None,
        int(xc.group(1)) if xc else None,
        reason.startswith("j_flip_hedge"),
        float(ask.group(1)) if ask else None,
    )


def slug_end_epoch(slug):
    m = re.search(r"(\d+)$", slug)
    return int(m.group(1)) + WINDOW_SECS if m else None


def load_run(run_dir):
    winner = {}
    with open(os.path.join(run_dir, "window_summary.csv")) as f:
        for r in csv.DictReader(f):
            winner[int(r["window_id"])] = r["winner"]

    windows = defaultdict(lambda: {"buys": [], "entry_cross": None, "primary": None})
    path = os.path.join(run_dir, "trade_events.csv")
    if not os.path.exists(path):
        return {}
    with open(path) as f:
        for r in csv.DictReader(f):
            if r["type"] != "BUY":
                continue
            wid = int(r["window_id"])
            gz, xc, is_flip, ask = parse_reason(r["reason"])
            side = r["side"]
            end_epoch = slug_end_epoch(r["slug"])
            ts = float(r["timestamp"]) / 1000.0
            secs_to_end = (end_epoch - ts) if end_epoch else None
            price = float(r["price"])
            if ask is None:
                ask = price
            # directional z for the side we are buying (sign-aware)
            dir_z = (gz if side == "UP" else -gz) if gz is not None else None
            p_win = normal_cdf(dir_z) if dir_z is not None else None
            edge = (p_win - ask) if p_win is not None else None
            buy = {
                "side": side,
                "price": price,
                "ask": ask,
                "shares": float(r["shares"]),
                "usd": float(r["usd_value"]),
                "gz": gz,
                "is_flip": is_flip,
                "secs_to_end": secs_to_end,
                "p_win": p_win,
                "edge": edge,
            }
            w = windows[wid]
            w["buys"].append(buy)
            if not is_flip and w["primary"] is None:
                w["primary"] = side
                w["entry_cross"] = xc
                w["entry_edge"] = edge
    out = {}
    for wid, w in windows.items():
        if wid in winner:
            w["winner"] = winner[wid]
            out[wid] = w
    return out


def clip_kept(b, flip_fix, ask_ceil, late_secs, edge_min):
    if ask_ceil is not None and b["price"] > ask_ceil + 1e-9:
        return False
    if late_secs is not None and b["secs_to_end"] is not None and b["secs_to_end"] > late_secs:
        return False
    if edge_min is not None and b["edge"] is not None and b["edge"] < edge_min:
        return False
    if flip_fix and b["is_flip"]:
        gz = b["gz"]
        if gz is None:
            return False
        genuine = (b["side"] == "UP" and gz > 0) or (b["side"] == "DOWN" and gz < 0)
        if not genuine:
            return False
    return True


def window_pnl(w, flip_fix=False, cross_gate=None, ask_ceil=None, late_secs=None, edge_min=None):
    if cross_gate is not None and w["entry_cross"] is not None and w["entry_cross"] >= cross_gate:
        return 0.0, True
    spend = 0.0
    shares_by_side = defaultdict(float)
    for b in w["buys"]:
        if not clip_kept(b, flip_fix, ask_ceil, late_secs, edge_min):
            continue
        spend += b["usd"]
        shares_by_side[b["side"]] += b["shares"]
    redeem = shares_by_side.get(w["winner"], 0.0) * 1.0
    return redeem - spend, False


def kelly_pnl(w, kappa=0.25, f_max=1.0, flip_fix=True):
    """Re-allocate the SAME per-window spend across the placed primary clips,
    weighting by Kelly numerator edge/(1-ask). Isolates allocation alpha."""
    clips = [b for b in w["buys"] if not (flip_fix and b["is_flip"])]
    base_spend = sum(b["usd"] for b in clips)
    if base_spend <= 0:
        return 0.0
    weights = []
    for b in clips:
        e = b["edge"] if b["edge"] is not None else 0.0
        odds = max(1.0 - b["ask"], 1e-3)
        w_i = max(0.0, min(f_max, kappa * e / odds))
        weights.append(w_i)
    wsum = sum(weights)
    if wsum <= 0:
        return 0.0  # no positive-edge clip -> skip window
    shares_by_side = defaultdict(float)
    for b, w_i in zip(clips, weights):
        alloc = base_spend * w_i / wsum
        shares_by_side[b["side"]] += alloc / b["ask"]
    redeem = shares_by_side.get(w["winner"], 0.0)
    return redeem - base_spend


def summarize(label, runs, **rule):
    total, losses, big, skipped = 0.0, 0, 0.0, 0
    for run in runs:
        for w in run.values():
            pnl, was_skipped = window_pnl(w, **rule)
            if was_skipped:
                skipped += 1
                continue
            total += pnl
            if pnl < -1e-6:
                losses += 1
                big = min(big, pnl)
    print(f"{label:<46} total={total:8.2f}  losses={losses:3d}  worst={big:8.2f}  skip={skipped:3d}")
    return total


def edge_by_outcome(runs):
    print("\n=== edge of PRIMARY entry clip vs window outcome (flip excluded) ===")
    print("  bucket            n   win%   mean_edge   mean_pwin  mean_ask")
    buckets = {"edge<0": [], "0<=edge<.02": [], "edge>=.02": []}
    for run in runs:
        for w in run.values():
            for b in w["buys"]:
                if b["is_flip"] or b["edge"] is None:
                    continue
                won = 1.0 if b["side"] == w["winner"] else 0.0
                key = "edge<0" if b["edge"] < 0 else ("0<=edge<.02" if b["edge"] < 0.02 else "edge>=.02")
                buckets[key].append((b["edge"], b["p_win"], b["ask"], won, b["usd"]))
    for k, rows in buckets.items():
        if not rows:
            print(f"  {k:<14} 0")
            continue
        n = len(rows)
        win = sum(r[3] for r in rows) / n * 100
        me = sum(r[0] for r in rows) / n
        mp = sum(r[1] for r in rows) / n
        ma = sum(r[2] for r in rows) / n
        print(f"  {k:<14} {n:4d}  {win:5.1f}  {me:+9.3f}   {mp:8.3f}  {ma:7.3f}")


def time_buckets(runs):
    print("\n=== PRIMARY clips by seconds-to-end (flip excluded) ===")
    print("  bucket        n   side_won%   mean_edge   $spent")
    edges = [(75, 999), (60, 75), (45, 60), (30, 45), (20, 30), (0, 20)]
    agg = {b: [] for b in edges}
    for run in runs:
        for w in run.values():
            for b in w["buys"]:
                if b["is_flip"] or b["secs_to_end"] is None:
                    continue
                for lo, hi in edges:
                    if lo <= b["secs_to_end"] < hi:
                        won = 1.0 if b["side"] == w["winner"] else 0.0
                        agg[(lo, hi)].append((won, b["edge"] or 0.0, b["usd"]))
                        break
    for (lo, hi), rows in agg.items():
        label = f">{lo}s" if hi == 999 else f"{lo}-{hi}s"
        if not rows:
            print(f"  {label:<10} 0")
            continue
        n = len(rows)
        won = sum(r[0] for r in rows) / n * 100
        me = sum(r[1] for r in rows) / n
        sp = sum(r[2] for r in rows)
        print(f"  {label:<10} {n:4d}   {won:6.1f}     {me:+9.3f}  {sp:8.2f}")


def main():
    runs = [load_run(d) for d in RUNS]
    nwin = sum(len(r) for r in runs)
    print(f"runs={len(RUNS)} traded_windows={nwin}\n")

    base = summarize("baseline (as-run)", runs)
    summarize("flip_fix", runs, flip_fix=True)
    summarize("flip_fix + cross_gate(>=9)", runs, flip_fix=True, cross_gate=9)

    edge_by_outcome(runs)
    time_buckets(runs)

    print("\n--- late_secs gate (only buy within last T s), with flip_fix ---")
    for T in (60, 45, 30, 25, 20, 15):
        summarize(f"flip_fix + late<= {T}s", runs, flip_fix=True, late_secs=T)

    print("\n--- edge_min gate (drop edge<m clips), with flip_fix ---")
    for m in (0.0, 0.01, 0.02, 0.03):
        summarize(f"flip_fix + edge>= {m}", runs, flip_fix=True, edge_min=m)

    print("\n--- stacked: flip_fix + cross_gate(9) + edge>=m + late<=T ---")
    for m in (0.0, 0.02):
        for T in (45, 30):
            summarize(
                f"edge>={m} + late<={T}s + gate9",
                runs, flip_fix=True, cross_gate=9, edge_min=m, late_secs=T,
            )

    print("\n--- Kelly re-allocation at EQUAL per-window spend (flip_fix) ---")
    for kappa in (0.25, 0.5, 1.0):
        total, losses = 0.0, 0
        for run in runs:
            for w in run.values():
                pnl = kelly_pnl(w, kappa=kappa)
                total += pnl
                if pnl < -1e-6:
                    losses += 1
        print(f"  kelly(kappa={kappa:<4})                       total={total:8.2f}  losses={losses:3d}")
    print(f"\nbaseline total = {base:.2f}")

    list_losses_extra(runs)


def list_losses_extra(runs):
    print("\n=== baseline losing windows (entry timing) ===")
    rows = []
    for ri, run in enumerate(runs):
        tag = os.path.basename(RUNS[ri]).split("_j_")[0]
        for wid, w in run.items():
            pnl, sk = window_pnl(w)
            if sk or pnl >= -1e-6:
                continue
            prim = [b for b in w["buys"] if not b["is_flip"]]
            first_s = max((b["secs_to_end"] for b in prim if b["secs_to_end"]), default=None)
            ee = w.get("entry_edge")
            rows.append((pnl, tag, wid, w["winner"], w["primary"], w["entry_cross"], first_s, ee))
    rows.sort()
    for pnl, tag, wid, win, prim, xc, fs, ee in rows:
        ee_s = f"{ee:+.3f}" if ee is not None else "  n/a"
        print(f"  {pnl:8.2f} {tag:8} w{wid:<3} win={win:<4} entry={prim} xc={xc} "
              f"first_clip={fs:.0f}s_to_end entry_edge={ee_s}")


if __name__ == "__main__":
    main()
