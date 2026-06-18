use crate::client::{MarketWindow, PricesState};
use crate::strategy::SpotSignalSnapshot;
use std::collections::HashMap;

pub const MID_CROSS_ARM_TIME_PCT: f64 = 8.0;
pub const MID_CROSS_SIGNIFICANT_GAP: f64 = 0.20;
pub const MID_CROSS_MIN_FLIP_GAP: f64 = 0.01;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeadSide {
    Up,
    Down,
    Tie,
}

impl LeadSide {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Up => "UP",
            Self::Down => "DOWN",
            Self::Tie => "TIE",
        }
    }

    fn from_mids(up_mid: f64, down_mid: f64) -> Self {
        let gap = (up_mid - down_mid).abs();
        if gap < MID_CROSS_MIN_FLIP_GAP {
            Self::Tie
        } else if up_mid > down_mid {
            Self::Up
        } else {
            Self::Down
        }
    }
}

#[derive(Debug, Clone)]
pub struct MidCrossEvent {
    pub event: &'static str,
    pub from_side: Option<LeadSide>,
    pub to_side: LeadSide,
    pub up_mid: f64,
    pub down_mid: f64,
    pub lead_gap: f64,
    pub peak_prev_gap: f64,
    pub is_significant: bool,
    pub cross_count: u32,
    pub significant_cross_count: u32,
    pub time_pct: f64,
    pub secs_to_end: i64,
    pub current_atr: f64,
}

#[derive(Debug, Clone, Default)]
pub struct MidCrossSnapshot {
    pub armed: bool,
    pub current_side: Option<LeadSide>,
    pub lead_gap: f64,
    pub up_mid: f64,
    pub down_mid: f64,
    pub cross_count: u32,
    pub significant_cross_count: u32,
    pub peak_lead_gap: f64,
    pub last_cross_from: Option<LeadSide>,
    pub last_cross_to: Option<LeadSide>,
    pub last_cross_time_pct: Option<f64>,
    pub last_cross_is_significant: bool,
    pub last_cross_atr: f64,
}

#[derive(Debug, Clone)]
pub struct MidCrossWindowSummary {
    pub cross_count: u32,
    pub significant_cross_count: u32,
    pub final_side: Option<LeadSide>,
    pub last_cross_atr: f64,
}

#[derive(Debug, Clone, Default)]
struct MidCrossWindowState {
    armed: bool,
    current_side: Option<LeadSide>,
    cross_count: u32,
    significant_cross_count: u32,
    peak_lead_gap: f64,
    up_mid: f64,
    down_mid: f64,
    last_cross_from: Option<LeadSide>,
    last_cross_to: Option<LeadSide>,
    last_cross_time_pct: Option<f64>,
    last_cross_is_significant: bool,
    last_cross_atr: f64,
}

#[derive(Debug, Clone, Default)]
pub struct MidCrossTracker {
    windows: HashMap<usize, MidCrossWindowState>,
}

fn midpoint(bid: f64, ask: f64) -> f64 {
    (bid + ask) / 2.0
}

fn side_mid(side: &str, prices: &PricesState) -> f64 {
    if side == "UP" {
        midpoint(prices.up.bid, prices.up.ask)
    } else {
        midpoint(prices.down.bid, prices.down.ask)
    }
}

fn market_duration_sec(market: &MarketWindow) -> f64 {
    match (
        chrono::DateTime::parse_from_rfc3339(&market.start_time),
        chrono::DateTime::parse_from_rfc3339(&market.end_time),
    ) {
        (Ok(s), Ok(e)) => ((e.timestamp_millis() - s.timestamp_millis()) as f64 / 1000.0).max(1.0),
        _ => 900.0,
    }
}

fn time_pct_for(market: &MarketWindow, secs_to_end: i64) -> f64 {
    let duration_sec = market_duration_sec(market);
    let elapsed_sec = (duration_sec - secs_to_end as f64).clamp(0.0, duration_sec);
    (elapsed_sec / duration_sec) * 100.0
}

