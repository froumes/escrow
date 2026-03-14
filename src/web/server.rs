use std::collections::HashSet;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

use axum::{
    extract::{
        ws::{Message, WebSocket},
        Request, State, WebSocketUpgrade,
    },
    http::StatusCode,
    middleware::Next,
    response::{Html, IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use crate::bot::BotClient;
use crate::state::CommandQueue;
use crate::types::{CommandPriority, CommandType};
use crate::websocket::CoflWebSocket;

// ── Shared state passed to every handler ─────────────────────

/// Holds references to all bot state that the web UI needs.
#[derive(Clone)]
pub struct WebSharedState {
    pub bot_client: BotClient,
    pub command_queue: CommandQueue,
    pub ws_client: CoflWebSocket,
    pub bazaar_flips_paused: Arc<AtomicBool>,
    /// Master macro pause — when true the command-processor loop skips work.
    pub macro_paused: Arc<AtomicBool>,
    pub enable_ah_flips: Arc<AtomicBool>,
    pub enable_bazaar_flips: Arc<AtomicBool>,
    /// Account names from config (may be single or multi).
    pub ingame_names: Vec<String>,
    pub current_account_index: usize,
    pub account_index_path: std::path::PathBuf,
    /// Broadcast channel for chat messages flowing to web clients.
    pub chat_tx: broadcast::Sender<String>,
    /// Password required to access the web panel (`None` = no auth).
    pub web_gui_password: Option<String>,
    /// Set of valid session tokens for authenticated clients.
    pub valid_sessions: Arc<Mutex<HashSet<String>>>,
    /// Cached Minecraft UUID for the current account (dashes format).
    /// Resolved lazily from the Mojang API on first `/api/auctions` request.
    pub player_uuid: Arc<tokio::sync::RwLock<Option<String>>>,
    /// Timestamp when the bot process started (for uptime tracking).
    pub started_at: std::time::Instant,
    /// Hypixel API key for fetching active auctions (optional).
    pub hypixel_api_key: Option<String>,
}

// ── JSON payloads ────────────────────────────────────────────

#[derive(Serialize)]
struct StatusResponse {
    state: String,
    macro_paused: bool,
    enable_ah_flips: bool,
    enable_bazaar_flips: bool,
    queue_depth: usize,
    current_account: String,
    current_account_index: usize,
    accounts: Vec<String>,
    purse: Option<u64>,
    uptime_seconds: u64,
}

#[derive(Deserialize)]
struct ChatMessage {
    message: String,
}

#[derive(Deserialize)]
struct TogglePayload {
    enabled: bool,
}

#[derive(Deserialize)]
struct SwitchPayload {
    index: usize,
}

#[derive(Deserialize)]
struct CancelAuctionPayload {
    item_name: String,
    starting_bid: i64,
}

#[derive(Deserialize)]
struct LoginPayload {
    password: String,
}

#[derive(Serialize)]
struct LoginResponse {
    success: bool,
}

#[derive(Serialize)]
struct AuctionEntry {
    uuid: String,
    item_name: String,
    /// SkyBlock item tag for icon lookup (e.g. "MITHRIL_DRILL_2")
    tag: Option<String>,
    highest_bid: i64,
    starting_bid: i64,
    bin: bool,
    /// ISO 8601 end timestamp
    end: String,
    /// Seconds remaining until auction expires (negative = expired)
    time_remaining_seconds: i64,
    /// Lore lines from the in-game item tooltip (only present for GUI-sourced entries)
    #[serde(skip_serializing_if = "Option::is_none")]
    lore: Option<Vec<String>>,
}

// ── Authentication middleware ─────────────────────────────────

/// Extract the `baf_session` cookie value from a request.
fn extract_session_cookie(req: &Request) -> Option<String> {
    req.headers()
        .get("cookie")?
        .to_str()
        .ok()?
        .split(';')
        .find_map(|c| {
            let c = c.trim();
            c.strip_prefix("baf_session=").map(|v| v.to_string())
        })
}

/// Middleware logic that enforces authentication when a password is configured.
/// Allows unauthenticated access to `GET /` (panel HTML) and `POST /api/login`.
async fn check_auth(
    s: WebSharedState,
    req: Request,
    next: Next,
) -> Response {
    // No password configured → skip auth entirely
    if s.web_gui_password.as_ref().map_or(true, |p| p.is_empty()) {
        return next.run(req).await;
    }

    let path = req.uri().path().to_string();

    // Always allow the panel page and the login endpoint without auth
    if path == "/" || path == "/api/login" {
        return next.run(req).await;
    }

    // Collect all tokens to check
    let mut tokens_to_check: Vec<String> = Vec::new();

    // Session cookie
    if let Some(token) = extract_session_cookie(&req) {
        tokens_to_check.push(token);
    }

    // Authorization: Bearer <token> header
    if let Some(auth) = req.headers().get("authorization") {
        if let Ok(auth_str) = auth.to_str() {
            if let Some(token) = auth_str.strip_prefix("Bearer ") {
                tokens_to_check.push(token.to_string());
            }
        }
    }

    // Query parameter `token=` (for WebSocket connections)
    if let Some(query) = req.uri().query() {
        for pair in query.split('&') {
            if let Some(token) = pair.strip_prefix("token=") {
                tokens_to_check.push(token.to_string());
            }
        }
    }

    // Check all tokens against valid sessions (lock + release before await)
    let is_valid = {
        let sessions = s.valid_sessions.lock().unwrap();
        tokens_to_check.iter().any(|t| sessions.contains(t))
    };

    if is_valid {
        return next.run(req).await;
    }

    StatusCode::UNAUTHORIZED.into_response()
}

// ── Start the web server ─────────────────────────────────────

pub async fn start_web_server(state: WebSharedState, port: u16) {
    let has_password = state
        .web_gui_password
        .as_ref()
        .map(|p| !p.is_empty())
        .unwrap_or(false);

    let auth_state = state.clone();
    let app = Router::new()
        .route("/", get(index_page))
        .route("/api/login", axum::routing::post(login))
        .route("/api/status", get(get_status))
        .route("/api/pause", get(pause_macro).post(pause_macro))
        .route("/api/resume", get(resume_macro).post(resume_macro))
        .route("/api/inventory", get(get_inventory))
        .route("/api/toggle_ah", axum::routing::post(toggle_ah))
        .route("/api/toggle_bazaar", axum::routing::post(toggle_bazaar))
        .route("/api/chat/send", axum::routing::post(send_chat))
        .route("/api/chat/ws", get(chat_ws_handler))
        .route("/api/switch_account", axum::routing::post(switch_account))
        .route("/api/cancel_auction", axum::routing::post(cancel_auction))
        .route("/api/claim_purchases", axum::routing::post(claim_purchases))
        .route("/api/collect_bz_orders", axum::routing::post(collect_bz_orders))
        .route("/api/auctions", get(get_auctions))
        .route("/api/logs/latest", get(download_latest_log))
        .layer(axum::middleware::from_fn(move |req: Request, next: Next| {
            let s = auth_state.clone();
            async move { check_auth(s, req, next).await }
        }))
        .with_state(state);

    let addr = format!("0.0.0.0:{}", port);
    if has_password {
        info!(
            "Web control panel starting on http://{} (password protected)",
            addr
        );
    } else {
        info!(
            "Web control panel starting on http://{} (no password — set web_gui_password in config.toml to protect)",
            addr
        );
    }

    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            error!("Failed to bind web server on {}: {}", addr, e);
            return;
        }
    };

    if let Err(e) = axum::serve(listener, app).await {
        error!("Web server error: {}", e);
    }
}

