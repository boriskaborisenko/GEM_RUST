use crate::client::{MarketWindow, PricesState};
use crate::config::Config;
use crate::redeem_hold::{expected_move_usd, side_is_itm};
use crate::strategy::{
    CexMicroSnapshot, MidCrossSnapshot, OrderSignal, SpotSignalSnapshot, StrategyState,
    TradeStrategy,
};
use crate::trader::WindowState;
use std::collections::HashMap;

const H_MIN_TRADEABLE_WINDOW: usize = 1;
const H_MIN_TRADE_USD: f64 = 1.0;
const H_LIVE_BUDGET_MULT: f64 = 0.90;
const H_ENTRY_END_TIME_PCT: f64 = 33.0;
const H_TARGET_ASK: f64 = 0.38;
const H_ASK_BAND: f64 = 0.02;
const H_MAX_ABS_GAP_Z: f64 = 0.20;
const H_SALVAGE_TIME_PCT: f64 = 75.0;
const H_MIN_BID_SALVAGE: f64 = 0.05;
const H_MIN_SHARES: f64 = 0.000001;

#[derive(Debug, Clone)]
struct HWindowState {
    entry_side: Option<String>,
    entry_done: bool,
    salvage_done: bool,
}

pub struct CheapHoldStrategy {
    windows: HashMap<usize, HWindowState>,
}

impl CheapHoldStrategy {
    pub fn new() -> Self {
        Self {
            windows: HashMap::new(),
        }
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
    let duration = market_duration_sec(market);
    let elapsed = (duration - secs_to_end as f64).clamp(0.0, duration);
    (elapsed / duration) * 100.0
}

fn side_ask(side: &str, prices: &PricesState) -> f64 {
    if side == "UP" {
        prices.up.ask
    } else {
        prices.down.ask
    }
}

fn side_bid(side: &str, prices: &PricesState) -> f64 {
    if side == "UP" {
        prices.up.bid
    } else {
        prices.down.bid
    }
}

fn shares_for_side(side: &str, win: &WindowState) -> f64 {
    if side == "UP" {
        win.up_shares
    } else {
        win.down_shares
    }
}

fn has_position(win: &WindowState) -> bool {
    win.up_shares > H_MIN_SHARES || win.down_shares > H_MIN_SHARES
}

fn held_side(win: &WindowState) -> Option<&'static str> {
    let has_up = win.up_shares > H_MIN_SHARES;
    let has_down = win.down_shares > H_MIN_SHARES;
    match (has_up, has_down) {
        (true, false) => Some("UP"),
        (false, true) => Some("DOWN"),
        (true, true) => {
            if win.up_shares >= win.down_shares {
                Some("UP")
            } else {
                Some("DOWN")
            }
        }
        _ => None,
    }
}

pub(crate) fn ask_in_band(ask: f64) -> bool {
    ask > 0.0 && ask >= H_TARGET_ASK - H_ASK_BAND && ask <= H_TARGET_ASK + H_ASK_BAND
}

pub(crate) fn gap_z_allows_entry(gap_z: f64) -> bool {
    gap_z.is_finite() && gap_z.abs() <= H_MAX_ABS_GAP_Z
}

pub(crate) fn pick_cheap_entry_side(prices: &PricesState) -> Option<&'static str> {
    let up_in = ask_in_band(prices.up.ask);
    let down_in = ask_in_band(prices.down.ask);
    match (up_in, down_in) {
        (true, true) => {
            if prices.up.ask <= prices.down.ask {
                Some("UP")
            } else {
                Some("DOWN")
            }
        }
        (true, false) => Some("UP"),
        (false, true) => Some("DOWN"),
        _ => None,
    }
}

pub fn phase_label(time_pct: f64, entry_done: bool, salvage_done: bool) -> &'static str {
    if !entry_done {
        if time_pct <= H_ENTRY_END_TIME_PCT {
            "entry"
        } else {
            "missed"
        }
    } else if salvage_done {
        "flat"
    } else if time_pct >= H_SALVAGE_TIME_PCT {
        "salvage"
    } else {
        "hold"
    }
}

impl TradeStrategy for CheapHoldStrategy {
    fn check_pre_start_entry(
        &mut self,
        _config: &Config,
        _prices: &PricesState,
        _market: &MarketWindow,
        _spot_price: Option<f64>,
        _window_number: usize,
        _secs_to_start: i64,
        _current_atr: f64,
        _spot_signal: SpotSignalSnapshot,
        _llm_forecast: Option<crate::strategy::LlmForecast>,
        _cex_micro: &CexMicroSnapshot,
    ) -> Option<crate::strategy::EntrySignal> {
        None
    }