impl MidCrossTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn observe_tick(
        &mut self,
        window_number: usize,
        market: &MarketWindow,
        prices: &PricesState,
        secs_to_end: i64,
        current_atr: f64,
        _spot_price: Option<f64>,
        _spot_signal: SpotSignalSnapshot,
        _timestamp_ms: i64,
    ) -> Option<MidCrossEvent> {
        let time_pct = time_pct_for(market, secs_to_end);
        if time_pct < MID_CROSS_ARM_TIME_PCT {
            return None;
        }

        let up_mid = side_mid("UP", prices);
        let down_mid = side_mid("DOWN", prices);
        let lead_gap = (up_mid - down_mid).abs();
        let observed_side = LeadSide::from_mids(up_mid, down_mid);

        let state = self.windows.entry(window_number).or_default();

        if !state.armed {
            state.armed = true;
            state.current_side = if observed_side == LeadSide::Tie {
                None
            } else {
                Some(observed_side)
            };
            state.up_mid = up_mid;
            state.down_mid = down_mid;
            state.peak_lead_gap = if observed_side == LeadSide::Tie {
                0.0
            } else {
                lead_gap
            };
            state.last_cross_atr = current_atr;

            return Some(MidCrossEvent {
                event: "armed_init",
                from_side: None,
                to_side: observed_side,
                up_mid,
                down_mid,
                lead_gap,
                peak_prev_gap: 0.0,
                is_significant: false,
                cross_count: 0,
                significant_cross_count: 0,
                time_pct,
                secs_to_end,
                current_atr,
            });
        }

        state.up_mid = up_mid;
        state.down_mid = down_mid;

        if observed_side == LeadSide::Tie {
            return None;
        }

        if let Some(current) = state.current_side {
            if current == observed_side {
                state.peak_lead_gap = state.peak_lead_gap.max(lead_gap);
                return None;
            }

            let peak_prev_gap = state.peak_lead_gap;
            let is_significant = peak_prev_gap >= MID_CROSS_SIGNIFICANT_GAP;
            state.cross_count += 1;
            if is_significant {
                state.significant_cross_count += 1;
            }

            let from_side = current;
            state.current_side = Some(observed_side);
            state.peak_lead_gap = lead_gap;
            state.last_cross_from = Some(from_side);
            state.last_cross_to = Some(observed_side);
            state.last_cross_time_pct = Some(time_pct);
            state.last_cross_is_significant = is_significant;
            state.last_cross_atr = current_atr;

            return Some(MidCrossEvent {
                event: "mid_cross",
                from_side: Some(from_side),
                to_side: observed_side,
                up_mid,
                down_mid,
                lead_gap,
                peak_prev_gap,
                is_significant,
                cross_count: state.cross_count,
                significant_cross_count: state.significant_cross_count,
                time_pct,
                secs_to_end,
                current_atr,
            });
        }

        state.current_side = Some(observed_side);
        state.peak_lead_gap = lead_gap;
        None
    }

    pub fn snapshot(&self, window_number: usize) -> MidCrossSnapshot {
        match self.windows.get(&window_number) {
            Some(state) => MidCrossSnapshot {
                armed: state.armed,
                current_side: state.current_side,
                lead_gap: (state.up_mid - state.down_mid).abs(),
                up_mid: state.up_mid,
                down_mid: state.down_mid,
                cross_count: state.cross_count,
                significant_cross_count: state.significant_cross_count,
                peak_lead_gap: state.peak_lead_gap,
                last_cross_from: state.last_cross_from,
                last_cross_to: state.last_cross_to,
                last_cross_time_pct: state.last_cross_time_pct,
                last_cross_is_significant: state.last_cross_is_significant,
                last_cross_atr: state.last_cross_atr,
            },
            None => MidCrossSnapshot::default(),
        }
    }

    pub fn finalize_window(&self, window_number: usize) -> MidCrossWindowSummary {
        match self.windows.get(&window_number) {
            Some(state) => MidCrossWindowSummary {
                cross_count: state.cross_count,
                significant_cross_count: state.significant_cross_count,
                final_side: state.current_side,
                last_cross_atr: state.last_cross_atr,
            },
            None => MidCrossWindowSummary::default(),
        }
    }

    pub fn remove_window(&mut self, window_number: usize) {
        self.windows.remove(&window_number);
    }
}

