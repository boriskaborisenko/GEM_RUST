//! Rolling UP/DOWN ask history for the current window (web chart).

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotChartPoint {
    pub t_ms: i64,
    pub up_ask: f64,
    pub down_ask: f64,
    pub up_bid: f64,
    pub down_bid: f64,
    pub spot: Option<f64>,
}

pub struct WindowChartTracker {
    window_number: Option<usize>,
    points: VecDeque<SnapshotChartPoint>,
    max_points: usize,
}

impl WindowChartTracker {
    pub fn new(max_points: usize) -> Self {
        Self {
            window_number: None,
            points: VecDeque::with_capacity(max_points.min(2048)),
            max_points,
        }
    }

    pub fn record(
        &mut self,
        window_number: usize,
        up_ask: f64,
        down_ask: f64,
        up_bid: f64,
        down_bid: f64,
        spot: Option<f64>,
        now_ms: i64,
    ) {
        if self.window_number != Some(window_number) {
            self.window_number = Some(window_number);
            self.points.clear();
        }

        if let Some(last) = self.points.back() {
            if last.t_ms == now_ms {
                return;
            }
        }

        self.points.push_back(SnapshotChartPoint {
            t_ms: now_ms,
            up_ask,
            down_ask,
            up_bid,
            down_bid,
            spot,
        });
        while self.points.len() > self.max_points {
            self.points.pop_front();
        }
    }

    pub fn snapshot(&self) -> Vec<SnapshotChartPoint> {
        self.points.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resets_on_window_change() {
        let mut t = WindowChartTracker::new(100);
        t.record(1, 0.5, 0.5, 0.49, 0.49, Some(100.0), 1000);
        assert_eq!(t.snapshot().len(), 1);
        t.record(2, 0.6, 0.4, 0.59, 0.39, None, 2000);
        assert_eq!(t.snapshot().len(), 1);
        assert!((t.snapshot()[0].up_ask - 0.6).abs() < 1e-9);
    }
}
