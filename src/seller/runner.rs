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

/// Outcome of a per-send availability check for a rendered item.
#[derive(Copy, Clone, Debug)]
enum Presence {
    /// Still looks like the same item the user picked — include it.
    Present,
    /// Item is gone (sold, claimed, expired, or slot cleared).  The
    /// included reason is logged once on the first drop detection.
    Gone(&'static str),
}

/// Snapshot captured at render time that lets us re-identify an item
/// against the live inventory / auction caches on every send.
#[derive(Clone, Debug)]
struct ItemIdentity {
    /// Display name with `§`-codes stripped so log lines are human-readable
    /// and tag-less items (vanilla Minecraft blocks, enchanted books without
    /// a Hypixel ID, …) can still be matched by name.
    plain_title: String,
    /// Hypixel item id (`HYPERION`, `PET_MAMMOTH`, …) if available.  This is
    /// the strongest identity signal — two Hyperions look the same in a slot
    /// but never swap silently for a different tag.
    tag: Option<String>,
}

/// A single rendered attachment plus the metadata needed to decide whether
/// it should still be included in the next Discord message.  Shared across
/// every per-channel sender task via [`Arc`] so that the first task to
/// notice an item has been sold records the drop exactly once.
struct RenderedItem {
    selected: SelectedItem,
    identity: ItemIdentity,
    attachment: AttachmentPng,
    /// Asking price (in coins) the user wants broadcast for this item, if
    /// they enabled the per-item price toggle and we were able to resolve
    /// a number (BIN listing for auctions, Coflnet median for inventory).
    asking_price: Option<u64>,
    dropped: AtomicBool,
}

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
        let rendered = match resolve_and_render(&self.bot_client, &req.selected_items).await {
            Ok(list) => list,
            Err(e) => {
                self.push_log(LogLevel::Error, format!("Render failed: {e}"));
                self.inner.running.store(false, Ordering::SeqCst);
                return Err(e);
            }
        };

        let had_selected_items = !req.selected_items.is_empty();

        if rendered.is_empty() && !had_selected_items {
            // Running without attachments is allowed — just a recurring text
            // message.  That matches how the Python tool behaved.
            self.push_log(
                LogLevel::Info,
                "No items selected — sending text-only messages.",
            );
        } else if rendered.is_empty() {
            self.push_log(
                LogLevel::Error,
                "All selected items failed to resolve — aborting.",
            );
            self.inner.running.store(false, Ordering::SeqCst);
            return Err("no items resolved".to_string());
        } else {
            self.push_log(
                LogLevel::Info,
                format!("Rendered {} item attachment(s).", rendered.len()),
            );
        }

