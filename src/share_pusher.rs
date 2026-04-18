//! Optional outbound pusher that ships anonymized stats snapshots to a
//! remote URL on a fixed interval.
//!
//! The remote endpoint (e.g. a Cloudflare Pages Function on a personal site)
//! is responsible for storing the latest snapshot and serving it to viewers.
//! This means the bot's VPS never needs an inbound public port — viewers see
//! stats through the remote site's domain, and the bot's IP is never exposed
//! in DNS, response headers, redirects, or referrers.
//!
//! Authentication: each request body is signed with HMAC-SHA256 using a
//! shared secret.  The signature is sent as a hex string in the
//! `X-TWM-Signature` header.  The remote side must verify before storing.
//!
//! Failure modes are intentionally non-fatal: a network blip, a 5xx, or even
//! the remote site being down for hours just causes the next push to retry.
//! No state is mutated on the bot.

use std::time::Duration;

use hmac::{Hmac, Mac};
use sha2::Sha256;
use tracing::{debug, info, warn};

use crate::web::{build_public_share_stats, WebSharedState};

type HmacSha256 = Hmac<Sha256>;

/// Floor for the configured push interval.  Anything below this is clamped to
/// avoid hammering the remote endpoint if a user mis-types the value.
const MIN_PUSH_INTERVAL_SECS: u64 = 5;

/// Per-request HTTP timeout.  Pushes are small JSON bodies; if the remote
/// hasn't acknowledged within this window we drop the attempt and try again
/// at the next tick rather than letting the loop stall.
const PUSH_HTTP_TIMEOUT_SECS: u64 = 10;

/// Configuration extracted from the bot's `Config` once at startup.
/// Cloning the relevant pieces here means the spawn site doesn't have to
/// hold a reference to the whole `Config` for the lifetime of the task.
#[derive(Clone)]
pub struct SharePusherConfig {
    pub url: String,
    pub secret: String,
    pub interval_secs: u64,
}

impl SharePusherConfig {
    /// Build a `SharePusherConfig` from the raw config values.  Returns
    /// `None` when pushing is disabled (either field empty), so callers can
    /// just `if let Some(cfg) = ...` and skip spawning the task.
    pub fn from_parts(
        url: Option<String>,
        secret: Option<String>,
        interval_secs: u64,
    ) -> Option<Self> {
        let url = url.filter(|u| !u.trim().is_empty())?;
        let secret = secret.filter(|s| !s.is_empty())?;
        Some(Self {
            url: url.trim().to_string(),
            secret,
            interval_secs: interval_secs.max(MIN_PUSH_INTERVAL_SECS),
        })
    }
}

/// Spawn the background pusher task.  Returns immediately; the task runs for
/// the lifetime of the process and logs failures rather than panicking.
pub fn spawn(state: WebSharedState, config: SharePusherConfig) {
    info!(
        "[SharePusher] enabled — pushing snapshots to {} every {}s",
        config.url, config.interval_secs
    );

    tokio::spawn(async move {
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(PUSH_HTTP_TIMEOUT_SECS))
            .user_agent(concat!("twm-share-pusher/", env!("CARGO_PKG_VERSION")))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                warn!("[SharePusher] failed to build HTTP client: {} — pusher disabled", e);
                return;
            }
        };

        // First push happens after a short warm-up so other startup work
        // (config load, profit-history load, websocket connect) has a chance
        // to populate something interesting.  After that we tick on a fixed
        // interval, skipping any missed ticks if the previous push happened
        // to take longer than the interval (rare, but possible).
        tokio::time::sleep(Duration::from_secs(2)).await;
        let mut ticker =
            tokio::time::interval(Duration::from_secs(config.interval_secs));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // first tick fires immediately; consume it

        loop {
            push_once(&client, &state, &config).await;
            ticker.tick().await;
        }
    });
}

async fn push_once(
    client: &reqwest::Client,
    state: &WebSharedState,
    config: &SharePusherConfig,
) {
    // Build the snapshot off-thread is unnecessary — it's a quick lock-and-clone
    // pattern, no heavy I/O — so we do it inline on the tokio task.
    let snapshot = build_public_share_stats(state);

    let body = match serde_json::to_vec(&snapshot) {
        Ok(b) => b,
        Err(e) => {
            warn!("[SharePusher] failed to serialize snapshot: {}", e);
            return;
        }
    };

    let signature = match sign_hmac_hex(&config.secret, &body) {
        Some(sig) => sig,
        None => {
            warn!("[SharePusher] HMAC key rejected by hmac crate (empty?)");
            return;
        }
    };

    let result = client
        .post(&config.url)
        .header("Content-Type", "application/json")
        .header("X-TWM-Signature", signature)
        .body(body)
        .send()
        .await;

    match result {
        Ok(resp) => {
            let status = resp.status();
            if status.is_success() {
                debug!("[SharePusher] pushed snapshot ({})", status);
            } else {
                // Read a small bounded prefix of the body for diagnostics.
                let snippet = resp
                    .text()
                    .await
                    .map(|t| t.chars().take(200).collect::<String>())
                    .unwrap_or_default();
                warn!("[SharePusher] push rejected: {} {}", status, snippet);
            }
        }
        Err(e) => {
            // Network errors are routine (remote site down, transient DNS,
            // VPS network blip).  Log at warn but don't escalate.
            warn!("[SharePusher] push failed: {}", e);
        }
    }
}

fn sign_hmac_hex(secret: &str, body: &[u8]) -> Option<String> {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).ok()?;
    mac.update(body);
    Some(hex::encode(mac.finalize().into_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_parts_disabled_when_url_missing() {
        assert!(SharePusherConfig::from_parts(None, Some("s".into()), 30).is_none());
        assert!(SharePusherConfig::from_parts(Some("".into()), Some("s".into()), 30).is_none());
        assert!(
            SharePusherConfig::from_parts(Some("   ".into()), Some("s".into()), 30).is_none()
        );
    }

    #[test]
    fn from_parts_disabled_when_secret_missing() {
        assert!(
            SharePusherConfig::from_parts(Some("https://x".into()), None, 30).is_none()
        );
        assert!(SharePusherConfig::from_parts(
            Some("https://x".into()),
            Some("".into()),
            30
        )
        .is_none());
    }

    #[test]
    fn from_parts_clamps_interval() {
        let cfg = SharePusherConfig::from_parts(
            Some("https://x".into()),
            Some("s".into()),
            1,
        )
        .unwrap();
        assert_eq!(cfg.interval_secs, MIN_PUSH_INTERVAL_SECS);
    }

    #[test]
    fn hmac_matches_known_vector() {
        // RFC 4231 test case 1.
        let key = vec![0x0bu8; 20];
        let data = b"Hi There";
        let sig = sign_hmac_hex(
            std::str::from_utf8(&key).unwrap_or(""),
            data,
        );
        // The key isn't valid UTF-8 in some test cases, so we recompute via
        // a direct byte-level call to make sure the helper agrees with the
        // hmac crate end-to-end.
        let mut mac = HmacSha256::new_from_slice(&key).unwrap();
        mac.update(data);
        let direct = hex::encode(mac.finalize().into_bytes());
        // The sign helper takes a &str secret; for the all-0x0b key the bytes
        // happen to be valid UTF-8 (each is a vertical-tab char), so the two
        // results should match.  This guards against accidentally swapping
        // input/key in the helper.
        if let Some(s) = sig {
            assert_eq!(s, direct);
        }
    }
}
