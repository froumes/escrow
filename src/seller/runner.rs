//! Background sender that drives the Seller panel.
//!
//! `SellerRunner` owns the public API the web layer uses: start / stop / read
//! status.  Internally it spawns one `tokio::task` per configured Discord
//! channel, each sleeping its own cooldown between sends.  Rendering happens
//! once per start so the channel loops don't repeatedly hit the Coflnet icon
//! CDN for the same items.

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use super::config::{SellerChannel, SelectedItem};
use super::discord::{send_message, validate_channel, validate_token, AttachmentPng};
use super::render::{render_item_png, strip_mc_codes, RenderableItem};
use crate::bot::BotClient;

const MAX_LOG_ENTRIES: usize = 120;
const ICON_FETCH_TIMEOUT_SECS: u64 = 10;

/// Severity tag used for both log colouring and filtering on the panel.
#[derive(Copy, Clone, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Info,
    Success,
    Warn,
    Error,
}

#[derive(Clone, Debug, Serialize)]
pub struct SellerLog {
    pub ts: DateTime<Utc>,
    pub level: LogLevel,
    pub message: String,
}

/// Snapshot returned by `SellerRunner::status` for the panel to render.
#[derive(Debug, Serialize)]
pub struct SellerStatus {
    pub running: bool,
    pub started_at: Option<DateTime<Utc>>,
    pub resolved_items: usize,
    pub active_channels: Vec<String>,
    pub sent_counts: HashMap<String, u64>,
    pub logs: Vec<SellerLog>,
}

/// Payload accepted by `start` — all persistent config plus the items/
/// channels the user confirmed in the panel.
#[derive(Clone, Debug, Deserialize)]
pub struct StartRequest {
    pub token: String,
    pub message: String,
    pub channels: Vec<SellerChannel>,
    pub selected_items: Vec<SelectedItem>,
}

struct RunnerState {
    running: AtomicBool,
    started_at: Mutex<Option<DateTime<Utc>>>,
    active_channels: Mutex<Vec<String>>,
    sent_counts: Mutex<HashMap<String, u64>>,
    logs: Mutex<VecDeque<SellerLog>>,
    stop_tx: Mutex<Option<watch::Sender<bool>>>,
    handles: Mutex<Vec<JoinHandle<()>>>,
}

/// Public handle exposed through `WebSharedState` to the HTTP layer.
#[derive(Clone)]
pub struct SellerRunner {
    inner: Arc<RunnerState>,
    bot_client: BotClient,
}

impl SellerRunner {
    pub fn new(bot_client: BotClient) -> Self {
        Self {
            inner: Arc::new(RunnerState {
                running: AtomicBool::new(false),
                started_at: Mutex::new(None),
                active_channels: Mutex::new(Vec::new()),
                sent_counts: Mutex::new(HashMap::new()),
                logs: Mutex::new(VecDeque::with_capacity(MAX_LOG_ENTRIES)),
                stop_tx: Mutex::new(None),
                handles: Mutex::new(Vec::new()),
            }),
            bot_client,
        }
    }

    pub fn is_running(&self) -> bool {
        self.inner.running.load(Ordering::SeqCst)
    }

    pub fn status(&self) -> SellerStatus {
        SellerStatus {
            running: self.is_running(),
            started_at: *self.inner.started_at.lock(),
            resolved_items: 0, // filled in at start time via logs; kept for forward compat
            active_channels: self.inner.active_channels.lock().clone(),
            sent_counts: self.inner.sent_counts.lock().clone(),
            logs: self.inner.logs.lock().iter().cloned().collect(),
        }
    }

    pub fn clear_logs(&self) {
        self.inner.logs.lock().clear();
    }

    fn push_log(&self, level: LogLevel, message: impl Into<String>) {
        let msg = message.into();
        match level {
            LogLevel::Error => warn!("[Seller] {msg}"),
            LogLevel::Warn => warn!("[Seller] {msg}"),
            _ => info!("[Seller] {msg}"),
        }
        let mut logs = self.inner.logs.lock();
        if logs.len() >= MAX_LOG_ENTRIES {
            logs.pop_front();
        }
        logs.push_back(SellerLog {
            ts: Utc::now(),
            level,
            message: msg,
        });
    }