// ── Route handlers ───────────────────────────────────────────

async fn index_page() -> Html<&'static str> {
    Html(include_str!("panel.html"))
}

async fn login(
    State(s): State<WebSharedState>,
    Json(payload): Json<LoginPayload>,
) -> impl IntoResponse {
    let expected = match &s.web_gui_password {
        Some(p) if !p.is_empty() => p,
        _ => {
            // No password configured — login always succeeds (no cookie needed)
            return (StatusCode::OK, Json(LoginResponse { success: true })).into_response();
        }
    };

    // Constant-time password comparison to prevent timing attacks
    if payload.password.len() != expected.len()
        || payload
            .password
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            != 0
    {
        info!("[WebGUI] Failed login attempt from web panel");
        return (
            StatusCode::UNAUTHORIZED,
            Json(LoginResponse { success: false }),
        )
            .into_response();
    }

    // Generate a random session token and cap the number of active sessions
    let token = uuid::Uuid::new_v4().to_string();
    {
        let mut sessions = s.valid_sessions.lock().unwrap();
        // Limit to 64 active sessions; evict oldest when full
        if sessions.len() >= 64 {
            if let Some(oldest) = sessions.iter().next().cloned() {
                sessions.remove(&oldest);
            }
        }
        sessions.insert(token.clone());
    }

    info!("[WebGUI] Successful login via web panel");

    let cookie = format!(
        "baf_session={}; Path=/; HttpOnly; SameSite=Strict; Max-Age=604800",
        token
    );
    (
        StatusCode::OK,
        [("set-cookie", cookie)],
        Json(LoginResponse { success: true }),
    )
        .into_response()
}

