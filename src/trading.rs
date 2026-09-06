use std::str::FromStr;
use std::sync::Arc;

use alloy::signers::Signer as _;
use alloy::signers::local::{LocalSigner, PrivateKeySigner};
use chrono::Utc;
use polymarket_client_sdk_v2::auth::{Credentials, Uuid};
use polymarket_client_sdk_v2::clob::types::{
    OrderStatusType, OrderType, Side, SignatureType, TickSize,
};
use polymarket_client_sdk_v2::clob::types::response::PostOrderResponse;
use polymarket_client_sdk_v2::clob::{Client as SdkClient, Config as SdkConfig};
use polymarket_client_sdk_v2::POLYGON;
use rust_decimal::{Decimal, RoundingStrategy};
use rust_decimal::prelude::ToPrimitive;
use tracing::info;

use crate::config::RuntimeConfig;
use crate::types::{EntrySignal, FillResult};

/// Authenticated SDK client type (post-.authenticate()).
pub type AuthedSdkClient =
    SdkClient<polymarket_client_sdk_v2::auth::state::Authenticated<polymarket_client_sdk_v2::auth::Normal>>;

/// Whether a place-order error string represents a transient FOK/FAK rejection
/// that's safe to retry. Network/protocol errors (500s, version mismatches) are not.
pub fn is_retriable_error(msg: &str) -> bool {
    if msg.starts_with("ORDER_STATE_UNKNOWN:") {
        return false;
    }
    msg.contains("FOK orders are fully filled")
        || msg.contains("no orders found to match")
        || msg.contains("FAK orders are partially filled")
}

/// Round down to the market tick while respecting both the observed ask's
/// slippage allowance and the absolute price cap. Never round risk upward.
pub fn buy_limit_price(
    ask: f64, max_slippage: f64, max_ask_price: f64, tick_size: &str,
) -> Result<f64, String> {
    if !ask.is_finite() || ask <= 0.0 || ask >= 1.0
        || !max_slippage.is_finite() || max_slippage < 0.0
        || !max_ask_price.is_finite() || max_ask_price <= 0.0 || max_ask_price >= 1.0
    {
        return Err("Invalid finite price/slippage bounds".into());
    }
    parse_tick_size(tick_size)?;
    let tick = Decimal::from_str(tick_size).map_err(|e| e.to_string())?;
    let ask_dec = Decimal::from_str(&ask.to_string()).map_err(|e| e.to_string())?;
    let slippage = Decimal::from_str(&max_slippage.to_string()).map_err(|e| e.to_string())?;
    let cap = Decimal::from_str(&max_ask_price.to_string()).map_err(|e| e.to_string())?;
    let ceiling = (ask_dec + slippage).min(cap).min(Decimal::ONE - tick);
    let limit = (ceiling / tick).floor() * tick;
    if limit < ask_dec || limit <= Decimal::ZERO {
        return Err("No market tick available within ask/slippage/price cap".into());
    }
    limit.to_f64().ok_or_else(|| "Price conversion failed".into())
}

fn order_shares(shares: f64) -> Result<Decimal, String> {
    if !shares.is_finite() || shares <= 0.0 {
        return Err("Shares must be finite and positive".into());
    }
    let size = Decimal::from_str(&shares.to_string()).map_err(|e| e.to_string())?
        .round_dp_with_strategy(2, RoundingStrategy::ToZero);
    if size <= Decimal::ZERO {
        return Err("Shares round down to zero".into());
    }
    Ok(size)
}

/// Upper bound over all execution prices up to the buy limit. Fees are
/// collateral debits in V2 and are additional to makingAmount, not shares.
pub fn fee_upper_bound(shares: f64, limit: f64, rate: f64, exponent: u32) -> Result<f64, String> {
    if !shares.is_finite() || shares < 0.0 || !limit.is_finite() || !(0.0..1.0).contains(&limit)
        || !rate.is_finite() || !(0.0..=1.0).contains(&rate) || exponent > 4
    {
        return Err("Unsupported fee parameters".into());
    }
    let as_decimal = |v: f64| Decimal::from_str(&v.to_string()).map_err(|e| e.to_string());
    let p = as_decimal(limit.min(0.5))?;
    let curve = p * (Decimal::ONE - p);
    let mut power = Decimal::ONE;
    for _ in 0..exponent { power *= curve; }
    let fee = as_decimal(shares)?.checked_mul(as_decimal(rate)?)
        .and_then(|value| value.checked_mul(power)).ok_or("Fee arithmetic overflow")?;
    fee.round_dp_with_strategy(5, RoundingStrategy::ToPositiveInfinity)
        .to_f64().ok_or_else(|| "Fee conversion failed".into())
}

