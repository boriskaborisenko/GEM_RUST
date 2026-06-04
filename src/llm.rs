use crate::client::{MarketWindow, PricesState};
use crate::strategy::{LlmForecast, SpotSignalSnapshot};
use gcp_auth::{CustomServiceAccount, TokenProvider};
use serde::Deserialize;
use serde_json::json;
use std::fs;
use std::path::Path;

pub struct LlmForecaster {
    service_account: CustomServiceAccount,
    project_id: String,
    model: String,
    location: String,
}

#[derive(Debug, Clone)]
pub struct LlmForecastRequest {
    pub asset: String,
    pub interval: String,
    pub current_time_utc: String,
    pub current_spot: Option<f64>,
    pub current_atr: f64,
    pub prices: PricesState,
    pub market: MarketWindow,
    pub secs_to_start: i64,
    pub spot_signal: SpotSignalSnapshot,
}

#[derive(Deserialize, Debug)]
struct VertexResponse {
    candidates: Vec<Candidate>,
}

#[derive(Deserialize, Debug)]
struct Candidate {
    content: Content,
    #[serde(rename = "finishReason")]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Debug)]
struct Content {
    parts: Option<Vec<Part>>,
}

#[derive(Deserialize, Debug)]
struct Part {
    text: String,
}

#[derive(Deserialize)]
struct LlmForecastWire {
    side: String,
    confidence: f64,
    signal_strength: Option<String>,
    reason_short: Option<String>,
    key_drivers: Option<Vec<String>>,
    risk_note: Option<String>,
}

#[derive(Deserialize)]
struct LlmHealthWire {
    ok: bool,
}

impl LlmForecaster {
    pub fn new<P: AsRef<Path>>(
        key_path: P,
        model: impl Into<String>,
        location: impl Into<String>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let path_ref = key_path.as_ref();
        if !path_ref.exists() {
            return Err(format!("LLM credentials not found: {}", path_ref.display()).into());
        }

        std::env::set_var("GOOGLE_APPLICATION_CREDENTIALS", path_ref);

        let key_content = fs::read_to_string(path_ref)?;
        let key_json: serde_json::Value = serde_json::from_str(&key_content)?;
        let project_id = key_json["project_id"]
            .as_str()
            .ok_or("llm.json is missing project_id")?
            .to_string();
        let service_account = CustomServiceAccount::from_file(key_path)?;

        Ok(Self {
            service_account,
            project_id,
            model: model.into(),
            location: location.into(),
        })
    }

    pub async fn health_check(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let text = self
            .generate_content_text(
                "Return strict JSON only: {\"ok\":true}",
                4096,
                "Vertex health check",
            )
            .await?;
        let json_text = extract_json_object(&text).unwrap_or(&text).trim();
        let wire: LlmHealthWire = serde_json::from_str(json_text).map_err(|e| {
            format!(
                "Vertex health check returned invalid JSON: {} | {}",
                e, text
            )
        })?;
        if wire.ok {
            Ok(())
        } else {
            Err(format!("Vertex health check returned ok=false: {}", text).into())
        }
    }

    pub async fn forecast(
        &self,
        req: LlmForecastRequest,
    ) -> Result<LlmForecast, Box<dyn std::error::Error + Send + Sync>> {
        let prompt = build_direction_prompt(&req);
        let text = self
            .generate_content_text(&prompt, 8192, "Vertex forecast")
            .await?;
        parse_forecast_json(&text)
    }

    async fn generate_content_text(
        &self,
        prompt: &str,
        max_output_tokens: u32,
        label: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let token = self
            .service_account
            .token(&["https://www.googleapis.com/auth/cloud-platform"])
            .await?;
        let url = self.endpoint_url();
        let payload = json!({
            "contents": [{
                "role": "user",
                "parts": [
                    { "text": prompt }
                ]
            }],
            "generationConfig": {
                "temperature": 0.15,
                "maxOutputTokens": max_output_tokens,
                "responseMimeType": "application/json",
                "thinkingConfig": self.thinking_config()
            }
        });

        let response = reqwest::Client::new()
            .post(&url)
            .bearer_auth(token.as_str())
            .json(&payload)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(format!("{} failed: {} {}", label, status, text).into());
        }

        let body = response.text().await?;
        let data = serde_json::from_str::<VertexResponse>(&body).map_err(|e| {
            format!(
                "{} response shape decode failed: {} | body: {}",
                label,
                e,
                compact_for_error(&body, 1200)
            )
        })?;
        let text = data
            .candidates
            .first()
            .and_then(|candidate| candidate.content.parts.as_ref())
            .and_then(|parts| parts.first())
            .map(|part| part.text.trim().to_string())
            .ok_or_else(|| {
                let finish_reason = data
                    .candidates
                    .first()
                    .and_then(|candidate| candidate.finish_reason.as_deref())
                    .unwrap_or("unknown");
                format!(
                    "{} response has no candidate text | finishReason: {} | body: {}",
                    label,
                    finish_reason,
                    compact_for_error(&body, 1200)
                )
            })?;
        Ok(text)
    }

    fn endpoint_url(&self) -> String {
        let location = self.location.trim();
        let api_host = if location.eq_ignore_ascii_case("global") {
            "aiplatform.googleapis.com".to_string()
        } else {
            format!("{}-aiplatform.googleapis.com", location)
        };
        format!(
            "https://{}/v1/projects/{}/locations/{}/publishers/google/models/{}:generateContent",
            api_host, self.project_id, location, self.model
        )
    }