async fn get_status(State(s): State<WebSharedState>) -> Json<StatusResponse> {
    Json(StatusResponse {
        state: format!("{:?}", s.bot_client.state()),
        macro_paused: s.macro_paused.load(Ordering::Relaxed),
        enable_ah_flips: s.enable_ah_flips.load(Ordering::Relaxed),
        enable_bazaar_flips: s.enable_bazaar_flips.load(Ordering::Relaxed),
        queue_depth: s.command_queue.len(),
        current_account: s.ingame_names.get(s.current_account_index).cloned().unwrap_or_default(),
        current_account_index: s.current_account_index,
        accounts: s.ingame_names.clone(),
        purse: s.bot_client.get_purse(),
        uptime_seconds: s.started_at.elapsed().as_secs(),
    })
}

async fn pause_macro(State(s): State<WebSharedState>) -> impl IntoResponse {
    s.macro_paused.store(true, Ordering::Relaxed);
    info!("[WebGUI] Macro paused via web panel");
    let _ = s.chat_tx.send("[BAF Web] Macro paused".to_string());
    StatusCode::OK
}

async fn resume_macro(State(s): State<WebSharedState>) -> impl IntoResponse {
    s.macro_paused.store(false, Ordering::Relaxed);
    info!("[WebGUI] Macro resumed via web panel");
    let _ = s.chat_tx.send("[BAF Web] Macro resumed".to_string());
    StatusCode::OK
}

