use std::str::FromStr;
use std::sync::Arc;

use alloy::signers::Signer as _;
use alloy::signers::local::{LocalSigner, PrivateKeySigner};
use chrono::Utc;
use polymarket_client_sdk_v2::auth::{Credentials, Uuid};
use polymarket_client_sdk_v2::clob::types::{
    OrderType, Side, SignatureType, TickSize,
};
use polymarket_client_sdk_v2::clob::{Client as SdkClient, Config as SdkConfig};
use polymarket_client_sdk_v2::POLYGON;
use rust_decimal::Decimal;
use tracing::{debug, info};

use crate::config::RuntimeConfig;
use crate::types::{EntrySignal, FillResult};

/// Authenticated SDK client type (post-.authenticate()).
pub type AuthedSdkClient =
    SdkClient<polymarket_client_sdk_v2::auth::state::Authenticated<polymarket_client_sdk_v2::auth::Normal>>;

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

/// Place a Fill-or-Kill market buy order at a limit pegged to the observed ask + max_slippage.
#[allow(clippy::too_many_arguments)]
pub async fn place_fok_buy(
    sdk_client: &AuthedSdkClient,
    config: &RuntimeConfig,
    wallet: &PrivateKeySigner,
    signal: &EntrySignal,
    shares: f64,
    tick_size: &str,
    neg_risk: bool,
) -> Result<FillResult, String> {
    let raw = signal.ask_price + config.max_slippage;
    let limit = (raw * 100.0).round() / 100.0;
    let limit = limit.clamp(0.02, 0.99);

    place_fok_buy_raw(sdk_client, config, wallet, signal, limit, shares, tick_size, neg_risk).await
}

/// Place a Fill-or-Kill market buy order at an exact limit price (no slippage adjustment).
#[allow(clippy::too_many_arguments)]
pub async fn place_fok_buy_raw(
    sdk_client: &AuthedSdkClient,
    _config: &RuntimeConfig,
    wallet: &PrivateKeySigner,
    signal: &EntrySignal,
    price: f64,
    shares: f64,
    tick_size: &str,
    neg_risk: bool,
) -> Result<FillResult, String> {
    let cost = price * shares;
    info!(
        "Placing FOK BUY: {} {} shares @ ${:.2} (cost ${:.2})",
        signal.side, shares, price, cost
    );

    let token_id = alloy::primitives::U256::from_str(&signal.token_id)
        .map_err(|e| format!("Invalid token ID: {}", e))?;

    let sdk_tick_size = parse_tick_size(tick_size)?;
    sdk_client.set_tick_size(token_id, sdk_tick_size);
    sdk_client.set_neg_risk(token_id, neg_risk);

    let dec_price = Decimal::from_str(&format!("{:.2}", price))
        .map_err(|e| format!("Invalid price decimal: {}", e))?;
    let dec_shares = Decimal::from_str(&format!("{:.2}", shares))
        .map_err(|e| format!("Invalid shares decimal: {}", e))?;

    let signable = sdk_client
        .limit_order()
        .token_id(token_id)
        .side(Side::Buy)
        .price(dec_price)
        .size(dec_shares)
        .order_type(OrderType::FOK)
        .build()
        .await
        .map_err(|e| format!("SDK build order: {}", e))?;

    let signed = sdk_client
        .sign(wallet, signable)
        .await
        .map_err(|e| format!("SDK sign order: {}", e))?;

    let body = serde_json::to_string(&signed)
        .unwrap_or_else(|_| "serialize failed".into());
    debug!("Order JSON: {}", body);

    let resp = sdk_client
        .post_order(signed)
        .await
        .map_err(|e| format!("SDK post order: {}", e))?;

    if let Some(ref error) = resp.error_msg {
        if !error.is_empty() {
            return Err(format!("Order error: {}", error));
        }
    }

    if !resp.success {
        return Err(format!("Order rejected: status={}", resp.status));
    }

    use polymarket_client_sdk_v2::clob::types::OrderStatusType;
    if resp.status == OrderStatusType::Unmatched {
        info!("FOK order {} was not filled", resp.order_id);
        return Ok(FillResult {
            order_id: resp.order_id,
            fill_price: 0.0,
            filled_size: 0.0,
        });
    }

    let filled_size = resp.taking_amount.to_string().parse::<f64>().unwrap_or(0.0);
    let making = resp.making_amount.to_string().parse::<f64>().unwrap_or(0.0);
    let fill_price = if filled_size > 0.0 {
        making / filled_size
    } else {
        price
    };

    info!(
        "Order {} filled: {:.2} shares @ ${:.4}",
        resp.order_id, filled_size, fill_price
    );
    Ok(FillResult {
        order_id: resp.order_id,
        fill_price,
        filled_size,
    })
}

/// Simulate a trade for dry-run mode. Returns a FillResult with simulated values.
pub fn simulate_trade(signal: &EntrySignal, shares: f64) -> FillResult {
    info!(
        "[DRY RUN] Simulated FOK BUY: {} {} shares @ ${:.2}",
        signal.side, shares, signal.ask_price
    );
    FillResult {
        order_id: format!("dry-run-{}", Utc::now().timestamp_millis()),
        fill_price: signal.ask_price,
        filled_size: shares,
    }
}

#[cfg(test)]
mod tests {
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
}
