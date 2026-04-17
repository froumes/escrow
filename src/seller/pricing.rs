//! Coflnet price-estimate helper used by the Seller panel.
//!
//! The "asking price" feature for inventory items needs a number to attach
//! to the outgoing Discord message even though the user hasn't listed the
//! item on the auction house yet.  Coflnet exposes a per-item price API
//! that returns the recent-sales median, which is the same number their
//! own UI calls "estimated price".  We hit that endpoint, cache the result
//! in-process, and gracefully degrade to "no price" on any failure rather
//! than blocking the send loop.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tracing::debug;

const COFLNET_PRICE_URL: &str = "https://sky.coflnet.com/api/item/price";
const HTTP_TIMEOUT_SECS: u64 = 8;
/// How long a fetched price stays warm.  Coflnet's median moves on the
/// order of minutes; five minutes is a comfortable balance between
/// freshness and not pummelling their API every panel refresh.
const CACHE_TTL_SECS: u64 = 300;

#[derive(Clone, Debug)]
struct CachedPrice {
    /// `None` means Coflnet had no data for this tag last time we asked —
    /// we still cache the negative answer briefly so back-to-back UI
    /// retries don't all hit the network.
    price: Option<u64>,
    fetched_at: Instant,
}

fn cache() -> &'static Mutex<HashMap<String, CachedPrice>> {
    static CACHE: OnceLock<Mutex<HashMap<String, CachedPrice>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .user_agent("twm-seller/1.0")
            .build()
            .expect("build coflnet http client")
    })
}

/// Look up the median recent-sales price Coflnet reports for `tag`.
///
/// Returns:
/// - `Ok(Some(price))` if Coflnet returned a usable median.
/// - `Ok(None)` if Coflnet responded successfully but reports no data
///   (e.g. brand-new item, or a tag with zero recent sales).
/// - `Err(message)` only on transport / parse failures the caller should
///   surface to the user.
pub async fn fetch_coflnet_median_price(tag: &str) -> Result<Option<u64>, String> {
    let tag = tag.trim();
    if tag.is_empty() {
        return Err("empty tag".to_string());
    }

    if let Some(cached) = lookup_cache(tag) {
        return Ok(cached);
    }

    let url = format!("{COFLNET_PRICE_URL}/{tag}");
    let resp = http_client()
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("network error: {e}"))?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        store_cache(tag, None);
        return Ok(None);
    }

    if !resp.status().is_success() {
        return Err(format!("Coflnet returned HTTP {}", resp.status()));
    }

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("decode JSON: {e}"))?;

    let price = extract_price(&body);
    debug!("[Seller/price] {tag} -> {price:?} (raw={body})");
    store_cache(tag, price);
    Ok(price)
}

/// Pull the best available price out of Coflnet's response.  The API has
/// shifted shape across versions — current responses include `median`,
/// `mean`, and `min`; older ones used `med`.  We try them in order from
/// "most representative of typical sale" to "most defensive fallback".
fn extract_price(body: &serde_json::Value) -> Option<u64> {
    let candidates = [
        "median",
        "med",
        "medianPrice",
        "mean",
        "average",
        "min",
        "lbin",
        "buyPrice",
    ];
    for key in candidates {
        if let Some(n) = body.get(key).and_then(json_to_positive_u64) {
            return Some(n);
        }
    }
    None
}

fn json_to_positive_u64(v: &serde_json::Value) -> Option<u64> {
    let n = v.as_f64().or_else(|| v.as_i64().map(|i| i as f64))?;
    if !n.is_finite() || n < 1.0 {
        return None;
    }
    Some(n.round() as u64)
}

fn lookup_cache(tag: &str) -> Option<Option<u64>> {
    let map = cache().lock();
    let entry = map.get(tag)?;
    if entry.fetched_at.elapsed() > Duration::from_secs(CACHE_TTL_SECS) {
        return None;
    }
    Some(entry.price)
}

fn store_cache(tag: &str, price: Option<u64>) {
    let mut map = cache().lock();
    map.insert(
        tag.to_string(),
        CachedPrice {
            price,
            fetched_at: Instant::now(),
        },
    );
}

/// Discard cached entries older than the TTL.  The runner calls this at
/// start time so that long-running TWM instances don't slowly accumulate
/// stale prices for items the user isn't selling anymore.
pub fn prune_expired_cache() {
    let mut map = cache().lock();
    map.retain(|_, v| v.fetched_at.elapsed() < Duration::from_secs(CACHE_TTL_SECS));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_price_prefers_median_over_mean() {
        let body = json!({ "median": 1000, "mean": 2000 });
        assert_eq!(extract_price(&body), Some(1000));
    }

    #[test]
    fn extract_price_falls_back_to_mean_then_min() {
        assert_eq!(extract_price(&json!({ "mean": 500 })), Some(500));
        assert_eq!(extract_price(&json!({ "min": 75 })), Some(75));
    }

    #[test]
    fn extract_price_returns_none_for_zero_or_missing() {
        assert_eq!(extract_price(&json!({ "median": 0 })), None);
        assert_eq!(extract_price(&json!({})), None);
        assert_eq!(extract_price(&json!({ "mode": "abc" })), None);
    }

    #[test]
    fn extract_price_rounds_floats() {
        assert_eq!(extract_price(&json!({ "median": 1234.7 })), Some(1235));
    }
}
