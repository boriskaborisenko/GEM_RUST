use crate::client::{get_now_ms, MarketWindow};
use crate::config::{
    ExecutionConfig, ExecutionMode, LiveLimitOrderType, LiveMarketOrderType, LiveSignatureType,
};
use crate::strategy::{OrderOperation, OrderSignal, OrderType as StrategyOrderType};
use alloy::signers::local::PrivateKeySigner;
use alloy::signers::Signer as _;
use anyhow::{anyhow, Context, Result};
use chrono::{Duration, Utc};
use polymarket_client_sdk_v2::auth::{state::Authenticated, Normal};
use polymarket_client_sdk_v2::clob::types::request::{
    BalanceAllowanceRequest, UpdateBalanceAllowanceRequest,
};
use polymarket_client_sdk_v2::clob::types::{
    Amount, AssetType, OrderStatusType, OrderType as ClobOrderType, Side as ClobSide,
    SignatureType,
};
use polymarket_client_sdk_v2::clob::{Client as ClobClient, Config as ClobConfig};
use polymarket_client_sdk_v2::types::{Address, Decimal, U256};
use polymarket_client_sdk_v2::POLYGON;
use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr as _;
use std::sync::Arc;
use tokio::sync::Mutex;

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
        let client = authenticate_clob_client(cfg, &secrets, &signer).await?;
        Ok(Arc::new(Self {
            cfg: cfg.clone(),
            signer,
            client: Mutex::new(client),
            secrets,
        }))
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
        let client = self.client.lock().await;

        let response = match (intent.operation, intent.strategy_order_type) {
            (OrderOperation::Buy, StrategyOrderType::Market) => {
                client
                    .market_order()
                    .token_id(token_id)
                    .side(ClobSide::Buy)
                    .price(decimal_price(intent.price)?)
                    .amount(Amount::usdc(decimal_usd(
                        intent.amount_usd.unwrap_or(intent.estimated_notional_usd),
                    )?)?)
                    .order_type(cfg.market_order_type.into())
                    .build_sign_and_post(&self.signer)
                    .await
            }
            (OrderOperation::Sell, StrategyOrderType::Market) => {
                client
                    .market_order()
                    .token_id(token_id)
                    .side(ClobSide::Sell)
                    .price(decimal_price(intent.price)?)
                    .amount(Amount::shares(decimal_shares(intent.shares)?)?)
                    .order_type(cfg.market_order_type.into())
                    .build_sign_and_post(&self.signer)
                    .await
            }
            (OrderOperation::Buy, StrategyOrderType::Limit) => {
                let mut builder = client
                    .limit_order()
                    .token_id(token_id)
                    .side(ClobSide::Buy)
                    .price(decimal_price(intent.price)?)
                    .size(decimal_shares(intent.shares)?)
                    .order_type(cfg.limit_order_type.into())
                    .post_only(cfg.limit_post_only);
                if cfg.limit_order_type == LiveLimitOrderType::Gtd {
                    builder =
                        builder.expiration(Utc::now() + Duration::milliseconds(cfg.limit_ttl_ms));
                }
                builder.build_sign_and_post(&self.signer).await
            }
            (OrderOperation::Sell, StrategyOrderType::Limit) => {
                let mut builder = client
                    .limit_order()
                    .token_id(token_id)
                    .side(ClobSide::Sell)
                    .price(decimal_price(intent.price)?)
                    .size(decimal_shares(intent.shares)?)
                    .order_type(cfg.limit_order_type.into())
                    .post_only(cfg.limit_post_only);
                if cfg.limit_order_type == LiveLimitOrderType::Gtd {
                    builder =
                        builder.expiration(Utc::now() + Duration::milliseconds(cfg.limit_ttl_ms));
                }
                builder.build_sign_and_post(&self.signer).await
            }
        }
        .map_err(|error| anyhow!("live CLOB post_order failed: {error}"))?;

        let fill = matched_response_fill(&intent, &response);
        let accepted = response.success && order_accepted(&intent, &response.status);
        let executed = accepted;
        Ok(LiveExecutionResult {
            submitted: response.success,
            executed,
            dry_run: false,
            reject_reason: if executed {
                String::new()
            } else {
                response
                    .error_msg
                    .clone()
                    .unwrap_or_else(|| format!("live order rejected status={}", response.status))
            },
            order_id: Some(response.order_id),
            status: Some(response.status.to_string()),
            trade_count: response.trade_ids.len(),
            transaction_count: response.transaction_hashes.len(),
            fill,
            intent: Some(intent),
        })
    }
}

pub async fn execute_j_live_signal(
    cfg: &ExecutionConfig,
    market: &MarketWindow,
    signal: &OrderSignal,
    created_at_ms: i64,
) -> LiveExecutionResult {
    match LiveExecutorSession::connect(cfg).await {
        Ok(session) => session.execute_j_signal(market, signal, created_at_ms).await,
        Err(error) => LiveExecutionResult {
            reject_reason: error.to_string(),
            ..LiveExecutionResult::default()
        },
    }
}

