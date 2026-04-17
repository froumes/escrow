//! Discord user-API helpers for the Seller panel.
//!
//! Uses `Authorization: <token>` (no `Bearer` prefix) to match the way the
//! Discord client itself authenticates — the same approach the original
//! Python tool used.  All calls go through a shared `reqwest::Client` with a
//! sane User-Agent string so requests look similar to a normal client.

use reqwest::multipart;
use serde::Deserialize;
use std::time::Duration;

/// Base URL.  We pin to `v9` for parity with the legacy Python tool; this is
/// the version the web client itself uses and it has been stable for years.
pub const DISCORD_API: &str = "https://discord.com/api/v9";

/// Shared UA string — the official web client's UA has too many moving parts
/// to replicate exactly, so we use a generic Chrome-like value that has
/// worked reliably in practice.
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

fn build_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("build http client: {e}"))
}

/// Minimal user shape — we only care about the display name for the panel.
#[derive(Debug, Deserialize)]
struct DiscordUser {
    username: String,
    #[serde(default)]
    global_name: Option<String>,
}

/// Validate a user token against `/users/@me`.  On success returns the best
/// display name (global name if set, otherwise username).
pub async fn validate_token(token: &str) -> Result<String, String> {
    let token = token.trim();
    if token.is_empty() {
        return Err("token is empty".to_string());
    }
    let client = build_client()?;
    let resp = client
        .get(format!("{DISCORD_API}/users/@me"))
        .header("Authorization", token)
        .send()
        .await
        .map_err(|e| format!("discord request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!(
            "discord returned {} — token rejected",
            resp.status().as_u16()
        ));
    }
    let user: DiscordUser = resp
        .json()
        .await
        .map_err(|e| format!("parse user response: {e}"))?;
    Ok(user.global_name.unwrap_or(user.username))
}

#[derive(Debug, Deserialize)]
struct DiscordChannel {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    recipients: Option<Vec<DiscordUser>>,
}

/// Validate a channel ID is reachable with the given token.  Returns a label
/// suitable for the panel UI (channel name for guild channels, the recipient
/// list for DMs, or the raw ID as a last resort).
pub async fn validate_channel(token: &str, channel_id: &str) -> Result<String, String> {
    let token = token.trim();
    let channel_id = channel_id.trim();
    if token.is_empty() || channel_id.is_empty() {
        return Err("token and channel id are required".to_string());
    }
    let client = build_client()?;
    let resp = client
        .get(format!("{DISCORD_API}/channels/{channel_id}"))
        .header("Authorization", token)
        .send()
        .await
        .map_err(|e| format!("discord request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!(
            "discord returned {} for channel {channel_id}",
            resp.status().as_u16()
        ));
    }
    let ch: DiscordChannel = resp
        .json()
        .await
        .map_err(|e| format!("parse channel response: {e}"))?;
    if let Some(name) = ch.name.filter(|n| !n.is_empty()) {
        return Ok(name);
    }
    if let Some(recipients) = ch.recipients {
        let joined = recipients
            .iter()
            .map(|u| u.global_name.clone().unwrap_or_else(|| u.username.clone()))
            .collect::<Vec<_>>()
            .join(", ");
        if !joined.is_empty() {
            return Ok(format!("DM: {joined}"));
        }
    }
    Ok(channel_id.to_string())
}

/// A single PNG attachment.  We own the bytes so the multipart form can take
/// ownership without re-cloning on every retry.
#[derive(Clone)]
pub struct AttachmentPng {
    pub filename: String,
    pub bytes: Vec<u8>,
}

/// POST a message to a channel with up to 10 PNG attachments.  Discord allows
/// either a `content` field, attachments, or both — we always attach the
/// `payload_json` part so the `content` survives even when the attachment
/// list is empty.
pub async fn send_message(
    token: &str,
    channel_id: &str,
    content: &str,
    attachments: &[AttachmentPng],
) -> Result<(), String> {
    let token = token.trim();
    let channel_id = channel_id.trim();
    if token.is_empty() || channel_id.is_empty() {
        return Err("token and channel id are required".to_string());
    }
    let client = build_client()?;
    let url = format!("{DISCORD_API}/channels/{channel_id}/messages");

    // `payload_json` is the documented way to send JSON alongside multipart
    // attachments.  `content` is fine as an empty string — Discord only
    // rejects the message when there's neither content nor attachments.
    let payload = serde_json::json!({ "content": content });
    let payload_json =
        serde_json::to_string(&payload).map_err(|e| format!("serialise payload: {e}"))?;

    let mut form = multipart::Form::new().text("payload_json", payload_json);

    for (idx, att) in attachments.iter().take(10).enumerate() {
        let part = multipart::Part::bytes(att.bytes.clone())
            .file_name(att.filename.clone())
            .mime_str("image/png")
            .map_err(|e| format!("mime: {e}"))?;
        form = form.part(format!("files[{idx}]"), part);
    }

    let resp = client
        .post(url)
        .header("Authorization", token)
        .multipart(form)
        .send()
        .await
        .map_err(|e| format!("discord request failed: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("discord {} — {body}", status.as_u16()));
    }
    Ok(())
}