async fn get_inventory(State(s): State<WebSharedState>) -> impl IntoResponse {
    match s.bot_client.get_cached_inventory_json() {
        Some(json) => (StatusCode::OK, json),
        None => (StatusCode::OK, r#"{"slots":[]}"#.to_string()),
    }
}

async fn toggle_ah(
    State(s): State<WebSharedState>,
    Json(payload): Json<TogglePayload>,
) -> impl IntoResponse {
    s.enable_ah_flips.store(payload.enabled, Ordering::Relaxed);
    info!("[WebGUI] AH flips set to {} via web panel", payload.enabled);
    let msg = format!("[BAF Web] AH flips {}", if payload.enabled { "enabled" } else { "disabled" });
    let _ = s.chat_tx.send(msg);
    StatusCode::OK
}

async fn toggle_bazaar(
    State(s): State<WebSharedState>,
    Json(payload): Json<TogglePayload>,
) -> impl IntoResponse {
    s.enable_bazaar_flips.store(payload.enabled, Ordering::Relaxed);
    info!("[WebGUI] Bazaar flips set to {} via web panel", payload.enabled);
    let msg = format!("[BAF Web] Bazaar flips {}", if payload.enabled { "enabled" } else { "disabled" });
    let _ = s.chat_tx.send(msg);
    StatusCode::OK
}

// ── Shared chat input processor ───────────────────────────────

/// Process a chat input string the same way the console does:
/// - `/cofl <cmd>` or `/baf <cmd>` → send to Coflnet WebSocket
/// - `/<command>` → queue as Minecraft SendChat command
/// - plain text → send to Coflnet as "chat" type
async fn process_chat_input(input: &str, state: &WebSharedState) {
    let lowercase = input.to_lowercase();

    if lowercase.starts_with("/cofl") || lowercase.starts_with("/baf") {
        let parts: Vec<&str> = input.split_whitespace().collect();
        if parts.len() > 1 {
            let command = parts[1];
            let args = parts[2..].join(" ");
            let data_json = serde_json::to_string(&args).unwrap_or_else(|_| "\"\"".to_string());
            let message = serde_json::json!({
                "type": command,
                "data": data_json
            })
            .to_string();
            if let Err(e) = state.ws_client.send_message(&message).await {
                error!("[WebGUI] Failed to send command to websocket: {}", e);
            }
        }
    } else if input.starts_with('/') {
        state.command_queue.enqueue(
            CommandType::SendChat {
                message: input.to_string(),
            },
            CommandPriority::High,
            false,
        );
    } else {
        let data_json = serde_json::to_string(&input).unwrap_or_else(|_| "\"\"".to_string());
        let message = serde_json::json!({
            "type": "chat",
            "data": data_json
        })
        .to_string();
        if let Err(e) = state.ws_client.send_message(&message).await {
            error!("[WebGUI] Failed to send chat to websocket: {}", e);
        }
    }

    let _ = state.chat_tx.send(format!("> {}", input));
}

async fn send_chat(
    State(s): State<WebSharedState>,
    Json(payload): Json<ChatMessage>,
) -> impl IntoResponse {
    let input = payload.message.trim().to_string();
    if input.is_empty() {
        return StatusCode::BAD_REQUEST;
    }

    process_chat_input(&input, &s).await;
    StatusCode::OK
}

async fn switch_account(
    State(s): State<WebSharedState>,
    Json(payload): Json<SwitchPayload>,
) -> impl IntoResponse {
    if s.ingame_names.len() <= 1 {
        return (StatusCode::BAD_REQUEST, "Multi-account not active");
    }
    if payload.index >= s.ingame_names.len() {
        return (StatusCode::BAD_REQUEST, "Invalid account index");
    }

    let next_name = &s.ingame_names[payload.index];
    info!(
        "[WebGUI] Switching to account {} ({}) via web panel",
        payload.index + 1,
        next_name
    );

    if let Err(e) = std::fs::write(&s.account_index_path, payload.index.to_string()) {
        warn!("[WebGUI] Failed to write account index: {}", e);
    }

    let _ = s
        .chat_tx
        .send(format!("[BAF Web] Switching to account {}...", next_name));

    // Restart the process with the new account index.
    tokio::spawn(async {
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        crate::utils::restart_process();
    });

    (StatusCode::OK, "Switching account — process will restart")
}

async fn cancel_auction(
    State(s): State<WebSharedState>,
    Json(payload): Json<CancelAuctionPayload>,
) -> impl IntoResponse {
    info!(
        "[WebGUI] Cancel auction requested: '{}' (bid: {})",
        payload.item_name, payload.starting_bid
    );

    let _ = s.chat_tx.send(format!(
        "[BAF Web] Cancelling auction: {}...",
        payload.item_name
    ));

    s.command_queue.enqueue(
        CommandType::CancelAuction {
            item_name: payload.item_name,
            starting_bid: payload.starting_bid,
        },
        CommandPriority::High,
        false,
    );

    (StatusCode::OK, "Cancel auction command queued")
}

async fn claim_purchases(
    State(s): State<WebSharedState>,
) -> impl IntoResponse {
    info!("[WebGUI] Claim purchases requested");

    let _ = s.chat_tx.send("[BAF Web] Checking unclaimed purchases...".to_string());

    s.command_queue.enqueue(
        CommandType::ClaimPurchasedItem,
        CommandPriority::High,
        false,
    );

    (StatusCode::OK, "Claim purchases command queued")
}

async fn collect_bz_orders(
    State(s): State<WebSharedState>,
) -> impl IntoResponse {
    info!("[WebGUI] Sell inventory instantly on bazaar requested");

    let _ = s.chat_tx.send("[BAF Web] Selling inventory on bazaar...".to_string());

    s.command_queue.enqueue(
        CommandType::SellInventoryBz,
        CommandPriority::High,
        false,
    );

    (StatusCode::OK, "Sell inventory on bazaar command queued")
}

// ── Active auctions ───────────────────────────────────────────

/// Resolve a Minecraft username to a UUID (with dashes) using the Mojang API.
/// Returns `None` if the lookup fails.
async fn fetch_player_uuid(username: &str) -> Option<String> {
    let url = format!(
        "https://api.mojang.com/users/profiles/minecraft/{}",
        username
    );
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .ok()?;
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let json: serde_json::Value = resp.json().await.ok()?;
    let raw_id = json.get("id")?.as_str()?;
    // Insert dashes into the raw 32-char hex UUID: 8-4-4-4-12
    if raw_id.len() != 32 {
        return None;
    }
    Some(format!(
        "{}-{}-{}-{}-{}",
        &raw_id[0..8],
        &raw_id[8..12],
        &raw_id[12..16],
        &raw_id[16..20],
        &raw_id[20..32]
    ))
}

async fn get_auctions(State(s): State<WebSharedState>) -> impl IntoResponse {
    // Try locally cached "My Auctions" data first (extracted from in-game GUI).
    // This provides immediate, accurate data without external API calls.
    if let Some(cached_json) = s.bot_client.get_cached_my_auctions_json() {
        // Parse the cached array and convert to AuctionEntry format
        if let Ok(cached_arr) = serde_json::from_str::<Vec<serde_json::Value>>(&cached_json) {
            let entries: Vec<AuctionEntry> = cached_arr
                .into_iter()
                .filter(|a| {
                    // Only include active auctions
                    a.get("status").and_then(|s| s.as_str()).unwrap_or("") == "active"
                })
                .map(|a| {
                    AuctionEntry {
                        uuid: String::new(),
                        item_name: a.get("item_name").and_then(|v| v.as_str()).unwrap_or("Unknown").to_string(),
                        tag: a.get("tag").and_then(|v| v.as_str()).map(|s| s.to_string()),
                        highest_bid: a.get("highest_bid").and_then(|v| v.as_i64()).unwrap_or(0),
                        starting_bid: a.get("starting_bid").and_then(|v| v.as_i64()).unwrap_or(0),
                        bin: a.get("bin").and_then(|v| v.as_bool()).unwrap_or(false),
                        end: String::new(),
                        time_remaining_seconds: a.get("time_remaining_seconds").and_then(|v| v.as_i64()).unwrap_or(0),
                        lore: a.get("lore").and_then(|v| v.as_array()).map(|arr| {
                            arr.iter().filter_map(|l| l.as_str().map(|s| s.to_string())).collect()
                        }),
                    }
                })
                .collect();
            if !entries.is_empty() {
                return Json(entries).into_response();
            }
        }
    }

    // Resolve UUID — use cache if available, otherwise fetch from Mojang
    let uuid = {
        let cached = s.player_uuid.read().await.clone();
        if let Some(u) = cached {
            u
        } else {
            let name = s
                .ingame_names
                .get(s.current_account_index)
                .cloned()
                .unwrap_or_default();
            if name.is_empty() {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(serde_json::json!({"error": "No player name configured"})),
                )
                    .into_response();
            }
            match fetch_player_uuid(&name).await {
                Some(u) => {
                    *s.player_uuid.write().await = Some(u.clone());
                    u
                }
                None => {
                    warn!("[WebGUI] Could not resolve UUID for player '{}'", name);
                    return (
                        StatusCode::SERVICE_UNAVAILABLE,
                        Json(serde_json::json!({"error": "Could not resolve player UUID"})),
                    )
                        .into_response();
                }
            }
        }
    };

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            error!("[WebGUI] Failed to build HTTP client for auctions: {}", e);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // Try Hypixel API first if an API key is configured
    if let Some(ref api_key) = s.hypixel_api_key {
        let uuid_no_dashes = uuid.replace('-', "");
        let url = format!(
            "https://api.hypixel.net/v2/skyblock/auction?player={}",
            uuid_no_dashes
        );
        match client
            .get(&url)
            .header("API-Key", api_key.as_str())
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<serde_json::Value>().await {
                    Ok(data) => {
                        if data.get("success").and_then(|v| v.as_bool()).unwrap_or(false) {
                            let entries = parse_hypixel_auctions(&data);
                            return Json(entries).into_response();
                        }
                        warn!("[WebGUI] Hypixel API returned success=false, falling back to Coflnet");
                    }
                    Err(e) => {
                        warn!("[WebGUI] Failed to parse Hypixel auction response: {}", e);
                    }
                }
            }
            Ok(resp) => {
                warn!("[WebGUI] Hypixel API returned status {}, falling back to Coflnet", resp.status());
            }
            Err(e) => {
                warn!("[WebGUI] Failed to fetch auctions from Hypixel: {}", e);
            }
        }
    }

    // Fallback: Fetch auctions from Coflnet
    let url = format!(
        "https://sky.coflnet.com/api/player/{}/auctions",
        uuid
    );

    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            warn!("[WebGUI] Failed to fetch auctions from Coflnet: {}", e);
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "Failed to fetch auctions"})),
            )
                .into_response();
        }
    };

    let raw: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            warn!("[WebGUI] Failed to parse auctions response: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "Failed to parse auction data"})),
            )
                .into_response();
        }
    };

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_else(|e| {
            warn!("[WebGUI] System clock appears to be before Unix epoch: {}", e);
            0
        });

    let entries: Vec<AuctionEntry> = raw
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|auction| {
            let end_str = auction.get("end")?.as_str()?;
            // Parse ISO 8601 end timestamp into epoch seconds; skip entries with invalid timestamps
            let end_secs = match chrono::DateTime::parse_from_rfc3339(end_str) {
                Ok(dt) => dt.timestamp(),
                Err(e) => {
                    warn!("[WebGUI] Skipping auction with invalid end timestamp '{}': {}", end_str, e);
                    return None;
                }
            };
            let time_remaining = end_secs - now_secs;
            // Only include auctions that are still active
            if time_remaining <= 0 {
                return None;
            }
            let item_name = auction
                .get("itemName")
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown")
                .to_string();
            let tag = auction
                .get("tag")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let highest_bid = auction
                .get("highestBid")
                .or_else(|| auction.get("highestBidAmount"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let starting_bid = auction
                .get("startingBid")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let bin = auction
                .get("bin")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let uuid = auction
                .get("uuid")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Some(AuctionEntry {
                uuid,
                item_name,
                tag,
                highest_bid,
                starting_bid,
                bin,
                end: end_str.to_string(),
                time_remaining_seconds: time_remaining,
                lore: None,
            })
        })
        .collect();

    Json(entries).into_response()
}

