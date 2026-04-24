use std::collections::{HashSet, VecDeque};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

use axum::{
    extract::{
        ws::{Message, WebSocket},
        Path, Request, State, WebSocketUpgrade,
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
use crate::bazaar_tracker::BazaarOrderTracker;
use crate::logging::print_mc_chat;
use crate::state::CommandQueue;
use crate::types::{CommandPriority, CommandType};
use crate::websocket::CoflWebSocket;

/// A single realized AH flip used for the flip-history panel.
///
/// Persisted to `flip_history.json` so the panel reflects every realised
/// flip across restarts — the in-memory ring would otherwise discard any
/// flip that was completed in a prior session, including flips whose
/// purchase happened in one session and sale in another.
#[derive(Clone, Serialize, Deserialize)]
pub struct FlipHistoryEntry {
    pub sold_at_unix: u64,
    pub item_name: String,
    pub buy_price: i64,
    pub sell_price: i64,
    pub profit: i64,
    pub time_to_sell_secs: u64,
    pub auction_uuid: Option<String>,
}

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
    /// Mirror of `config.skip_flips_when_inventory_full`.  Updated from the
    /// raw config save path so runtime behavior tracks the persisted value.
    pub skip_flips_when_inventory_full: Arc<AtomicBool>,
    /// Account names from config (may be single or multi).
    pub ingame_names: Vec<String>,
    pub current_account_index: usize,
    pub account_index_path: std::path::PathBuf,
    /// Broadcast channel for chat messages flowing to web clients.
    pub chat_tx: broadcast::Sender<String>,
    /// Password required to access the web panel (`None` = no auth).
    pub web_gui_password: Option<String>,
    /// Whether auth session cookies should include the `Secure` attribute.
    pub web_gui_cookie_secure: bool,
    /// Optional unguessable token enabling the read-only public stats page at
    /// `/share/{token}`.  `None` (or empty) disables the route entirely.
    pub web_share_token: Option<String>,
    /// Pre-formed public URL handed out by the panel's "Share Stats" button.
    /// When set, takes precedence over the locally-derived `/share/{token}`
    /// URL so operators can point viewers at a remote site (e.g. their own
    /// domain backed by an outbound `share_push_url`) instead of exposing
    /// the bot's host directly.
    pub share_public_url: Option<String>,
    /// Active web sessions with deterministic FIFO eviction at capacity.
    pub valid_sessions: Arc<Mutex<SessionStore>>,
    /// Cached Minecraft UUID for the current account (dashes format).
    /// Resolved lazily from the Mojang API on first `/api/auctions` request.
    pub player_uuid: Arc<tokio::sync::RwLock<Option<String>>>,
    /// Timestamp when the bot process started (for uptime tracking).
    pub started_at: std::time::Instant,
    /// Accumulated running time from previous sessions (seconds).
    /// Added to `started_at.elapsed()` to get total uptime across restarts.
    pub previous_session_secs: u64,
    /// Hypixel API key for fetching active auctions (optional).
    pub hypixel_api_key: Option<String>,
    /// Auto-detected COFL license index for the current IGN (0 = none detected).
    pub detected_cofl_license: Arc<std::sync::atomic::AtomicU32>,
    /// Shared profit tracker for AH and Bazaar realized profits.
    pub profit_tracker: Arc<crate::profit::ProfitTracker>,
    /// Session-only anonymize toggle for the web panel (defaults to OFF).
    /// Not persisted to config — resets to OFF on each process start.
    pub anonymize_webhook_name: Arc<AtomicBool>,
    /// Tracks active bazaar orders for the web panel and profit calculation.
    pub bazaar_tracker: Arc<BazaarOrderTracker>,
    /// Recent realized AH flips for the flip-history panel.
    pub flip_history: Arc<Mutex<VecDeque<FlipHistoryEntry>>>,
    /// Config loader for persisting changes to config.toml.
    pub config_loader: Arc<crate::config::ConfigLoader>,
    /// Background Discord auto-sender driving the "Seller" tab.
    pub seller_runner: crate::seller::SellerRunner,
    /// On-disk path for the Seller config JSON (sibling of `config.toml`).
    pub seller_config_path: std::path::PathBuf,
}

/// Ordered set-like storage for active web sessions.
///
/// - `order` preserves insertion order for deterministic eviction.
/// - `members` provides O(1) membership checks for auth validation.
pub struct SessionStore {
    order: VecDeque<String>,
    members: HashSet<String>,
}

impl SessionStore {
    pub fn new() -> Self {
        Self {
            order: VecDeque::new(),
            members: HashSet::new(),
        }
    }

    pub fn contains(&self, token: &str) -> bool {
        self.members.contains(token)
    }

    /// Inserts a session token while enforcing a hard max session count.
    ///
    /// When full, this evicts the oldest inserted token first (FIFO).
    pub fn insert_with_capacity(&mut self, token: String, max_sessions: usize) {
        if self.members.contains(&token) {
            self.order.retain(|t| t != &token);
            self.order.push_back(token);
            return;
        }

        if self.members.len() >= max_sessions {
            if let Some(oldest) = self.order.pop_front() {
                self.members.remove(&oldest);
            }
        }

        self.order.push_back(token.clone());
        self.members.insert(token);
    }
}

// ── JSON payloads ────────────────────────────────────────────

#[derive(Serialize)]
struct StatusResponse {
    state: String,
    macro_paused: bool,
    enable_ah_flips: bool,
    enable_bazaar_flips: bool,
    anonymize_webhook_name: bool,
    queue_depth: usize,
    current_account: String,
    current_account_index: usize,
    accounts: Vec<String>,
    purse: Option<u64>,
    uptime_seconds: u64,
    bazaar_at_limit: bool,
    auction_at_limit: bool,
    inventory_full: bool,
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
struct CancelBzOrderPayload {
    item_name: String,
    is_buy_order: bool,
}

#[derive(Deserialize)]
struct BuyAuctionPayload {
    auction_id: String,
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
struct ProfitResponse {
    all_time_ah_points: Vec<(u64, i64)>,
    all_time_bz_points: Vec<(u64, i64)>,
    all_time_ah_total: i64,
    all_time_bz_total: i64,
    session_ah_points: Vec<(u64, i64)>,
    session_bz_points: Vec<(u64, i64)>,
    session_ah_total: i64,
    session_bz_total: i64,
    session_uptime_seconds: u64,
}

/// Public (unauthenticated) profit summary — no IGN, no account info.
/// Used by the login page and OpenGraph embeds.
#[derive(Serialize)]
struct PublicProfitResponse {
    all_time_ah_total: i64,
    all_time_bz_total: i64,
    all_time_total: i64,
    session_ah_total: i64,
    session_bz_total: i64,
    session_total: i64,
    session_per_hour: f64,
    session_uptime_seconds: u64,
    session_ah_points: Vec<(u64, i64)>,
    session_bz_points: Vec<(u64, i64)>,
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

/// Extract the `twm_session` cookie value from a request.
fn extract_session_cookie(req: &Request) -> Option<String> {
    req.headers()
        .get("cookie")?
        .to_str()
        .ok()?
        .split(';')
        .find_map(|c| {
            let c = c.trim();
            c.strip_prefix("twm_session=").map(|v| v.to_string())
        })
}

/// Middleware logic that enforces authentication when a password is configured.
/// Allows unauthenticated access to `GET /` (panel HTML) and `POST /api/login`.
fn has_valid_auth_session(
    req: &Request,
    valid_sessions: &Arc<Mutex<SessionStore>>,
) -> bool {
    // Collect all tokens to check
    let mut tokens_to_check: Vec<String> = Vec::new();

    // Session cookie
    if let Some(token) = extract_session_cookie(req) {
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

    let sessions = valid_sessions.lock().unwrap();
    tokens_to_check.iter().any(|t| sessions.contains(t))
}

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

    // Always allow the panel page, shared theme css, login endpoint, public profit, and OG image without auth
    if path == "/"
        || path == "/shared-theme.css"
        || path == "/api/login"
        || path == "/api/profit/public"
        || path == "/api/og-image.png"
    {
        return next.run(req).await;
    }

    // The public stats share page enforces its own token check inside the
    // handler.  Bypass the password gate so anyone with the share URL can view
    // it even when the panel is otherwise password-protected.
    //
    // `/api/share/link` is the panel's own "give me the share URL" helper and
    // must stay behind the password gate so anonymous callers can't fish for
    // a configured viewer token.
    if path.starts_with("/share/")
        || (path.starts_with("/api/share/") && path != "/api/share/link")
    {
        return next.run(req).await;
    }

    if has_valid_auth_session(&req, &s.valid_sessions) {
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
        .route("/shared-theme.css", get(shared_theme_css))
        .route("/api/login", axum::routing::post(login))
        .route("/api/profit/public", get(get_profit_public))
        .route("/api/og-image.png", get(get_og_image))
        .route("/share/{token}", get(get_share_page))
        .route("/api/share/{token}/stats", get(get_share_stats))
        .route("/api/share/link", get(get_share_link))
        .route("/api/status", get(get_status))
        .route("/api/pause", get(pause_macro).post(pause_macro))
        .route("/api/resume", get(resume_macro).post(resume_macro))
        .route("/api/inventory", get(get_inventory))
        .route("/api/game-view", get(get_game_view))
        .route("/api/toggle_ah", axum::routing::post(toggle_ah))
        .route("/api/toggle_bazaar", axum::routing::post(toggle_bazaar))
        .route("/api/toggle_anonymize", axum::routing::post(toggle_anonymize))
        .route("/api/chat/send", axum::routing::post(send_chat))
        .route("/api/chat/ws", get(chat_ws_handler))
        .route("/api/switch_account", axum::routing::post(switch_account))
        .route("/api/cancel_auction", axum::routing::post(cancel_auction))
        .route("/api/buy_auction", axum::routing::post(buy_auction))
        .route("/api/claim_bz_orders", axum::routing::post(claim_bz_orders))
        .route("/api/cancel_bz_order", axum::routing::post(cancel_bz_order))
        .route("/api/cancel_all_bz_orders", axum::routing::post(cancel_all_bz_orders))
        .route("/api/auctions", get(get_auctions))
        .route("/api/bazaar_orders", get(get_bazaar_orders))
        .route("/api/queue", get(get_queue_status))
        .route("/api/config", get(get_config).post(save_config))
        .route("/api/logs/latest", get(download_latest_log))
        .route("/api/profit", get(get_profit))
        .route("/api/flip-history", get(get_flip_history))
        .route("/api/seller/config", get(get_seller_config).post(save_seller_config))
        .route("/api/seller/status", get(get_seller_status))
        .route("/api/seller/start", axum::routing::post(start_seller))
        .route("/api/seller/stop", axum::routing::post(stop_seller))
        .route("/api/seller/validate_token", axum::routing::post(seller_validate_token))
        .route("/api/seller/validate_channel", axum::routing::post(seller_validate_channel))
        .route("/api/seller/login", axum::routing::post(seller_login))
        .route("/api/seller/preview", axum::routing::post(seller_preview))
        .route("/api/seller/available_items", get(seller_available_items))
        .route(
            "/api/seller/price_estimate",
            axum::routing::post(seller_price_estimate),
        )
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

/// Helper to format large numbers for OG tags (e.g. 1.5M, 250K)
fn format_og_number(val: f64) -> String {
    let abs = val.abs();
    let formatted = if abs >= 1e9 {
        format!("{:.1}B", val / 1e9)
    } else if abs >= 1e6 {
        format!("{:.1}M", val / 1e6)
    } else if abs >= 1e3 {
        format!("{:.1}K", val / 1e3)
    } else {
        format!("{:.0}", val)
    };
    formatted
}

/// Helper to format uptime for OG tags
fn format_og_uptime(secs: u64) -> String {
    let d = secs / 86400;
    let h = (secs % 86400) / 3600;
    let m = (secs % 3600) / 60;
    if d > 0 {
        format!("{}d {}h {}m", d, h, m)
    } else if h > 0 {
        format!("{}h {}m", h, m)
    } else {
        format!("{}m", m)
    }
}

async fn index_page(State(s): State<WebSharedState>) -> Html<String> {
    let snapshot = s.profit_tracker.snapshot();
    let session_total = snapshot.session_ah_total + snapshot.session_bz_total;
    let all_time_total = snapshot.all_time_ah_total + snapshot.all_time_bz_total;
    let uptime = s.started_at.elapsed().as_secs();
    let hours = uptime as f64 / 3600.0;
    let per_hour = if hours > 0.0 { session_total as f64 / hours } else { 0.0 };

    let og_title = "TWM — Control Panel";
    let og_description = format!(
        "💰 Session Profit: {} | 🏦 All-Time Profit: {} | ⏱️ P/H: {} | 🕐 Uptime: {}",
        format_og_number(session_total as f64),
        format_og_number(all_time_total as f64),
        format_og_number(per_hour),
        format_og_uptime(uptime),
    );

    // Inject OG meta tags at the designated marker in the HTML template
    let og_tags = format!(
        "<meta property=\"og:title\" content=\"{og_title}\">\n\
         <meta property=\"og:description\" content=\"{og_description}\">\n\
         <meta property=\"og:type\" content=\"website\">\n\
         <meta property=\"og:image\" content=\"/api/og-image.png\">\n\
         <meta property=\"og:image:width\" content=\"1200\">\n\
         <meta property=\"og:image:height\" content=\"630\">\n\
         <meta name=\"twitter:card\" content=\"summary_large_image\">\n\
         <meta name=\"twitter:image\" content=\"/api/og-image.png\">",
    );

    let html = include_str!("panel.html")
        .replacen("<!-- OG_META_TAGS -->", &og_tags, 1);

    Html(html)
}

async fn shared_theme_css() -> impl IntoResponse {
    (
        [
            ("content-type", "text/css; charset=utf-8"),
            ("cache-control", "public, max-age=3600, stale-while-revalidate=86400"),
        ],
        include_str!("shared-theme.css"),
    )
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

    // Generate a random session token and cap active sessions with FIFO eviction.
    let token = uuid::Uuid::new_v4().to_string();
    {
        let mut sessions = s.valid_sessions.lock().unwrap();
        // Hard limit of 64 active sessions; always evicts the true oldest token first.
        sessions.insert_with_capacity(token.clone(), 64);
    }

    info!("[WebGUI] Successful login via web panel");

    let cookie = build_session_cookie(&token, s.web_gui_cookie_secure);
    (
        StatusCode::OK,
        [("set-cookie", cookie)],
        Json(LoginResponse { success: true }),
    )
        .into_response()
}

/// Build `Set-Cookie` value for an authenticated web panel session.
fn build_session_cookie(token: &str, secure: bool) -> String {
    let mut attrs = vec![
        format!("twm_session={token}"),
        "Path=/".to_string(),
        "HttpOnly".to_string(),
        "SameSite=Strict".to_string(),
        "Max-Age=604800".to_string(),
    ];
    if secure {
        attrs.push("Secure".to_string());
    }
    attrs.join("; ")
}

async fn get_status(State(s): State<WebSharedState>) -> Json<StatusResponse> {
    let anonymize = s.anonymize_webhook_name.load(Ordering::Relaxed);

    // When anonymize is enabled, hide account names in the web panel so
    // screenshots don't leak the player's IGN.
    let (current_account, accounts) = if anonymize {
        let hidden = "Hidden".to_string();
        let anon_accounts: Vec<String> = s.ingame_names.iter().map(|_| hidden.clone()).collect();
        let anon_current = anon_accounts.get(s.current_account_index).cloned().unwrap_or_default();
        (anon_current, anon_accounts)
    } else {
        (
            s.ingame_names.get(s.current_account_index).cloned().unwrap_or_default(),
            s.ingame_names.clone(),
        )
    };

    Json(StatusResponse {
        state: format!("{:?}", s.bot_client.state()),
        macro_paused: s.macro_paused.load(Ordering::Relaxed),
        enable_ah_flips: s.enable_ah_flips.load(Ordering::Relaxed),
        enable_bazaar_flips: s.enable_bazaar_flips.load(Ordering::Relaxed),
        anonymize_webhook_name: anonymize,
        queue_depth: s.command_queue.len(),
        current_account,
        current_account_index: s.current_account_index,
        accounts,
        purse: s.bot_client.get_purse(),
        uptime_seconds: s.previous_session_secs + s.started_at.elapsed().as_secs(),
        bazaar_at_limit: s.bot_client.is_bazaar_at_limit(),
        auction_at_limit: s.bot_client.is_auction_at_limit(),
        inventory_full: s.bot_client.is_inventory_full(),
    })
}

async fn pause_macro(State(s): State<WebSharedState>) -> impl IntoResponse {
    s.macro_paused.store(true, Ordering::Relaxed);
    info!("[WebGUI] Macro paused via web panel");
    let msg = "[TWM Web] Macro paused".to_string();
    print_mc_chat(&msg);
    let _ = s.chat_tx.send(msg);
    StatusCode::OK
}

async fn resume_macro(State(s): State<WebSharedState>) -> impl IntoResponse {
    s.macro_paused.store(false, Ordering::Relaxed);
    info!("[WebGUI] Macro resumed via web panel");
    let msg = "[TWM Web] Macro resumed".to_string();
    print_mc_chat(&msg);
    let _ = s.chat_tx.send(msg);
    StatusCode::OK
}

async fn get_inventory(State(s): State<WebSharedState>) -> impl IntoResponse {
    match s.bot_client.get_cached_inventory_json() {
        Some(json) => (StatusCode::OK, json),
        None => (StatusCode::OK, r#"{"slots":[]}"#.to_string()),
    }
}

async fn get_game_view(State(s): State<WebSharedState>) -> impl IntoResponse {
    match s.bot_client.get_cached_window_json() {
        Some(json) => (StatusCode::OK, json),
        None => (StatusCode::OK, r#"{"open":false,"botState":"Unknown","windowId":null,"title":null,"slots":[]}"#.to_string()),
    }
}

async fn toggle_ah(
    State(s): State<WebSharedState>,
    Json(payload): Json<TogglePayload>,
) -> impl IntoResponse {
    let enabled = payload.enabled;
    let loader = s.config_loader.clone();
    let persist = tokio::task::spawn_blocking(move || {
        loader.update_property(|c| c.enable_ah_flips = enabled)
    }).await;
    match persist {
        Ok(Ok(())) => {
            s.enable_ah_flips.store(enabled, Ordering::Relaxed);
            info!("[WebGUI] AH flips set to {} via web panel", enabled);
            let msg = format!("[TWM Web] AH flips {}", if enabled { "enabled" } else { "disabled" });
            print_mc_chat(&msg);
            let _ = s.chat_tx.send(msg);
            StatusCode::OK.into_response()
        }
        Ok(Err(e)) => {
            error!("[WebGUI] Failed to persist AH flips toggle to config: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, format!("Config save failed: {e}")).into_response()
        }
        Err(e) => {
            error!("[WebGUI] AH flips toggle task panicked: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal error".to_string()).into_response()
        }
    }
}

async fn toggle_bazaar(
    State(s): State<WebSharedState>,
    Json(payload): Json<TogglePayload>,
) -> impl IntoResponse {
    let enabled = payload.enabled;
    let loader = s.config_loader.clone();
    let persist = tokio::task::spawn_blocking(move || {
        loader.update_property(|c| c.enable_bazaar_flips = enabled)
    }).await;
    match persist {
        Ok(Ok(())) => {
            s.enable_bazaar_flips.store(enabled, Ordering::Relaxed);
            info!("[WebGUI] Bazaar flips set to {} via web panel", enabled);
            let msg = format!("[TWM Web] Bazaar flips {}", if enabled { "enabled" } else { "disabled" });
            print_mc_chat(&msg);
            let _ = s.chat_tx.send(msg);
            StatusCode::OK.into_response()
        }
        Ok(Err(e)) => {
            error!("[WebGUI] Failed to persist Bazaar flips toggle to config: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, format!("Config save failed: {e}")).into_response()
        }
        Err(e) => {
            error!("[WebGUI] Bazaar flips toggle task panicked: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal error".to_string()).into_response()
        }
    }
}

async fn toggle_anonymize(
    State(s): State<WebSharedState>,
    Json(payload): Json<TogglePayload>,
) -> impl IntoResponse {
    s.anonymize_webhook_name.store(payload.enabled, Ordering::Relaxed);
    info!("[WebGUI] Anonymize set to {} via web panel", payload.enabled);
    let msg = format!("[TWM Web] Anonymize {}", if payload.enabled { "enabled" } else { "disabled" });
    print_mc_chat(&msg);
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

    let echo = format!("> {}", input);
    print_mc_chat(&echo);
    let _ = state.chat_tx.send(echo);
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
        .send(format!("[TWM Web] Switching to account {}...", next_name));

    // Transfer the COFL license to the next account before restarting.
    let license_index = s.detected_cofl_license.load(std::sync::atomic::Ordering::Relaxed);
    let ws = s.ws_client.clone();
    let target_name = next_name.clone();

    // Restart the process with the new account index.
    tokio::spawn(async move {
        if license_index > 0 {
            if let Err(e) = ws.transfer_license(license_index, &target_name).await {
                warn!("[WebGUI] Failed to transfer license: {}", e);
            }
            // Give COFL time to process the license transfer before restarting.
            tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
        } else {
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        }
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

    let msg = format!(
        "[TWM Web] Cancelling auction: {}...",
        payload.item_name
    );
    print_mc_chat(&msg);
    let _ = s.chat_tx.send(msg);

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

/// Queue a purchase of a specific auction by UUID.
///
/// Accepts flexible input: raw UUID (with or without dashes), a full
/// `/viewauction <uuid>` paste, or the UUID surrounded by whitespace.  The
/// input is normalized to a dash-less lowercase 32-char hex string before the
/// `PurchaseAuction` command is enqueued, so the existing purchase flow (send
/// `/viewauction`, wait for slot 31, click buy, confirm) does the rest.
async fn buy_auction(
    State(s): State<WebSharedState>,
    Json(payload): Json<BuyAuctionPayload>,
) -> impl IntoResponse {
    let uuid = match normalize_auction_uuid(&payload.auction_id) {
        Some(u) => u,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                "Invalid auction ID — expected a 32-character hex UUID".to_string(),
            );
        }
    };

    info!("[WebGUI] Buy auction requested for UUID {}", uuid);

    let msg = format!("[TWM Web] Buying auction {}...", uuid);
    print_mc_chat(&msg);
    let _ = s.chat_tx.send(msg);

    // Synthetic flip carrying just the UUID.  The PurchaseAuction handler only
    // requires a non-empty UUID; price fields are logged but never validated,
    // so zero values are safe here.
    let flip = crate::types::Flip {
        item_name: format!("Web Buy {}", &uuid[..8]),
        starting_bid: 0,
        target: 0,
        finder: None,
        profit_perc: None,
        purchase_at_ms: None,
        uuid: Some(uuid),
    };

    s.command_queue.enqueue(
        CommandType::PurchaseAuction { flip },
        CommandPriority::Critical,
        false,
    );

    (StatusCode::OK, "Buy auction command queued".to_string())
}

/// Normalize an auction ID input from the web panel into a 32-char lowercase
/// hex string.  Accepts inputs such as:
/// - `c6b7e9e2c1f74eb7a59b0e9f5c1d2e3a`
/// - `C6B7E9E2-C1F7-4EB7-A59B-0E9F5C1D2E3A`
/// - `/viewauction c6b7e9e2-c1f7-4eb7-a59b-0e9f5c1d2e3a`
/// Returns `None` if the resulting string is not exactly 32 hex characters.
fn normalize_auction_uuid(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    // Strip a leading `/viewauction` (case-insensitive) so users can paste the
    // full command they would otherwise type in-game.
    let without_cmd = trimmed
        .strip_prefix("/viewauction ")
        .or_else(|| trimmed.strip_prefix("/VIEWAUCTION "))
        .or_else(|| trimmed.strip_prefix("/Viewauction "))
        .unwrap_or(trimmed)
        .trim();
    let cleaned: String = without_cmd
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect::<String>()
        .to_ascii_lowercase();
    if cleaned.len() == 32 && cleaned.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(cleaned)
    } else {
        None
    }
}

async fn claim_purchases(
    State(s): State<WebSharedState>,
) -> impl IntoResponse {
    info!("[WebGUI] Claim purchases requested");

    let msg = "[TWM Web] Checking unclaimed purchases...".to_string();
    print_mc_chat(&msg);
    let _ = s.chat_tx.send(msg);

    s.command_queue.enqueue(
        CommandType::ClaimPurchasedItem,
        CommandPriority::High,
        false,
    );

    (StatusCode::OK, "Claim purchases command queued")
}

async fn cancel_bz_order(
    State(s): State<WebSharedState>,
    Json(payload): Json<CancelBzOrderPayload>,
) -> impl IntoResponse {
    let order_type = if payload.is_buy_order { "BUY" } else { "SELL" };
    info!(
        "[WebGUI] Cancel bazaar order requested: '{}' ({})",
        payload.item_name, order_type
    );

    let msg = format!(
        "[TWM Web] Cancelling bazaar {} order: {}...",
        order_type, payload.item_name
    );
    print_mc_chat(&msg);
    let _ = s.chat_tx.send(msg);

    // Remove the order from the tracker immediately so the web GUI reflects
    // the intent.  The in-game cancellation happens asynchronously via
    // ManageOrders and will fire BazaarOrderCancelled on success.
    s.bazaar_tracker.remove_order(&payload.item_name, payload.is_buy_order);

    s.command_queue.enqueue(
        CommandType::ManageOrders { cancel_open: true },
        CommandPriority::High,
        false,
    );

    (StatusCode::OK, "Cancel bazaar order command queued")
}

async fn cancel_all_bz_orders(
    State(s): State<WebSharedState>,
) -> impl IntoResponse {
    info!("[WebGUI] Cancel ALL bazaar orders requested");

    let msg = "[TWM Web] Cancelling all bazaar orders...".to_string();
    print_mc_chat(&msg);
    let _ = s.chat_tx.send(msg);

    // Clear the tracker immediately so the web GUI reflects the intent.
    let removed = s.bazaar_tracker.clear_all_orders();
    info!("[WebGUI] Cleared {} order(s) from tracker", removed);

    // Queue a ManageOrders cycle with cancel_open=true to cancel in-game orders.
    s.command_queue.enqueue(
        CommandType::ManageOrders { cancel_open: true },
        CommandPriority::High,
        false,
    );

    (StatusCode::OK, "Cancel all bazaar orders command queued")
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

// ── Bazaar orders endpoint ──────────────────────────────────

async fn get_bazaar_orders(State(s): State<WebSharedState>) -> Json<Vec<crate::bazaar_tracker::TrackedBazaarOrder>> {
    Json(s.bazaar_tracker.get_orders())
}

// ── Queue status endpoint ───────────────────────────────────

async fn get_queue_status(State(s): State<WebSharedState>) -> Json<Vec<crate::state::QueueEntry>> {
    Json(s.command_queue.queue_snapshot())
}

// ── Config endpoint ─────────────────────────────────────────

async fn get_config(State(s): State<WebSharedState>) -> impl IntoResponse {
    let loader = s.config_loader.clone();
    match tokio::task::spawn_blocking(move || {
        loader.load()
    }).await {
        Ok(Ok(config)) => {
            match toml::to_string_pretty(&config) {
                Ok(toml_str) => (StatusCode::OK, toml_str).into_response(),
                Err(e) => {
                    error!("[WebGUI] Failed to serialize config: {}", e);
                    (StatusCode::INTERNAL_SERVER_ERROR, "Failed to serialize config").into_response()
                }
            }
        }
        Ok(Err(e)) => {
            error!("[WebGUI] Failed to load config: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Failed to load config").into_response()
        }
        Err(e) => {
            error!("[WebGUI] Config task panicked: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
        }
    }
}

#[derive(Deserialize)]
struct SaveConfigPayload {
    config_toml: String,
}

async fn save_config(
    State(s): State<WebSharedState>,
    Json(payload): Json<SaveConfigPayload>,
) -> impl IntoResponse {
    let loader = s.config_loader.clone();
    let enable_ah = s.enable_ah_flips.clone();
    let enable_bz = s.enable_bazaar_flips.clone();
    let skip_inv_full = s.skip_flips_when_inventory_full.clone();
    let toml_str = payload.config_toml;
    match tokio::task::spawn_blocking(move || -> Result<(), String> {
        save_config_inner(loader, enable_ah, enable_bz, skip_inv_full, &toml_str)
    }).await {
        Ok(Ok(())) => {
            info!("[WebGUI] Config saved via web panel");
            let msg = "[TWM Web] Config saved".to_string();
            print_mc_chat(&msg);
            let _ = s.chat_tx.send(msg);
            StatusCode::OK.into_response()
        }
        Ok(Err(msg)) => {
            warn!("[WebGUI] Config save failed: {}", msg);
            (StatusCode::BAD_REQUEST, msg).into_response()
        }
        Err(e) => {
            error!("[WebGUI] Config save task panicked: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal error".to_string()).into_response()
        }
    }
}

fn save_config_inner(
    loader: Arc<crate::config::ConfigLoader>,
    enable_ah: Arc<AtomicBool>,
    enable_bz: Arc<AtomicBool>,
    skip_inv_full: Arc<AtomicBool>,
    toml_str: &str,
) -> Result<(), String> {
    // Parse the TOML to validate it first
    let config: crate::config::Config = toml::from_str(toml_str)
        .map_err(|e| format!("Invalid config TOML: {}", e))?;
    // Save validated config first so runtime toggles only change if persistence succeeds.
    loader.save(&config).map_err(|e| format!("Failed to save config: {}", e))?;
    // Update in-memory toggle flags to match the saved config
    enable_ah.store(config.enable_ah_flips, Ordering::Relaxed);
    enable_bz.store(config.enable_bazaar_flips, Ordering::Relaxed);
    skip_inv_full.store(config.skip_flips_when_inventory_full, Ordering::Relaxed);
    Ok(())
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

async fn get_profit(State(s): State<WebSharedState>) -> Json<ProfitResponse> {
    let snapshot = s.profit_tracker.snapshot();
    Json(ProfitResponse {
        all_time_ah_points: snapshot.all_time_ah_points,
        all_time_bz_points: snapshot.all_time_bz_points,
        all_time_ah_total: snapshot.all_time_ah_total,
        all_time_bz_total: snapshot.all_time_bz_total,
        session_ah_points: snapshot.session_ah_points,
        session_bz_points: snapshot.session_bz_points,
        session_ah_total: snapshot.session_ah_total,
        session_bz_total: snapshot.session_bz_total,
        session_uptime_seconds: s.started_at.elapsed().as_secs(),
    })
}

/// Recent realized AH flips for the flip-history panel.
async fn get_flip_history(State(s): State<WebSharedState>) -> Json<Vec<FlipHistoryEntry>> {
    let history = s
        .flip_history
        .lock()
        .map(|h| h.clone().into_iter().collect())
        .unwrap_or_else(|_| Vec::new());
    Json(history)
}

/// Public profit endpoint — no authentication required.
/// Returns anonymized profit data (no IGN, no account info) for the
/// login page display and OpenGraph embeds.
async fn get_profit_public(State(s): State<WebSharedState>) -> Json<PublicProfitResponse> {
    let snapshot = s.profit_tracker.snapshot();
    let session_total = snapshot.session_ah_total + snapshot.session_bz_total;
    let all_time_total = snapshot.all_time_ah_total + snapshot.all_time_bz_total;
    let uptime = s.started_at.elapsed().as_secs();
    let hours = uptime as f64 / 3600.0;
    let per_hour = if hours > 0.0 { session_total as f64 / hours } else { 0.0 };
    Json(PublicProfitResponse {
        all_time_ah_total: snapshot.all_time_ah_total,
        all_time_bz_total: snapshot.all_time_bz_total,
        all_time_total,
        session_ah_total: snapshot.session_ah_total,
        session_bz_total: snapshot.session_bz_total,
        session_total,
        session_per_hour: per_hour,
        session_uptime_seconds: uptime,
        session_ah_points: snapshot.session_ah_points,
        session_bz_points: snapshot.session_bz_points,
    })
}

/// Public OG image endpoint — no authentication required.
/// Generates a 1200×630 PNG stats card for Discord / social media embeds.
async fn get_og_image(State(s): State<WebSharedState>) -> impl IntoResponse {
    let snapshot = s.profit_tracker.snapshot();
    let total = snapshot.session_ah_total + snapshot.session_bz_total;
    let uptime = s.started_at.elapsed().as_secs();
    let hours = uptime as f64 / 3600.0;
    let per_hour = if hours > 0.0 { total as f64 / hours } else { 0.0 };

    let ah_pts = snapshot.session_ah_points;
    let bz_pts = snapshot.session_bz_points;
    let png = super::og_image::generate_og_image(total, per_hour, uptime, &ah_pts, &bz_pts);

    (
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, "image/png"),
            (
                axum::http::header::CACHE_CONTROL,
                "public, max-age=30",
            ),
        ],
        png,
    )
}

// ── Public share page ────────────────────────────────────────
//
// `web_share_token` in config.toml gates a read-only stats dashboard at
// `GET /share/{token}`.  The dashboard fetches its data from
// `GET /api/share/{token}/stats`, both protected by constant-time token
// comparison so the page is impossible to brute-force or time-attack.
//
// Unset (or empty) token in config → both routes return 404 (route disabled,
// no leaked information about whether sharing exists).
//
// The payload is intentionally anonymized:
//   • no IGN, no Minecraft UUID, no Discord ID, no auction UUIDs
//   • no chat, no inventory, no command/queue access, no config
//   • only profit charts, profit/hour, uptime, recent realized flips
//     (item name + prices + duration), and counts of active auctions /
//     bazaar orders
//
// This means handing the URL to anyone reveals "how the bot is doing" without
// exposing any control surface or account-identifying data.

const SHARE_PAGE_HTML: &str = include_str!("share.html");

/// Constant-time string comparison so a bad token can't be detected via
/// response-time differences.  The expected value comes from the config
/// share token; the supplied value comes from the URL.
fn share_token_matches(expected: &str, supplied: &str) -> bool {
    if expected.is_empty() {
        return false;
    }
    let a = expected.as_bytes();
    let b = supplied.as_bytes();
    let len_eq = a.len() == b.len();
    let mut diff: u8 = if len_eq { 0 } else { 1 };
    let n = a.len().max(b.len());
    for i in 0..n {
        let ai = *a.get(i).unwrap_or(&0);
        let bi = *b.get(i).unwrap_or(&0);
        diff |= ai ^ bi;
    }
    len_eq && diff == 0
}

/// Returns `true` when the supplied token matches the configured share token
/// (and sharing is enabled at all).
fn share_token_authorized(state: &WebSharedState, supplied: &str) -> bool {
    match state.web_share_token.as_deref() {
        Some(expected) if !expected.is_empty() => share_token_matches(expected, supplied),
        _ => false,
    }
}

async fn get_share_page(
    State(s): State<WebSharedState>,
    Path(token): Path<String>,
) -> Response {
    if !share_token_authorized(&s, &token) {
        return StatusCode::NOT_FOUND.into_response();
    }
    Html(SHARE_PAGE_HTML).into_response()
}

/// Anonymized realized AH flip used by the public share page.
/// Drops `auction_uuid` so consumers can't link flips back to specific
/// listings on Hypixel.
#[derive(Serialize)]
pub struct PublicFlipEntry {
    pub sold_at_unix: u64,
    pub item_name: String,
    pub buy_price: i64,
    pub sell_price: i64,
    pub profit: i64,
    pub time_to_sell_secs: u64,
}

/// Anonymized active auction shown on the share page.
/// Drops UUIDs and end timestamps so the dashboard is purely informational.
#[derive(Serialize)]
pub struct PublicActiveAuction {
    pub item_name: String,
    pub starting_bid: i64,
    pub highest_bid: i64,
    pub bin: bool,
    pub time_remaining_seconds: i64,
}

/// Anonymized active bazaar order shown on the share page.
#[derive(Serialize)]
pub struct PublicBazaarOrder {
    pub item_name: String,
    pub amount: u64,
    pub price_per_unit: f64,
    pub is_buy_order: bool,
    pub status: String,
    pub placed_at: u64,
}

#[derive(Serialize)]
pub struct PublicShareStats {
    /// Cumulative AH profit points: `(unix_seconds, cumulative_coins)`.
    pub all_time_ah_points: Vec<(u64, i64)>,
    /// Cumulative BZ profit points: `(unix_seconds, cumulative_coins)`.
    pub all_time_bz_points: Vec<(u64, i64)>,
    pub all_time_ah_total: i64,
    pub all_time_bz_total: i64,
    pub all_time_total: i64,
    /// Session-only series (resets at process start) for the live "today" chart.
    pub session_ah_points: Vec<(u64, i64)>,
    pub session_bz_points: Vec<(u64, i64)>,
    pub session_ah_total: i64,
    pub session_bz_total: i64,
    pub session_total: i64,
    pub session_per_hour: f64,
    pub session_uptime_seconds: u64,
    pub session_started_at_unix: u64,
    /// Last ~200 realized AH flips (most recent first).
    pub recent_flips: Vec<PublicFlipEntry>,
    /// Counts only — full lists are intentionally excluded for the share view
    /// to keep the payload tiny and the strategy mostly opaque.
    pub active_auctions_count: usize,
    pub active_bazaar_orders_count: usize,
    /// Anonymized current AH listings (no UUIDs, no end timestamp).
    pub active_auctions: Vec<PublicActiveAuction>,
    /// Anonymized current Bazaar orders.
    pub active_bazaar_orders: Vec<PublicBazaarOrder>,
    /// Server clock at response time — clients use this instead of `Date.now()`
    /// so chart x-axes stay correct even when the viewer's clock is skewed.
    pub now_unix: u64,
}

/// Build the anonymized snapshot used by both `/api/share/{token}/stats` and
/// the outbound `share_push_url` pusher.  Pulled out into a helper so the two
/// callers can never drift apart in shape or anonymization rules.
pub fn build_public_share_stats(s: &WebSharedState) -> PublicShareStats {
    let snapshot = s.profit_tracker.snapshot();
    let session_total = snapshot.session_ah_total + snapshot.session_bz_total;
    let all_time_total = snapshot.all_time_ah_total + snapshot.all_time_bz_total;
    let uptime = s.started_at.elapsed().as_secs();
    let hours = uptime as f64 / 3600.0;
    let per_hour = if hours > 0.0 { session_total as f64 / hours } else { 0.0 };

    // Most-recent-first window of realized flips.  The on-disk ring already
    // bounds this; we cap again here to keep the share payload small even if
    // an operator bumps the ring size in a future change.
    const MAX_FLIPS: usize = 200;
    let recent_flips: Vec<PublicFlipEntry> = s
        .flip_history
        .lock()
        .map(|h| {
            h.iter()
                .rev()
                .take(MAX_FLIPS)
                .map(|f| PublicFlipEntry {
                    sold_at_unix: f.sold_at_unix,
                    item_name: f.item_name.clone(),
                    buy_price: f.buy_price,
                    sell_price: f.sell_price,
                    profit: f.profit,
                    time_to_sell_secs: f.time_to_sell_secs,
                })
                .collect()
        })
        .unwrap_or_default();

    // Active auctions: prefer the in-memory cache populated by the in-game
    // GUI parser (no external API hit) so the share page stays responsive
    // even when Hypixel/Coflnet is rate-limiting.
    let active_auctions: Vec<PublicActiveAuction> = s
        .bot_client
        .get_cached_my_auctions_json()
        .and_then(|j| serde_json::from_str::<Vec<serde_json::Value>>(&j).ok())
        .map(|arr| {
            arr.into_iter()
                .filter(|a| a.get("status").and_then(|v| v.as_str()).unwrap_or("") == "active")
                .map(|a| PublicActiveAuction {
                    item_name: a
                        .get("item_name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("Unknown")
                        .to_string(),
                    starting_bid: a.get("starting_bid").and_then(|v| v.as_i64()).unwrap_or(0),
                    highest_bid: a.get("highest_bid").and_then(|v| v.as_i64()).unwrap_or(0),
                    bin: a.get("bin").and_then(|v| v.as_bool()).unwrap_or(false),
                    time_remaining_seconds: a
                        .get("time_remaining_seconds")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0),
                })
                .collect()
        })
        .unwrap_or_default();

    let active_bazaar_orders: Vec<PublicBazaarOrder> = s
        .bazaar_tracker
        .get_orders()
        .into_iter()
        .map(|o| PublicBazaarOrder {
            item_name: o.item_name,
            amount: o.amount,
            price_per_unit: o.price_per_unit,
            is_buy_order: o.is_buy_order,
            status: o.status,
            placed_at: o.placed_at,
        })
        .collect();

    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    PublicShareStats {
        all_time_ah_points: snapshot.all_time_ah_points,
        all_time_bz_points: snapshot.all_time_bz_points,
        all_time_ah_total: snapshot.all_time_ah_total,
        all_time_bz_total: snapshot.all_time_bz_total,
        all_time_total,
        session_ah_points: snapshot.session_ah_points,
        session_bz_points: snapshot.session_bz_points,
        session_ah_total: snapshot.session_ah_total,
        session_bz_total: snapshot.session_bz_total,
        session_total,
        session_per_hour: per_hour,
        session_uptime_seconds: uptime,
        session_started_at_unix: snapshot.session_started_at_unix,
        active_auctions_count: active_auctions.len(),
        active_bazaar_orders_count: active_bazaar_orders.len(),
        recent_flips,
        active_auctions,
        active_bazaar_orders,
        now_unix,
    }
}

async fn get_share_stats(
    State(s): State<WebSharedState>,
    Path(token): Path<String>,
) -> Response {
    if !share_token_authorized(&s, &token) {
        return StatusCode::NOT_FOUND.into_response();
    }
    Json(build_public_share_stats(&s)).into_response()
}

/// Where the configured share link points: either a remote URL the operator
/// pre-set (e.g. their own domain) or the bot's own `/share/{token}` page.
#[derive(Serialize)]
struct ShareLinkResponse {
    url: String,
    /// `"remote"` when sourced from `share_public_url`, `"local"` when
    /// derived from `web_share_token` + the request's Host header.
    kind: &'static str,
}

/// GET /api/share/link — auth-gated helper for the panel's "Share Stats"
/// button.  Returns the pre-configured `share_public_url` if set; otherwise
/// falls back to a locally-derived `http(s)://<host>/share/<token>` URL when
/// `web_share_token` is configured.  Returns 404 when neither is set so the
/// frontend can show a clear "configure a token first" message without
/// leaking whether sharing exists at all.
///
/// Lives behind the same auth gate as the rest of `/api/*` so anonymous
/// callers can't enumerate whether sharing is configured.
async fn get_share_link(
    State(s): State<WebSharedState>,
    headers: axum::http::HeaderMap,
) -> Response {
    if let Some(remote) = s.share_public_url.as_deref().filter(|u| !u.is_empty()) {
        return Json(ShareLinkResponse {
            url: remote.to_string(),
            kind: "remote",
        })
        .into_response();
    }

    let Some(token) = s.web_share_token.as_deref().filter(|t| !t.is_empty()) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "no share link configured — set share_public_url or web_share_token"
            })),
        )
            .into_response();
    };

    // Prefer the proxy-supplied scheme when present (the bot is normally
    // accessed directly over HTTP, but operators sometimes front it with
    // Caddy/Nginx/Cloudflare for HTTPS).  Fall back to http otherwise so
    // the link is always copy-pasteable even without a proxy.
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "http".to_string());

    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get("host"))
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "localhost".to_string());

    Json(ShareLinkResponse {
        url: format!("{scheme}://{host}/share/{token}"),
        kind: "local",
    })
    .into_response()
}

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

// ── Seller tab handlers ──────────────────────────────────────────────────
//
// The Seller tab owns a recurring Discord sender that posts rendered item
// "screenshot" cards to one or more channels at configurable cooldowns.
// Panel flow:
//   • GET  /api/seller/config              → persisted settings (token masked)
//   • POST /api/seller/config              → save settings (accepts raw token)
//   • GET  /api/seller/status              → running state + logs + counters
//   • POST /api/seller/start               → validate + render + spawn tasks
//   • POST /api/seller/stop                → signal all tasks to stop
//   • POST /api/seller/validate_token      → { token } → { ok, username }
//   • POST /api/seller/validate_channel    → { token, channel_id } → { ok, name }
//   • POST /api/seller/login               → open browser, capture user token
//   • GET  /api/seller/available_items     → inventory + own-auctions snapshot
//   • POST /api/seller/preview             → render a single selection to PNG

use crate::seller::{
    mask_token, SelectedItem, SellerChannel, SellerConfig, StartRequest,
};

#[derive(Deserialize)]
struct SaveSellerConfigPayload {
    /// Token from the UI.  Empty string or the masked placeholder keeps the
    /// existing on-disk token so the panel never has to re-prompt for it.
    #[serde(default)]
    token: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    selected_items: Vec<SelectedItem>,
    #[serde(default)]
    channels: Vec<SellerChannel>,
}

async fn get_seller_config(State(s): State<WebSharedState>) -> impl IntoResponse {
    let cfg = SellerConfig::load_from(&s.seller_config_path).unwrap_or_default();
    Json(serde_json::json!({
        "token": mask_token(&cfg.token),
        "has_token": !cfg.token.is_empty(),
        "message": cfg.message,
        "selected_items": cfg.selected_items,
        "channels": cfg.channels,
    }))
}

async fn save_seller_config(
    State(s): State<WebSharedState>,
    Json(payload): Json<SaveSellerConfigPayload>,
) -> impl IntoResponse {
    let existing = SellerConfig::load_from(&s.seller_config_path).unwrap_or_default();

    // Preserve the on-disk token when the panel sends back the masked form
    // (or nothing at all) — the browser never receives the raw secret, so it
    // can't resubmit it without the user re-typing.
    let new_token = payload.token.trim();
    let token = if new_token.is_empty() || new_token == mask_token(&existing.token) {
        existing.token.clone()
    } else {
        new_token.to_string()
    };

    let cfg = SellerConfig {
        token,
        message: payload.message,
        selected_items: payload.selected_items,
        channels: payload.channels,
    }
    .sanitized();

    if let Err(e) = cfg.save_to(&s.seller_config_path) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e})),
        )
            .into_response();
    }
    Json(serde_json::json!({"ok": true})).into_response()
}

