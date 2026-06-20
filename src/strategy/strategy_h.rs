use crate::client::{MarketWindow, PricesState};
use crate::config::Config;
use crate::redeem_hold::{expected_move_usd, side_is_itm};
use crate::strategy::{
    CexMicroSnapshot, MidCrossSnapshot, OrderSignal, SpotSignalSnapshot, StrategyState,
    TradeStrategy,
};
use crate::trader::WindowState;
use chrono::Timelike;
use std::collections::HashMap;

const H_MIN_TRADEABLE_WINDOW: usize = 1;
const H_MIN_TRADE_USD: f64 = 1.0;
const H_LIVE_BUDGET_MULT: f64 = 0.90;
const H_ENTRY_END_TIME_PCT: f64 = 33.0;
const H_TARGET_ASK: f64 = 0.38;
const H_ASK_BAND: f64 = 0.02;
const H_MAX_ENTRY_ASK: f64 = 0.39;
const H_MAX_ABS_GAP_Z: f64 = 0.20;
const H_SALVAGE_TIME_PCT: f64 = 80.0;
const H_MIN_BID_SALVAGE: f64 = 0.05;
const H_SALVAGE_HOLD_ABS_GAP_Z: f64 = 0.80;
const H_SALVAGE_HOLD_MIN_BID: f64 = 0.30;
const H_SALVAGE_FORCE_ABS_GAP_Z: f64 = 1.20;
const H_SALVAGE_FORCE_MAX_BID: f64 = 0.12;
const H_MIN_SHARES: f64 = 0.000001;

#[derive(Debug, Clone, Copy)]
struct AtrRegime {
    norm_max: f64,
    vol_max: f64,
}

fn atr_regime_for(asset: &str) -> AtrRegime {
    match asset.to_uppercase().as_str() {
        // ~1m ATR bands in USD; ETH/SOL calibrated to their typical quote volatility.
        "ETH" => AtrRegime {
            norm_max: 3.0,
            vol_max: 6.0,
        },
        "SOL" => AtrRegime {
            norm_max: 0.30,
            vol_max: 0.60,
        },
        _ => AtrRegime {
            norm_max: 45.0,
            vol_max: 90.0,
        },
    }
}

pub(crate) fn is_vol_atr(current_atr: f64, asset: &str) -> bool {
    let regime = atr_regime_for(asset);
    current_atr >= regime.norm_max && current_atr < regime.vol_max
}

fn is_norm_atr(current_atr: f64, asset: &str) -> bool {
    current_atr < atr_regime_for(asset).norm_max
}

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
    ask > 0.0 && ask >= H_TARGET_ASK - H_ASK_BAND && ask <= H_MAX_ENTRY_ASK
}

fn utc_hour_now() -> u32 {
    chrono::Utc::now().hour()
}

/// Skip entry when logs show persistent toxic UTC/ATR combos (BTC-calibrated).
pub(crate) fn entry_sleep_blocks(
    utc_hour: u32,
    current_atr: f64,
    interval: &str,
    asset: &str,
) -> Option<&'static str> {
    if !asset.eq_ignore_ascii_case("BTC") {
        return None;
    }
    if utc_hour == 9 {
        return Some("sleep_utc09");
    }
    if is_norm_atr(current_atr, asset) {
        if utc_hour == 0 {
            return Some("sleep_utc00_norm");
        }
        if utc_hour == 1 {
            return Some("sleep_utc01_norm");
        }
    }
    if is_vol_atr(current_atr, asset) {
        if utc_hour == 19 {
            return Some("sleep_utc19_vol");
        }
        if utc_hour == 20 && interval == "5m" {
            return Some("sleep_utc20_vol_5m");
        }
    }
    None
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

pub(crate) fn is_contrarian_entry(side: &str, gap_z: f64) -> bool {
    if !gap_z.is_finite() {
        return false;
    }
    match side {
        "UP" => gap_z < 0.0,
        "DOWN" => gap_z > 0.0,
        _ => false,
    }
}

pub(crate) fn entry_activity_allows(mid_cross: &MidCrossSnapshot) -> bool {
    mid_cross.cross_count >= 1 || mid_cross.significant_cross_count >= 1
}

pub(crate) fn entry_gate_allows(mid_cross: &MidCrossSnapshot) -> bool {
    entry_activity_allows(mid_cross)
}

