use crate::client::{get_now_ms, MarketWindow};
use crate::config::{ExecutionConfig, ExecutionMode, LiveMarketOrderType, LiveSignatureType};
use crate::strategy::{OrderOperation, OrderSignal, OrderType as StrategyOrderType};
use alloy::signers::local::PrivateKeySigner;
use alloy::signers::Signer as _;
use anyhow::{anyhow, Context, Result};
use polymarket_client_sdk_v2::auth::{state::Authenticated, Normal};
use polymarket_client_sdk_v2::clob::types::request::{
    BalanceAllowanceRequest, UpdateBalanceAllowanceRequest,
};
use polymarket_client_sdk_v2::clob::types::{
    Amount, AssetType, OrderStatusType, OrderType as ClobOrderType, Side as ClobSide, SignatureType,
};
use polymarket_client_sdk_v2::clob::{Client as ClobClient, Config as ClobConfig};
use polymarket_client_sdk_v2::types::{Address, Decimal, U256};
use polymarket_client_sdk_v2::POLYGON;
use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr as _;
use std::sync::Arc;
use tokio::sync::Mutex;

const LIVE_FOK_RETRY_ATTEMPTS: usize = 3;
const LIVE_FOK_RETRY_DELAY_MS: u64 = 250;

#[derive(Debug, Clone, PartialEq)]
pub struct LiveOrderIntent {
    pub operation: OrderOperation,
    pub strategy_order_type: StrategyOrderType,
    pub clob_order_type: String,
    pub side: String,
    pub token_id: String,
    pub price: f64,
    pub amount_usd: Option<f64>,
    pub shares: f64,
    pub estimated_notional_usd: f64,
    pub post_only: bool,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LiveFill {
    pub amount_usd: f64,
    pub shares: f64,
    pub avg_price: f64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LiveExecutionResult {
    pub submitted: bool,
    /// Strategy should call `notify_order_executed` when true (instant fill or resting limit accepted).
    pub executed: bool,
    pub dry_run: bool,
    pub reject_reason: String,
    pub order_id: Option<String>,
    pub status: Option<String>,
    pub trade_count: usize,
    pub transaction_count: usize,
    pub fill: Option<LiveFill>,
    pub intent: Option<LiveOrderIntent>,
    pub raw_making_amount: Option<String>,
    pub raw_taking_amount: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LiveWarmupStatus {
    pub balance_usd: f64,
    pub allowance_contracts: usize,
    pub relayer_configured: bool,
}

/// Latest CLOB account snapshot for terminal display (balance refreshes on window switch).
#[derive(Debug, Clone, Default)]
pub struct LiveAccountStatus {
    pub authenticated: bool,
    pub balance_usd: f64,
    pub allowance_contracts: usize,
    pub ready_to_trade: bool,
    pub dry_run: bool,
    pub relayer_configured: bool,
    pub signer_address: String,
    pub funder_address: String,
    pub last_error: Option<String>,
    pub updated_at_ms: i64,
    pub window_number: Option<usize>,
}

/// Cached authenticated CLOB session (one client per bot run, heartbeats enabled).
pub struct LiveExecutorSession {
    cfg: ExecutionConfig,
    signer: PrivateKeySigner,
    client: Mutex<ClobClient<Authenticated<Normal>>>,
    secrets: LiveSecrets,
}

impl LiveExecutorSession {
    pub async fn connect(cfg: &ExecutionConfig) -> Result<Arc<Self>> {
        if cfg.mode != ExecutionMode::Live {
            return Err(anyhow!("live session requires execution.mode=live"));
        }
        let secrets = LiveSecrets::load(&cfg.secrets_file)?;
        let signer = secrets.signer()?;
        validate_deposit_wallet_live_config(cfg, &secrets, &signer)?;
        let client = authenticate_clob_client(cfg, &secrets, &signer).await?;
        Ok(Arc::new(Self {
            cfg: cfg.clone(),
            signer,
            client: Mutex::new(client),
            secrets,
        }))
    }

    /// GEM_RUST uses Polymarket deposit wallets (POLY_1271), not proto Safe/Proxy.
    pub fn wallet_profile(&self) -> &'static str {
        "DEPOSIT_WALLET/POLY_1271"
    }

    pub fn signer_address(&self) -> String {
        format!("{:#x}", self.signer.address())
    }

    pub fn configured_funder_address(&self) -> String {
        self.cfg
            .funder_address
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(ToOwned::to_owned)
            .or_else(|| self.secrets.deposit_wallet_address())
            .unwrap_or_else(|| "(not set)".to_string())
    }

    pub fn relayer_address(&self) -> Option<String> {
        self.secrets.get("POLY_RELAYER_ADDRESS").cloned()
    }

    pub async fn warmup(&self) -> Result<LiveWarmupStatus> {
        let status = self.refresh_account().await?;
        Ok(LiveWarmupStatus {
            balance_usd: status.balance_usd,
            allowance_contracts: status.allowance_contracts,
            relayer_configured: status.relayer_configured,
        })
    }

    pub async fn refresh_account(&self) -> Result<LiveAccountStatus> {
        let signer_address = self.signer_address();
        let funder_address = self.configured_funder_address();
        let client = self.client.lock().await;
        if let Err(error) = client
            .update_balance_allowance(
                UpdateBalanceAllowanceRequest::builder()
                    .asset_type(AssetType::Collateral)
                    .build(),
            )
            .await
        {
            return Err(explain_clob_account_error(
                &format!("CLOB balance-allowance sync failed: {error}"),
                &signer_address,
                &funder_address,
                self.relayer_address().as_deref(),
            ));
        }

        let balance = client
            .balance_allowance(
                BalanceAllowanceRequest::builder()
                    .asset_type(AssetType::Collateral)
                    .build(),
            )
            .await
            .map_err(|error| {
                explain_clob_account_error(
                    &format!("CLOB balance-allowance query failed: {error}"),
                    &signer_address,
                    &funder_address,
                    self.relayer_address().as_deref(),
                )
            })?;

        let balance_usd = collateral_balance_usd(&balance.balance);
        let allowance_contracts = balance.allowances.len();
        let min_order = self.cfg.min_order_usd.max(1.0);
        let relayer_configured = self.secrets.get("POLY_RELAYER_API_KEY").is_some()
            && self.secrets.get("POLY_RELAYER_ADDRESS").is_some();
        Ok(LiveAccountStatus {
            authenticated: true,
            balance_usd,
            allowance_contracts,
            ready_to_trade: allowance_contracts > 0 && balance_usd + 1e-9 >= min_order,
            dry_run: self.cfg.dry_run,
            relayer_configured,
            signer_address,
            funder_address,
            last_error: None,
            updated_at_ms: get_now_ms(),
            window_number: None,
        })
    }

    pub async fn execute_j_signal(
        &self,
        market: &MarketWindow,
        signal: &OrderSignal,
        created_at_ms: i64,
    ) -> LiveExecutionResult {
        match self
            .execute_j_signal_inner(market, signal, created_at_ms)
            .await
        {
            Ok(result) => result,
            Err(error) => LiveExecutionResult {
                reject_reason: error.to_string(),
                ..LiveExecutionResult::default()
            },
        }
    }

    async fn execute_j_signal_inner(
        &self,
        market: &MarketWindow,
        signal: &OrderSignal,
        created_at_ms: i64,
    ) -> Result<LiveExecutionResult> {
        let cfg = &self.cfg;
        let age_ms = get_now_ms().saturating_sub(created_at_ms);
        if cfg.max_order_age_ms > 0 && age_ms > cfg.max_order_age_ms {
            return Err(anyhow!(
                "live order stale before submit: age_ms={} max_order_age_ms={}",
                age_ms,
                cfg.max_order_age_ms
            ));
        }

        let intent = build_live_order_intent(cfg, market, signal)?;
        if cfg.dry_run {
            return Ok(LiveExecutionResult {
                dry_run: true,
                reject_reason: "live dry-run: order was planned but not submitted".to_string(),
                intent: Some(intent),
                ..LiveExecutionResult::default()
            });
        }

        let token_id = parse_token_id(&intent.token_id)?;
        let max_attempts = live_fok_retry_attempts(&intent);
        let client = self.client.lock().await;

        for attempt in 1..=max_attempts {
            let response_result = match intent.operation {
                OrderOperation::Buy => {
                    client
                        .market_order()
                        .token_id(token_id)
                        .side(ClobSide::Buy)
                        .amount(Amount::usdc(decimal_usd(
                            intent.amount_usd.unwrap_or(signal.amount),
                        )?)?)
                        .order_type(cfg.buy_market_order_type.into())
                        .build_sign_and_post(&self.signer)
                        .await
                }
                OrderOperation::Sell => {
                    client
                        .market_order()
                        .token_id(token_id)
                        .side(ClobSide::Sell)
                        .amount(Amount::shares(decimal_shares(intent.shares)?)?)
                        .order_type(cfg.sell_market_order_type.into())
                        .build_sign_and_post(&self.signer)
                        .await
                }
            };

            let response = match response_result {
                Ok(response) => response,
                Err(error) => {
                    let reject_reason = explain_clob_order_error(
                        &format!("live CLOB post_order failed: {error}"),
                        &self.signer_address(),
                        &self.configured_funder_address(),
                    )
                    .to_string();
                    if attempt < max_attempts && should_retry_fok_reject(&reject_reason) {
                        tokio::time::sleep(std::time::Duration::from_millis(
                            LIVE_FOK_RETRY_DELAY_MS,
                        ))
                        .await;
                        continue;
                    }
                    return Err(anyhow!(
                        "{reject_reason} | attempts={attempt}/{max_attempts}"
                    ));
                }
            };

            let fill = matched_response_fill(&intent, &response);
            let accepted = response.success && order_accepted(&response, &fill);
            let executed = accepted;
            let mut reject_reason = if executed {
                String::new()
            } else {
                response
                    .error_msg
                    .clone()
                    .unwrap_or_else(|| format!("live order rejected status={}", response.status))
            };

            if !executed && attempt < max_attempts && should_retry_fok_reject(&reject_reason) {
                tokio::time::sleep(std::time::Duration::from_millis(LIVE_FOK_RETRY_DELAY_MS)).await;
                continue;
            }

            if !reject_reason.is_empty() && attempt > 1 {
                reject_reason.push_str(&format!(" | attempts={attempt}/{max_attempts}"));
            }
            return Ok(LiveExecutionResult {
                submitted: response.success,
                executed,
                dry_run: false,
                reject_reason,
                order_id: Some(response.order_id),
                status: Some(response.status.to_string()),
                trade_count: response.trade_ids.len(),
                transaction_count: response.transaction_hashes.len(),
                fill,
                intent: Some(intent),
                raw_making_amount: Some(response.making_amount.to_string()),
                raw_taking_amount: Some(response.taking_amount.to_string()),
            });
        }

        Err(anyhow!("live CLOB order retry loop exhausted unexpectedly"))
    }
}

pub async fn execute_j_live_signal(
    cfg: &ExecutionConfig,
    market: &MarketWindow,
    signal: &OrderSignal,
    created_at_ms: i64,
) -> LiveExecutionResult {
    match LiveExecutorSession::connect(cfg).await {
        Ok(session) => {
            session
                .execute_j_signal(market, signal, created_at_ms)
                .await
        }
        Err(error) => LiveExecutionResult {
            reject_reason: error.to_string(),
            ..LiveExecutionResult::default()
        },
    }
}

fn order_accepted(
    response: &polymarket_client_sdk_v2::clob::types::response::PostOrderResponse,
    fill: &Option<LiveFill>,
) -> bool {
    if matches!(response.status, OrderStatusType::Matched) {
        return true;
    }
    // Rare: success with matched amounts but non-Matched status.
    response.success && fill.is_some()
}

fn live_fok_retry_attempts(intent: &LiveOrderIntent) -> usize {
    if intent.clob_order_type.eq_ignore_ascii_case("fok") {
        LIVE_FOK_RETRY_ATTEMPTS
    } else {
        1
    }
}

fn should_retry_fok_reject(reason: &str) -> bool {
    let lower = reason.to_ascii_lowercase();
    lower.contains("fok")
        && (lower.contains("couldn't be fully filled")
            || lower.contains("fully filled or killed")
            || lower.contains("full size not available"))
}

pub fn build_live_order_intent(
    cfg: &ExecutionConfig,
    market: &MarketWindow,
    signal: &OrderSignal,
) -> Result<LiveOrderIntent> {
    if signal.price <= 0.0 || !signal.price.is_finite() {
        return Err(anyhow!("bad live signal price {}", signal.price));
    }
    if signal.amount <= 0.0 || !signal.amount.is_finite() {
        return Err(anyhow!("bad live signal amount {}", signal.amount));
    }

    let operation = signal.operation();
    let order_type = StrategyOrderType::Market;
    let token_id = token_id_for_side(market, &signal.side)?;
    let (amount_usd, shares, estimated_notional_usd) = match operation {
        OrderOperation::Buy => {
            if signal.amount + 1e-9 < cfg.min_order_usd.max(1.0) {
                return Err(anyhow!(
                    "live buy below min notional: {:.4} < {:.4}",
                    signal.amount,
                    cfg.min_order_usd.max(1.0)
                ));
            }
            let min_usd = cfg.min_order_usd.max(1.0);
            let shares = signal.amount / signal.price;
            let estimated_notional_usd = shares * signal.price;
            if estimated_notional_usd + 1e-9 < min_usd {
                return Err(anyhow!(
                    "live buy notional {:.4} below min {:.4} at price {:.4}",
                    estimated_notional_usd,
                    min_usd,
                    signal.price
                ));
            }
            (Some(signal.amount), shares, estimated_notional_usd)
        }
        OrderOperation::Sell => {
            let notional = signal.amount * signal.price;
            if notional + 1e-9 < cfg.min_order_usd.max(1.0) {
                return Err(anyhow!(
                    "live sell below min notional: {:.4} < {:.4}",
                    notional,
                    cfg.min_order_usd.max(1.0)
                ));
            }
            (None, floor_2(signal.amount), notional)
        }
    };

    if shares <= 0.0 || !shares.is_finite() {
        return Err(anyhow!("live order shares are invalid: {:.8}", shares));
    }

    Ok(LiveOrderIntent {
        operation,
        strategy_order_type: order_type,
        clob_order_type: match operation {
            OrderOperation::Buy => cfg.buy_market_order_type.as_str().to_string(),
            OrderOperation::Sell => cfg.sell_market_order_type.as_str().to_string(),
        },
        side: signal.side.clone(),
        token_id,
        price: round_price(signal.price),
        amount_usd,
        shares,
        estimated_notional_usd,
        post_only: false,
        reason: signal.reason.clone(),
    })
}

/// Human-readable line for stderr + dashboard SYSTEM EVENT LOG (survives screen clear).
pub fn format_live_terminal_event(
    window_number: usize,
    sig: &OrderSignal,
    result: &LiveExecutionResult,
) -> Option<String> {
    let op = match sig.operation() {
        OrderOperation::Buy => "BUY",
        OrderOperation::Sell => "SELL",
    };
    if result.dry_run {
        if !result.executed && !result.reject_reason.is_empty() {
            return Some(format!(
                "[LIVE DRY-RUN REJECT] W#{window_number} {op} {} ${:.2} @ {:.4} — {}",
                sig.side,
                sig.amount,
                sig.price,
                short_live_reject_reason(&result.reject_reason)
            ));
        }
        return Some(format!(
            "[LIVE DRY-RUN] W#{window_number} {op} {} ${:.2} @ {:.4} — not sent to CLOB",
            sig.side, sig.amount, sig.price
        ));
    }
    if result.executed {
        let (usd, shares, avg_px) = if let Some(fill) = &result.fill {
            (fill.amount_usd, fill.shares, fill.avg_price)
        } else if let Some(intent) = result.intent.as_ref() {
            let usd = intent
                .amount_usd
                .unwrap_or(intent.estimated_notional_usd)
                .min(sig.amount);
            (usd, intent.shares, sig.price)
        } else {
            (sig.amount, sig.amount / sig.price.max(1e-9), sig.price)
        };
        let order_id = result
            .order_id
            .as_deref()
            .map(|id| {
                if id.len() > 14 {
                    format!("{}…{}", &id[..8], &id[id.len() - 4..])
                } else {
                    id.to_string()
                }
            })
            .unwrap_or_else(|| "?".to_string());
        return Some(format!(
            "[LIVE FILL] W#{window_number} {op} {} ${usd:.2} @ {avg_px:.4} = {shares:.4} sh | id {order_id}",
            sig.side
        ));
    }
    if !result.reject_reason.is_empty() {
        return Some(format!(
            "[LIVE REJECT] W#{window_number} {op} {} ${:.2} — {}",
            sig.side,
            sig.amount,
            short_live_reject_reason(&result.reject_reason)
        ));
    }
    None
}

fn short_live_reject_reason(reason: &str) -> String {
    if reason.contains("FOK orders are fully filled") {
        return "FOK kill — full size not available at CLOB book".to_string();
    }
    if reason.contains("signer address has to be the address of the API KEY") {
        return "CLOB auth: API key / owner mismatch".to_string();
    }
    let one_line = reason.replace('\n', " ");
    if one_line.len() > 120 {
        format!("{}…", &one_line[..120])
    } else {
        one_line
    }
}

pub fn apply_live_result_to_portfolio(
    port: &mut crate::trader::Portfolio,
    window_number: usize,
    market: &MarketWindow,
    sig: &OrderSignal,
    result: &LiveExecutionResult,
) -> Result<(), String> {
    if let Some(fill) = &result.fill {
        match sig.operation() {
            OrderOperation::Buy => {
                port.record_external_buy(
                    window_number,
                    market,
                    &sig.side,
                    fill.amount_usd,
                    fill.shares,
                    fill.avg_price,
                    &sig.reason,
                )
                .ok_or_else(|| "live ledger buy rejected".to_string())?;
            }
            OrderOperation::Sell => {
                port.record_external_sell(
                    window_number,
                    market,
                    &sig.side,
                    fill.shares,
                    fill.amount_usd,
                    fill.avg_price,
                    &sig.reason,
                )
                .ok_or_else(|| "live ledger sell rejected".to_string())?;
            }
        }
        return Ok(());
    }

    if !result.executed {
        return Ok(());
    }

    let Some(intent) = result.intent.as_ref() else {
        return Ok(());
    };

    match sig.operation() {
        OrderOperation::Buy => {
            let usd = intent
                .amount_usd
                .unwrap_or(intent.estimated_notional_usd)
                .min(sig.amount);
            port.record_external_buy(
                window_number,
                market,
                &sig.side,
                usd,
                intent.shares,
                sig.price,
                &sig.reason,
            )
            .ok_or_else(|| "live ledger buy fallback rejected".to_string())?;
        }
        OrderOperation::Sell => {
            let usd = intent.shares * sig.price;
            port.record_external_sell(
                window_number,
                market,
                &sig.side,
                intent.shares,
                usd,
                sig.price,
                &sig.reason,
            )
            .ok_or_else(|| "live ledger sell fallback rejected".to_string())?;
        }
    }
    Ok(())
}

/// Live trading in GEM_RUST is deposit-wallet only. Do not use proto Safe/Proxy paths here.
fn validate_deposit_wallet_live_config(
    cfg: &ExecutionConfig,
    secrets: &LiveSecrets,
    signer: &PrivateKeySigner,
) -> Result<()> {
    if cfg.signature_type != LiveSignatureType::Poly1271 {
        return Err(anyhow!(
            "live mode requires execution.signatureType=poly1271 (deposit wallet). \
             Got {:?}. Safe/Proxy (proto_v08) is not supported in GEM_RUST.",
            cfg.signature_type
        ));
    }

    let deposit = cfg
        .funder_address
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| secrets.deposit_wallet_address())
        .ok_or_else(|| {
            anyhow!(
                "POLYMARKET_DEPOSIT_WALLET_ADDRESS is required for live deposit-wallet trading. \
                 This is where pUSD lives — not the owner EOA and not POLY_RELAYER_ADDRESS."
            )
        })?;

    let owner = format!("{:#x}", signer.address());
    if owner.eq_ignore_ascii_case(&deposit) {
        return Err(anyhow!(
            "deposit wallet address ({deposit}) must differ from owner EOA ({owner}). \
             For POLY_1271: owner signs, deposit holds funds. \
             Copy the Transfer Crypto / deposit address from polymarket.com, not the relayer signer."
        ));
    }

    deposit
        .parse::<Address>()
        .with_context(|| format!("invalid POLYMARKET_DEPOSIT_WALLET_ADDRESS: {deposit}"))?;

    Ok(())
}

async fn authenticate_clob_client(
    cfg: &ExecutionConfig,
    secrets: &LiveSecrets,
    signer: &PrivateKeySigner,
) -> Result<polymarket_client_sdk_v2::clob::Client<Authenticated<Normal>>> {
    let host = secrets
        .get("CLOB_API_URL")
        .filter(|value| !value.is_empty())
        .cloned()
        .unwrap_or_else(|| cfg.clob_host.clone());
    let client = ClobClient::new(&host, ClobConfig::default())
        .with_context(|| format!("failed to create CLOB client for {host}"))?;
    let mut builder = client
        .authentication_builder(signer)
        .signature_type(cfg.signature_type.into());

    let funder = cfg
        .funder_address
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| secrets.deposit_wallet_address());
    if cfg.signature_type == LiveSignatureType::Poly1271 && funder.is_none() {
        return Err(anyhow!(
            "POLYMARKET_DEPOSIT_WALLET_ADDRESS is required for signatureType=poly1271"
        ));
    }
    let deposit_wallet = funder.clone().unwrap_or_default();
    if let Some(funder_addr) = funder {
        builder =
            builder.funder(funder_addr.parse::<Address>().with_context(|| {
                format!("failed to parse deposit wallet address {funder_addr}")
            })?);
    }