async fn get_seller_status(State(s): State<WebSharedState>) -> impl IntoResponse {
    Json(s.seller_runner.status())
}

async fn start_seller(
    State(s): State<WebSharedState>,
    Json(payload): Json<SaveSellerConfigPayload>,
) -> impl IntoResponse {
    // Reuse save_seller_config's token-preservation logic so starting never
    // requires the panel to re-send the secret.  We persist the submitted
    // config before starting so a crash mid-run doesn't lose the user's
    // latest selections.
    let existing = SellerConfig::load_from(&s.seller_config_path).unwrap_or_default();
    let new_token = payload.token.trim();
    let token = if new_token.is_empty() || new_token == mask_token(&existing.token) {
        existing.token.clone()
    } else {
        new_token.to_string()
    };
    let cfg = SellerConfig {
        token: token.clone(),
        message: payload.message.clone(),
        selected_items: payload.selected_items.clone(),
        channels: payload.channels.clone(),
    }
    .sanitized();
    let _ = cfg.save_to(&s.seller_config_path);

    let req = StartRequest {
        token,
        message: cfg.message,
        channels: cfg.channels,
        selected_items: cfg.selected_items,
    };
    match s.seller_runner.start(req).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
    }
}

async fn stop_seller(State(s): State<WebSharedState>) -> impl IntoResponse {
    s.seller_runner.stop().await;
    Json(serde_json::json!({"ok": true}))
}