pub(crate) fn pick_h_entry_side(
    prices: &PricesState,
    gap_z: f64,
    mid_cross: &MidCrossSnapshot,
) -> Option<&'static str> {
    if !entry_gate_allows(mid_cross) {
        return None;
    }
    let cheap = pick_cheap_entry_side(prices)?;
    if is_contrarian_entry(cheap, gap_z) {
        return Some(cheap);
    }
    let other = if cheap == "UP" { "DOWN" } else { "UP" };
    if ask_in_band(side_ask(other, prices)) && is_contrarian_entry(other, gap_z) {
        return Some(other);
    }
    if !is_contrarian_entry(cheap, gap_z) {
        return Some(cheap);
    }
    None
}

pub(crate) fn salvage_allows_otm_exit(gap_z: f64, bid: f64, current_atr: f64, asset: &str) -> bool {
    if !gap_z.is_finite() || bid < H_MIN_BID_SALVAGE {
        return false;
    }
    let abs_gap_z = gap_z.abs();
    if is_vol_atr(current_atr, asset) {
        return abs_gap_z >= H_SALVAGE_FORCE_ABS_GAP_Z || bid <= H_SALVAGE_FORCE_MAX_BID;
    }
    if abs_gap_z >= H_SALVAGE_FORCE_ABS_GAP_Z || bid <= H_SALVAGE_FORCE_MAX_BID {
        return true;
    }
    if abs_gap_z < H_SALVAGE_HOLD_ABS_GAP_Z || bid > H_SALVAGE_HOLD_MIN_BID {
        return false;
    }
    true
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
        mid_cross: &MidCrossSnapshot,
        _cex_micro: &CexMicroSnapshot,
        _tape: &crate::trade_tape::TradeTapeSnapshot,
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
            if entry_sleep_blocks(utc_hour_now(), current_atr, &market.interval, &market.asset)
                .is_some()
            {
                return signals;
            }
            if gap_z_allows_entry(gap_z) {
                if let Some(side) = pick_h_entry_side(prices, gap_z, mid_cross) {
                    let ask = side_ask(side, prices);
                    let budget_total = (config.session.min_window_budget * H_LIVE_BUDGET_MULT)
                        .clamp(H_MIN_TRADE_USD, config.session.max_window_budget);
                    let remaining = (budget_total - win_state.spent).max(0.0);
                    if remaining >= H_MIN_TRADE_USD {
                        let mode = if is_contrarian_entry(side, gap_z) {
                            "active"
                        } else {
                            "aligned"
                        };
                        signals.push(OrderSignal {
                            side: side.to_string(),
                            is_buy: true,
                            order_type: crate::strategy::OrderType::Market,
                            amount: remaining,
                            price: ask,
                            reason: format!(
                                "h_entry_{}_ask_{:.2}_gap_z_{:+.2}_{}_xc{}",
                                side.to_lowercase(),
                                ask,
                                gap_z,
                                mode,
                                mid_cross.cross_count
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

        let Some(side) = state.entry_side.as_deref().or_else(|| held_side(win_state)) else {
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
        if !salvage_allows_otm_exit(gap_z, bid, current_atr, &market.asset) {
            return signals;
        }

        signals.push(OrderSignal {
            side: side.to_string(),
            is_buy: false,
            order_type: crate::strategy::OrderType::Market,
            amount: shares,
            price: bid,
            reason: format!("h_salvage_otm_bid_{:.2}_gap_z_{:+.2}", bid, gap_z),
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

    fn sample_market_5m(secs_to_end: i64) -> MarketWindow {
        let mut market = sample_market(secs_to_end);
        market.interval = "5m".to_string();
        market.slug = "btc-updown-5m-test".to_string();
        market
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
            up: ContractPrices::top(0.36, 0.38),
            down: ContractPrices::top(0.61, 0.63),
        }
    }

    fn active_mid_cross() -> MidCrossSnapshot {
        MidCrossSnapshot {
            cross_count: 1,
            significant_cross_count: 1,
            peak_lead_gap: 0.12,
            armed: true,
            ..MidCrossSnapshot::default()
        }
    }

    // Wall-clock dependent (UTC-hour sleep windows); flaky by design. H is out of scope.
    #[ignore]
    #[test]
    fn entry_allowed_cheap_side_near_ptb() {
        let mut strat = CheapHoldStrategy::new();
        let config = sample_config();
        let prices = prices_up_cheap();
        let market = sample_market(720);
        let win = sample_win(1, 0.0, 0.0);
        let mid = active_mid_cross();
        let cex = CexMicroSnapshot::default();
        let spot = 59_990.0;
        let atr = 30.0;

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
            &crate::trade_tape::TradeTapeSnapshot::default(),
        );
        assert_eq!(signals.len(), 1);
        assert!(signals[0].is_buy);
        assert_eq!(signals[0].side, "UP");
        assert!(signals[0].reason.starts_with("h_entry_up"));
        assert!(signals[0].reason.contains("_active_"));
    }

    #[test]
    fn entry_blocked_without_activity_early() {
        let mut strat = CheapHoldStrategy::new();
        let config = sample_config();
        let prices = prices_up_cheap();
        let market = sample_market(720);
        let win = sample_win(1, 0.0, 0.0);
        let mid = MidCrossSnapshot::default();
        let cex = CexMicroSnapshot::default();
        let spot = 59_990.0;
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
            &crate::trade_tape::TradeTapeSnapshot::default(),
        );
        assert!(signals.is_empty());
    }

    #[test]
    fn entry_blocked_late_without_cross_no_fallback() {
        let mut strat = CheapHoldStrategy::new();
        let config = sample_config();
        let prices = prices_up_cheap();
        let market = sample_market(680);
        let win = sample_win(1, 0.0, 0.0);
        let mid = MidCrossSnapshot::default();
        let cex = CexMicroSnapshot::default();
        let spot = 59_995.0;
        let atr = 30.0;

        let signals = strat.process_live_tick(
            &config,
            &prices,
            Some(spot),
            &market,
            &win,
            680,
            atr,
            SpotSignalSnapshot::default(),
            &mid,
            &cex,
            &crate::trade_tape::TradeTapeSnapshot::default(),
        );
        assert!(signals.is_empty());
    }

    #[test]
    fn vol_atr_does_not_blanket_sleep_5m() {
        assert_eq!(entry_sleep_blocks(12, 55.0, "5m", "BTC"), None);
        assert_eq!(
            entry_sleep_blocks(19, 50.0, "5m", "BTC"),
            Some("sleep_utc19_vol")
        );
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
            &crate::trade_tape::TradeTapeSnapshot::default(),
        );
        assert!(signals.is_empty());
    }

    #[test]
    fn entry_blocked_ask_too_high() {
        assert!(pick_cheap_entry_side(&PricesState {
            up: ContractPrices::top(0.43, 0.45),
            down: ContractPrices::top(0.54, 0.56),
        })
        .is_none());
    }

    #[test]
    fn salvage_otm_at_80pct() {
        let mut strat = CheapHoldStrategy::new();
        let config = sample_config();
        let prices = PricesState {
            up: ContractPrices::top(0.25, 0.27),
            down: ContractPrices::top(0.72, 0.74),
        };
        let market = sample_market(180);
        let win = sample_win(1, 26.0, 0.0);
        let mid = MidCrossSnapshot::default();
        let cex = CexMicroSnapshot::default();
        let spot = 59_900.0;
        let atr = 30.0;

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
            180,
            atr,
            SpotSignalSnapshot::default(),
            &mid,
            &cex,
            &crate::trade_tape::TradeTapeSnapshot::default(),
        );
        assert_eq!(signals.len(), 1);
        assert!(!signals[0].is_buy);
        assert_eq!(signals[0].side, "UP");
        assert!(signals[0].reason.starts_with("h_salvage_otm"));
        assert!((signals[0].price - 0.25).abs() < 1e-6);
    }

    #[test]
    fn no_salvage_close_race_or_rich_bid() {
        let mut strat = CheapHoldStrategy::new();
        let config = sample_config();
        let prices = PricesState {
            up: ContractPrices::top(0.36, 0.38),
            down: ContractPrices::top(0.61, 0.63),
        };
        let market = sample_market(180);
        let win = sample_win(1, 26.0, 0.0);
        let mid = MidCrossSnapshot::default();
        let cex = CexMicroSnapshot::default();
        let spot = 59_970.0;
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
            180,
            atr,
            SpotSignalSnapshot::default(),
            &mid,
            &cex,
            &crate::trade_tape::TradeTapeSnapshot::default(),
        );
        assert!(signals.is_empty());
        assert!(!salvage_allows_otm_exit(-0.35, 0.36, 30.0, "BTC"));
        assert!(!salvage_allows_otm_exit(-1.0, 0.35, 30.0, "BTC"));
        assert!(salvage_allows_otm_exit(-1.3, 0.25, 30.0, "BTC"));
        assert!(!salvage_allows_otm_exit(-0.70, 0.25, 55.0, "BTC"));
        assert!(salvage_allows_otm_exit(-1.3, 0.25, 55.0, "BTC"));
    }

    #[test]
    fn no_salvage_vol_atr_unless_hopeless() {
        let mut strat = CheapHoldStrategy::new();
        let config = sample_config();
        let prices = PricesState {
            up: ContractPrices::top(0.22, 0.24),
            down: ContractPrices::top(0.75, 0.77),
        };
        let market = sample_market(180);
        let win = sample_win(1, 26.0, 0.0);
        let mid = MidCrossSnapshot::default();
        let cex = CexMicroSnapshot::default();
        let spot = 59_920.0;
        let atr = 55.0;

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
            180,
            atr,
            SpotSignalSnapshot::default(),
            &mid,
            &cex,
            &crate::trade_tape::TradeTapeSnapshot::default(),
        );
        assert!(signals.is_empty());
    }

    #[test]
    fn no_salvage_when_itm() {
        let mut strat = CheapHoldStrategy::new();
        let config = sample_config();
        let prices = PricesState {
            up: ContractPrices::top(0.80, 0.82),
            down: ContractPrices::top(0.17, 0.19),
        };
        let market = sample_market(180);
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
            180,
            atr,
            SpotSignalSnapshot::default(),
            &mid,
            &cex,
            &crate::trade_tape::TradeTapeSnapshot::default(),
        );
        assert!(signals.is_empty());
    }

    #[test]
    fn entry_blocked_peak_lead_without_cross() {
        assert!(!entry_activity_allows(&MidCrossSnapshot {
            peak_lead_gap: 0.12,
            ..MidCrossSnapshot::default()
        }));
    }

    #[test]
    fn ask_cap_blocks_040() {
        assert!(ask_in_band(0.39));
        assert!(!ask_in_band(0.40));
    }

    #[test]
    fn entry_sleep_rules_from_logs() {
        assert_eq!(entry_sleep_blocks(12, 55.0, "5m", "BTC"), None);
        assert_eq!(
            entry_sleep_blocks(9, 30.0, "15m", "BTC"),
            Some("sleep_utc09")
        );
        assert_eq!(
            entry_sleep_blocks(1, 30.0, "5m", "BTC"),
            Some("sleep_utc01_norm")
        );
        assert_eq!(
            entry_sleep_blocks(19, 50.0, "15m", "BTC"),
            Some("sleep_utc19_vol")
        );
        assert_eq!(
            entry_sleep_blocks(19, 50.0, "5m", "BTC"),
            Some("sleep_utc19_vol")
        );
        assert_eq!(entry_sleep_blocks(20, 50.0, "15m", "BTC"), None);
        assert_eq!(entry_sleep_blocks(17, 50.0, "15m", "BTC"), None);
        assert_eq!(entry_sleep_blocks(12, 55.0, "5m", "ETH"), None);
    }

    #[test]
    fn ask_in_band_accepts_target_prices() {
        assert!(ask_in_band(0.36));
        assert!(ask_in_band(0.38));
        assert!(ask_in_band(0.39));
        assert!(!ask_in_band(0.40));
        assert!(!ask_in_band(0.45));
    }
    #[test]
    fn gap_z_gate_blocks_large_distance() {
        assert!(gap_z_allows_entry(0.10));
        assert!(!gap_z_allows_entry(0.45));
    }

    #[test]
    fn entry_activity_and_salvage_gates() {
        assert!(entry_activity_allows(&MidCrossSnapshot {
            cross_count: 1,
            ..MidCrossSnapshot::default()
        }));
        assert!(!entry_activity_allows(&MidCrossSnapshot {
            peak_lead_gap: 0.10,
            ..MidCrossSnapshot::default()
        }));
        assert!(!entry_activity_allows(&MidCrossSnapshot::default()));
        assert!(is_vol_atr(45.0, "BTC"));
        assert!(is_vol_atr(89.9, "BTC"));
        assert!(!is_vol_atr(44.9, "BTC"));
        assert!(is_vol_atr(4.0, "ETH"));
        assert!(!is_vol_atr(2.0, "ETH"));
        assert!(!salvage_allows_otm_exit(-0.70, 0.25, 30.0, "BTC"));
        assert!(!salvage_allows_otm_exit(-0.70, 0.25, 55.0, "BTC"));
        assert!(salvage_allows_otm_exit(-1.3, 0.25, 55.0, "BTC"));
    }
}