fn order_accepted(intent: &LiveOrderIntent, status: &OrderStatusType) -> bool {
    match intent.strategy_order_type {
        StrategyOrderType::Market => matches!(status, OrderStatusType::Matched),
        StrategyOrderType::Limit => matches!(
            status,
            OrderStatusType::Matched | OrderStatusType::Live | OrderStatusType::Delayed
        ),
    }
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
    let order_type = match operation {
        OrderOperation::Buy => signal.order_type,
        // Live J sells are defensive tail/rescue exits. Do not leave them resting.
        OrderOperation::Sell => StrategyOrderType::Market,
    };
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
            let shares = match order_type {
                StrategyOrderType::Market => signal.amount / signal.price,
                // Floor so notional never exceeds the strategy USD budget on resting limits.
                StrategyOrderType::Limit => floor_2(signal.amount / signal.price),
            };
            (Some(signal.amount), shares, shares * signal.price)
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
        clob_order_type: match order_type {
            StrategyOrderType::Market => cfg.market_order_type.as_str().to_string(),
            StrategyOrderType::Limit => cfg.limit_order_type.as_str().to_string(),
        },
        side: signal.side.clone(),
        token_id,
        price: round_price(signal.price),
        amount_usd,
        shares,
        estimated_notional_usd,
        post_only: order_type == StrategyOrderType::Limit && cfg.limit_post_only,
        reason: signal.reason.clone(),
    })
}

pub fn apply_live_result_to_portfolio(
    port: &mut crate::trader::Portfolio,
    window_number: usize,
    sig: &OrderSignal,
    result: &LiveExecutionResult,
) {
    if let Some(fill) = &result.fill {
        match sig.operation() {
            OrderOperation::Buy => {
                let _ = port.execute_buy(
                    window_number,
                    &sig.side,
                    fill.amount_usd,
                    fill.avg_price,
                    &sig.reason,
                );
            }
            OrderOperation::Sell => {
                let _ = port.execute_sell(
                    window_number,
                    &sig.side,
                    fill.shares,
                    fill.avg_price,
                    &sig.reason,
                );
            }
        }
        return;
    }

    if !result.executed {
        return;
    }

    let Some(intent) = result.intent.as_ref() else {
        return;
    };

    match sig.operation() {
        OrderOperation::Buy => {
            let usd = intent
                .amount_usd
                .unwrap_or(intent.estimated_notional_usd)
                .min(sig.amount);
            let _ = port.execute_buy(window_number, &sig.side, usd, sig.price, &sig.reason);
        }
        OrderOperation::Sell => {
            let _ = port.execute_sell(
                window_number,
                &sig.side,
                intent.shares,
                sig.price,
                &sig.reason,
            );
        }
    }
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
    if let Some(funder) = funder {
        builder = builder.funder(
            funder
                .parse::<Address>()
                .with_context(|| format!("failed to parse deposit wallet address {funder}"))?,
        );
    }

    builder
        .authenticate()
        .await
        .map_err(|error| anyhow!("CLOB authentication failed: {error}"))
}

fn matched_response_fill(
    intent: &LiveOrderIntent,
    response: &polymarket_client_sdk_v2::clob::types::response::PostOrderResponse,
) -> Option<LiveFill> {
    if !response.success || !matches!(response.status, OrderStatusType::Matched) {
        return None;
    }
    let (amount_usd, shares) = match intent.operation {
        OrderOperation::Buy => (
            decimal_to_f64(&response.making_amount).or(intent.amount_usd)?,
            decimal_to_f64(&response.taking_amount).filter(|v| *v > 0.0)?,
        ),
        OrderOperation::Sell => (
            decimal_to_f64(&response.taking_amount).filter(|v| *v > 0.0)?,
            decimal_to_f64(&response.making_amount).filter(|v| *v > 0.0)?,
        ),
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

impl From<LiveLimitOrderType> for ClobOrderType {
    fn from(value: LiveLimitOrderType) -> Self {
        match value {
            LiveLimitOrderType::Gtc => ClobOrderType::GTC,
            LiveLimitOrderType::Gtd => ClobOrderType::GTD,
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

impl LiveLimitOrderType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Gtc => "gtc",
            Self::Gtd => "gtd",
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

fn decimal_price(value: f64) -> Result<Decimal> {
    Decimal::from_str(&format!("{:.2}", round_price(value)))
        .with_context(|| format!("failed to convert price {value} to Decimal"))
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

fn ceil_2(value: f64) -> f64 {
    (value * 100.0).ceil() / 100.0
}

fn floor_2(value: f64) -> f64 {
    (value * 100.0).floor() / 100.0
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
    fn limit_buy_intent_floors_shares_to_two_decimals() {
        let cfg = ExecutionConfig::default();
        let sig = OrderSignal::buy("DOWN", OrderType::Limit, 1.0, 0.99, "j_test");
        let intent = build_live_order_intent(&cfg, &market(), &sig).unwrap();
        assert_eq!(intent.clob_order_type, "gtd");
        assert_eq!(intent.token_id, "22");
        assert_eq!(intent.shares, 1.01);
        assert!(intent.estimated_notional_usd <= 1.0 + 1e-9);
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
    fn live_sell_forces_market_fok_even_if_signal_is_limit() {
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

    #[test]
    fn resting_limit_counts_as_accepted() {
        let intent = LiveOrderIntent {
            operation: OrderOperation::Buy,
            strategy_order_type: StrategyOrderType::Limit,
            clob_order_type: "gtd".to_string(),
            side: "UP".to_string(),
            token_id: "11".to_string(),
            price: 0.95,
            amount_usd: Some(1.0),
            shares: 1.05,
            estimated_notional_usd: 0.9975,
            post_only: true,
            reason: "j_test".to_string(),
        };
        assert!(order_accepted(&intent, &OrderStatusType::Live));
        assert!(!order_accepted(
            &LiveOrderIntent {
                strategy_order_type: StrategyOrderType::Market,
                ..intent.clone()
            },
            &OrderStatusType::Live
        ));
    }
}