#[derive(Deserialize)]
struct TokenPayload {
    token: String,
}

async fn seller_validate_token(
    State(s): State<WebSharedState>,
    Json(payload): Json<TokenPayload>,
) -> impl IntoResponse {
    // Accept either a raw token or the masked placeholder — in the latter
    // case, fall back to the token stored on disk so users can re-validate
    // without re-typing their secret.
    let mut token = payload.token.trim().to_string();
    let existing = SellerConfig::load_from(&s.seller_config_path).unwrap_or_default();
    if token.is_empty() || token == mask_token(&existing.token) {
        token = existing.token;
    }
    match crate::seller::discord::validate_token(&token).await {
        Ok(name) => Json(serde_json::json!({"ok": true, "username": name})).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"ok": false, "error": e})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct ChannelValidatePayload {
    #[serde(default)]
    token: String,
    channel_id: String,
}

async fn seller_validate_channel(
    State(s): State<WebSharedState>,
    Json(payload): Json<ChannelValidatePayload>,
) -> impl IntoResponse {
    let mut token = payload.token.trim().to_string();
    let existing = SellerConfig::load_from(&s.seller_config_path).unwrap_or_default();
    if token.is_empty() || token == mask_token(&existing.token) {
        token = existing.token;
    }
    match crate::seller::discord::validate_channel(&token, &payload.channel_id).await {
        Ok(name) => Json(serde_json::json!({"ok": true, "name": name})).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"ok": false, "error": e})),
        )
            .into_response(),
    }
}

