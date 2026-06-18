/// Per-asset spot/strike ranges and display formatting for up/down crypto markets.

pub fn is_supported(asset: &str) -> bool {
    matches!(
        asset.to_uppercase().as_str(),
        "BTC" | "ETH" | "SOL" | "XRP" | "DOGE"
    )
}

pub fn strike_min(asset: &str) -> f64 {
    match asset.to_uppercase().as_str() {
        "BTC" => 1_000.0,
        "ETH" => 100.0,
        "SOL" => 1.0,
        "XRP" => 0.01,
        "DOGE" => 0.0001,
        _ => 0.0,
    }
}

pub fn strike_max(asset: &str) -> f64 {
    match asset.to_uppercase().as_str() {
        "BTC" => 2_000_000.0,
        "ETH" => 50_000.0,
        "SOL" => 10_000.0,
        "XRP" => 100.0,
        "DOGE" => 10.0,
        _ => f64::MAX,
    }
}

/// True when parsed PTB cannot be the window open price (e.g. datetime digits mistaken for strike).
pub fn ptb_implausible(asset: &str, ptb: f64, spot: f64) -> bool {
    if ptb <= 0.0 || spot <= 0.0 {
        return true;
    }
    if ptb < strike_min(asset) || ptb > strike_max(asset) {
        return true;
    }
    let ratio = (ptb / spot).max(spot / ptb);
    ratio > 3.0
}

pub fn format_asset_price(asset: &str, price: f64) -> String {
    match asset.to_uppercase().as_str() {
        "BTC" | "ETH" => format!("${:.2}", price),
        "SOL" => format!("${:.4}", price),
        "XRP" | "DOGE" => format!("${:.6}", price),
        _ => format!("${:.4}", price),
    }
}

pub fn format_atr(asset: &str, atr: f64) -> String {
    format_asset_price(asset, atr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sol_ptb_35_vs_spot_140_is_implausible() {
        assert!(ptb_implausible("SOL", 35.0, 140.0));
    }

    #[test]
    fn sol_chainlink_ptb_near_spot_is_plausible() {
        assert!(!ptb_implausible("SOL", 139.85, 140.10));
    }

    #[test]
    fn format_xrp_and_doge_decimals() {
        assert_eq!(format_asset_price("XRP", 2.345678), "$2.345678");
        assert_eq!(format_asset_price("DOGE", 0.123456), "$0.123456");
    }
}
