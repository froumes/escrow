//! Persistent configuration for the Discord auto-sender ("Seller" panel tab).
//!
//! The shape intentionally mirrors the original Python `config.json` used by
//! the standalone `minecraftSelling` tool so users can migrate by copying the
//! file in.  New fields (`selected_items`) default to empty and are ignored by
//! the legacy tool.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Default per-channel cooldown when the user adds a new channel without
/// specifying one explicitly.  Matches the Python default.
pub const DEFAULT_COOLDOWN_SECONDS: u64 = 60;

/// Minimum cooldown the backend will accept.  Shorter values risk hitting
/// Discord's rate-limiter and triggering a spam flag against the account.
pub const MIN_COOLDOWN_SECONDS: u64 = 5;

/// Hard cap on the number of items a user can select per run.  Discord allows
/// at most 10 attachments per message, so this is also the attachment limit
/// we enforce at send time.
pub const MAX_SELECTED_ITEMS: usize = 10;

/// Reference to an item the user has picked in the panel.
///
/// We keep these opaque (inventory slot or auction UUID) instead of snapshots
/// so that the rendered image always reflects the live state of the item when
/// the Discord message is sent.  The `include_price` flag — defaulted to false
/// for backward compatibility with existing config files — controls whether
/// the runner appends an "asking price" line for that item in the outgoing
/// Discord message.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SelectedItem {
    /// Item in the player's inventory, addressed by the mineflayer slot id
    /// (9..=44 — hotbar + main inventory).
    Inventory {
        slot: u32,
        #[serde(default)]
        include_price: bool,
    },
    /// Active auction-house listing, addressed by auction UUID.
    Auction {
        uuid: String,
        #[serde(default)]
        include_price: bool,
    },
}

impl SelectedItem {
    /// Whether the user has opted into broadcasting this item's asking price.
    pub fn include_price(&self) -> bool {
        match self {
            SelectedItem::Inventory { include_price, .. }
            | SelectedItem::Auction { include_price, .. } => *include_price,
        }
    }
}

/// A Discord channel the sender posts into, with its own cooldown.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SellerChannel {
    pub channel_id: String,
    #[serde(default = "default_cooldown_seconds", alias = "cooldown")]
    pub cooldown_seconds: u64,
}

fn default_cooldown_seconds() -> u64 {
    DEFAULT_COOLDOWN_SECONDS
}

/// Everything the Seller panel persists across process restarts.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SellerConfig {
    /// Raw Discord user token.  Stored plaintext on disk (matches the Python
    /// tool) but masked whenever the backend returns it to the browser.
    #[serde(default)]
    pub token: String,
    /// Message body sent alongside the rendered item attachments.
    #[serde(default)]
    pub message: String,
    /// Items the panel currently has selected.  Each run re-resolves these
    /// through the live inventory / auction caches, so stale references
    /// simply drop out at render time.
    #[serde(default)]
    pub selected_items: Vec<SelectedItem>,
    /// Target channels with per-channel cooldowns.
    #[serde(default)]
    pub channels: Vec<SellerChannel>,
}

impl SellerConfig {
    /// Default file path — a sibling of the main TWM config, i.e. next to the
    /// executable so parallel instances with different working directories
    /// share the same storage.
    pub fn default_path() -> PathBuf {
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|p| p.join("twm_seller.json")))
            .unwrap_or_else(|| PathBuf::from("twm_seller.json"))
    }

    /// Load from `path`, returning `SellerConfig::default()` when the file
    /// does not exist.  Any parse error is reported to the caller so the
    /// panel can surface it instead of silently wiping user settings.
    pub fn load_from(path: &Path) -> Result<Self, String> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("read {}: {}", path.display(), e))?;
        let cfg: SellerConfig = serde_json::from_str(&raw)
            .map_err(|e| format!("parse {}: {}", path.display(), e))?;
        Ok(cfg.sanitized())
    }

    /// Persist to disk, overwriting any existing file.
    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("create dir {}: {}", parent.display(), e))?;
            }
        }
        let cleaned = self.clone().sanitized();
        let raw = serde_json::to_string_pretty(&cleaned)
            .map_err(|e| format!("serialise: {}", e))?;
        std::fs::write(path, raw)
            .map_err(|e| format!("write {}: {}", path.display(), e))?;
        Ok(())
    }

    /// Enforce invariants: clamp cooldowns, drop blank channels, cap the
    /// selected-item list at the Discord attachment limit.
    pub fn sanitized(mut self) -> Self {
        self.token = self.token.trim().to_string();
        self.channels.retain(|c| !c.channel_id.trim().is_empty());
        for ch in &mut self.channels {
            ch.channel_id = ch.channel_id.trim().to_string();
            if ch.cooldown_seconds < MIN_COOLDOWN_SECONDS {
                ch.cooldown_seconds = MIN_COOLDOWN_SECONDS;
            }
        }
        if self.selected_items.len() > MAX_SELECTED_ITEMS {
            self.selected_items.truncate(MAX_SELECTED_ITEMS);
        }
        self
    }

    /// Return a copy with the token replaced by a short preview so the panel
    /// never re-displays the raw secret to the browser.
    pub fn masked(&self) -> Self {
        let mut out = self.clone();
        out.token = mask_token(&self.token);
        out
    }
}

/// Produce a non-secret display string for a token: empty if unset, otherwise
/// just the first four and last four characters.
pub fn mask_token(token: &str) -> String {
    let token = token.trim();
    if token.is_empty() {
        return String::new();
    }
    if token.len() <= 10 {
        return "•".repeat(token.chars().count().min(8));
    }
    let prefix: String = token.chars().take(4).collect();
    let suffix: String = token.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    format!("{prefix}…{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitized_clamps_cooldown_and_drops_blank_channels() {
        let cfg = SellerConfig {
            token: "  abc  ".to_string(),
            message: "hello".to_string(),
            selected_items: vec![],
            channels: vec![
                SellerChannel {
                    channel_id: "   ".to_string(),
                    cooldown_seconds: 60,
                },
                SellerChannel {
                    channel_id: "1234".to_string(),
                    cooldown_seconds: 1,
                },
            ],
        }
        .sanitized();
        assert_eq!(cfg.token, "abc");
        assert_eq!(cfg.channels.len(), 1);
        assert_eq!(cfg.channels[0].channel_id, "1234");
        assert_eq!(cfg.channels[0].cooldown_seconds, MIN_COOLDOWN_SECONDS);
    }

    #[test]
    fn sanitized_truncates_selected_items_to_discord_limit() {
        let cfg = SellerConfig {
            selected_items: (0..20)
                .map(|i| SelectedItem::Inventory {
                    slot: 9 + i,
                    include_price: false,
                })
                .collect(),
            ..SellerConfig::default()
        }
        .sanitized();
        assert_eq!(cfg.selected_items.len(), MAX_SELECTED_ITEMS);
    }

    #[test]
    fn legacy_selected_item_without_include_price_deserialises() {
        let raw = r#"[{"kind":"inventory","slot":12},{"kind":"auction","uuid":"abc"}]"#;
        let parsed: Vec<SelectedItem> = serde_json::from_str(raw).expect("decode");
        assert_eq!(parsed.len(), 2);
        assert!(!parsed[0].include_price());
        assert!(!parsed[1].include_price());
    }

    #[test]
    fn mask_token_hides_body() {
        assert_eq!(mask_token(""), "");
        assert_eq!(mask_token("short"), "•••••");
        assert_eq!(mask_token("ABCD12345678WXYZ"), "ABCD…WXYZ");
    }
}