/// Guard so only one browser login flow can be in flight at a time.  A
/// second concurrent click would otherwise spawn a second headless browser
/// and interfere with the first window's token capture.
static LOGIN_IN_PROGRESS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// POST /api/seller/login
///
/// Launches a visible browser window pointed at Discord's login page and
/// waits up to 5 minutes for the user to sign in.  On success the captured
/// token is written through to `twm_seller.json` (so subsequent page loads
/// already have it set) and returned to the caller.
///
/// This endpoint blocks for the entire login duration — the frontend must
/// show a clear "logging in…" state and not fire other requests against it
/// during that window.
async fn seller_login(State(s): State<WebSharedState>) -> impl IntoResponse {
    use std::sync::atomic::Ordering;

    if LOGIN_IN_PROGRESS
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "ok": false,
                "error": "another browser login is already in progress"
            })),
        )
            .into_response();
    }

    // Scope-guard so the flag always clears, even on panic/early-return.
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            LOGIN_IN_PROGRESS.store(false, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let _guard = Guard;

    match crate::seller::login::extract_token_via_login().await {
        Ok(result) => {
            // Persist the freshly captured token so reloading the panel or
            // restarting the bot keeps the login — matches the legacy tool's
            // behaviour of writing straight to config.json.
            let mut cfg = SellerConfig::load_from(&s.seller_config_path).unwrap_or_default();
            cfg.token = result.token.clone();
            if let Err(e) = cfg.save_to(&s.seller_config_path) {
                tracing::warn!(target: "seller", "failed to persist captured token: {e}");
            }
            Json(serde_json::json!({
                "ok": true,
                "username": result.display_name,
                "token_mask": mask_token(&result.token),
            }))
            .into_response()
        }
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"ok": false, "error": e})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct PreviewPayload {
    item: SelectedItem,
}

