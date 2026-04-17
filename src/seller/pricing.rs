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

/// Is `tag` a Hypixel SkyBlock cosmetic-skin item?
///
/// Skin items are fungible — two copies of "WISE_DRAGON_SKIN" are
/// indistinguishable in-game and should always be priced the same on
/// the auction house.  Every official skin tag I'm aware of follows the
/// same naming convention: either it starts with `SKIN_`, or it has
/// `_SKIN` as a discrete underscore-delimited word.  We match exactly
/// that pattern so non-skin tags that merely *contain* the letters
/// "skin" (e.g. `PUMPKIN_SKINNED`, `SKINNY_BLOCK`) don't false-positive.
pub fn is_skin_tag(tag: &str) -> bool {
    let upper = tag.to_uppercase();
    if upper.starts_with("SKIN_") {
        return true;
    }
    if let Some(idx) = upper.find("_SKIN") {
        // Accept "_SKIN" only when it stands alone — at the end of the
        // tag, or followed by another underscore-delimited word.
        let after = &upper[idx + "_SKIN".len()..];
        if after.is_empty() || after.starts_with('_') {
            return true;
        }
    }
    false
}

/// Source label for a resolved asking price; surfaced to the panel UI
/// via the `source` field on `/api/seller/price_estimate` responses.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PriceSource {
    /// Coflnet recent-sales median for the item tag.
    CoflnetMedian,
    /// The user's own active BIN listing price.
    BinListing,
    /// The user's own active starting-bid price.
    AuctionListing,
}

impl PriceSource {
    pub fn as_str(self) -> &'static str {
        match self {
            PriceSource::CoflnetMedian => "coflnet_median",
            PriceSource::BinListing => "bin_listing",
            PriceSource::AuctionListing => "auction_listing",
        }
    }
}

/// Resolve the canonical asking price for an inventory item.  Inventory
/// has no listing yet, so we always go to Coflnet.
pub async fn resolve_inventory_price(tag: &str) -> Result<Option<(u64, PriceSource)>, String> {
    let p = fetch_coflnet_median_price(tag).await?;
    Ok(p.map(|price| (price, PriceSource::CoflnetMedian)))
}

/// Resolve the canonical asking price for an active-auction item.
///
/// For skin items we deliberately ignore the user's listing price and
/// fall back to the Coflnet median: skins are interchangeable, and
/// duplicate listings should never appear at different prices in the
/// outgoing message.  For non-skin items the user's listing is what
/// they're actually asking, so we keep that.
pub async fn resolve_auction_price(
    tag: Option<&str>,
    listing_price: Option<u64>,
    bin: bool,
) -> Result<Option<(u64, PriceSource)>, String> {
    if let Some(t) = tag {
        if is_skin_tag(t) {
            if let Some(price) = fetch_coflnet_median_price(t).await? {
                return Ok(Some((price, PriceSource::CoflnetMedian)));
            }
            // Coflnet had no data for this skin — fall through to the
            // listing price below so the user still sees *something*.
        }
    }
    let listing = listing_price.filter(|p| *p > 0);
    Ok(listing.map(|price| {
        let source = if bin {
            PriceSource::BinListing
        } else {
            PriceSource::AuctionListing
        };
        (price, source)
    }))
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

    #[test]
    fn skin_tag_detection_covers_known_patterns() {
        assert!(is_skin_tag("WISE_DRAGON_SKIN"));
        assert!(is_skin_tag("BEE_DRAGON_PET_SKIN"));
        assert!(is_skin_tag("WARDEN_HELMET_SKIN_NICOLE"));
        assert!(is_skin_tag("SKIN_HYPERION_GOLDEN"));
        assert!(is_skin_tag("hyperion_skin_blue")); // case-insensitive
    }

    #[test]
    fn skin_tag_detection_avoids_false_positives() {
        assert!(!is_skin_tag("HYPERION"));
        assert!(!is_skin_tag("NECRON_HANDLE"));
        assert!(!is_skin_tag("SKINNY_BLOCK")); // contains SKIN but not as a token
        assert!(!is_skin_tag("PUMPKIN_SKINNED")); // ditto
    }

    #[test]
    fn price_source_string_labels_round_trip() {
        assert_eq!(PriceSource::CoflnetMedian.as_str(), "coflnet_median");
        assert_eq!(PriceSource::BinListing.as_str(), "bin_listing");
        assert_eq!(PriceSource::AuctionListing.as_str(), "auction_listing");
    }

    #[tokio::test]
    async fn auction_price_uses_listing_for_non_skin() {
        // Non-skin tag: the user's listing wins, no Coflnet hit needed.
        let (price, src) = resolve_auction_price(Some("HYPERION"), Some(1_000_000), true)
            .await
            .expect("must succeed")
            .expect("must produce a price");
        assert_eq!(price, 1_000_000);
        assert_eq!(src, PriceSource::BinListing);
    }

    #[tokio::test]
    async fn auction_price_falls_back_to_listing_when_no_tag() {
        let (price, src) = resolve_auction_price(None, Some(42_000), false)
            .await
            .expect("must succeed")
            .expect("must produce a price");
        assert_eq!(price, 42_000);
        assert_eq!(src, PriceSource::AuctionListing);
    }

    #[tokio::test]
    async fn auction_price_returns_none_when_listing_is_zero_and_no_tag() {
        let resolved = resolve_auction_price(None, Some(0), true).await.unwrap();
        assert!(resolved.is_none());
    }
}