    builder.authenticate().await.map_err(|error| {
        explain_clob_account_error(
            &format!("CLOB authentication failed: {error}"),
            &format!("{:#x}", signer.address()),
            &deposit_wallet,
            secrets.get("POLY_RELAYER_ADDRESS").map(String::as_str),
        )
    })
}

fn matched_response_fill(
    intent: &LiveOrderIntent,
    response: &polymarket_client_sdk_v2::clob::types::response::PostOrderResponse,
) -> Option<LiveFill> {
    if !response.success || !matches!(response.status, OrderStatusType::Matched) {
        return None;
    }
    let raw_making = decimal_to_f64(&response.making_amount);
    let raw_taking = decimal_to_f64(&response.taking_amount);
    let expected_usd = intent
        .amount_usd
        .unwrap_or(intent.estimated_notional_usd)
        .max(0.0);
    let expected_shares = intent.shares.max(0.0);

    let (amount_usd, shares) = match intent.operation {
        OrderOperation::Buy => {
            let amount_usd = normalize_clob_amount(raw_making)
                .or_else(|| positive(expected_usd))
                .filter(|v| plausible_fill_amount(*v, expected_usd))
                .or_else(|| positive(expected_usd))?;
            let shares = normalize_clob_amount(raw_taking)
                .or_else(|| positive(expected_shares))
                .filter(|v| plausible_fill_amount(*v, expected_shares))
                .or_else(|| positive(amount_usd / intent.price.max(1e-9)))?;
            (amount_usd, shares)
        }
        OrderOperation::Sell => {
            let amount_usd = normalize_clob_amount(raw_taking)
                .filter(|v| plausible_fill_amount(*v, expected_usd))
                .or_else(|| positive(expected_usd))?;
            let shares = normalize_clob_amount(raw_making)
                .filter(|v| plausible_fill_amount(*v, expected_shares))
                .or_else(|| positive(expected_shares))?;
            (amount_usd, shares)
        }
    };
    let avg_price = amount_usd / shares;
    if !avg_price.is_finite() || avg_price <= 0.0 {
        return None;
    }
    Some(LiveFill {
        amount_usd,
        shares,
        avg_price,
    })
}