/// Parse auctions from Hypixel API response format.
/// Hypixel uses millisecond timestamps and different field names than Coflnet.
fn parse_hypixel_auctions(data: &serde_json::Value) -> Vec<AuctionEntry> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    data.get("auctions")
        .and_then(|a| a.as_array())
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|auction| {
            // Skip claimed auctions
            if auction.get("claimed").and_then(|v| v.as_bool()).unwrap_or(false) {
                return None;
            }
            let end_ms = auction.get("end").and_then(|v| v.as_i64()).unwrap_or(0);
            let time_remaining_ms = end_ms - now_ms;
            if time_remaining_ms <= 0 {
                return None;
            }
            let item_name = auction
                .get("item_name")
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown")
                .to_string();
            // Hypixel doesn't return a tag directly; derive from item_name for icon lookup
            let tag = derive_item_tag(&item_name);
            let highest_bid = auction
                .get("highest_bid_amount")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let starting_bid = auction
                .get("starting_bid")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let bin = auction
                .get("bin")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let uuid = auction
                .get("uuid")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            // Convert millisecond end timestamp to ISO 8601
            let nanos = ((end_ms % 1000).unsigned_abs() as u32) * 1_000_000;
            let end_iso = chrono::DateTime::from_timestamp(end_ms / 1000, nanos)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_default();
            Some(AuctionEntry {
                uuid,
                item_name,
                tag,
                highest_bid,
                starting_bid,
                bin,
                end: end_iso,
                time_remaining_seconds: (time_remaining_ms / 1000).max(0),
                lore: None,
            })
        })
        .collect()
}