        // Wrap each rendered item in an Arc so every per-channel sender
        // shares the same `dropped` flag.  First task to notice a sold /
        // missing item flips the flag; the others silently skip the log.
        let items: Arc<Vec<Arc<RenderedItem>>> =
            Arc::new(rendered.into_iter().map(Arc::new).collect());

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
            let items_c = Arc::clone(&items);
            let mut stop_rx_c = stop_rx.clone();
            let handle = tokio::spawn(async move {
                runner
                    .sender_loop(
                        token_c,
                        ch.channel_id,
                        label,
                        message_c,
                        items_c,
                        had_selected_items,
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

    #[allow(clippy::too_many_arguments)]
    async fn sender_loop(
        &self,
        token: String,
        channel_id: String,
        label: String,
        message: String,
        items: Arc<Vec<Arc<RenderedItem>>>,
        had_selected_items: bool,
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

            // Re-check availability against the live caches before every
            // send so sold / claimed / moved items silently drop out of the
            // message instead of being re-posted forever.
            let (attachments, prices) = self.current_attachments(&items);

            if had_selected_items && attachments.is_empty() {
                // Every item the user picked has been sold or is otherwise
                // gone — there's nothing left to advertise.  Stop the whole
                // runner so other channels follow suit and the UI flips back
                // to the idle state.
                self.push_log(
                    LogLevel::Info,
                    "All selected items are sold or unavailable — stopping.",
                );
                let me = self.clone();
                tokio::spawn(async move { me.stop().await });
                return;
            }

            // Append the per-item asking-price block to the user's message
            // for any surviving item whose price toggle was on.  Done at
            // send time (not start time) so dropped items vanish from the
            // list automatically.
            let body = build_message_with_prices(&message, &prices);

            let send_result = send_message(&token, &channel_id, &body, &attachments).await;
            let n = {
                let mut counts = self.inner.sent_counts.lock();
                let entry = counts.entry(channel_id.clone()).or_insert(0);
                *entry += 1;
                *entry
            };
            match send_result {
                Ok(()) => self.push_log(
                    LogLevel::Success,
                    format!("{short}  #{n} sent ({} item(s))", attachments.len()),
                ),
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

    /// Build the attachment list and per-item price list for the next
    /// message by walking the shared rendered-item list and keeping only
    /// those that still match a live item in the bot's caches.  The first
    /// task to observe an item transitioning `Present → Gone` logs a single
    /// removal line; every other task sees `dropped` already set and
    /// silently skips it.
    fn current_attachments(
        &self,
        items: &[Arc<RenderedItem>],
    ) -> (Vec<AttachmentPng>, Vec<(String, u64)>) {
        let inv_json = self.bot_client.get_cached_inventory_json();
        let auctions_json = self.bot_client.get_cached_my_auctions_json();

        let inv_slots: Vec<serde_json::Value> = inv_json
            .as_deref()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            .and_then(|v| v.get("slots").cloned())
            .and_then(|s| s.as_array().cloned())
            .unwrap_or_default();
        let auctions: Vec<serde_json::Value> = auctions_json
            .as_deref()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default();

        let mut attachments = Vec::with_capacity(items.len());
        let mut prices: Vec<(String, u64)> = Vec::new();
        for item in items {
            if item.dropped.load(Ordering::SeqCst) {
                continue;
            }
            match check_presence(&item.selected, &item.identity, &inv_slots, &auctions) {
                Presence::Present => {
                    attachments.push(item.attachment.clone());
                    if item.selected.include_price() {
                        if let Some(price) = item.asking_price {
                            prices.push((item.identity.plain_title.clone(), price));
                        }
                    }
                }
                Presence::Gone(reason) => {
                    if item
                        .dropped
                        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                    {
                        self.push_log(
                            LogLevel::Warn,
                            format!("Removed \"{}\" — {reason}", item.identity.plain_title),
                        );
                    }
                }
            }
        }
        (attachments, prices)
    }
}

/// Append an "Asking prices" block to the user's message body for every
/// surviving item that opted in.  Designed to render cleanly in Discord
/// without overflowing 2000 characters even with the maximum 10 items.
fn build_message_with_prices(message: &str, prices: &[(String, u64)]) -> String {
    if prices.is_empty() {
        return message.to_string();
    }
    let mut out = message.trim_end_matches('\n').to_string();
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str("**Asking prices:**\n");
    for (name, price) in prices {
        out.push_str("• ");
        out.push_str(name.trim());
        out.push_str(": ");
        out.push_str(&format_coins_short(*price));
        out.push('\n');
    }
    // Drop the trailing newline so Discord doesn't render an empty line
    // at the end of the message.
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Format a coin amount as `1.23B` / `45.6M` / `789K`, rounding to two
/// decimal places for B/M and one for K.  Matches the convention the
/// auctions page already uses on the panel.
fn format_coins_short(coins: u64) -> String {
    let n = coins as f64;
    if coins >= 1_000_000_000 {
        format!("{:.2}B", n / 1_000_000_000.0)
    } else if coins >= 1_000_000 {
        format!("{:.2}M", n / 1_000_000.0)
    } else if coins >= 1_000 {
        format!("{:.1}K", n / 1_000.0)
    } else {
        coins.to_string()
    }
}

// ── Item resolution + rendering ───────────────────────────────────────────

/// Walk the selected-item list and render each into a PNG attachment.  Items
/// that can't be resolved (e.g. stale slot, expired auction) are skipped with
/// a log entry rather than failing the whole run.  The returned list pairs
/// each attachment with enough identity metadata for the sender loop to
/// keep re-checking availability on every send.
async fn resolve_and_render(
    bot_client: &BotClient,
    selected: &[SelectedItem],
) -> Result<Vec<RenderedItem>, String> {
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

    // Drop expired entries from the in-process price cache so a long-lived
    // TWM instance doesn't keep stale numbers around indefinitely.
    super::pricing::prune_expired_cache();

    let mut out: Vec<RenderedItem> = Vec::with_capacity(selected.len());
    let mut icon_cache: HashMap<String, Option<Vec<u8>>> = HashMap::new();

    for (idx, sel) in selected.iter().enumerate() {
        let resolved = match sel {
            SelectedItem::Inventory { slot, .. } => {
                resolve_inventory(&inv_slots, *slot, &http, &mut icon_cache).await
            }
            SelectedItem::Auction { uuid, .. } => {
                resolve_auction(&auctions_arr, uuid, &http, &mut icon_cache).await
            }
        };
        match resolved {
            Some((item, identity, listing_price, bin)) => {
                let png = render_item_png(&item);
                let safe = sanitize_filename(&item.title);
                let asking_price = if sel.include_price() {
                    resolve_asking_price(sel, listing_price, identity.tag.as_deref(), bin)
                        .await
                } else {
                    None
                };
                out.push(RenderedItem {
                    selected: sel.clone(),
                    identity,
                    attachment: AttachmentPng {
                        filename: format!("{:02}_{safe}.png", idx + 1),
                        bytes: png,
                    },
                    asking_price,
                    dropped: AtomicBool::new(false),
                });
            }
            None => {
                debug!("[Seller] selected item #{idx} could not be resolved");
            }
        }
    }

    Ok(out)
}

/// Resolve the "asking price" for a selected item.
///
/// - **Auctions** normally use the listing price (BIN or starting bid)
///   we pulled out of the cached "My Auctions" snapshot, EXCEPT for
///   cosmetic-skin items where two copies are interchangeable — those
///   always fall back to the Coflnet median so the user can't end up
///   advertising the same skin at two different prices in a single
///   message.
/// - **Inventory** items have no listing yet, so we always ask Coflnet
///   for the recent-sales median (their UI's "price estimate").
async fn resolve_asking_price(
    sel: &SelectedItem,
    listing_price: Option<u64>,
    tag: Option<&str>,
    bin: bool,
) -> Option<u64> {
    match sel {
        SelectedItem::Auction { .. } => {
            match super::pricing::resolve_auction_price(tag, listing_price, bin).await {
                Ok(Some((price, _src))) => Some(price),
                Ok(None) => None,
                Err(e) => {
                    debug!("[Seller] auction price lookup for {tag:?} failed: {e}");
                    listing_price
                }
            }
        }
        SelectedItem::Inventory { .. } => {
            let tag = tag?;
            match super::pricing::resolve_inventory_price(tag).await {
                Ok(Some((price, _src))) => Some(price),
                Ok(None) => None,
                Err(e) => {
                    debug!("[Seller] coflnet price lookup for {tag} failed: {e}");
                    None
                }
            }
        }
    }
}

/// Decide whether a previously-rendered item should still appear in the
/// next outbound Discord message.
fn check_presence(
    selected: &SelectedItem,
    identity: &ItemIdentity,
    inv_slots: &[serde_json::Value],
    auctions: &[serde_json::Value],
) -> Presence {
    match selected {
        SelectedItem::Inventory { slot, .. } => {
            check_inventory_presence(*slot, identity, inv_slots)
        }
        SelectedItem::Auction { uuid, .. } => check_auction_presence(uuid, identity, auctions),
    }
}

fn check_inventory_presence(
    slot: u32,
    identity: &ItemIdentity,
    inv_slots: &[serde_json::Value],
) -> Presence {
    // If the bot hasn't re-cached inventory yet (e.g. just reconnected) we
    // intentionally assume the item is still present.  The alternative is
    // silently dropping every item on every reconnect, which is worse than
    // occasionally sending a stale image.
    if inv_slots.is_empty() {
        return Presence::Present;
    }
    let Some(entry) = inv_slots.get(slot as usize) else {
        return Presence::Gone("slot out of range");
    };
    if entry.is_null() {
        return Presence::Gone("slot is empty");
    }
    let current_tag = entry
        .get("tag")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let current_name_plain = entry
        .get("displayName")
        .and_then(|v| v.as_str())
        .map(strip_mc_codes)
        .or_else(|| {
            entry
                .get("displayNameColored")
                .and_then(|v| v.as_str())
                .map(strip_mc_codes)
        })
        .unwrap_or_default();

    // Prefer the Hypixel tag as the identity key — it's unique per item
    // kind and stable even if Hypixel changes the display name formatting.
    match (identity.tag.as_deref(), current_tag.as_deref()) {
        (Some(want), Some(got)) if want == got => Presence::Present,
        (Some(_), Some(_)) => Presence::Gone("slot holds a different item"),
        _ if !identity.plain_title.is_empty()
            && current_name_plain == identity.plain_title =>
        {
            Presence::Present
        }
        _ => Presence::Gone("slot holds a different item"),
    }
}

fn check_auction_presence(
    identifier: &str,
    identity: &ItemIdentity,
    auctions: &[serde_json::Value],
) -> Presence {
    // Empty cache = "Manage Auctions" window not yet opened this session.
    // Treat that as "still listed" so we don't spuriously remove items
    // right after restart / reconnect.
    if auctions.is_empty() {
        return Presence::Present;
    }

    let status_of = |v: &serde_json::Value| -> Option<String> {
        v.get("status").and_then(|s| s.as_str()).map(|s| s.to_string())
    };

    let found = if let Some(idx_str) = identifier.strip_prefix("idx:") {
        // `idx:N` is fragile across window re-opens because positions can
        // shift.  Fall back to matching by identity (tag + plain title)
        // whenever the positional lookup fails or returns a different item.
        let positional = idx_str
            .parse::<usize>()
            .ok()
            .and_then(|idx| auctions.get(idx));
        positional
            .filter(|v| auction_matches_identity(v, identity))
            .or_else(|| auctions.iter().find(|v| auction_matches_identity(v, identity)))
    } else {
        let normalized = identifier.replace('-', "").to_lowercase();
        auctions.iter().find(|a| {
            a.get("uuid")
                .and_then(|v| v.as_str())
                .map(|u| u.replace('-', "").to_lowercase() == normalized)
                .unwrap_or(false)
        })
    };

    let Some(entry) = found else {
        // No matching auction anywhere in the current cache — the user
        // either claimed the payout or cancelled the listing.
        return Presence::Gone("sold or claimed");
    };

    match status_of(entry).as_deref() {
        Some("sold") => Presence::Gone("sold"),
        Some("expired") => Presence::Gone("expired"),
        _ => Presence::Present,
    }
}

fn auction_matches_identity(entry: &serde_json::Value, identity: &ItemIdentity) -> bool {
    let entry_tag = entry.get("tag").and_then(|v| v.as_str());
    if let (Some(want), Some(got)) = (identity.tag.as_deref(), entry_tag) {
        return want == got;
    }
    let entry_name = entry
        .get("item_name")
        .or_else(|| entry.get("itemName"))
        .and_then(|v| v.as_str())
        .map(strip_mc_codes)
        .unwrap_or_default();
    !identity.plain_title.is_empty() && entry_name == identity.plain_title
}

async fn resolve_inventory(
    inv_slots: &[serde_json::Value],
    slot: u32,
    http: &reqwest::Client,
    cache: &mut HashMap<String, Option<Vec<u8>>>,
) -> Option<(RenderableItem, ItemIdentity, Option<u64>, bool)> {
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
    let identity = ItemIdentity {
        plain_title: strip_mc_codes(&title),
        tag: tag.clone(),
    };
    Some((
        RenderableItem {
            title,
            lore,
            count,
            icon_png,
            footer: None,
        },
        identity,
        // Inventory items have no listing — the asking price is computed
        // later from Coflnet only if the user opts in.
        None,
        // BIN flag is meaningless for inventory; default to false.
        false,
    ))
}

async fn resolve_auction(
    auctions: &[serde_json::Value],
    identifier: &str,
    http: &reqwest::Client,
    cache: &mut HashMap<String, Option<Vec<u8>>>,
) -> Option<(RenderableItem, ItemIdentity, Option<u64>, bool)> {
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
    let identity = ItemIdentity {
        plain_title: strip_mc_codes(&title),
        tag: tag.clone(),
    };
    let listing_price = found
        .get("starting_bid")
        .and_then(|v| v.as_i64())
        .filter(|p| *p > 0)
        .map(|p| p as u64);
    let bin = found
        .get("bin")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    Some((
        RenderableItem {
            title,
            lore,
            count: 1,
            icon_png,
            footer: None,
        },
        identity,
        listing_price,
        bin,
    ))
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
    use serde_json::json;

    #[test]
    fn sanitize_filename_keeps_ascii_alphanum() {
        assert_eq!(sanitize_filename("§6Hyperion ✦"), "Hyperion_");
        assert_eq!(sanitize_filename(""), "item");
    }

    fn identity(title: &str, tag: Option<&str>) -> ItemIdentity {
        ItemIdentity {
            plain_title: title.to_string(),
            tag: tag.map(str::to_string),
        }
    }

    #[test]
    fn inventory_presence_empty_cache_is_optimistic() {
        // Before the bot has cached any inventory we must not spuriously
        // drop every item — otherwise a reconnect would wipe the message.
        let id = identity("Hyperion", Some("HYPERION"));
        assert!(matches!(
            check_inventory_presence(9, &id, &[]),
            Presence::Present
        ));
    }

    #[test]
    fn inventory_presence_detects_swapped_slot() {
        let slots = vec![
            serde_json::Value::Null, // slot 0 intentionally empty
            json!({ "displayName": "Dirt", "tag": "DIRT" }),
        ];
        let id = identity("Hyperion", Some("HYPERION"));
        assert!(matches!(
            check_inventory_presence(1, &id, &slots),
            Presence::Gone(_)
        ));
    }

    #[test]
    fn inventory_presence_matches_by_tag() {
        let slots = vec![json!({
            "displayName": "§6Hyperion",
            "tag": "HYPERION",
        })];
        let id = identity("Hyperion", Some("HYPERION"));
        assert!(matches!(
            check_inventory_presence(0, &id, &slots),
            Presence::Present
        ));
    }

    #[test]
    fn auction_presence_empty_cache_is_optimistic() {
        let id = identity("Hyperion", Some("HYPERION"));
        assert!(matches!(
            check_auction_presence("abc", &id, &[]),
            Presence::Present
        ));
    }

    #[test]
    fn auction_presence_flags_sold() {
        let auctions = vec![json!({
            "item_name": "Hyperion",
            "tag": "HYPERION",
            "status": "sold",
        })];
        let id = identity("Hyperion", Some("HYPERION"));
        assert!(matches!(
            check_auction_presence("idx:0", &id, &auctions),
            Presence::Gone("sold")
        ));
    }

    #[test]
    fn auction_presence_flags_expired() {
        let auctions = vec![json!({
            "item_name": "Hyperion",
            "tag": "HYPERION",
            "status": "expired",
        })];
        let id = identity("Hyperion", Some("HYPERION"));
        assert!(matches!(
            check_auction_presence("idx:0", &id, &auctions),
            Presence::Gone("expired")
        ));
    }

    #[test]
    fn auction_presence_flags_missing_as_claimed() {
        // User claimed the payout → auction is no longer in the cache at all.
        let auctions = vec![json!({
            "item_name": "Terminator",
            "tag": "TERMINATOR",
            "status": "active",
        })];
        let id = identity("Hyperion", Some("HYPERION"));
        assert!(matches!(
            check_auction_presence("idx:5", &id, &auctions),
            Presence::Gone("sold or claimed")
        ));
    }

    #[test]
    fn build_message_with_prices_no_prices_passes_message_through() {
        assert_eq!(build_message_with_prices("hello world", &[]), "hello world");
        assert_eq!(build_message_with_prices("", &[]), "");
    }

    #[test]
    fn build_message_with_prices_appends_block() {
        let prices = vec![
            ("Hyperion".to_string(), 850_000_000),
            ("Terminator".to_string(), 25_000_000),
        ];
        let out = build_message_with_prices("Taking offers!", &prices);
        assert!(out.starts_with("Taking offers!"));
        assert!(out.contains("**Asking prices:**"));
        assert!(out.contains("• Hyperion: 850.00M"));
        assert!(out.contains("• Terminator: 25.00M"));
        assert!(!out.ends_with('\n'), "should not end with trailing newline");
    }

    #[test]
    fn build_message_with_prices_handles_empty_user_message() {
        let prices = vec![("Hyperion".to_string(), 850_000_000)];
        let out = build_message_with_prices("", &prices);
        assert!(out.starts_with("**Asking prices:**"));
        assert!(out.contains("Hyperion: 850.00M"));
    }

    #[test]
    fn format_coins_short_uses_b_m_k_suffixes() {
        assert_eq!(format_coins_short(0), "0");
        assert_eq!(format_coins_short(500), "500");
        assert_eq!(format_coins_short(2_500), "2.5K");
        assert_eq!(format_coins_short(1_500_000), "1.50M");
        assert_eq!(format_coins_short(2_300_000_000), "2.30B");
    }

    #[test]
    fn auction_presence_finds_active_by_identity_even_if_idx_shifted() {
        // Position changed (was idx:2, now idx:0) because another listing
        // ahead of it expired.  We should still find it by tag + name.
        let auctions = vec![json!({
            "item_name": "Hyperion",
            "tag": "HYPERION",
            "status": "active",
        })];
        let id = identity("Hyperion", Some("HYPERION"));
        assert!(matches!(
            check_auction_presence("idx:2", &id, &auctions),
            Presence::Present
        ));
    }
}