fn parse_tick_size(s: &str) -> Result<TickSize, String> {
    let d = Decimal::from_str(s).map_err(|e| format!("Invalid tick size '{}': {}", s, e))?;
    TickSize::try_from(d).map_err(|e| format!("{}", e))
}

/// Build and authenticate the SDK client once. Returns an Arc for shared use.
pub async fn build_shared_sdk_client(
    config: &RuntimeConfig,
) -> Result<Arc<AuthedSdkClient>, String> {
    let signer = LocalSigner::from_str(&config.poly_private_key)
        .map_err(|e| format!("Invalid private key for SDK signer: {}", e))?
        .with_chain_id(Some(POLYGON));

    let api_key = Uuid::parse_str(&config.poly_api_key)
        .map_err(|e| format!("Invalid POLY_API_KEY UUID: {}", e))?;
    let credentials = Credentials::new(
        api_key,
        config.poly_api_secret.clone(),
        config.poly_api_passphrase.clone(),
    );

    let funder: polymarket_client_sdk_v2::types::Address =
        config.poly_proxy_address.parse()
            .map_err(|e| format!("Invalid proxy address: {}", e))?;

    let sdk_client = SdkClient::new("https://clob.polymarket.com", SdkConfig::default())
        .map_err(|e| format!("SDK client init: {}", e))?;

    let authenticated = sdk_client
        .authentication_builder(&signer)
        .credentials(credentials)
        .funder(funder)
        .signature_type(SignatureType::Proxy)
        .authenticate()
        .await
        .map_err(|e| format!("SDK authenticate: {}", e))?;

    Ok(Arc::new(authenticated))
}

/// Place a Fill-and-Kill market buy order at a limit pegged to the observed ask + max_slippage.
#[allow(clippy::too_many_arguments)]
pub async fn place_fak_buy(
    sdk_client: &AuthedSdkClient,
    config: &RuntimeConfig,
    wallet: &PrivateKeySigner,
    signal: &EntrySignal,
    shares: f64,
    tick_size: &str,
    neg_risk: bool,
) -> Result<FillResult, String> {
    let limit = buy_limit_price(signal.ask_price, config.max_slippage, config.max_ask_price, tick_size)?;

    place_fak_buy_raw(sdk_client, config, wallet, signal, limit, shares, tick_size, neg_risk).await
}