/// Derive a SkyBlock item tag from an item name for icon lookup.
/// Converts "Aspect of the End" → "ASPECT_OF_THE_END".
fn derive_item_tag(item_name: &str) -> Option<String> {
    if item_name.is_empty() || item_name == "Unknown" {
        return None;
    }
    Some(
        item_name
            .chars()
            .map(|c| if c.is_alphanumeric() { c.to_ascii_uppercase() } else { '_' })
            .collect::<String>()
            .trim_matches('_')
            .to_string(),
    )
}

/// Serve the latest.log file as a downloadable file.
async fn download_latest_log() -> impl IntoResponse {
    let logs_dir = crate::logging::get_logs_dir();
    let log_path = logs_dir.join("latest.log");

    match tokio::fs::read(&log_path).await {
        Ok(contents) => {
            let headers = [
                (axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8"),
                (
                    axum::http::header::CONTENT_DISPOSITION,
                    "attachment; filename=\"latest.log\"",
                ),
            ];
            (StatusCode::OK, headers, contents).into_response()
        }
        Err(e) => {
            warn!("[WebGUI] Failed to read latest.log: {}", e);
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Log file not found"})),
            )
                .into_response()
        }
    }
}

// ── WebSocket handler for live chat ──────────────────────────

async fn chat_ws_handler(
    ws: WebSocketUpgrade,
    State(s): State<WebSharedState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_chat_ws(socket, s))
}