fn positive(value: f64) -> Option<f64> {
    if value.is_finite() && value > 0.0 {
        Some(value)
    } else {
        None
    }
}

fn normalize_clob_amount(raw: Option<f64>) -> Option<f64> {
    let value = raw?;
    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    if value > 100_000.0 {
        Some(value / 1_000_000.0)
    } else {
        Some(value)
    }
}

fn plausible_fill_amount(value: f64, expected: f64) -> bool {
    if expected <= 0.0 {
        return value.is_finite() && value > 0.0;
    }
    value.is_finite() && value > 0.0 && value <= expected * 5.0 + 5.0
}

#[derive(Debug, Clone, Default)]
struct LiveSecrets {
    values: HashMap<String, String>,
}

impl LiveSecrets {
    fn load(path: &str) -> Result<Self> {
        let mut values = HashMap::new();
        if Path::new(path).exists() {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("failed to read live secrets file {path}"))?;
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let Some((key, value)) = line.split_once('=') else {
                    continue;
                };
                values.insert(key.trim().to_string(), clean_env_value(value));
            }
        }
        for key in [
            "POLYMARKET_PRIVATE_KEY",
            "POLYMARKET_DEPOSIT_WALLET_ADDRESS",
            "POLYMARKET_FUNDER_ADDRESS",
            "CLOB_API_URL",
            "POLY_RELAYER_API_KEY",
            "POLY_RELAYER_ADDRESS",
        ] {
            if let Ok(value) = std::env::var(key) {
                if !value.trim().is_empty() {
                    values.insert(key.to_string(), value.trim().to_string());
                }
            }
        }
        Ok(Self { values })
    }

    fn get(&self, key: &str) -> Option<&String> {
        self.values
            .get(key)
            .filter(|value| !value.trim().is_empty())
    }

    fn deposit_wallet_address(&self) -> Option<String> {
        self.get("POLYMARKET_DEPOSIT_WALLET_ADDRESS")
            .or_else(|| self.get("POLYMARKET_FUNDER_ADDRESS"))
            .cloned()
    }

    fn signer(&self) -> Result<PrivateKeySigner> {
        let private_key = self
            .get("POLYMARKET_PRIVATE_KEY")
            .ok_or_else(|| anyhow!("POLYMARKET_PRIVATE_KEY is required for CLOB order signing"))?;
        let normalized = normalize_private_key(private_key);
        PrivateKeySigner::from_str(&normalized)
            .map(|signer| signer.with_chain_id(Some(POLYGON)))
            .map_err(|error| anyhow!("failed to parse POLYMARKET_PRIVATE_KEY: {error}"))
    }
}