    /// Stop all running tasks.  Idempotent — stopping an already-stopped
    /// runner is a no-op.
    pub async fn stop(&self) {
        if !self.inner.running.swap(false, Ordering::SeqCst) {
            return;
        }
        if let Some(tx) = self.inner.stop_tx.lock().take() {
            let _ = tx.send(true);
        }
        let handles: Vec<JoinHandle<()>> = std::mem::take(&mut *self.inner.handles.lock());
        for h in handles {
            h.abort();
        }
        self.inner.active_channels.lock().clear();
        self.push_log(LogLevel::Info, "Stopped");
    }

    /// Validate config, resolve + render every selected item, and spawn a
    /// sender task per channel.  Returns immediately; progress appears in
    /// the log ring.
    pub async fn start(&self, req: StartRequest) -> Result<(), String> {
        if self
            .inner
            .running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err("already running".to_string());
        }

        self.clear_logs();
        self.inner.sent_counts.lock().clear();
        self.inner.active_channels.lock().clear();

        let token = req.token.trim().to_string();
        let message = req.message.clone();

        if token.is_empty() {
            self.inner.running.store(false, Ordering::SeqCst);
            return Err("discord token is required".to_string());
        }
        if req.channels.is_empty() {
            self.inner.running.store(false, Ordering::SeqCst);
            return Err("add at least one channel".to_string());
        }

        // Validate token + collect the username for log feedback.
        self.push_log(LogLevel::Info, "Validating token...");
        let username = match validate_token(&token).await {
            Ok(name) => name,
            Err(e) => {
                self.push_log(LogLevel::Error, format!("Token rejected: {e}"));
                self.inner.running.store(false, Ordering::SeqCst);
                return Err(e);
            }
        };
        self.push_log(LogLevel::Success, format!("Logged in as {username}"));

        // Validate each channel individually.  Any channel that fails is
        // dropped from the run with a warning so a typo doesn't kill the
        // whole batch.
        let mut ok_channels: Vec<(SellerChannel, String)> = Vec::new();
        for ch in &req.channels {
            match validate_channel(&token, &ch.channel_id).await {
                Ok(label) => {
                    self.push_log(
                        LogLevel::Success,
                        format!(
                            "Channel OK: #{label} ({}) — {}s delay",
                            ch.channel_id, ch.cooldown_seconds
                        ),
                    );
                    ok_channels.push((ch.clone(), label));
                }
                Err(e) => self.push_log(
                    LogLevel::Warn,
                    format!("Skipping channel {}: {e}", ch.channel_id),
                ),
            }
        }

        if ok_channels.is_empty() {
            self.push_log(LogLevel::Error, "No valid channels — aborting.");
            self.inner.running.store(false, Ordering::SeqCst);
            return Err("no valid channels".to_string());
        }

        // Resolve each selected item through the live bot caches.
        let attachments = match resolve_and_render(&self.bot_client, &req.selected_items).await {
            Ok(list) => list,
            Err(e) => {
                self.push_log(LogLevel::Error, format!("Render failed: {e}"));
                self.inner.running.store(false, Ordering::SeqCst);
                return Err(e);
            }
        };

        if attachments.is_empty() && req.selected_items.is_empty() {
            // Running without attachments is allowed — just a recurring text
            // message.  That matches how the Python tool behaved.
            self.push_log(
                LogLevel::Info,
                "No items selected — sending text-only messages.",
            );
        } else if attachments.is_empty() {
            self.push_log(
                LogLevel::Error,
                "All selected items failed to resolve — aborting.",
            );
            self.inner.running.store(false, Ordering::SeqCst);
            return Err("no items resolved".to_string());
        } else {
            self.push_log(
                LogLevel::Info,
                format!("Rendered {} item attachment(s).", attachments.len()),
            );
        }

        // Install a broadcast channel the per-channel tasks listen to for
        // cooperative cancellation.  Each task re-checks this flag on every
        // iteration and between cooldown seconds.
        let (stop_tx, stop_rx) = watch::channel(false);
        *self.inner.stop_tx.lock() = Some(stop_tx);
        *self.inner.started_at.lock() = Some(Utc::now());

