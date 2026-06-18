//! Rolling CLOB trade tape for J endgame flow detection (the green/red pluses).

use std::collections::{HashMap, VecDeque};

#[derive(Debug, Clone, Copy)]
struct TapePrint {
    ts_ms: i64,
    usd: f64,
}

#[derive(Debug, Clone, Default)]
struct SideTape {
    prints: VecDeque<TapePrint>,
}

#[derive(Debug, Clone, Default)]
struct WindowTape {
    up: SideTape,
    down: SideTape,
}

#[derive(Debug, Clone, Default)]
pub struct TradeTapeSnapshot {
    pub up_buy_usd: f64,
    pub up_buy_count: u32,
    pub down_buy_usd: f64,
    pub down_buy_count: u32,
    pub window_ms: i64,
}

#[derive(Debug, Default)]
pub struct TradeTapeTracker {
    windows: HashMap<usize, WindowTape>,
}

impl TradeTapeTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_buy(
        &mut self,
        window_number: usize,
        side: &str,
        usd: f64,
        ts_ms: i64,
        window_ms: i64,
    ) {
        if usd <= 0.0 {
            return;
        }
        let state = self.windows.entry(window_number).or_default();
        let tape = if side == "UP" {
            &mut state.up
        } else {
            &mut state.down
        };
        tape.prints.push_back(TapePrint { ts_ms, usd });
        Self::prune(tape, ts_ms, window_ms);
    }

    fn prune(tape: &mut SideTape, now_ms: i64, window_ms: i64) {
        let cutoff = now_ms - window_ms;
        while tape
            .prints
            .front()
            .is_some_and(|p| p.ts_ms < cutoff)
        {
            tape.prints.pop_front();
        }
    }

    pub fn snapshot(&self, window_number: usize, now_ms: i64, window_ms: i64) -> TradeTapeSnapshot {
        let empty = WindowTape::default();
        let state = self.windows.get(&window_number).unwrap_or(&empty);
        let mut snap = TradeTapeSnapshot {
            window_ms,
            ..Default::default()
        };

        for p in state.up.prints.iter() {
            if p.ts_ms >= now_ms - window_ms {
                snap.up_buy_usd += p.usd;
                snap.up_buy_count += 1;
            }
        }
        for p in state.down.prints.iter() {
            if p.ts_ms >= now_ms - window_ms {
                snap.down_buy_usd += p.usd;
                snap.down_buy_count += 1;
            }
        }
        snap
    }

    pub fn winner_stats(snap: &TradeTapeSnapshot, winner_side: &str) -> (f64, u32) {
        if winner_side == "UP" {
            (snap.up_buy_usd, snap.up_buy_count)
        } else {
            (snap.down_buy_usd, snap.down_buy_count)
        }
    }

    pub fn clear_window(&mut self, window_number: usize) {
        self.windows.remove(&window_number);
    }
}

pub fn infer_trade_usd(price: f64, size_shares: Option<f64>, min_usd: f64) -> f64 {
    if price <= 0.0 {
        return 0.0;
    }
    let shares = size_shares.unwrap_or(min_usd / price);
    (price * shares).max(min_usd)
}

pub fn is_aggressive_buy(price: f64, bid: f64, ask: f64) -> bool {
    if price <= 0.0 {
        return false;
    }
    if ask > 0.0 && price >= ask - 0.005 {
        return true;
    }
    if bid > 0.0 && ask > 0.0 {
        return (ask - price).abs() <= (price - bid).abs();
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tape_rolls_window() {
        let mut t = TradeTapeTracker::new();
        t.record_buy(1, "UP", 5.0, 1000, 5000);
        t.record_buy(1, "UP", 7.0, 4000, 5000);
        t.record_buy(1, "UP", 3.0, 7000, 5000);
        let snap = t.snapshot(1, 7000, 5000);
        assert_eq!(snap.up_buy_count, 2);
        assert!((snap.up_buy_usd - 10.0).abs() < 1e-6);
    }
}