    fn thinking_config(&self) -> serde_json::Value {
        let model = self.model.to_lowercase();
        if model.starts_with("gemini-3") {
            json!({
                "includeThoughts": false,
                "thinkingLevel": "LOW"
            })
        } else if model.contains("flash") {
            json!({
                "includeThoughts": false,
                "thinkingBudget": 0
            })
        } else {
            json!({
                "includeThoughts": false
            })
        }
    }
}

fn build_direction_prompt(req: &LlmForecastRequest) -> String {
    let open_time = req.market.start_time.clone();
    let spot = req
        .current_spot
        .map(|p| format!("{:.4}", p))
        .unwrap_or_else(|| "null".to_string());
    let raw_velocity = opt_num(req.spot_signal.raw_velocity_usd_per_sec);
    let smoothed_velocity = opt_num(req.spot_signal.smoothed_velocity_usd_per_sec);
    let acceleration = opt_num(req.spot_signal.acceleration_usd_per_sec2);

    format!(
        "You are a short-horizon directional signal assistant for Polymarket crypto UP/DOWN windows.\n\n\
Task:\n\
A new Polymarket {asset} Up/Down {interval} window opens at {open_time}.\n\
Before the window opens, choose which ONE side is directionally preferable to buy near parity: UP or DOWN.\n\n\
Important:\n\
- You are NOT deciding whether the trade is allowed.\n\
- Assume the trading bot will only buy if ask is near parity, around 0.48-0.52.\n\
- Your job is only to provide a directional micro-prior.\n\
- Do not chase current contract prices.\n\
- Since PTB is set at window open, focus on likely short-term direction AFTER the open.\n\
- If evidence is weak, still choose UP or DOWN, but use low confidence.\n\
- Output strict JSON only.\n\n\
Input JSON:\n\
{{\n\
  \"asset\": \"{asset}\",\n\
  \"interval\": \"{interval}\",\n\
  \"open_time_utc\": \"{open_time}\",\n\
  \"current_time_utc\": \"{current_time}\",\n\
  \"secs_to_start\": {secs_to_start},\n\
  \"current_spot\": {spot},\n\
  \"atr_1m\": {atr:.4},\n\
  \"up_bid\": {up_bid:.4},\n\
  \"up_ask\": {up_ask:.4},\n\
  \"down_bid\": {down_bid:.4},\n\
  \"down_ask\": {down_ask:.4},\n\
  \"combined_ask\": {combined:.4},\n\
  \"raw_velocity_usd_per_sec\": {raw_velocity},\n\
  \"smoothed_velocity_usd_per_sec\": {smoothed_velocity},\n\
  \"acceleration_usd_per_sec2\": {acceleration}\n\
}}\n\n\
Return JSON schema:\n\
{{\n\
  \"side\": \"UP or DOWN\",\n\
  \"confidence\": 0.0,\n\
  \"signal_strength\": \"weak | medium | strong\",\n\
  \"reason_short\": \"one sentence\",\n\
  \"key_drivers\": [\"driver 1\", \"driver 2\", \"driver 3\"],\n\
  \"risk_note\": \"one sentence\"\n\
}}",
        asset = req.asset,
        interval = req.interval,
        open_time = open_time,
        current_time = req.current_time_utc,
        secs_to_start = req.secs_to_start,
        spot = spot,
        atr = req.current_atr,
        up_bid = req.prices.up.bid,
        up_ask = req.prices.up.ask,
        down_bid = req.prices.down.bid,
        down_ask = req.prices.down.ask,
        combined = req.prices.up.ask + req.prices.down.ask,
        raw_velocity = raw_velocity,
        smoothed_velocity = smoothed_velocity,
        acceleration = acceleration,
    )
}

fn opt_num(value: Option<f64>) -> String {
    value
        .map(|v| format!("{:.6}", v))
        .unwrap_or_else(|| "null".to_string())
}

fn parse_forecast_json(
    text: &str,
) -> Result<LlmForecast, Box<dyn std::error::Error + Send + Sync>> {
    let json_text = extract_json_object(text).unwrap_or(text).trim();
    let wire: LlmForecastWire = serde_json::from_str(json_text)?;
    let side = wire.side.trim().to_uppercase();
    if side != "UP" && side != "DOWN" {
        return Err(format!("Invalid LLM side: {}", wire.side).into());
    }
    Ok(LlmForecast {
        side,
        confidence: wire.confidence.clamp(0.0, 1.0),
        signal_strength: wire
            .signal_strength
            .unwrap_or_else(|| "unknown".to_string()),
        reason_short: wire.reason_short.unwrap_or_default(),
        key_drivers: wire.key_drivers.unwrap_or_default(),
        risk_note: wire.risk_note.unwrap_or_default(),
    })
}

fn extract_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end >= start {
        Some(&text[start..=end])
    } else {
        None
    }
}

fn compact_for_error(text: &str, max_chars: usize) -> String {
    let mut compact = text.replace('\n', " ");
    if compact.len() > max_chars {
        compact.truncate(max_chars);
        compact.push_str("...");
    }
    compact
}