        {
            let mut counts = self.inner.sent_counts.lock();
            let mut names = self.inner.active_channels.lock();
            for (ch, label) in &ok_channels {
                counts.entry(ch.channel_id.clone()).or_insert(0);
                names.push(format!("#{label}"));
            }
        }

        let mut new_handles: Vec<JoinHandle<()>> = Vec::new();
        for (ch, label) in ok_channels {
            let runner = self.clone();
            let token_c = token.clone();
            let message_c = message.clone();
            let atts_c = attachments.clone();
            let mut stop_rx_c = stop_rx.clone();
            let handle = tokio::spawn(async move {
                runner
                    .sender_loop(
                        token_c,
                        ch.channel_id,
                        label,
                        message_c,
                        atts_c,
                        ch.cooldown_seconds,
                        &mut stop_rx_c,
                    )
                    .await;
            });
            new_handles.push(handle);
        }
        *self.inner.handles.lock() = new_handles;

        Ok(())
    }

    async fn sender_loop(
        &self,
        token: String,
        channel_id: String,
        label: String,
        message: String,
        attachments: Vec<AttachmentPng>,
        cooldown_seconds: u64,
        stop_rx: &mut watch::Receiver<bool>,
    ) {
        let short = format!("#{label}");
        loop {
            // Copy the stop flag out immediately so the `parking_lot`-adjacent
            // Ref returned by `watch::Receiver::borrow()` never lives across
            // an `.await`.
            let should_stop = *stop_rx.borrow();
            if should_stop {
                break;
            }
            let send_result = send_message(&token, &channel_id, &message, &attachments).await;
            let n = {
                let mut counts = self.inner.sent_counts.lock();
                let entry = counts.entry(channel_id.clone()).or_insert(0);
                *entry += 1;
                *entry
            };
            match send_result {
                Ok(()) => self.push_log(LogLevel::Success, format!("{short}  #{n} sent")),
                Err(e) => self.push_log(LogLevel::Error, format!("{short}  #{n} FAILED — {e}")),
            }

            // Sleep in 1-second chunks so a stop signal interrupts quickly.
            for _ in 0..cooldown_seconds {
                if *stop_rx.borrow() {
                    return;
                }
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                    _ = stop_rx.changed() => {
                        if *stop_rx.borrow() {
                            return;
                        }
                    }
                }
            }
        }
    }
}

// ── Item resolution + rendering ───────────────────────────────────────────

/// Walk the selected-item list and render each into a PNG attachment.  Items
/// that can't be resolved (e.g. stale slot, expired auction) are skipped with
/// a log entry rather than failing the whole run.
async fn resolve_and_render(
    bot_client: &BotClient,
    selected: &[SelectedItem],
) -> Result<Vec<AttachmentPng>, String> {
    let inv_json = bot_client.get_cached_inventory_json();
    let my_auctions_json = bot_client.get_cached_my_auctions_json();

    let inv_slots: Vec<serde_json::Value> = inv_json
        .as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .and_then(|v| v.get("slots").cloned())
        .and_then(|s| s.as_array().cloned())
        .unwrap_or_default();

    let auctions_arr: Vec<serde_json::Value> = my_auctions_json
        .as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(ICON_FETCH_TIMEOUT_SECS))
        .build()
        .map_err(|e| format!("build http client: {e}"))?;

    let mut out: Vec<AttachmentPng> = Vec::with_capacity(selected.len());
    let mut icon_cache: HashMap<String, Option<Vec<u8>>> = HashMap::new();

    for (idx, sel) in selected.iter().enumerate() {
        let renderable = match sel {
            SelectedItem::Inventory { slot } => {
                resolve_inventory(&inv_slots, *slot, &http, &mut icon_cache).await
            }
            SelectedItem::Auction { uuid } => {
                resolve_auction(&auctions_arr, uuid, &http, &mut icon_cache).await
            }
        };
        match renderable {
            Some(item) => {
                let png = render_item_png(&item);
                let safe = sanitize_filename(&item.title);
                out.push(AttachmentPng {
                    filename: format!("{:02}_{safe}.png", idx + 1),
                    bytes: png,
                });
            }
            None => {
                debug!("[Seller] selected item #{idx} could not be resolved");
            }
        }
    }

    Ok(out)
}