async fn handle_chat_ws(mut socket: WebSocket, state: WebSharedState) {
    let mut rx = state.chat_tx.subscribe();

    loop {
        tokio::select! {
            // Forward broadcast messages to the WebSocket client
            Ok(msg) = rx.recv() => {
                if socket.send(Message::Text(msg.into())).await.is_err() {
                    break;
                }
            }
            // Handle incoming messages from the WebSocket client (chat input)
            Some(Ok(msg)) = socket.recv() => {
                if let Message::Text(text) = msg {
                    let input = text.trim().to_string();
                    if !input.is_empty() {
                        process_chat_input(&input, &state).await;
                    }
                }
            }
            else => break,
        }
    }
    debug!("[WebGUI] WebSocket client disconnected");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_tag_from_item_name() {
        assert_eq!(derive_item_tag("Aspect of the End"), Some("ASPECT_OF_THE_END".to_string()));
        assert_eq!(derive_item_tag("Mithril Drill SX-R326"), Some("MITHRIL_DRILL_SX_R326".to_string()));
        assert_eq!(derive_item_tag(""), None);
        assert_eq!(derive_item_tag("Unknown"), None);
    }

    #[test]
    fn parse_hypixel_auctions_filters_claimed_and_expired() {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        let data = serde_json::json!({
            "success": true,
            "auctions": [
                {
                    "uuid": "abc123",
                    "item_name": "Diamond Sword",
                    "starting_bid": 1000,
                    "highest_bid_amount": 5000,
                    "end": now_ms + 3_600_000, // 1 hour from now
                    "bin": true,
                    "claimed": false
                },
                {
                    "uuid": "def456",
                    "item_name": "Expired Item",
                    "starting_bid": 500,
                    "highest_bid_amount": 0,
                    "end": now_ms - 1000, // Already expired
                    "bin": false,
                    "claimed": false
                },
                {
                    "uuid": "ghi789",
                    "item_name": "Claimed Item",
                    "starting_bid": 2000,
                    "highest_bid_amount": 3000,
                    "end": now_ms + 3_600_000,
                    "bin": false,
                    "claimed": true
                }
            ]
        });

        let entries = parse_hypixel_auctions(&data);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].item_name, "Diamond Sword");
        assert_eq!(entries[0].highest_bid, 5000);
        assert!(entries[0].bin);
        assert!(entries[0].tag.is_some());
        assert_eq!(entries[0].tag.as_deref(), Some("DIAMOND_SWORD"));
    }
}