/// Place a Fill-and-Kill market buy order at an exact limit price (no slippage adjustment).
#[allow(clippy::too_many_arguments)]
pub async fn place_fak_buy_raw(
    sdk_client: &AuthedSdkClient,
    config: &RuntimeConfig,
    wallet: &PrivateKeySigner,
    signal: &EntrySignal,
    price: f64,
    shares: f64,
    tick_size: &str,
    neg_risk: bool,
) -> Result<FillResult, String> {
    if !config.taker_enabled || config.dry_run {
        return Err("Live taker orders are disabled by configuration".into());
    }
    let window_end = (Utc::now().timestamp() / 300 + 1) * 300;
    let valid_limit = buy_limit_price(price, 0.0, config.max_ask_price, tick_size)?;
    let dec_shares = order_shares(shares)?;
    let shares = dec_shares.to_f64().ok_or("Shares conversion failed")?;
    if !config.max_trade_cost_usdc.is_finite() || config.max_trade_cost_usdc <= 0.0
        || valid_limit * shares > config.max_trade_cost_usdc
    {
        return Err("Trade exceeds the configured cost limit".into());
    }
    let price = valid_limit;
    let cost = price * shares;
    info!(
        "Placing FAK BUY: {} {} shares @ ${:.2} (cost ${:.2})",
        signal.side, shares, price, cost
    );

    let token_id = alloy::primitives::U256::from_str(&signal.token_id)
        .map_err(|e| format!("Invalid token ID: {}", e))?;

    let market = sdk_client.market_by_token(token_id).await
        .map_err(|e| format!("Fee metadata lookup failed before submission: {}", e))?;
    let metadata = sdk_client.clob_market_info(&market.condition_id.to_string()).await
        .map_err(|e| format!("Market metadata failed before submission: {}", e))?;
    let fee = metadata.fee_details.as_ref().ok_or("Missing fee metadata; order not submitted")?;
    let fee_rate = fee.rate.to_f64().ok_or("Invalid fee rate")?;
    let fee_budget = fee_upper_bound(shares, price, fee_rate, fee.exponent)?;
    if price * shares + fee_budget > config.max_trade_cost_usdc + 1e-9 {
        return Err("Trade including estimated fees exceeds configured cost limit".into());
    }
    if shares < metadata.min_order_size.to_f64().ok_or("Invalid minimum order size")? {
        return Err("Trade is smaller than the market minimum".into());
    }
    if metadata.neg_risk != neg_risk {
        return Err("Market risk metadata changed; refresh before submitting".into());
    }

    let dec_price = Decimal::from_str(&price.to_string())
        .map_err(|e| format!("Invalid price decimal: {}", e))?;

    let signable = sdk_client
        .limit_order()
        .token_id(token_id)
        .side(Side::Buy)
        .price(dec_price)
        .size(dec_shares)
        .order_type(OrderType::FAK)
        .build()
        .await
        .map_err(|e| format!("SDK build order: {}", e))?;
    if signable.payload.version() != 2 {
        return Err("Legacy exchange fee accounting unsupported; order not submitted".into());
    }

    let signed = sdk_client
        .sign(wallet, signable)
        .await
        .map_err(|e| format!("SDK sign order: {}", e))?;

    // Metadata/build/sign may await network calls. Do not post after the
    // decision's window has expired or entered its final 30 seconds.
    if Utc::now().timestamp() >= window_end - 30 {
        return Err("Decision expired before submission".into());
    }

    let resp = sdk_client
        .post_order(signed)
        .await
        .map_err(|e| format!("ORDER_STATE_UNKNOWN: SDK post order: {}", e))?;

    parse_buy_response(&resp, price, shares, fee_rate, fee.exponent)
}

fn parse_buy_response(
    resp: &PostOrderResponse, price: f64, shares: f64, fee_rate: f64, fee_exponent: u32,
) -> Result<FillResult, String> {
    if !resp.success {
        if !resp.order_id.is_empty() || resp.taking_amount != Decimal::ZERO || resp.making_amount != Decimal::ZERO {
            return Err(format!("ORDER_STATE_UNKNOWN: rejection with possible fills for {}", resp.order_id));
        }
        return Err(format!("Order rejected: status={} {}", resp.status, resp.error_msg.as_deref().unwrap_or("")));
    }
    if resp.error_msg.as_deref().is_some_and(|error| !error.is_empty()) {
        return Err(format!("ORDER_STATE_UNKNOWN: accepted response with error for {}", resp.order_id));
    }

    if resp.status != OrderStatusType::Matched || resp.order_id.is_empty() {
        return Err(format!("ORDER_STATE_UNKNOWN: accepted order {} status={}; reconcile before another order", resp.order_id, resp.status));
    }

    let filled_size = resp.taking_amount.to_string().parse::<f64>().unwrap_or(0.0);
    let making = resp.making_amount.to_string().parse::<f64>().unwrap_or(0.0);
    let fill_price = making / filled_size;
    if !filled_size.is_finite() || filled_size <= 0.0 || filled_size > shares + 1e-5
        || !making.is_finite() || making <= 0.0 || !fill_price.is_finite()
        || fill_price <= 0.0 || fill_price > price + 1e-5
    {
        return Err(format!("ORDER_STATE_UNKNOWN: inconsistent matched amounts for {}; reconcile before another order", resp.order_id));
    }

    if filled_size > 0.0 && filled_size < shares * 0.99 {
        info!(
            "FAK partial fill: requested {:.2} shares, filled {:.2} ({:.1}% of target) @ ${:.4} avg",
            shares, filled_size, (filled_size / shares) * 100.0, fill_price
        );
    } else {
        info!(
            "Order {} filled: {:.2} shares @ ${:.4}",
            resp.order_id, filled_size, fill_price
        );
    }
    Ok(FillResult {
        order_id: resp.order_id.clone(),
        fill_price,
        filled_size,
        fee_estimate_usdc: fee_upper_bound(filled_size, price, fee_rate, fee_exponent)?,
    })
}