    fn process_live_tick(
        &mut self,
        config: &Config,
        prices: &PricesState,
        spot_price: Option<f64>,
        market: &MarketWindow,
        win_state: &WindowState,
        secs_to_end: i64,
        current_atr: f64,
        _spot_signal: SpotSignalSnapshot,
        _mid_cross: &MidCrossSnapshot,
        _cex_micro: &CexMicroSnapshot,
    ) -> Vec<OrderSignal> {
        let mut signals = Vec::new();
        let window_number = win_state.window_number;
        if window_number < H_MIN_TRADEABLE_WINDOW {
            return signals;
        }

        let time_pct = time_pct_for(market, secs_to_end);
        let state = self.windows.entry(window_number).or_insert(HWindowState {
            entry_side: None,
            entry_done: false,
            salvage_done: false,
        });

        let (Some(spot), Some(ptb)) = (spot_price, market.price_to_beat) else {
            return signals;
        };

        let expected = expected_move_usd(current_atr, secs_to_end);
        let gap_z = if expected > 0.0 {
            (spot - ptb) / expected
        } else {
            f64::NAN
        };

        if let Some(side) = held_side(win_state) {
            if state.entry_side.is_none() {
                state.entry_side = Some(side.to_string());
                state.entry_done = true;
            }
        }

        if !has_position(win_state) && !state.entry_done && time_pct <= H_ENTRY_END_TIME_PCT {
            if gap_z_allows_entry(gap_z) {
                if let Some(side) = pick_cheap_entry_side(prices) {
                    let ask = side_ask(side, prices);
                    let budget_total = (config.session.min_window_budget * H_LIVE_BUDGET_MULT)
                        .clamp(H_MIN_TRADE_USD, config.session.max_window_budget);
                    let remaining = (budget_total - win_state.spent).max(0.0);
                    if remaining >= H_MIN_TRADE_USD {
                        signals.push(OrderSignal {
                            side: side.to_string(),
                            is_buy: true,
                            amount: remaining,
                            price: ask,
                            reason: format!(
                                "h_entry_{}_ask_{:.2}_gap_z_{:+.2}",
                                side.to_lowercase(),
                                ask,
                                gap_z
                            ),
                        });
                        state.entry_side = Some(side.to_string());
                        state.entry_done = true;
                    }
                }
            }
            return signals;
        }

        if state.salvage_done || time_pct < H_SALVAGE_TIME_PCT {
            return signals;
        }

        let Some(side) = state
            .entry_side
            .as_deref()
            .or_else(|| held_side(win_state))
        else {
            return signals;
        };

        let shares = shares_for_side(side, win_state);
        if shares <= H_MIN_SHARES {
            return signals;
        }

        if side_is_itm(side, spot, ptb) {
            return signals;
        }

        let bid = side_bid(side, prices);
        if bid < H_MIN_BID_SALVAGE {
            return signals;
        }

        signals.push(OrderSignal {
            side: side.to_string(),
            is_buy: false,
            amount: shares,
            price: bid,
            reason: format!(
                "h_salvage_otm_bid_{:.2}_gap_z_{:+.2}",
                bid, gap_z
            ),
        });
        state.salvage_done = true;
        signals
    }

