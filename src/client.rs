#![allow(dead_code, unused_imports, non_snake_case, unused_variables, unused_mut)]

use serde::{Deserialize, Serialize};
use serde_json::json;
use serde_json::Value;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use futures_util::{SinkExt, StreamExt};

const GAMMA_API: &str = "https://gamma-api.polymarket.com";
const CLOB_REST: &str = "https://clob.polymarket.com";
const CLOB_WS: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";
const CHAINLINK_WS: &str = "wss://ws-live-data.polymarket.com";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenInfo {
    #[serde(rename = "tokenId")]
    pub token_id: String,
    #[serde(rename = "outcomeName")]
    pub outcome_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokensMap {
    #[serde(rename = "UP")]
    pub up: TokenInfo,
    #[serde(rename = "DOWN")]
    pub down: TokenInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketWindow {
    #[serde(rename = "marketId")]
    pub id: String,
    pub slug: String,
    pub question: String,
    pub asset: String,
    pub interval: String,
    #[serde(rename = "startTime")]
    pub start_time: String,
    #[serde(rename = "endTime")]
    pub end_time: String,
    #[serde(rename = "priceToBeat")]
    pub price_to_beat: Option<f64>,
    pub tokens: TokensMap,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContractPrices {
    pub bid: f64,
    pub ask: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PricesState {
    pub up: ContractPrices,
    pub down: ContractPrices,
}

#[derive(Debug, Clone)]
pub enum MarketEvent {
    Log(String),
    SpotTick {
        asset: String,
        price: f64,
        timestamp: i64,
    },
    MarketTick {
        window_number: usize,
        role: String,
        market: MarketWindow,
        prices: PricesState,
        timestamp: i64,
    },
}

// Global offset between local system clock and server clock (updated at startup)
static mut TIME_OFFSET_MS: i64 = 0;

pub fn set_time_offset(offset: i64) {
    unsafe {
        TIME_OFFSET_MS = offset;
    }
}

pub fn get_now_ms() -> i64 {
    let local = chrono::Utc::now().timestamp_millis();
    unsafe { local + TIME_OFFSET_MS }
}

/**
 * Sync clock offset with Polymarket Gamma HTTP date header on process startup.
 */
pub async fn fetch_time_offset() -> anyhow::Result<i64> {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(5)).build()?;
    // Query fast events endpoint with limit 1
    let res = client.get(format!("{}/events?limit=1", GAMMA_API)).send().await?;
    
    if let Some(date_header) = res.headers().get(reqwest::header::DATE) {
        if let Ok(date_str) = date_header.to_str() {
            if let Ok(server_time) = chrono::DateTime::parse_from_rfc2822(date_str) {
                let server_ms = server_time.timestamp_millis();
                let local_ms = chrono::Utc::now().timestamp_millis();
                return Ok(server_ms - local_ms);
            }
        }
    }
    
    anyhow::bail!("Failed to read server Date header")
}

/**
 * Search Gamma API for BTC/ETH/SOL Up/Down windows using exact-match slug lookups
 * similar to the implementation in proto_v08_Rust's polymarket connector.
 */
pub async fn find_upcoming_markets(asset: &str, interval: &str) -> Vec<MarketWindow> {
    let client = match reqwest::Client::builder().timeout(Duration::from_secs(8)).build() {
        Ok(c) => c,
        Err(_) => return vec![],
    };

    let mut results = vec![];
    let slug_prefix = format!("{}-updown-{}", asset.to_lowercase(), interval);
    let bucket_sec = if interval == "15m" { 900 } else { 300 };
    let now_sec = get_now_ms() / 1000;
    let current_bucket = (now_sec / bucket_sec) * bucket_sec;

    // Scan current + next 6 buckets
    for i in -1..=6 {
        let target_sec = current_bucket + i * bucket_sec;
        let slug = format!("{}-{}", slug_prefix, target_sec);
        let url = format!("{}/markets/slug/{}", GAMMA_API, slug);
        
        if let Ok(res) = client.get(&url).send().await {
            if let Ok(val) = res.json::<Value>().await {
                // Parse either as single object or array
                let m = if val.is_array() {
                    val.as_array().and_then(|arr| arr.first()).cloned()
                } else {
                    Some(val)
                };

                if let Some(m) = m {
                    if let Some(parsed) = parse_market(&m, asset, interval) {
                        results.push(parsed);
                    }
                }
            }
        }
    }

    // Deduplicate by slug
    results.sort_by_key(|m| m.slug.clone());
    results.dedup_by(|a, b| a.slug == b.slug);

    // Sort by start time ascending
    results.sort_by_key(|m| m.start_time.clone());
    results
}

pub async fn find_active_market(asset: &str, interval: &str) -> Option<MarketWindow> {
    let markets = find_upcoming_markets(asset, interval).await;
    let now_ms = get_now_ms();

    for m in markets {
        if let (Ok(start), Ok(end)) = (chrono::DateTime::parse_from_rfc3339(&m.start_time), chrono::DateTime::parse_from_rfc3339(&m.end_time)) {
            let start_ms = start.timestamp_millis();
            let end_ms = end.timestamp_millis();
            if now_ms >= start_ms && now_ms < end_ms {
                return Some(m);
            }
        }
    }
    None
}

pub async fn find_next_market(asset: &str, interval: &str, after_time_ms: i64, exclude_slugs: &[String]) -> Option<MarketWindow> {
    let markets = find_upcoming_markets(asset, interval).await;

    for m in markets {
        if let Ok(start) = chrono::DateTime::parse_from_rfc3339(&m.start_time) {
            let start_ms = start.timestamp_millis();
            if start_ms > after_time_ms && !exclude_slugs.contains(&m.slug) {
                return Some(m);
            }
        }
    }
    None
}

/**
 * Fetch orderbook snapshot for a token via CLOB REST.
 */
pub async fn get_book_snapshot(token_id: &str) -> (f64, f64) {
    let client = match reqwest::Client::builder().timeout(Duration::from_secs(5)).build() {
        Ok(c) => c,
        Err(_) => return (0.0, 0.0),
    };

    let url = format!("{}/book?token_id={}", CLOB_REST, token_id);
    if let Ok(res) = client.get(&url).send().await {
        if let Ok(book) = res.json::<Value>().await {
            let mut bids = vec![];
            let mut asks = vec![];

            if let Some(bids_arr) = book.get("bids").and_then(|v| v.as_array()) {
                for b in bids_arr {
                    if let (Some(px), Some(sz)) = (b.get("price").and_then(|v| v.as_str()), b.get("size").and_then(|v| v.as_str())) {
                        if let (Ok(price), Ok(_)) = (px.parse::<f64>(), sz.parse::<f64>()) {
                            bids.push(price);
                        }
                    }
                }
            }

            if let Some(asks_arr) = book.get("asks").and_then(|v| v.as_array()) {
                for a in asks_arr {
                    if let (Some(px), Some(sz)) = (a.get("price").and_then(|v| v.as_str()), a.get("size").and_then(|v| v.as_str())) {
                        if let (Ok(price), Ok(_)) = (px.parse::<f64>(), sz.parse::<f64>()) {
                            asks.push(price);
                        }
                    }
                }
            }

            bids.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
            asks.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

            let best_bid = bids.first().cloned().unwrap_or(0.0);
            let best_ask = asks.first().cloned().unwrap_or(0.0);
            return (best_bid, best_ask);
        }
    }
    (0.0, 0.0)
}

pub async fn get_market_snapshot(market: &MarketWindow) -> PricesState {
    let (up_bid, up_ask) = get_book_snapshot(&market.tokens.up.token_id).await;
    let (dn_bid, dn_ask) = get_book_snapshot(&market.tokens.down.token_id).await;
    PricesState {
        up: ContractPrices { bid: up_bid, ask: up_ask },
        down: ContractPrices { bid: dn_bid, ask: dn_ask },
    }
}

// ─── Real-time Event-Driven Streams with Robust Reconnect ───────

/**
 * Subscribe to live contract prices via CLOB WebSocket.
 * Spawns an isolated background loop with automatic reconnection.
 */
pub fn subscribe_prices(
    window_number: usize,
    role: String,
    market: MarketWindow,
    tx: mpsc::UnboundedSender<MarketEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tx.send(MarketEvent::Log(format!("Connecting to CLOB WS for Window #{} [{}]...", window_number, role))).unwrap_or_default();
            
            let (mut ws, _) = match connect_async(CLOB_WS).await {
                Ok(conn) => conn,
                Err(e) => {
                    tx.send(MarketEvent::Log(format!("CLOB WS connection failed: {}. Retrying...", e))).unwrap_or_default();
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };

            tx.send(MarketEvent::Log(format!("CLOB WS Connected for Window #{} [{}]", window_number, role))).unwrap_or_default();

            // Subscribe to both tokens
            let sub_msg = json!({
                "assets_ids": [market.tokens.up.token_id, market.tokens.down.token_id],
                "type": "market"
            });
            if let Err(_) = ws.send(Message::Text(sub_msg.to_string().into())).await {
                continue;
            }

            // Fetch snapshot to fill prices immediately
            let mut prices = get_market_snapshot(&market).await;
            tx.send(MarketEvent::MarketTick {
                window_number,
                role: role.clone(),
                market: market.clone(),
                prices: prices.clone(),
                timestamp: get_now_ms(),
            }).unwrap_or_default();

            // Price updates receiver loop
            while let Some(msg) = ws.next().await {
                if let Ok(Message::Text(text)) = msg {
                    if let Ok(raw) = serde_json::from_str::<Value>(&text) {
                        let msgs = if raw.is_array() {
                            raw.as_array().cloned().unwrap_or_default()
                        } else {
                            vec![raw]
                        };

                        let mut updated = false;
                        for m in msgs {
                            let event_type = m.get("event_type").and_then(|v| v.as_str()).unwrap_or("");
                            let asset_id = m.get("asset_id").and_then(|v| v.as_str()).unwrap_or("");
                            
                            let side = if asset_id == market.tokens.up.token_id {
                                Some("UP")
                            } else if asset_id == market.tokens.down.token_id {
                                Some("DOWN")
                            } else {
                                None
                            };

                            if let Some(side) = side {
                                if event_type == "book" {
                                    let bids = m.get("bids").and_then(|v| v.as_array());
                                    let asks = m.get("asks").and_then(|v| v.as_array());
                                    
                                    let mut up_bid = prices.up.bid;
                                    let mut up_ask = prices.up.ask;
                                    let mut dn_bid = prices.down.bid;
                                    let mut dn_ask = prices.down.ask;

                                    if side == "UP" {
                                        if let Some(b) = bids {
                                            let mut parsed: Vec<f64> = b.iter().filter_map(|v| v.get("price").and_then(|p| p.as_str()).and_then(|p| p.parse().ok())).collect();
                                            parsed.sort_by(|x, y| y.partial_cmp(x).unwrap_or(std::cmp::Ordering::Equal));
                                            if let Some(best) = parsed.first() { up_bid = *best; }
                                        }
                                        if let Some(a) = asks {
                                            let mut parsed: Vec<f64> = a.iter().filter_map(|v| v.get("price").and_then(|p| p.as_str()).and_then(|p| p.parse().ok())).collect();
                                            parsed.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
                                            if let Some(best) = parsed.first() { up_ask = *best; }
                                        }
                                    } else {
                                        if let Some(b) = bids {
                                            let mut parsed: Vec<f64> = b.iter().filter_map(|v| v.get("price").and_then(|p| p.as_str()).and_then(|p| p.parse().ok())).collect();
                                            parsed.sort_by(|x, y| y.partial_cmp(x).unwrap_or(std::cmp::Ordering::Equal));
                                            if let Some(best) = parsed.first() { dn_bid = *best; }
                                        }
                                        if let Some(a) = asks {
                                            let mut parsed: Vec<f64> = a.iter().filter_map(|v| v.get("price").and_then(|p| p.as_str()).and_then(|p| p.parse().ok())).collect();
                                            parsed.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
                                            if let Some(best) = parsed.first() { dn_ask = *best; }
                                        }
                                    }

                                    prices = PricesState {
                                        up: ContractPrices { bid: up_bid, ask: up_ask },
                                        down: ContractPrices { bid: dn_bid, ask: dn_ask },
                                    };
                                    updated = true;
                                } else if event_type == "price_change" || event_type == "tick_size_change" || event_type == "last_trade_price" {
                                    if let Some(px_val) = m.get("price").or_else(|| m.get("last_trade_price")) {
                                        let px_str = px_val.as_str().unwrap_or("0");
                                        if let Ok(price) = px_str.parse::<f64>() {
                                            if price > 0.0 {
                                                if side == "UP" {
                                                    prices.up.bid = price;
                                                    if prices.up.ask <= price { prices.up.ask = price + 0.01; }
                                                } else {
                                                    prices.down.bid = price;
                                                    if prices.down.ask <= price { prices.down.ask = price + 0.01; }
                                                }
                                                updated = true;
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        if updated {
                            tx.send(MarketEvent::MarketTick {
                                window_number,
                                role: role.clone(),
                                market: market.clone(),
                                prices: prices.clone(),
                                timestamp: get_now_ms(),
                            }).unwrap_or_default();
                        }
                    }
                }
            }
            tx.send(MarketEvent::Log(format!("CLOB WS connection closed for Window #{} [{}]. Reconnecting in 3s...", window_number, role))).unwrap_or_default();
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    })
}

/**
 * Subscribe to the Polymarket public spot price feed WebSocket with automatic reconnect.
 */
pub fn subscribe_chainlink(asset: String, tx: mpsc::UnboundedSender<MarketEvent>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tx.send(MarketEvent::Log(format!("Connecting to Chainlink Spot WS for {}...", asset))).unwrap_or_default();

            let (mut ws, _) = match connect_async(CHAINLINK_WS).await {
                Ok(conn) => conn,
                Err(e) => {
                    tx.send(MarketEvent::Log(format!("Chainlink WS connection failed: {}. Retrying...", e))).unwrap_or_default();
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };

            tx.send(MarketEvent::Log(format!("Chainlink Spot WS Connected for {}", asset))).unwrap_or_default();

            let symbol = format!("{}/usd", asset.to_lowercase());
            let sub_msg = json!({
                "action": "subscribe",
                "subscriptions": [
                    {
                        "topic": "crypto_prices_chainlink",
                        "type": "*",
                        "filters": json!({ "symbol": symbol }).to_string()
                    }
                ]
            });

            if let Err(_) = ws.send(Message::Text(sub_msg.to_string().into())).await {
                continue;
            }

            let mut ws_ping = ws;
            let asset_ping = asset.clone();
            let tx_ping = tx.clone();

            let mut ping_interval = tokio::time::interval(Duration::from_secs(5));
            loop {
                tokio::select! {
                    _ = ping_interval.tick() => {
                        if let Err(_) = ws_ping.send(Message::Text("PING".into())).await {
                            break;
                        }
                    }
                    msg = ws_ping.next() => {
                        match msg {
                            Some(Ok(Message::Text(text))) => {
                                if text == "PONG" {
                                    continue;
                                }
                                if let Ok(payload) = serde_json::from_str::<Value>(&text) {
                                    if payload.get("topic").and_then(|v| v.as_str()) == Some("crypto_prices_chainlink") {
                                        if let Some(inner) = payload.get("payload") {
                                            let sym = inner.get("symbol").and_then(|v| v.as_str()).unwrap_or("");
                                            if sym.to_lowercase() == symbol.to_lowercase() {
                                                if let Some(px_val) = inner.get("value") {
                                                    let price = match px_val {
                                                        Value::Number(n) => n.as_f64().unwrap_or(0.0),
                                                        Value::String(s) => s.parse::<f64>().unwrap_or(0.0),
                                                        _ => 0.0,
                                                    };
                                                    let timestamp = inner.get("timestamp").and_then(|v| v.as_i64()).unwrap_or_else(|| get_now_ms());
                                                    if price > 0.0 {
                                                        tx_ping.send(MarketEvent::SpotTick {
                                                            asset: asset_ping.clone(),
                                                            price,
                                                            timestamp,
                                                        }).unwrap_or_default();
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            _ => break,
                        }
                    }
                }
            }

            tx.send(MarketEvent::Log(format!("Chainlink Spot WS disconnected. Reconnecting in 3s..."))).unwrap_or_default();
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    })
}

// ─── Parsing Helper ─────────────────────────────────────────────

fn parse_market(m: &Value, asset: &str, interval: &str) -> Option<MarketWindow> {
    let slug = m.get("slug")?.as_str()?.to_string();
    
    // Parse times
    let bucket_sec = if interval == "15m" { 900 } else { 300 };
    let slug_parts: Vec<&str> = slug.split('-').collect();
    let slug_timestamp = slug_parts.last().and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
    
    let (start_ms, end_ms) = if slug_timestamp > 1000000000 {
        let s = slug_timestamp * 1000;
        (s, s + bucket_sec * 1000)
    } else if let Some(start_date) = m.get("startDate").and_then(|v| v.as_str()) {
        if let Ok(start) = chrono::DateTime::parse_from_rfc3339(start_date) {
            let s = start.timestamp_millis();
            let e = m.get("endDate").and_then(|v| v.as_str())
                .and_then(|d| chrono::DateTime::parse_from_rfc3339(d).ok())
                .map(|t| t.timestamp_millis())
                .unwrap_or(s + bucket_sec * 1000);
            (s, e)
        } else {
            return None;
        }
    } else {
        return None;
    };

    let clob_token_ids = m.get("clobTokenIds")?;
    let clob_token_ids_parsed: Vec<String> = if clob_token_ids.is_array() {
        clob_token_ids.as_array()?.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect()
    } else {
        let s = clob_token_ids.as_str()?;
        serde_json::from_str(s).ok()?
    };

    let outcomes_val = m.get("outcomes")?;
    let outcomes_parsed: Vec<String> = if outcomes_val.is_array() {
        outcomes_val.as_array()?.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect()
    } else {
        let s = outcomes_val.as_str()?;
        serde_json::from_str(s).ok()?
    };

    if clob_token_ids_parsed.len() < 2 {
        return None;
    }

    let up_index = outcomes_parsed.iter().position(|label| {
        let l = label.to_lowercase();
        l == "up" || l == "yes"
    }).unwrap_or(0);

    let dn_index = outcomes_parsed.iter().position(|label| {
        let l = label.to_lowercase();
        l == "down" || l == "no"
    }).unwrap_or(if up_index == 0 { 1 } else { 0 });

    let up_token_id = clob_token_ids_parsed.get(up_index)?.to_string();
    let dn_token_id = clob_token_ids_parsed.get(dn_index)?.to_string();

    let mut price_to_beat = None;
    if let Some(events) = m.get("events").and_then(|v| v.as_array()) {
        if let Some(ev) = events.first() {
            if let Some(meta) = ev.get("eventMetadata") {
                if let Some(ptb_val) = meta.get("priceToBeat") {
                    price_to_beat = match ptb_val {
                        Value::Number(n) => n.as_f64(),
                        Value::String(s) => s.parse::<f64>().ok(),
                        _ => None,
                    };
                }
            }
        }
    }

    if price_to_beat.is_none() {
        let question = m.get("question").and_then(|v| v.as_str()).unwrap_or("");
        price_to_beat = parse_strike_from_text(question, asset);
    }

    Some(MarketWindow {
        id: m.get("id")?.to_string().trim_matches('"').to_string(),
        slug,
        question: m.get("question").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        asset: asset.to_string(),
        interval: interval.to_string(),
        start_time: chrono::DateTime::from_timestamp(start_ms / 1000, 0)?.to_rfc3339(),
        end_time: chrono::DateTime::from_timestamp(end_ms / 1000, 0)?.to_rfc3339(),
        price_to_beat,
        tokens: TokensMap {
            up: TokenInfo { token_id: up_token_id, outcome_name: "Up".to_string() },
            down: TokenInfo { token_id: dn_token_id, outcome_name: "Down".to_string() },
        },
    })
}

fn parse_strike_from_text(text: &str, asset: &str) -> Option<f64> {
    let asset_upper = asset.to_uppercase();
    
    // Extract all numbers, keeping track of whether they are preceded by '$'
    let mut candidates = vec![];
    let mut current = String::new();
    let mut is_after_dollar = false;
    let mut has_decimal = false;
    
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '$' {
            is_after_dollar = true;
            current.clear();
        } else if c.is_ascii_digit() {
            current.push(c);
        } else if c == ',' && !current.is_empty() {
            // skip commas inside numbers
        } else if c == '.' && !current.is_empty() && !has_decimal {
            if i + 1 < chars.len() && chars[i + 1].is_ascii_digit() {
                current.push('.');
                has_decimal = true;
            } else {
                if let Ok(val) = current.parse::<f64>() {
                    candidates.push((val, is_after_dollar));
                }
                current.clear();
                is_after_dollar = false;
                has_decimal = false;
            }
        } else {
            if !current.is_empty() {
                if let Ok(val) = current.parse::<f64>() {
                    candidates.push((val, is_after_dollar));
                }
                current.clear();
                is_after_dollar = false;
                has_decimal = false;
            }
        }
        i += 1;
    }
    if !current.is_empty() {
        if let Ok(val) = current.parse::<f64>() {
            candidates.push((val, is_after_dollar));
        }
    }

    // Filter candidates strictly by valid price range for the specified asset
    let min_allowed = match asset_upper.as_str() {
        "BTC" => 10000.0,
        "ETH" => 1000.0,
        "SOL" => 10.0,
        _ => 0.0,
    };

    let mut filtered: Vec<(f64, bool)> = candidates.into_iter()
        .filter(|&(val, _)| val >= min_allowed)
        .collect();

    if filtered.is_empty() {
        return None;
    }

    // Heuristic 1: If there's any valid candidate directly preceded by '$', return it first!
    if let Some(&(val, _)) = filtered.iter().find(|(_, dollar)| *dollar) {
        return Some(val);
    }

    // Fallback: return the largest remaining candidate (sorted descending)
    filtered.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    filtered.first().map(|&(val, _)| val)
}
