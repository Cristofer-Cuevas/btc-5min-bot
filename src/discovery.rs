use reqwest::Client;
use tracing::{debug, error, info, warn};

use crate::types::{GammaMarket, MarketWindow};

const GAMMA_API_URL: &str = "https://gamma-api.polymarket.com/markets";

/// Compute the current 5-minute window timestamp (rounded down to nearest 300s).
pub fn current_window_ts() -> u64 {
    let now = chrono::Utc::now().timestamp() as u64;
    now - (now % 300)
}

/// Compute the next 5-minute window timestamp.
pub fn next_window_ts() -> u64 {
    current_window_ts() + 300
}

/// Seconds remaining in the current window.
pub fn secs_remaining() -> i64 {
    let now = chrono::Utc::now().timestamp() as u64;
    let window_end = current_window_ts() + 300;
    (window_end as i64) - (now as i64)
}

/// Generate the slug for a given window timestamp.
pub fn window_slug(window_ts: u64) -> String {
    format!("btc-updown-5m-{}", window_ts)
}

/// Fetch market details from Gamma API for a given slug.
/// Retries with exponential backoff on failure.
pub async fn fetch_market(client: &Client, slug: &str) -> Option<MarketWindow> {
    let mut delay = std::time::Duration::from_secs(1);
    let max_retries = 5;

    for attempt in 0..max_retries {
        match try_fetch_market(client, slug).await {
            Ok(Some(market)) => return Some(market),
            Ok(None) => {
                if attempt < max_retries - 1 {
                    debug!(
                        "Market not found for slug {}, retrying in {:?} (attempt {}/{})",
                        slug,
                        delay,
                        attempt + 1,
                        max_retries
                    );
                    tokio::time::sleep(delay).await;
                    delay = std::cmp::min(delay * 2, std::time::Duration::from_secs(15));
                }
            }
            Err(e) => {
                warn!(
                    "Gamma API error for slug {}: {} (attempt {}/{})",
                    slug,
                    e,
                    attempt + 1,
                    max_retries
                );
                if attempt < max_retries - 1 {
                    tokio::time::sleep(delay).await;
                    delay = std::cmp::min(delay * 2, std::time::Duration::from_secs(15));
                }
            }
        }
    }

    error!("Failed to fetch market for slug {} after {} retries", slug, max_retries);
    None
}

async fn try_fetch_market(
    client: &Client,
    slug: &str,
) -> Result<Option<MarketWindow>, String> {
    let url = format!("{}?slug={}&limit=1", GAMMA_API_URL, slug);
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("HTTP error: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    let markets: Vec<GammaMarket> = resp
        .json()
        .await
        .map_err(|e| format!("JSON parse error: {}", e))?;

    let market = match markets.into_iter().next() {
        Some(m) => m,
        None => return Ok(None),
    };

    // Parse token IDs from JSON string array
    let token_ids_str = market
        .clob_token_ids
        .as_deref()
        .unwrap_or("[]");
    let token_ids: Vec<String> = serde_json::from_str(token_ids_str)
        .map_err(|e| format!("Failed to parse clobTokenIds: {}", e))?;

    if token_ids.len() < 2 {
        return Err("Market has fewer than 2 token IDs".into());
    }

    // Parse outcomes
    let outcomes_str = market.outcomes.as_deref().unwrap_or("[]");
    let outcomes: Vec<String> = serde_json::from_str(outcomes_str)
        .map_err(|e| format!("Failed to parse outcomes: {}", e))?;

    if outcomes.len() < 2 {
        return Err("Market has fewer than 2 outcomes".into());
    }

    // Map outcomes to token IDs by position
    let (up_token_id, down_token_id) = if outcomes[0].to_lowercase() == "up" {
        (token_ids[0].clone(), token_ids[1].clone())
    } else {
        (token_ids[1].clone(), token_ids[0].clone())
    };

    let tick_size = market.tick_size.unwrap_or_else(|| "0.01".into());
    let neg_risk = market.neg_risk.unwrap_or(false);

    info!(
        "Discovered market: slug={}, up={:.8}..., down={:.8}..., negRisk={}",
        slug,
        &up_token_id[..up_token_id.len().min(8)],
        &down_token_id[..down_token_id.len().min(8)],
        neg_risk
    );

    Ok(Some(MarketWindow {
        up_token_id,
        down_token_id,
        neg_risk,
        tick_size,
    }))
}