async fn resolve_inventory(
    inv_slots: &[serde_json::Value],
    slot: u32,
    http: &reqwest::Client,
    cache: &mut HashMap<String, Option<Vec<u8>>>,
) -> Option<RenderableItem> {
    let entry = inv_slots.get(slot as usize)?;
    if entry.is_null() {
        return None;
    }
    let title = entry
        .get("displayNameColored")
        .and_then(|v| v.as_str())
        .or_else(|| entry.get("displayName").and_then(|v| v.as_str()))
        .or_else(|| entry.get("name").and_then(|v| v.as_str()))
        .unwrap_or("Item")
        .to_string();
    let lore: Vec<String> = entry
        .get("lore")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let count = entry.get("count").and_then(|v| v.as_u64()).unwrap_or(1) as u32;
    let tag = entry
        .get("tag")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let icon_png = fetch_icon(http, tag.as_deref(), cache).await;
    Some(RenderableItem {
        title,
        lore,
        count,
        icon_png,
        footer: None,
    })
}

async fn resolve_auction(
    auctions: &[serde_json::Value],
    identifier: &str,
    http: &reqwest::Client,
    cache: &mut HashMap<String, Option<Vec<u8>>>,
) -> Option<RenderableItem> {
    // Support both real UUIDs (returned by the Hypixel API path) and the
    // synthetic `idx:N` form the panel falls back to when `/api/auctions`
    // came from the in-game GUI cache (no UUID available there).
    let found_owned: serde_json::Value;
    let found: &serde_json::Value = if let Some(idx_str) = identifier.strip_prefix("idx:") {
        let idx: usize = idx_str.parse().ok()?;
        let v = auctions.get(idx)?;
        found_owned = v.clone();
        &found_owned
    } else {
        let normalized = identifier.replace('-', "").to_lowercase();
        auctions.iter().find(|a| {
            a.get("uuid")
                .and_then(|v| v.as_str())
                .map(|u| u.replace('-', "").to_lowercase() == normalized)
                .unwrap_or(false)
        })?
    };

    let title = found
        .get("item_name_colored")
        .or_else(|| found.get("itemNameColored"))
        .or_else(|| found.get("item_name"))
        .or_else(|| found.get("itemName"))
        .and_then(|v| v.as_str())
        .unwrap_or("Auction")
        .to_string();
    let lore: Vec<String> = found
        .get("lore")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    // Drop the auction-house metadata block (Seller / Buy it now / Ends in
    // / Click to inspect / …) and omit the BIN/Bid price footer — both
    // would give away that the item is currently listed on the AH.
    let lore = crate::seller::render::strip_auction_meta_lore(lore);
    let tag = found
        .get("tag")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let icon_png = fetch_icon(http, tag.as_deref(), cache).await;
    Some(RenderableItem {
        title,
        lore,
        count: 1,
        icon_png,
        footer: None,
    })
}

async fn fetch_icon(
    http: &reqwest::Client,
    tag: Option<&str>,
    cache: &mut HashMap<String, Option<Vec<u8>>>,
) -> Option<Vec<u8>> {
    let tag = tag?.trim();
    if tag.is_empty() {
        return None;
    }
    if let Some(cached) = cache.get(tag) {
        return cached.clone();
    }
    let url = format!("https://sky.coflnet.com/static/icon/{tag}");
    let fetched = match http.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => resp.bytes().await.ok().map(|b| b.to_vec()),
        Ok(resp) => {
            debug!("[Seller] icon fetch {tag} returned {}", resp.status());
            None
        }
        Err(e) => {
            debug!("[Seller] icon fetch {tag} errored: {e}");
            None
        }
    };
    cache.insert(tag.to_string(), fetched.clone());
    fetched
}

fn sanitize_filename(title: &str) -> String {
    let plain = strip_mc_codes(title);
    let mut out = String::with_capacity(plain.len());
    for ch in plain.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else if ch == ' ' {
            out.push('_');
        }
    }
    if out.is_empty() {
        "item".to_string()
    } else {
        out.chars().take(40).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_filename_keeps_ascii_alphanum() {
        assert_eq!(sanitize_filename("§6Hyperion ✦"), "Hyperion_");
        assert_eq!(sanitize_filename(""), "item");
    }
}
