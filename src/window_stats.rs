use crate::client::get_now_ms;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

#[derive(Debug, Clone, Default)]
pub struct WindowCloseRecord {
    pub window_number: usize,
    pub slug: String,
    pub strategy_name: String,
    pub pnl: f64,
    pub spent: f64,
    pub final_atr: f64,
    pub time_pct_at_close: f64,
    pub final_gap_z: Option<f64>,
    pub mid_cross_count: u32,
    pub significant_mid_cross_count: u32,
    pub entry_side: String,
    pub entry_reason: String,
    pub would_redeem_hold: bool,
    pub winner: String,
    pub utc_hour: u32,
}

#[derive(Debug, Clone, Default)]
struct BucketStats {
    count: u32,
    wins: u32,
    total_pnl: f64,
}

#[derive(Debug, Clone, Default)]
pub struct WindowStatsAggregator {
    by_atr_regime: HashMap<String, BucketStats>,
    by_mid_cross_bucket: HashMap<String, BucketStats>,
    by_strategy: HashMap<String, BucketStats>,
    redeem_hold_windows: u32,
    total_closed: u32,
}

impl WindowStatsAggregator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_close(&mut self, record: &WindowCloseRecord) {
        if record.spent <= 0.0 {
            return;
        }
        self.total_closed += 1;

        let atr_bucket = atr_regime_bucket(record.final_atr);
        let cross_bucket = mid_cross_bucket(record.mid_cross_count);
        let won = record.pnl > 0.0;

        update_bucket(
            self.by_atr_regime.entry(atr_bucket).or_default(),
            won,
            record.pnl,
        );
        update_bucket(
            self.by_mid_cross_bucket
                .entry(cross_bucket)
                .or_default(),
            won,
            record.pnl,
        );
        update_bucket(
            self.by_strategy
                .entry(record.strategy_name.clone())
                .or_default(),
            won,
            record.pnl,
        );

        if record.would_redeem_hold {
            self.redeem_hold_windows += 1;
        }
    }

    pub fn session_summary_line(&self) -> String {
        let dcross = self.by_strategy.get("dynamic_grid_dcross");
        let (wr, avg_pnl) = match dcross {
            Some(b) if b.count > 0 => (
                (b.wins as f64 / b.count as f64) * 100.0,
                b.total_pnl / b.count as f64,
            ),
            _ => (0.0, 0.0),
        };
        format!(
            "Session stats: {} closed | D-CROSS WR {:.1}% avgPnL {:+.2} | redeem-hold windows {}",
            self.total_closed, wr, avg_pnl, self.redeem_hold_windows
        )
    }

    pub fn flush_to_csv(&self, log_dir: &str) {
        let path = Path::new(log_dir).join("session_stats.csv");
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut file = match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(f) => f,
            Err(_) => return,
        };

        if file.metadata().map(|m| m.len() == 0).unwrap_or(true) {
            let _ = writeln!(
                file,
                "timestamp,bucket_type,bucket_key,count,wins,win_rate,total_pnl,avg_pnl"
            );
        }

        let ts = get_now_ms();
        for (bucket_type, map) in [
            ("atr_regime", &self.by_atr_regime),
            ("mid_cross", &self.by_mid_cross_bucket),
            ("strategy", &self.by_strategy),
        ] {
            for (key, stats) in map {
                if stats.count == 0 {
                    continue;
                }
                let wr = stats.wins as f64 / stats.count as f64;
                let avg = stats.total_pnl / stats.count as f64;
                let _ = writeln!(
                    file,
                    "{},{},{},{},{},{:.4},{:.4},{:.4}",
                    ts, bucket_type, key, stats.count, stats.wins, wr, stats.total_pnl, avg
                );
            }
        }
    }
}

fn update_bucket(bucket: &mut BucketStats, won: bool, pnl: f64) {
    bucket.count += 1;
    if won {
        bucket.wins += 1;
    }
    bucket.total_pnl += pnl;
}

fn atr_regime_bucket(atr: f64) -> String {
    if atr < 20.0 {
        "calm".to_string()
    } else if atr < 45.0 {
        "normal".to_string()
    } else if atr < 90.0 {
        "volatile".to_string()
    } else {
        "storm".to_string()
    }
}

fn mid_cross_bucket(count: u32) -> String {
    if count == 0 {
        "0".to_string()
    } else if count <= 2 {
        "1-2".to_string()
    } else {
        "3+".to_string()
    }
}