/// Simulate a trade for dry-run mode. Returns a FillResult with simulated values.
pub fn simulate_trade(signal: &EntrySignal, shares: f64) -> FillResult {
    info!(
        "[DRY RUN] Simulated FAK BUY: {} {} shares @ ${:.2}",
        signal.side, shares, signal.ask_price
    );
    FillResult {
        order_id: format!("dry-run-{}", Utc::now().timestamp_millis()),
        fill_price: signal.ask_price,
        filled_size: shares,
        fee_estimate_usdc: fee_upper_bound(shares, signal.ask_price, 0.07, 1).unwrap_or(f64::INFINITY),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ethers::types::U256;

    const TOKEN_DECIMALS: u32 = 6;

    fn to_raw_amount(value: f64) -> U256 {
        let raw = (value * 10f64.powi(TOKEN_DECIMALS as i32)).round() as u128;
        U256::from(raw)
    }

    #[test]
    fn test_to_raw_amount() {
        assert_eq!(to_raw_amount(5.0), U256::from(5_000_000u64));
        assert_eq!(to_raw_amount(0.50), U256::from(500_000u64));
        assert_eq!(to_raw_amount(10.0), U256::from(10_000_000u64));
        assert_eq!(to_raw_amount(0.01), U256::from(10_000u64));
    }

    #[test]
    fn caps_and_tick_rounding_never_raise_the_price_budget() {
        assert_eq!(buy_limit_price(0.798, 0.03, 0.80, "0.001").unwrap(), 0.80);
        assert_eq!(buy_limit_price(0.50, 0.039, 0.80, "0.01").unwrap(), 0.53);
        assert!(buy_limit_price(0.805, 0.03, 0.80, "0.001").is_err());
        assert!(buy_limit_price(f64::NAN, 0.03, 0.80, "0.01").is_err());
        assert_eq!(order_shares(5.999).unwrap().to_string(), "5.99");
    }

    #[test]
    fn fee_reserve_covers_every_fill_price_below_limit() {
        let bound = fee_upper_bound(100.0, 0.80, 0.07, 1).unwrap();
        assert!((bound - 1.75).abs() < 1e-9);
        for cent in 1..=80 {
            let p = cent as f64 / 100.0;
            assert!(100.0 * 0.07 * p * (1.0 - p) <= bound + 1e-9);
        }
    }

    fn response(status: &str, making: &str, taking: &str) -> PostOrderResponse {
        serde_json::from_value(serde_json::json!({
            "errorMsg": "", "makingAmount": making, "takingAmount": taking,
            "orderID": "order-1", "status": status, "success": true,
        })).unwrap()
    }

    #[test]
    fn delayed_order_is_unknown_exposure_never_a_retryable_zero_fill() {
        let err = parse_buy_response(&response("delayed", "0", "0"), 0.6, 5.0, 0.07, 1).unwrap_err();
        assert!(err.starts_with("ORDER_STATE_UNKNOWN:"));
        assert!(!is_retriable_error(&err));
        assert!(parse_buy_response(&response("matched", "0", "0"), 0.6, 5.0, 0.07, 1).is_err());
    }

    #[test]
    fn partial_fill_preserves_paid_amount_and_additional_fee_estimate() {
        let fill = parse_buy_response(&response("matched", "1.2", "2"), 0.6, 5.0, 0.07, 1).unwrap();
        assert_eq!(fill.filled_size, 2.0);
        assert!((fill.fill_price * fill.filled_size - 1.2).abs() < 1e-9);
        assert!(fill.fee_estimate_usdc >= 2.0 * 0.07 * 0.6 * 0.4);
        assert!(parse_buy_response(&response("matched", "3.6", "6"), 0.6, 5.0, 0.07, 1).is_err());
    }
}