fn clean_env_value(value: &str) -> String {
    value
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .to_string()
}

impl From<LiveSignatureType> for SignatureType {
    fn from(value: LiveSignatureType) -> Self {
        match value {
            LiveSignatureType::Eoa => SignatureType::Eoa,
            LiveSignatureType::Proxy => SignatureType::Proxy,
            LiveSignatureType::GnosisSafe => SignatureType::GnosisSafe,
            LiveSignatureType::Poly1271 => SignatureType::Poly1271,
        }
    }
}

impl From<LiveMarketOrderType> for ClobOrderType {
    fn from(value: LiveMarketOrderType) -> Self {
        match value {
            LiveMarketOrderType::Fok => ClobOrderType::FOK,
            LiveMarketOrderType::Fak => ClobOrderType::FAK,
        }
    }
}

impl LiveMarketOrderType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Fok => "fok",
            Self::Fak => "fak",
        }
    }
}

fn token_id_for_side(market: &MarketWindow, side: &str) -> Result<String> {
    match side {
        "UP" => Ok(market.tokens.up.token_id.clone()),
        "DOWN" => Ok(market.tokens.down.token_id.clone()),
        other => Err(anyhow!("unknown live signal side {other}")),
    }
}

fn parse_token_id(value: &str) -> Result<U256> {
    value
        .parse::<U256>()
        .with_context(|| format!("failed to parse token id {value}"))
}

