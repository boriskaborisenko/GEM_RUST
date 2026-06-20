pub const REDEEM_HOLD_LATE_SECS: i64 = 180;
pub const REDEEM_HOLD_LATE_TIME_PCT: f64 = 75.0;
pub const REDEEM_HOLD_MIN_ITM_GAP_Z: f64 = 1.0;
pub const REDEEM_HOLD_MIN_ITM_PCT: f64 = 0.04;
pub const REDEEM_HOLD_MIN_FAIR_PROB: f64 = 0.65;
pub const REDEEM_HOLD_NEAR_REDEEM_BID: f64 = 0.95;
pub const REDEEM_HOLD_SALVAGE_MAX_FAIR_PROB: f64 = 0.50;
pub const REDEEM_HOLD_MIN_VALID_ATR: f64 = 1.0;

#[derive(Debug, Clone)]
pub struct RedeemHoldInput<'a> {
    pub side: &'a str,
    pub spot: f64,
    pub ptb: f64,
    pub secs_to_end: i64,
    pub time_pct: f64,
    pub current_atr: f64,
    pub bid: f64,
    pub fair_prob: f64,
    pub ptb_crossed: bool,
    pub counter_velocity_against: bool,
    pub cex_velocity_against: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RedeemHoldDecision {
    pub should_hold: bool,
    pub reason: &'static str,
}

pub fn expected_move_usd(current_atr: f64, secs_left: i64) -> f64 {
    current_atr.max(REDEEM_HOLD_MIN_VALID_ATR) * ((secs_left as f64).max(1.0) / 60.0).sqrt()
}

pub fn itm_gap_z(side: &str, spot: f64, ptb: f64, current_atr: f64, secs_to_end: i64) -> f64 {
    let distance = if side == "UP" {
        (spot - ptb).max(0.0)
    } else {
        (ptb - spot).max(0.0)
    };
    let expected_move = expected_move_usd(current_atr, secs_to_end);
    if expected_move <= 0.0 {
        return 0.0;
    }
    distance / expected_move
}

pub fn side_is_itm(side: &str, spot: f64, ptb: f64) -> bool {
    if side == "UP" {
        spot > ptb
    } else {
        spot < ptb
    }
}

pub fn evaluate_redeem_hold(input: &RedeemHoldInput<'_>) -> RedeemHoldDecision {
    if !side_is_itm(input.side, input.spot, input.ptb) {
        return RedeemHoldDecision {
            should_hold: false,
            reason: "otm",
        };
    }

    if input.ptb_crossed {
        return RedeemHoldDecision {
            should_hold: false,
            reason: "ptb_crossed",
        };
    }

    if input.fair_prob < REDEEM_HOLD_SALVAGE_MAX_FAIR_PROB {
        return RedeemHoldDecision {
            should_hold: false,
            reason: "salvage_overpay",
        };
    }

    if input.bid >= REDEEM_HOLD_NEAR_REDEEM_BID {
        return RedeemHoldDecision {
            should_hold: false,
            reason: "bid_near_par",
        };
    }

    let late_time =
        input.secs_to_end <= REDEEM_HOLD_LATE_SECS || input.time_pct >= REDEEM_HOLD_LATE_TIME_PCT;
    if !late_time {
        return RedeemHoldDecision {
            should_hold: false,
            reason: "time_not_late",
        };
    }

    let gap_z = itm_gap_z(
        input.side,
        input.spot,
        input.ptb,
        input.current_atr,
        input.secs_to_end,
    );
    let itm_pct = if input.ptb.abs() > 0.0 {
        ((input.spot - input.ptb).abs() / input.ptb.abs()) * 100.0
    } else {
        0.0
    };
    let deep_itm = gap_z >= REDEEM_HOLD_MIN_ITM_GAP_Z || itm_pct >= REDEEM_HOLD_MIN_ITM_PCT;
    if !deep_itm {
        return RedeemHoldDecision {
            should_hold: false,
            reason: "distance_not_deep",
        };
    }

    if input.fair_prob < REDEEM_HOLD_MIN_FAIR_PROB {
        return RedeemHoldDecision {
            should_hold: false,
            reason: "fair_prob_low",
        };
    }

    if input.counter_velocity_against || input.cex_velocity_against {
        return RedeemHoldDecision {
            should_hold: false,
            reason: "counter_momentum",
        };
    }

    RedeemHoldDecision {
        should_hold: true,
        reason: "itm_deep_late",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_input() -> RedeemHoldInput<'static> {
        RedeemHoldInput {
            side: "UP",
            spot: 100_500.0,
            ptb: 100_000.0,
            secs_to_end: 60,
            time_pct: 92.0,
            current_atr: 40.0,
            bid: 0.80,
            fair_prob: 0.85,
            ptb_crossed: false,
            counter_velocity_against: false,
            cex_velocity_against: false,
        }
    }

    #[test]
    fn holds_itm_deep_late() {
        let decision = evaluate_redeem_hold(&base_input());
        assert!(decision.should_hold);
        assert_eq!(decision.reason, "itm_deep_late");
    }

    #[test]
    fn no_hold_when_distance_shallow() {
        let mut input = base_input();
        input.spot = 100_020.0;
        let decision = evaluate_redeem_hold(&input);
        assert!(!decision.should_hold);
        assert_eq!(decision.reason, "distance_not_deep");
    }

    #[test]
    fn no_hold_when_otm() {
        let mut input = base_input();
        input.spot = 99_500.0;
        let decision = evaluate_redeem_hold(&input);
        assert!(!decision.should_hold);
        assert_eq!(decision.reason, "otm");
    }

    #[test]
    fn no_hold_on_counter_velocity() {
        let mut input = base_input();
        input.counter_velocity_against = true;
        let decision = evaluate_redeem_hold(&input);
        assert!(!decision.should_hold);
        assert_eq!(decision.reason, "counter_momentum");
    }
}
