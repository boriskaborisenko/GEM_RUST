#!/usr/bin/env python3
"""Offline: how often 5m windows had winner ask 0.94-0.98 in last 90s (from strategy_signals)."""

from __future__ import annotations

import csv
import json
import sys
from collections import defaultdict
from pathlib import Path

RUNS = Path(__file__).resolve().parents[1] / "logs" / "runs"
OUT = Path(__file__).resolve().parents[1] / "docs" / "j_endgame_frequency.json"

LAST_SECS = 90
ASK_MIN = 0.94
ASK_MAX = 0.98
GAP_Z_MIN = 0.8


def winner_side(spot: float, ptb: float) -> str:
    return "UP" if spot > ptb else "DOWN"


def winner_ask(row: dict, side: str) -> float:
    return float(row["up_ask"] if side == "UP" else row["down_ask"])


def gap_z(row: dict) -> float:
    ptb = float(row["ptb"])
    spot = float(row["spot_price"])
    atr = float(row["current_atr"])
    secs = max(int(float(row["secs_to_end"])), 1)
    expected = max(atr * (secs / 900.0) ** 0.5, 1e-6)
    return abs((spot - ptb) / expected)


def analyze_file(path: Path) -> dict:
    by_slug: dict[str, list[dict]] = defaultdict(list)
    with path.open(newline="") as f:
        for row in csv.DictReader(f):
            if "-5m-" not in row.get("slug", ""):
                continue
            by_slug[row["slug"]].append(row)

    windows = len(by_slug)
    eligible = 0
    snipe_ticks = 0
    examples = []

    for slug, rows in by_slug.items():
        late = [r for r in rows if int(float(r["secs_to_end"])) <= LAST_SECS]
        if not late:
            continue
        last = max(late, key=lambda r: float(r["timestamp"]))
        try:
            spot = float(last["spot_price"])
            ptb = float(last["ptb"])
        except (ValueError, KeyError):
            continue
        if ptb <= 0:
            continue
        side = winner_side(spot, ptb)
        ask = winner_ask(last, side)
        gz = gap_z(last)
        if ASK_MIN <= ask <= ASK_MAX and gz >= GAP_Z_MIN:
            eligible += 1
            examples.append(
                {
                    "slug": slug,
                    "winner": side,
                    "ask": round(ask, 4),
                    "gap_z": round(gz, 3),
                    "secs_to_end": int(float(last["secs_to_end"])),
                }
            )
        for r in late:
            try:
                s = float(r["spot_price"])
                p = float(r["ptb"])
                if p <= 0:
                    continue
                ws = winner_side(s, p)
                a = winner_ask(r, ws)
                if ASK_MIN <= a <= ASK_MAX and gap_z(r) >= GAP_Z_MIN:
                    snipe_ticks += 1
            except (ValueError, KeyError):
                pass

    return {
        "run_dir": path.parent.name,
        "windows_with_signals": windows,
        "windows_endgame_snipe_eligible": eligible,
        "eligible_pct": round(100.0 * eligible / windows, 1) if windows else 0.0,
        "late_snipe_ticks": snipe_ticks,
        "examples": examples[:10],
    }


def main() -> None:
    paths = sorted(RUNS.glob("*_btc_5m_*/strategy_signals.csv"))
    if not paths:
        print("No btc 5m strategy_signals.csv found", file=sys.stderr)
        sys.exit(1)

    results = [analyze_file(p) for p in paths]
    total_w = sum(r["windows_with_signals"] for r in results)
    total_e = sum(r["windows_endgame_snipe_eligible"] for r in results)

    summary = {
        "params": {
            "last_secs": LAST_SECS,
            "ask_min": ASK_MIN,
            "ask_max": ASK_MAX,
            "gap_z_min": GAP_Z_MIN,
        },
        "runs_analyzed": len(results),
        "total_windows": total_w,
        "total_eligible_windows": total_e,
        "eligible_pct": round(100.0 * total_e / total_w, 1) if total_w else 0.0,
        "per_run": results,
    }

    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    main()