async fn seller_preview(
    State(s): State<WebSharedState>,
    Json(payload): Json<PreviewPayload>,
) -> impl IntoResponse {
    // We reuse the runner's render helper but it's not pub — instead, build
    // a minimal RenderableItem here so the preview avoids hitting Discord.
    let png = match render_single_preview(&s, &payload.item).await {
        Some(bytes) => bytes,
        None => return (StatusCode::NOT_FOUND, "Item could not be resolved").into_response(),
    };
    (
        [
            (axum::http::header::CONTENT_TYPE, "image/png"),
            (axum::http::header::CACHE_CONTROL, "no-store"),
        ],
        png,
    )
        .into_response()
}

async fn render_single_preview(
    s: &WebSharedState,
    sel: &SelectedItem,
) -> Option<Vec<u8>> {
    use crate::seller::render::{render_item_png, RenderableItem};

    let inv_json = s.bot_client.get_cached_inventory_json();
    let auctions_json = s.bot_client.get_cached_my_auctions_json();

    let item: RenderableItem = match sel {
        SelectedItem::Inventory { slot, .. } => {
            let v: serde_json::Value = serde_json::from_str(&inv_json?).ok()?;
            let slots = v.get("slots")?.as_array()?;
            let entry = slots.get(*slot as usize)?;
            if entry.is_null() {
                return None;
            }
            build_inventory_renderable(entry).await
        }
        SelectedItem::Auction { uuid, .. } => {
            let arr: Vec<serde_json::Value> =
                serde_json::from_str(&auctions_json?).ok()?;
            let found = if let Some(idx_str) = uuid.strip_prefix("idx:") {
                let idx: usize = idx_str.parse().ok()?;
                arr.get(idx)?.clone()
            } else {
                let normalized = uuid.replace('-', "").to_lowercase();
                arr.iter()
                    .find(|a| {
                        a.get("uuid")
                            .and_then(|v| v.as_str())
                            .map(|u| u.replace('-', "").to_lowercase() == normalized)
                            .unwrap_or(false)
                    })?
                    .clone()
            };
            build_auction_renderable(&found).await
        }
    };
    Some(render_item_png(&item))
}

