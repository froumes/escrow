//! Outbound share-stats pusher.
//!
//! When the operator clicks "Share Stats" in the panel for the first time
//! the bot generates a fresh `slot_id` + `push_secret` (see `share_state.rs`)
//! and persists them.  This task wakes on a fixed interval, reads the live
//! `WebSharedState.share_state` lock, and POSTs an anonymized snapshot to:
//!
//!   `<share_remote_base_url>/api/twm/push/<slot_id>`
//!
//! Authentication is a simple bearer secret in the `X-TWM-Push-Secret`
//! header.  The remote uses TOFU (trust-on-first-use): the very first push
//! to an unknown slot stores the secret in KV, and every subsequent push
//! must match byte-for-byte.  HTTPS keeps the secret confidential on the
//! wire.
//!
//! Failure modes are intentionally non-fatal: a network blip, a 5xx, or
//! even the remote site being down for hours just causes the next push to
//! retry.  No state is mutated on the bot when a push fails.
//!
//! Lifecycle: the task is spawned once at startup and runs forever.  When
//! `share_state` is `None` (operator hasn't opted in yet) each tick is a
//! cheap no-op.  As soon as auto-setup populates it, the next tick starts
//! pushing — no restart required.

use std::time::Duration;

use tracing::{debug, info, warn};

use crate::share_state::ShareState;
use crate::web::{build_public_share_stats, WebSharedState};

/// Floor for the configured push interval.  Anything below this is clamped
/// so a misconfigured value can't hammer the remote endpoint.
const MIN_PUSH_INTERVAL_SECS: u64 = 5;

/// Per-request HTTP timeout.  Pushes are small JSON bodies; if the remote
/// hasn't acknowledged within this window we drop the attempt and try
/// again at the next tick rather than letting the loop stall.
const PUSH_HTTP_TIMEOUT_SECS: u64 = 10;

/// Minimum configuration the pusher needs.  All knobs the operator might
/// reasonably want to override live here, separate from the share state
/// (which is generated and managed by the bot itself).
#[derive(Clone, Debug)]
pub struct SharePusherConfig {
    pub interval_secs: u64,
}

impl SharePusherConfig {
    pub fn new(interval_secs: u64) -> Self {
        Self {
            interval_secs: interval_secs.max(MIN_PUSH_INTERVAL_SECS),
        }
    }
}

/// Spawn the background pusher task.  Returns immediately; the task runs
/// for the lifetime of the process and logs failures rather than panicking.
///
/// The task never reads `config.share_remote_base_url` directly — it pulls
/// the URL out of the persisted `ShareState` so a stale config edit can't
/// quietly redirect pushes to a different origin while a slot is already
/// live on the original one.
pub fn spawn(state: WebSharedState, config: SharePusherConfig) {
    info!(
        "[SharePusher] task started — pushing every {}s once auto-setup runs",
        config.interval_secs
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

        // Brief warm-up so other startup work (config load, profit-history
        // load, websocket connect) populates something interesting before
        // the first push lands.  After that we tick on a fixed interval,
        // letting tokio coalesce missed ticks if a push takes longer than
        // the interval.
        tokio::time::sleep(Duration::from_secs(2)).await;
        let mut ticker =
            tokio::time::interval(Duration::from_secs(config.interval_secs));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // first tick fires immediately; consume it

        loop {
            // Snapshot the share state under a read lock so we never hold
            // the lock across the await on `client.post(...).send()`.
            // Cloning is cheap (two short hex strings + a URL string).
            let snapshot = {
                let guard = state.share_state.read().await;
                guard.clone()
            };
            match snapshot {
                Some(share) => push_once(&client, &state, &share).await,
                None => debug!("[SharePusher] share state not configured yet — skipping tick"),
            }
            ticker.tick().await;
        }
    });
}

/// One-shot wrapper used by `POST /api/share/share-link` to ensure the
/// Cloudflare KV slot is populated *before* we hand the operator a
/// signed Discord-link URL.  Without this, the user would have to wait
/// up to one full push interval (default 30s) after first slot creation
/// for `/auth/discord/start` to find the slot record.
///
/// Returns `Ok(status)` for any HTTP completion (the caller can decide
/// whether 2xx is required) and `Err(_)` for transport errors.  Builds
/// its own short-lived client so the public surface stays a single
/// function call — no shared state to thread through.
pub async fn push_now(
    state: &WebSharedState,
    share: &ShareState,
) -> Result<reqwest::StatusCode, reqwest::Error> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(PUSH_HTTP_TIMEOUT_SECS))
        .user_agent(concat!("twm-share-pusher/", env!("CARGO_PKG_VERSION")))
        .build()?;

    let snapshot = build_public_share_stats(state);
    let body = serde_json::to_vec(&snapshot)
        .expect("PublicShareStats always serializes");

    let resp = client
        .post(share.push_url())
        .header("Content-Type", "application/json")
        .header("X-TWM-Push-Secret", &share.push_secret)
        .body(body)
        .send()
        .await?;
    Ok(resp.status())
}

async fn push_once(
    client: &reqwest::Client,
    state: &WebSharedState,
    share: &ShareState,
) {
    let snapshot = build_public_share_stats(state);

    let body = match serde_json::to_vec(&snapshot) {
        Ok(b) => b,
        Err(e) => {
            warn!("[SharePusher] failed to serialize snapshot: {}", e);
            return;
        }
    };

    let url = share.push_url();
    let result = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("X-TWM-Push-Secret", &share.push_secret)
        .body(body)
        .send()
        .await;

    match result {
        Ok(resp) => {
            let status = resp.status();
            if status.is_success() {
                debug!("[SharePusher] pushed snapshot to {url} ({status})");
            } else {
                let snippet = resp
                    .text()
                    .await
                    .map(|t| t.chars().take(200).collect::<String>())
                    .unwrap_or_default();
                warn!("[SharePusher] push rejected: {} {}", status, snippet);
            }
        }
        Err(e) => {
            // Network errors are routine (remote down, transient DNS, VPS
            // network blip).  Log at warn but don't escalate.
            warn!("[SharePusher] push failed: {}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_clamps_short_intervals() {
        let cfg = SharePusherConfig::new(1);
        assert_eq!(cfg.interval_secs, MIN_PUSH_INTERVAL_SECS);
    }

    #[test]
    fn config_keeps_reasonable_intervals() {
        let cfg = SharePusherConfig::new(60);
        assert_eq!(cfg.interval_secs, 60);
    }
}