    fn get_strategy_state(&self, window_number: usize) -> Option<StrategyState> {
        self.windows.get(&window_number).map(|s| StrategyState {
            up_sold: s.salvage_done && s.entry_side.as_deref() == Some("UP"),
            down_sold: s.salvage_done && s.entry_side.as_deref() == Some("DOWN"),
            first_sold_side: None,
            ptb_crossed: false,
            ptb_baseline: None,
            e_conviction_side: None,
            e_tranches_done: 0,
            e_grid_steps_done: 0,
            h_entry_side: s.entry_side.clone(),
            h_entry_done: s.entry_done,
            h_salvage_done: s.salvage_done,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{ContractPrices, TokenInfo, TokensMap};
    use crate::mid_cross_tracker::MidCrossSnapshot;

    fn sample_market(secs_to_end: i64) -> MarketWindow {
        let duration = 900_i64;
        let end = chrono::Utc::now() + chrono::Duration::seconds(secs_to_end);
        let start = end - chrono::Duration::seconds(duration);
        MarketWindow {
            id: "test".to_string(),
            slug: "btc-updown-15m-test".to_string(),
            question: "test".to_string(),
            asset: "BTC".to_string(),
            interval: "15m".to_string(),
            start_time: start.to_rfc3339(),
            end_time: end.to_rfc3339(),
            price_to_beat: Some(60_000.0),
            tokens: TokensMap {
                up: TokenInfo {
                    token_id: "up".to_string(),
                    outcome_name: "Up".to_string(),
                },
                down: TokenInfo {
                    token_id: "down".to_string(),
                    outcome_name: "Down".to_string(),
                },
            },
        }
    }

    fn sample_config() -> Config {
        Config::load("config.json").expect("config.json")
    }

    fn sample_win(window_number: usize, up_shares: f64, down_shares: f64) -> WindowState {
        WindowState {
            window_number,
            role: "CURRENT".to_string(),
            status: "LIVE".to_string(),
            market: sample_market(600),
            spent: 0.0,
            cash_returned: 0.0,
            up_shares,
            down_shares,
            initial_up_shares: up_shares,
            initial_down_shares: down_shares,
            trades: vec![],
            prices: prices_up_cheap(),
        }
    }

    fn prices_up_cheap() -> PricesState {
        PricesState {
            up: ContractPrices {
                bid: 0.36,
                ask: 0.38,
            },
            down: ContractPrices {
                bid: 0.61,
                ask: 0.63,
            },
        }
    }

    #[test]
    fn entry_allowed_cheap_side_near_ptb() {
        let mut strat = CheapHoldStrategy::new();
        let config = sample_config();
        let prices = prices_up_cheap();
        let market = sample_market(720);
        let win = sample_win(1, 0.0, 0.0);
        let mid = MidCrossSnapshot::default();
        let cex = CexMicroSnapshot::default();
        let spot = 60_010.0;
        let atr = 50.0;

        let signals = strat.process_live_tick(
            &config,
            &prices,
            Some(spot),
            &market,
            &win,
            720,
            atr,
            SpotSignalSnapshot::default(),
            &mid,
            &cex,
        );
        assert_eq!(signals.len(), 1);
        assert!(signals[0].is_buy);
        assert_eq!(signals[0].side, "UP");
        assert!(signals[0].reason.starts_with("h_entry_up"));
    }

    #[test]
    fn entry_blocked_far_from_ptb() {
        let mut strat = CheapHoldStrategy::new();
        let config = sample_config();
        let prices = prices_up_cheap();
        let market = sample_market(720);
        let win = sample_win(1, 0.0, 0.0);
        let mid = MidCrossSnapshot::default();
        let cex = CexMicroSnapshot::default();
        let spot = 60_500.0;
        let atr = 50.0;

        let signals = strat.process_live_tick(
            &config,
            &prices,
            Some(spot),
            &market,
            &win,
            720,
            atr,
            SpotSignalSnapshot::default(),
            &mid,
            &cex,
        );
        assert!(signals.is_empty());
    }

    #[test]
    fn entry_blocked_ask_too_high() {
        assert!(pick_cheap_entry_side(&PricesState {
            up: ContractPrices {
                bid: 0.43,
                ask: 0.45,
            },
            down: ContractPrices {
                bid: 0.54,
                ask: 0.56,
            },
        })
        .is_none());
    }

    #[test]
    fn salvage_otm_at_75pct() {
        let mut strat = CheapHoldStrategy::new();
        let config = sample_config();
        let prices = PricesState {
            up: ContractPrices {
                bid: 0.25,
                ask: 0.27,
            },
            down: ContractPrices {
                bid: 0.72,
                ask: 0.74,
            },
        };
        let market = sample_market(216);
        let win = sample_win(1, 26.0, 0.0);
        let mid = MidCrossSnapshot::default();
        let cex = CexMicroSnapshot::default();
        let spot = 59_900.0;
        let atr = 50.0;

        strat.windows.insert(
            1,
            HWindowState {
                entry_side: Some("UP".to_string()),
                entry_done: true,
                salvage_done: false,
            },
        );

        let signals = strat.process_live_tick(
            &config,
            &prices,
            Some(spot),
            &market,
            &win,
            216,
            atr,
            SpotSignalSnapshot::default(),
            &mid,
            &cex,
        );
        assert_eq!(signals.len(), 1);
        assert!(!signals[0].is_buy);
        assert_eq!(signals[0].side, "UP");
        assert!(signals[0].reason.starts_with("h_salvage_otm"));
        assert!((signals[0].price - 0.25).abs() < 1e-6);
    }

    #[test]
    fn no_salvage_when_itm() {
        let mut strat = CheapHoldStrategy::new();
        let config = sample_config();
        let prices = PricesState {
            up: ContractPrices {
                bid: 0.80,
                ask: 0.82,
            },
            down: ContractPrices {
                bid: 0.17,
                ask: 0.19,
            },
        };
        let market = sample_market(216);
        let win = sample_win(1, 26.0, 0.0);
        let mid = MidCrossSnapshot::default();
        let cex = CexMicroSnapshot::default();
        let spot = 60_200.0;
        let atr = 50.0;

        strat.windows.insert(
            1,
            HWindowState {
                entry_side: Some("UP".to_string()),
                entry_done: true,
                salvage_done: false,
            },
        );

        let signals = strat.process_live_tick(
            &config,
            &prices,
            Some(spot),
            &market,
            &win,
            216,
            atr,
            SpotSignalSnapshot::default(),
            &mid,
            &cex,
        );
        assert!(signals.is_empty());
    }

    #[test]
    fn ask_in_band_accepts_target_prices() {
        assert!(ask_in_band(0.36));
        assert!(ask_in_band(0.38));
        assert!(ask_in_band(0.40));
        assert!(!ask_in_band(0.45));
    }

    #[test]
    fn gap_z_gate_blocks_large_distance() {
        assert!(gap_z_allows_entry(0.10));
        assert!(!gap_z_allows_entry(0.45));
    }
}