async fn fetch_icon_bytes(tag: Option<&str>) -> Option<Vec<u8>> {
    let tag = tag?.trim();
    if tag.is_empty() {
        return None;
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .ok()?;
    let url = format!("https://sky.coflnet.com/static/icon/{tag}");
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.bytes().await.ok().map(|b| b.to_vec())
}

async fn build_inventory_renderable(
    entry: &serde_json::Value,
) -> crate::seller::render::RenderableItem {
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
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let count = entry.get("count").and_then(|v| v.as_u64()).unwrap_or(1) as u32;
    let tag = entry.get("tag").and_then(|v| v.as_str());
    let icon_png = fetch_icon_bytes(tag).await;
    crate::seller::render::RenderableItem {
        title,
        lore,
        count,
        icon_png,
        footer: None,
    }
}

async fn build_auction_renderable(
    entry: &serde_json::Value,
) -> crate::seller::render::RenderableItem {
    let title = entry
        .get("item_name_colored")
        .or_else(|| entry.get("item_name"))
        .and_then(|v| v.as_str())
        .unwrap_or("Auction")
        .to_string();
    let lore: Vec<String> = entry
        .get("lore")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    // Filter out the auction-house metadata block so the preview matches
    // what actually gets posted, and drop the BIN/Bid price footer — both
    // would give the recipient a hint that the item is listed on the AH.
    let lore = crate::seller::render::strip_auction_meta_lore(lore);
    let tag = entry.get("tag").and_then(|v| v.as_str());
    let icon_png = fetch_icon_bytes(tag).await;
    crate::seller::render::RenderableItem {
        title,
        lore,
        count: 1,
        icon_png,
        footer: None,
    }
}

/// Lightweight summary of everything the panel can select from — inventory
/// slots plus the player's own active auctions.  The panel renders these as
/// selectable tiles without needing extra API calls.
async fn seller_available_items(State(s): State<WebSharedState>) -> impl IntoResponse {
    let mut inventory: Vec<serde_json::Value> = Vec::new();
    if let Some(raw) = s.bot_client.get_cached_inventory_json() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
            if let Some(slots) = v.get("slots").and_then(|v| v.as_array()) {
                for entry in slots {
                    if entry.is_null() {
                        continue;
                    }
                    let slot = entry.get("slot").and_then(|v| v.as_u64()).unwrap_or(0);
                    // Forward every field the existing `getTooltipData()`
                    // helper on the panel knows how to consume so the tile
                    // can pop the same Minecraft hover card the inventory
                    // and auctions tabs already use.
                    inventory.push(serde_json::json!({
                        "slot": slot,
                        "display_name": entry.get("displayNameColored").or_else(|| entry.get("displayName")).cloned(),
                        "displayName": entry.get("displayName").cloned(),
                        "displayNameColored": entry.get("displayNameColored").cloned(),
                        "name": entry.get("name").cloned(),
                        "lore": entry.get("lore").cloned().unwrap_or(serde_json::json!([])),
                        "tag": entry.get("tag").cloned(),
                        "count": entry.get("count").cloned().unwrap_or(serde_json::json!(1)),
                    }));
                }
            }
        }
    }

    let mut auctions: Vec<serde_json::Value> = Vec::new();
    if let Some(raw) = s.bot_client.get_cached_my_auctions_json() {
        if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(&raw) {
            for (idx, a) in arr.iter().enumerate() {
                if a.get("status").and_then(|v| v.as_str()) != Some("active") {
                    continue;
                }
                let uuid = a
                    .get("uuid")
                    .and_then(|v| v.as_str())
                    .filter(|u| !u.is_empty())
                    .map(String::from)
                    .unwrap_or_else(|| format!("idx:{idx}"));
                let starting_bid = a
                    .get("starting_bid")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                auctions.push(serde_json::json!({
                    "id": uuid,
                    "display_name": a.get("item_name_colored").or_else(|| a.get("item_name")).cloned(),
                    "item_name": a.get("item_name").cloned(),
                    "displayNameColored": a.get("item_name_colored").cloned(),
                    "lore": a.get("lore").cloned().unwrap_or(serde_json::json!([])),
                    "tag": a.get("tag").cloned(),
                    "bin": a.get("bin").cloned().unwrap_or(serde_json::json!(false)),
                    "starting_bid": starting_bid,
                    // Auctions know what they cost — the listing price.
                    // Inventory items have to be priced separately via
                    // /api/seller/price_estimate (lazy Coflnet lookup).
                    "suggested_price": starting_bid,
                    "time_remaining_seconds": a.get("time_remaining_seconds").cloned().unwrap_or(serde_json::json!(0)),
                }));
            }
        }
    }

    Json(serde_json::json!({
        "inventory": inventory,
        "auctions": auctions,
    }))
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SellerPriceEstimateReq {
    Inventory { slot: u32 },
    Auction { uuid: String },
}

