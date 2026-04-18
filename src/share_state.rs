//! Persistent share-link state.
//!
//! When the operator clicks "Share Stats" in the web panel for the first
//! time the bot generates a fresh `slot_id` + `push_secret` pair and saves
//! them next to `config.toml` as `share_state.json`.  The same file is then
//! read on every subsequent startup so the public URL stays stable across
//! restarts and the bot can keep authenticating its outbound pushes.
//!
//! Why a separate file (instead of stuffing this into `config.toml`):
//!   * It's machine-generated — operators should never touch it.
//!   * The push secret is sensitive and shouldn't be diffed into git when
//!     someone shares their config for support.
//!   * Decoupling from the user-edited config means we can rewrite the file
//!     atomically (write-then-rename) without racing against the operator
//!     reloading the config from the panel.
//!
//! Trust model:
//!   * `slot_id` doubles as the routing key in the URL viewers open
//!     (`https://<remote>/twm?s=<slot_id>`).  It's intentionally unguessable
//!     so anyone who has the URL can read, and nobody else can find it.
//!   * `push_secret` never leaves this machine except as an HTTP body sent
//!     to the configured remote over HTTPS.  The remote stores it on the
//!     first push (TOFU) and rejects subsequent pushes whose signature
//!     doesn't match.

use std::path::{Path, PathBuf};

use rand::RngCore;
use serde::{Deserialize, Serialize};

/// Default file name placed alongside `config.toml`.
const FILE_NAME: &str = "share_state.json";

/// Number of random bytes used for both `slot_id` and `push_secret`.  32
/// bytes → 64 hex characters → 256 bits of entropy.  Far past the point
/// where guessing or birthday-collision becomes feasible against any
/// realistic adversary.
const SECRET_BYTES: usize = 32;

/// On-disk shape of `share_state.json`.  Every field is `String` so that
/// rotating the format later (different hash, different alphabet) doesn't
/// require migrating older files in place — we just generate a fresh state
/// and the operator clicks "Share Stats" once.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ShareState {
    /// Unguessable identifier baked into the public URL.  Hex-encoded.
    pub slot_id: String,
    /// Bearer secret the bot sends with every push.  Hex-encoded.  Stored
    /// by the remote on first push (TOFU); subsequent pushes must match
    /// byte-for-byte or the remote returns 401.
    pub push_secret: String,
    /// Base URL of the remote site that hosts the viewer page and the push
    /// endpoint, e.g. `https://austinxyz.lol`.  Frozen at first generation
    /// so the saved URL keeps pointing at the same origin even if the
    /// default in code changes later.
    pub remote_base_url: String,
    /// Wall-clock seconds since Unix epoch when the state was generated.
    /// Purely informational; useful when debugging stale slots.
    pub created_at_unix: u64,
}

impl ShareState {
    /// Default location of the state file: a sibling of `config.toml` in
    /// the working directory, matching the convention already used by
    /// `seller_config.json` and `flip_history.json`.
    pub fn default_path() -> PathBuf {
        PathBuf::from(FILE_NAME)
    }

    /// Read the state file if it exists.  Returns `Ok(None)` when the file
    /// is absent — that's the normal "operator hasn't clicked Share Stats
    /// yet" case, not an error.
    pub fn load_from(path: &Path) -> std::io::Result<Option<Self>> {
        match std::fs::read_to_string(path) {
            Ok(contents) => {
                let parsed: ShareState = serde_json::from_str(&contents).map_err(|e| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("share_state.json is malformed: {e}"),
                    )
                })?;
                if parsed.slot_id.is_empty() || parsed.push_secret.is_empty() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "share_state.json missing slot_id or push_secret",
                    ));
                }
                Ok(Some(parsed))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Generate a fresh state with cryptographically-strong random values.
    /// `remote_base_url` is normalised to drop a trailing slash so URL
    /// concatenation later doesn't produce `https://x.com//api/...`.
    pub fn generate(remote_base_url: &str) -> Self {
        let slot_id = random_hex(SECRET_BYTES);
        let push_secret = random_hex(SECRET_BYTES);
        let trimmed = remote_base_url.trim().trim_end_matches('/').to_string();
        let created_at_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self {
            slot_id,
            push_secret,
            remote_base_url: trimmed,
            created_at_unix,
        }
    }

    /// Atomically write the state file.  Done as write-temp + rename so a
    /// crash mid-write can never leave a half-written file that would
    /// break the next startup.
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
        })?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// URL viewers open in their browser.  Built from the saved
    /// `remote_base_url` and `slot_id`; never includes the push secret.
    pub fn viewer_url(&self) -> String {
        format!("{}/twm?s={}", self.remote_base_url, self.slot_id)
    }

    /// URL the bot POSTs snapshots to.  Includes the slot id as a path
    /// segment so the remote can route to the right KV record.
    pub fn push_url(&self) -> String {
        format!("{}/api/twm/push/{}", self.remote_base_url, self.slot_id)
    }
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_path(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "twm-share-state-{}-{}-{}",
            label,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir.join("share_state.json")
    }

    #[test]
    fn generate_produces_distinct_long_hex_values() {
        let a = ShareState::generate("https://example.com");
        let b = ShareState::generate("https://example.com");
        assert_eq!(a.slot_id.len(), SECRET_BYTES * 2);
        assert_eq!(a.push_secret.len(), SECRET_BYTES * 2);
        assert!(a.slot_id.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(a.push_secret.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a.slot_id, b.slot_id);
        assert_ne!(a.push_secret, b.push_secret);
    }

    #[test]
    fn generate_strips_trailing_slash_from_base() {
        let s = ShareState::generate("https://example.com/");
        assert_eq!(s.remote_base_url, "https://example.com");
        let s2 = ShareState::generate("  https://example.com/// ");
        assert_eq!(s2.remote_base_url, "https://example.com");
    }

    #[test]
    fn viewer_and_push_urls_use_slot_id() {
        let s = ShareState {
            slot_id: "abc123".into(),
            push_secret: "secret".into(),
            remote_base_url: "https://example.com".into(),
            created_at_unix: 0,
        };
        assert_eq!(s.viewer_url(), "https://example.com/twm?s=abc123");
        assert_eq!(s.push_url(), "https://example.com/api/twm/push/abc123");
    }

    #[test]
    fn save_then_load_round_trips() {
        let path = temp_path("roundtrip");
        let original = ShareState::generate("https://example.com");
        original.save_to(&path).expect("save");
        let loaded = ShareState::load_from(&path).expect("load").expect("some");
        assert_eq!(loaded.slot_id, original.slot_id);
        assert_eq!(loaded.push_secret, original.push_secret);
        assert_eq!(loaded.remote_base_url, original.remote_base_url);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_missing_file_returns_none() {
        let path = temp_path("missing");
        let _ = std::fs::remove_file(&path);
        let loaded = ShareState::load_from(&path).expect("ok");
        assert!(loaded.is_none());
    }

    #[test]
    fn load_rejects_empty_fields() {
        let path = temp_path("invalid");
        std::fs::write(
            &path,
            r#"{"slot_id":"","push_secret":"x","remote_base_url":"https://x","created_at_unix":0}"#,
        )
        .unwrap();
        assert!(ShareState::load_from(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }
}