fn decimal_usd(value: f64) -> Result<Decimal> {
    Decimal::from_str(&format!("{:.6}", value))
        .with_context(|| format!("failed to convert USD amount {value} to Decimal"))
}

fn decimal_shares(value: f64) -> Result<Decimal> {
    Decimal::from_str(&format!("{:.2}", value))
        .with_context(|| format!("failed to convert shares {value} to Decimal"))
}

fn decimal_to_f64(value: &Decimal) -> Option<f64> {
    value.to_string().parse::<f64>().ok()
}

/// CLOB collateral balance is returned in USDC micro-units (6 decimals).
fn collateral_balance_usd(balance: &Decimal) -> f64 {
    decimal_to_f64(balance).unwrap_or(0.0) / 1_000_000.0
}

fn round_price(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn floor_2(value: f64) -> f64 {
    (value * 100.0).floor() / 100.0
}

fn explain_clob_order_error(err: &str, owner: &str, deposit: &str) -> anyhow::Error {
    if err.contains("signer address has to be the address of the API KEY") {
        return anyhow!(
            "{err} | deposit-wallet auth: API key must belong to owner EOA ({owner}), \
             not deposit ({deposit}). Check POLYMARKET_PRIVATE_KEY matches the account that created API creds."
        );
    }
    if err.contains("UNMATCHED") || err.to_ascii_lowercase().contains("fok") {
        return anyhow!(
            "{err} | FOK: full size not available at current book. SDK prices from live CLOB book at submit."
        );
    }
    anyhow!("{err} | owner={owner} deposit={deposit}")
}

fn explain_clob_account_error(
    err: &str,
    signer: &str,
    funder: &str,
    relayer: Option<&str>,
) -> anyhow::Error {
    if err.contains("no deposit wallet found for owner") {
        let same_owner_relayer = relayer
            .map(|addr| signer.eq_ignore_ascii_case(addr))
            .unwrap_or(false);
        let owner_note = if same_owner_relayer {
            " Owner=relayer EOA is normal; deposit is a separate smart-wallet address."
        } else {
            ""
        };
        return anyhow!(
            "{err} | owner={signer} deposit={funder}.{owner_note} \
             CLOB has no deposit wallet registered for this owner yet. Usual fixes: \
             (1) make one small trade on polymarket.com (deploys deposit proxy on-chain), \
             (2) deposit USDC to {funder}, \
             (3) wait a few minutes and retry balance sync. \
             If still failing after UI trade: known POLY_1271 deposit-wallet SDK/CLOB limitation for new accounts."
        );
    }
    anyhow!("{err}")
}

fn normalize_private_key(value: &str) -> String {
    let trimmed = value.trim().trim_matches('"').trim_matches('\'');
    if trimmed.starts_with("0x") || trimmed.starts_with("0X") {
        trimmed.to_string()
    } else {
        format!("0x{trimmed}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{MarketWindow, TokenInfo, TokensMap};
    use crate::strategy::{OrderSignal, OrderType};

    fn market() -> MarketWindow {
        MarketWindow {
            id: "m1".to_string(),
            slug: "btc-updown-5m-test".to_string(),
            question: "BTC test".to_string(),
            asset: "BTC".to_string(),
            interval: "5m".to_string(),
            start_time: "2026-06-20T00:00:00Z".to_string(),
            end_time: "2026-06-20T00:05:00Z".to_string(),
            price_to_beat: Some(100.0),
            tokens: TokensMap {
                up: TokenInfo {
                    token_id: "11".to_string(),
                    outcome_name: "Up".to_string(),
                },
                down: TokenInfo {
                    token_id: "22".to_string(),
                    outcome_name: "Down".to_string(),
                },
            },
        }
    }

    #[test]
    fn market_buy_intent_uses_usd_amount_and_fok() {
        let cfg = ExecutionConfig::default();
        let sig = OrderSignal::buy("UP", OrderType::Market, 3.0, 0.94, "j_test");
        let intent = build_live_order_intent(&cfg, &market(), &sig).unwrap();
        assert_eq!(intent.operation, OrderOperation::Buy);
        assert_eq!(intent.clob_order_type, "fok");
        assert_eq!(intent.token_id, "11");
        assert_eq!(intent.amount_usd, Some(3.0));
        assert!(intent.shares > 3.19 && intent.shares < 3.20);
    }

    #[test]
    fn live_buy_forces_market_fok_even_if_signal_says_limit() {
        let cfg = ExecutionConfig::default();
        let sig = OrderSignal::buy("DOWN", OrderType::Limit, 1.0, 0.99, "j_test");
        let intent = build_live_order_intent(&cfg, &market(), &sig).unwrap();
        assert_eq!(intent.strategy_order_type, StrategyOrderType::Market);
        assert_eq!(intent.clob_order_type, "fok");
        assert_eq!(intent.token_id, "22");
        assert_eq!(intent.amount_usd, Some(1.0));
        assert!(intent.shares > 1.0);
    }

    #[test]
    fn sell_intent_uses_shares_and_notional_gate() {
        let cfg = ExecutionConfig::default();
        let sig = OrderSignal::sell("UP", OrderType::Market, 2.345, 0.50, "j_sell");
        let intent = build_live_order_intent(&cfg, &market(), &sig).unwrap();
        assert_eq!(intent.operation, OrderOperation::Sell);
        assert_eq!(intent.shares, 2.34);
        assert!(intent.estimated_notional_usd > 1.17);
    }

    #[test]
    fn live_j_forces_market_even_if_signal_says_limit() {
        let cfg = ExecutionConfig::default();
        let sig = OrderSignal::sell("UP", OrderType::Limit, 2.345, 0.50, "j_sell");
        let intent = build_live_order_intent(&cfg, &market(), &sig).unwrap();
        assert_eq!(intent.operation, OrderOperation::Sell);
        assert_eq!(intent.strategy_order_type, StrategyOrderType::Market);
        assert_eq!(intent.clob_order_type, "fok");
        assert!(!intent.post_only);
    }

    #[test]
    fn rejects_sub_dollar_live_buy() {
        let cfg = ExecutionConfig::default();
        let sig = OrderSignal::buy("UP", OrderType::Market, 0.99, 0.94, "j_test");
        let err = build_live_order_intent(&cfg, &market(), &sig).unwrap_err();
        assert!(err.to_string().contains("below min notional"));
    }

    #[test]
    fn collateral_balance_converts_micro_usdc_to_usd() {
        let balance = Decimal::from_str("9960311").unwrap();
        assert!((collateral_balance_usd(&balance) - 9.960311).abs() < 1e-6);
    }

    use polymarket_client_sdk_v2::clob::types::response::PostOrderResponse;

    #[test]
    fn market_matched_counts_as_executed() {
        let matched = PostOrderResponse::builder()
            .making_amount(Decimal::from_str("1").unwrap())
            .taking_amount(Decimal::from_str("1").unwrap())
            .order_id("oid")
            .status(OrderStatusType::Matched)
            .success(true)
            .build();
        assert!(order_accepted(&matched, &None));
        let live = PostOrderResponse::builder()
            .making_amount(Decimal::from_str("1").unwrap())
            .taking_amount(Decimal::from_str("1").unwrap())
            .order_id("oid")
            .status(OrderStatusType::Live)
            .success(true)
            .build();
        assert!(!order_accepted(&live, &None));
    }

    #[test]
    fn live_fill_terminal_event_is_visible() {
        let sig = OrderSignal::buy("DOWN", OrderType::Market, 1.0, 0.97, "j_test");
        let result = LiveExecutionResult {
            executed: true,
            submitted: true,
            order_id: Some("0xabcdef1234567890".to_string()),
            status: Some("matched".to_string()),
            fill: Some(LiveFill {
                amount_usd: 1.0,
                shares: 1.03,
                avg_price: 0.97,
            }),
            intent: None,
            ..LiveExecutionResult::default()
        };
        let msg = format_live_terminal_event(1, &sig, &result).unwrap();
        assert!(msg.contains("[LIVE FILL]"));
        assert!(msg.contains("DOWN"));
        assert!(msg.contains("$1.00"));
    }

    #[test]
    fn live_reject_terminal_event_shortens_fok() {
        let sig = OrderSignal::buy("DOWN", OrderType::Market, 1.0, 0.97, "j_test");
        let result = LiveExecutionResult {
            reject_reason: "FOK orders are fully filled or killed".to_string(),
            ..LiveExecutionResult::default()
        };
        let msg = format_live_terminal_event(1, &sig, &result).unwrap();
        assert!(msg.contains("[LIVE REJECT]"));
        assert!(msg.contains("FOK kill"));
    }
}