/// Resolve the asking price for a single selected item.  Auctions answer
/// from the cached "My Auctions" snapshot (the listing price the user
/// already chose).  Inventory items hit Coflnet for a price estimate.
async fn seller_price_estimate(
    State(s): State<WebSharedState>,
    Json(req): Json<SellerPriceEstimateReq>,
) -> impl IntoResponse {
    use crate::seller::pricing;

    match req {
        SellerPriceEstimateReq::Auction { uuid } => {
            let cached = s.bot_client.get_cached_my_auctions_json();
            let arr: Vec<serde_json::Value> = cached
                .as_deref()
                .and_then(|raw| serde_json::from_str(raw).ok())
                .unwrap_or_default();
            let found = if let Some(idx_str) = uuid.strip_prefix("idx:") {
                idx_str.parse::<usize>().ok().and_then(|i| arr.get(i).cloned())
            } else {
                let normalized = uuid.replace('-', "").to_lowercase();
                arr.into_iter().find(|a| {
                    a.get("uuid")
                        .and_then(|v| v.as_str())
                        .map(|u| u.replace('-', "").to_lowercase() == normalized)
                        .unwrap_or(false)
                })
            };
            let Some(entry) = found else {
                return Json(serde_json::json!({
                    "ok": false,
                    "error": "auction not found in current cache",
                }));
            };
            let listing_price = entry
                .get("starting_bid")
                .and_then(|v| v.as_i64())
                .filter(|p| *p > 0)
                .map(|p| p as u64);
            let bin = entry.get("bin").and_then(|v| v.as_bool()).unwrap_or(false);
            let tag = entry
                .get("tag")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            // Skin items take the Coflnet median override here too — see
            // pricing::resolve_auction_price for rationale.
            match pricing::resolve_auction_price(tag.as_deref(), listing_price, bin).await {
                Ok(Some((price, src))) => Json(serde_json::json!({
                    "ok": true,
                    "price": price,
                    "source": src.as_str(),
                    "is_skin": tag.as_deref().map(pricing::is_skin_tag).unwrap_or(false),
                })),
                Ok(None) => Json(serde_json::json!({
                    "ok": false,
                    "error": "no listing price and Coflnet has no data for this tag",
                })),
                Err(e) => Json(serde_json::json!({
                    "ok": false,
                    "error": format!("Coflnet lookup failed: {e}"),
                })),
            }
        }
        SellerPriceEstimateReq::Inventory { slot } => {
            let cached = s.bot_client.get_cached_inventory_json();
            let tag = cached
                .as_deref()
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
                .and_then(|v| v.get("slots").cloned())
                .and_then(|s| s.as_array().cloned())
                .and_then(|slots| slots.get(slot as usize).cloned())
                .filter(|entry| !entry.is_null())
                .and_then(|entry| {
                    entry
                        .get("tag")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                });
            let Some(tag) = tag else {
                return Json(serde_json::json!({
                    "ok": false,
                    "error": "inventory slot is empty or has no SkyBlock tag",
                }));
            };
            match pricing::resolve_inventory_price(&tag).await {
                Ok(Some((price, src))) => Json(serde_json::json!({
                    "ok": true,
                    "price": price,
                    "source": src.as_str(),
                    "tag": tag,
                    "is_skin": pricing::is_skin_tag(&tag),
                })),
                Ok(None) => Json(serde_json::json!({
                    "ok": false,
                    "error": format!("Coflnet has no price data for {tag}"),
                })),
                Err(e) => Json(serde_json::json!({
                    "ok": false,
                    "error": format!("Coflnet lookup failed: {e}"),
                })),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::Ordering;

    #[test]
    fn normalize_auction_uuid_accepts_common_formats() {
        let dashless = "c6b7e9e2c1f74eb7a59b0e9f5c1d2e3a";
        assert_eq!(
            normalize_auction_uuid(dashless).as_deref(),
            Some(dashless)
        );
        assert_eq!(
            normalize_auction_uuid("C6B7E9E2-C1F7-4EB7-A59B-0E9F5C1D2E3A").as_deref(),
            Some(dashless)
        );
        assert_eq!(
            normalize_auction_uuid("  /viewauction c6b7e9e2-c1f7-4eb7-a59b-0e9f5c1d2e3a  ")
                .as_deref(),
            Some(dashless)
        );
    }

    #[test]
    fn normalize_auction_uuid_rejects_bad_input() {
        assert!(normalize_auction_uuid("").is_none());
        assert!(normalize_auction_uuid("not-a-uuid").is_none());
        assert!(normalize_auction_uuid("c6b7e9e2c1f74eb7a59b0e9f5c1d2e3").is_none()); // 31 chars
        assert!(normalize_auction_uuid("g6b7e9e2c1f74eb7a59b0e9f5c1d2e3a").is_none()); // non-hex
    }

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

    #[test]
    fn save_config_does_not_mutate_runtime_toggles_when_save_fails() {
        let unique = format!(
            "twm-config-save-failure-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time should be after epoch")
                .as_nanos()
        );
        let dir_path = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&dir_path).expect("temp test directory should be created");

        // Use a directory path as the "file" path so fs::write fails.
        let loader = Arc::new(crate::config::ConfigLoader::with_path(PathBuf::from(&dir_path)));
        let enable_ah = Arc::new(AtomicBool::new(false));
        let enable_bz = Arc::new(AtomicBool::new(true));
        let skip_inv_full = Arc::new(AtomicBool::new(false));
        let config_toml = "enable_ah_flips = true\nenable_bazaar_flips = false\n";

        let result = save_config_inner(
            loader,
            enable_ah.clone(),
            enable_bz.clone(),
            skip_inv_full.clone(),
            config_toml,
        );
        assert!(result.is_err(), "saving to a directory path should fail");
        assert!(
            result
                .expect_err("save should fail")
                .starts_with("Failed to save config:"),
            "error should use existing save-failure BAD_REQUEST message path"
        );
        assert!(
            !enable_ah.load(Ordering::Relaxed),
            "AH toggle must remain unchanged on save failure"
        );
        assert!(
            enable_bz.load(Ordering::Relaxed),
            "Bazaar toggle must remain unchanged on save failure"
        );

        std::fs::remove_dir_all(&dir_path).expect("temp test directory should be removed");
    }
}