impl Default for MidCrossWindowSummary {
    fn default() -> Self {
        Self {
            cross_count: 0,
            significant_cross_count: 0,
            final_side: None,
            last_cross_atr: 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{ContractPrices, MarketWindow, PricesState, TokenInfo, TokensMap};

    fn test_market() -> MarketWindow {
        MarketWindow {
            id: "1".to_string(),
            slug: "test".to_string(),
            question: "test".to_string(),
            asset: "BTC".to_string(),
            interval: "15m".to_string(),
            start_time: "2026-01-01T00:00:00Z".to_string(),
            end_time: "2026-01-01T00:15:00Z".to_string(),
            price_to_beat: Some(100_000.0),
            tokens: TokensMap {
                up: TokenInfo {
                    token_id: "up".to_string(),
                    outcome_name: "UP".to_string(),
                },
                down: TokenInfo {
                    token_id: "down".to_string(),
                    outcome_name: "DOWN".to_string(),
                },
            },
        }
    }

    fn prices(up_bid: f64, up_ask: f64, dn_bid: f64, dn_ask: f64) -> PricesState {
        PricesState {
            up: ContractPrices::top(up_bid, up_ask),
            down: ContractPrices::top(dn_bid, dn_ask),
        }
    }

    #[test]
    fn no_tracking_before_arm_threshold() {
        let mut tracker = MidCrossTracker::new();
        let market = test_market();
        let event = tracker.observe_tick(
            1,
            &market,
            &prices(0.62, 0.64, 0.36, 0.38),
            840,
            40.0,
            Some(100_000.0),
            SpotSignalSnapshot::default(),
            0,
        );
        assert!(event.is_none());
        assert!(!tracker.snapshot(1).armed);
    }

    #[test]
    fn arms_at_eight_percent_with_init_event() {
        let mut tracker = MidCrossTracker::new();
        let market = test_market();
        let event = tracker.observe_tick(
            1,
            &market,
            &prices(0.62, 0.64, 0.36, 0.38),
            828,
            42.5,
            Some(100_000.0),
            SpotSignalSnapshot::default(),
            0,
        )
        .expect("armed_init");
        assert_eq!(event.event, "armed_init");
        assert_eq!(event.to_side, LeadSide::Up);
        assert_eq!(event.cross_count, 0);
        let snap = tracker.snapshot(1);
        assert!(snap.armed);
        assert_eq!(snap.current_side, Some(LeadSide::Up));
    }

    #[test]
    fn flip_increments_cross_count() {
        let mut tracker = MidCrossTracker::new();
        let market = test_market();
        tracker.observe_tick(
            1,
            &market,
            &prices(0.62, 0.64, 0.36, 0.38),
            828,
            40.0,
            None,
            SpotSignalSnapshot::default(),
            0,
        );
        let event = tracker
            .observe_tick(
                1,
                &market,
                &prices(0.36, 0.38, 0.62, 0.64),
                700,
                41.0,
                None,
                SpotSignalSnapshot::default(),
                0,
            )
            .expect("mid_cross");
        assert_eq!(event.event, "mid_cross");
        assert_eq!(event.from_side, Some(LeadSide::Up));
        assert_eq!(event.to_side, LeadSide::Down);
        assert_eq!(event.cross_count, 1);
    }

    #[test]
    fn significant_only_when_peak_gap_large() {
        let mut tracker = MidCrossTracker::new();
        let market = test_market();
        tracker.observe_tick(
            1,
            &market,
            &prices(0.62, 0.64, 0.36, 0.38),
            828,
            40.0,
            None,
            SpotSignalSnapshot::default(),
            0,
        );
        let event = tracker
            .observe_tick(
                1,
                &market,
                &prices(0.36, 0.38, 0.62, 0.64),
                700,
                41.0,
                None,
                SpotSignalSnapshot::default(),
                0,
            )
            .expect("mid_cross");
        assert!(event.is_significant);
        assert_eq!(event.significant_cross_count, 1);
    }

    #[test]
    fn noise_flip_not_significant() {
        let mut tracker = MidCrossTracker::new();
        let market = test_market();
        tracker.observe_tick(
            1,
            &market,
            &prices(0.51, 0.53, 0.47, 0.49),
            828,
            40.0,
            None,
            SpotSignalSnapshot::default(),
            0,
        );
        let event = tracker
            .observe_tick(
                1,
                &market,
                &prices(0.47, 0.49, 0.51, 0.53),
                700,
                41.0,
                None,
                SpotSignalSnapshot::default(),
                0,
            )
            .expect("mid_cross");
        assert!(!event.is_significant);
        assert_eq!(event.significant_cross_count, 0);
    }

    #[test]
    fn tie_preserves_previous_side() {
        let mut tracker = MidCrossTracker::new();
        let market = test_market();
        tracker.observe_tick(
            1,
            &market,
            &prices(0.62, 0.64, 0.36, 0.38),
            828,
            40.0,
            None,
            SpotSignalSnapshot::default(),
            0,
        );
        let event = tracker.observe_tick(
            1,
            &market,
            &prices(0.50, 0.50, 0.50, 0.50),
            700,
            41.0,
            None,
            SpotSignalSnapshot::default(),
            0,
        );
        assert!(event.is_none());
        assert_eq!(tracker.snapshot(1).current_side, Some(LeadSide::Up));
    }
}
