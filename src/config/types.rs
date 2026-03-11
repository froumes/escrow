use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Ingame Minecraft username(s). Supports multiple comma-separated accounts:
    /// `ingame_name = "Account1"` for a single account, or
    /// `ingame_name = "Account1,Account2"` for automatic switching.
    #[serde(default)]
    pub ingame_name: Option<String>,

    /// Time in hours after which the bot switches to the next account in `ingame_name`.
    /// Only used when multiple accounts are specified. E.g. `multi_switch_time = 12.0`
    /// means switch accounts every 12 hours.
    #[serde(default)]
    pub multi_switch_time: Option<f64>,
    
    #[serde(default = "default_websocket_url")]
    pub websocket_url: String,
    
    #[serde(default = "default_web_gui_port")]
    pub web_gui_port: u16,

    /// Minimum delay between consecutive queued commands in milliseconds.
    /// Prevents back-to-back Hypixel interactions from overlapping.
    /// Default: 500ms.
    #[serde(default = "default_command_delay_ms")]
    pub command_delay_ms: u64,
    
    #[serde(default = "default_bed_spam_click_delay")]
    pub bed_spam_click_delay: u64,
    
    #[serde(default)]
    pub bed_multiple_clicks_delay: u64,
    
    /// How many ms before the COFL `purchaseAt` deadline to start clicking (default: 30).
    /// Only used when `freemoney = true`. Without freemoney, bed spam starts immediately
    /// using `bed_spam_click_delay` and this value is ignored.
    #[serde(default = "default_bed_pre_click_ms")]
    pub bed_pre_click_ms: u64,
    
    #[serde(default = "default_bazaar_order_check_interval_seconds")]
    pub bazaar_order_check_interval_seconds: u64,
    
    #[serde(default = "default_bazaar_order_cancel_minutes")]
    pub bazaar_order_cancel_minutes: u64,
    
    #[serde(default = "default_true")]
    pub enable_bazaar_flips: bool,
    
    #[serde(default = "default_true")]
    pub enable_ah_flips: bool,
    
    #[serde(default)]
    pub bed_spam: bool,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freemoney: Option<bool>,
    
    #[serde(default = "default_true")]
    pub use_cofl_chat: bool,
    
    #[serde(default)]
    pub auto_cookie: u64,

    /// Enable fastbuy (window-skip): click BIN buy (slot 31) and pre-click confirm (slot 11).
    /// Disabled by default and omitted from generated config unless manually added.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fastbuy: Option<bool>,
    
    #[serde(default = "default_true")]
    pub enable_console_input: bool,
    
    #[serde(default = "default_auction_duration_hours")]
    pub auction_duration_hours: u64,
    
    /// Enable proxy for both the Minecraft and WebSocket connections.
    #[serde(default)]
    pub proxy_enabled: bool,
    
    /// Proxy server address in `host:port` format, e.g. `"121.124.241.211:3313"`.
    /// Only used when `proxy_enabled = true`.
    #[serde(default)]
    pub proxy_address: Option<String>,
    
    /// Proxy credentials in `username:password` format, e.g. `"myuser:mypassword"`.
    /// Leave unset if the proxy requires no authentication.
    #[serde(default)]
    pub proxy_credentials: Option<String>,
    
    #[serde(default)]
    /// Discord webhook URL for notifications.
    /// `None` = not yet configured (prompts on next startup).
    /// `Some("")` = explicitly disabled (no further prompts).
    /// `Some(url)` = active webhook.
    pub webhook_url: Option<String>,
    
    #[serde(default)]
    pub web_gui_password: Option<String>,
    
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub sessions: HashMap<String, CoflSession>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoflSession {
    pub id: String,
    pub expires: DateTime<Utc>,
}

// Default values
fn default_websocket_url() -> String {
    "wss://sky.coflnet.com/modsocket".to_string()
}

fn default_web_gui_port() -> u16 {
    8080
}

fn default_command_delay_ms() -> u64 {
    500
}

fn default_bed_spam_click_delay() -> u64 {
    100
}

fn default_bed_pre_click_ms() -> u64 {
    30
}

fn default_bazaar_order_check_interval_seconds() -> u64 {
    30
}

fn default_bazaar_order_cancel_minutes() -> u64 {
    5
}

fn default_auction_duration_hours() -> u64 {
    24
}

fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ingame_name: None,
            multi_switch_time: None,
            websocket_url: default_websocket_url(),
            web_gui_port: default_web_gui_port(),
            command_delay_ms: default_command_delay_ms(),
            bed_spam_click_delay: default_bed_spam_click_delay(),
            bed_multiple_clicks_delay: 0,
            bed_pre_click_ms: default_bed_pre_click_ms(),
            bazaar_order_check_interval_seconds: default_bazaar_order_check_interval_seconds(),
            bazaar_order_cancel_minutes: default_bazaar_order_cancel_minutes(),
            enable_bazaar_flips: true,
            enable_ah_flips: true,
            bed_spam: false,
            freemoney: None,
            use_cofl_chat: true,
            auto_cookie: 0,
            fastbuy: None,
            enable_console_input: true,
            auction_duration_hours: default_auction_duration_hours(),
            proxy_enabled: false,
            proxy_address: None,
            proxy_credentials: None,
            webhook_url: None,
            web_gui_password: None,
            sessions: HashMap::new(),
        }
    }
}

impl Config {
    pub fn freemoney_enabled(&self) -> bool {
        self.freemoney.unwrap_or(false)
    }

    pub fn fastbuy_enabled(&self) -> bool {
        self.fastbuy.unwrap_or(true)
    }

    /// Returns the webhook URL only if it is non-empty.
    pub fn active_webhook_url(&self) -> Option<&str> {
        self.webhook_url.as_deref().filter(|u| !u.is_empty())
    }

    /// Returns all ingame names parsed from the (comma-separated) `ingame_name` field.
    /// `"Account1,Account2"` → `["Account1", "Account2"]`
    pub fn ingame_names(&self) -> Vec<String> {
        match &self.ingame_name {
            None => vec![],
            Some(s) => s
                .split(',')
                .map(|n| n.trim().to_string())
                .filter(|n| !n.is_empty())
                .collect(),
        }
    }

    /// Returns the proxy username parsed from `proxy_credentials` (`"user:pass"` → `"user"`).
    pub fn proxy_username(&self) -> Option<&str> {
        let creds = self.proxy_credentials.as_deref()?;
        // splitn(2, ':').next() always returns Some for non-empty iterators
        Some(creds.splitn(2, ':').next().unwrap())
    }

    /// Returns the proxy password parsed from `proxy_credentials` (`"user:pass"` → `"pass"`).
    pub fn proxy_password(&self) -> Option<&str> {
        let creds = self.proxy_credentials.as_deref()?;
        let colon_pos = creds.find(':')?;
        Some(&creds[colon_pos + 1..])
    }
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn default_config_omits_freemoney() {
        let toml = toml::to_string_pretty(&Config::default()).expect("default config should serialize");
        assert!(!toml.contains("freemoney"));
    }

    #[test]
    fn manual_freemoney_true_enables_flag() {
        let config: Config = toml::from_str("freemoney = true").expect("config should parse");
        assert!(config.freemoney_enabled());
    }

    #[test]
    fn fastbuy_defaults_to_true() {
        assert!(Config::default().fastbuy_enabled());
    }

    #[test]
    fn default_config_omits_fastbuy() {
        let toml = toml::to_string_pretty(&Config::default()).expect("default config should serialize");
        assert!(!toml.contains("fastbuy"));
    }

    #[test]
    fn confirm_skip_does_not_affect_fastbuy() {
        // confirm_skip is a separate setting; fastbuy defaults to true
        let config: Config = toml::from_str("confirm_skip = true").expect("config should parse");
        assert!(config.fastbuy_enabled());
        // Explicit fastbuy = false still overrides the default
        let config: Config = toml::from_str("fastbuy = false\nconfirm_skip = true").expect("config should parse");
        assert!(!config.fastbuy_enabled());
    }

    #[test]
    fn parses_bed_spam_click_delay() {
        let config: Config = toml::from_str("bed_spam_click_delay = 125").expect("config should parse");
        assert_eq!(config.bed_spam_click_delay, 125);
    }

    #[test]
    fn default_bed_pre_click_ms() {
        let config = Config::default();
        assert_eq!(config.bed_pre_click_ms, 30);
    }

    #[test]
    fn parses_bed_pre_click_ms() {
        let config: Config = toml::from_str("bed_pre_click_ms = 300").expect("config should parse");
        assert_eq!(config.bed_pre_click_ms, 300);
    }

    #[test]
    fn single_ingame_name() {
        let config: Config = toml::from_str(r#"ingame_name = "Player1""#).expect("config should parse");
        assert_eq!(config.ingame_names(), vec!["Player1"]);
    }

    #[test]
    fn multiple_ingame_names() {
        let config: Config = toml::from_str(r#"ingame_name = "Player1,Player2,Player3""#)
            .expect("config should parse");
        assert_eq!(config.ingame_names(), vec!["Player1", "Player2", "Player3"]);
    }

    #[test]
    fn multiple_ingame_names_with_spaces() {
        let config: Config = toml::from_str(r#"ingame_name = "Player1, Player2 , Player3""#)
            .expect("config should parse");
        assert_eq!(config.ingame_names(), vec!["Player1", "Player2", "Player3"]);
    }

    #[test]
    fn no_ingame_name() {
        let config = Config::default();
        assert!(config.ingame_names().is_empty());
    }

    #[test]
    fn parses_multi_switch_time() {
        let config: Config = toml::from_str("multi_switch_time = 12.0").expect("config should parse");
        assert_eq!(config.multi_switch_time, Some(12.0));
    }

    #[test]
    fn proxy_credentials_parsing() {
        let config: Config =
            toml::from_str(r#"proxy_credentials = "myuser:mypassword""#).expect("config should parse");
        assert_eq!(config.proxy_username(), Some("myuser"));
        assert_eq!(config.proxy_password(), Some("mypassword"));
    }

    #[test]
    fn proxy_credentials_password_with_colon() {
        let config: Config =
            toml::from_str(r#"proxy_credentials = "user:pass:word""#).expect("config should parse");
        assert_eq!(config.proxy_username(), Some("user"));
        assert_eq!(config.proxy_password(), Some("pass:word"));
    }

    #[test]
    fn proxy_fields_use_new_names() {
        let config: Config = toml::from_str(
            r#"
proxy_enabled = true
proxy_address = "121.124.241.211:3313"
proxy_credentials = "myuser:mypassword"
"#,
        )
        .expect("config should parse");
        assert!(config.proxy_enabled);
        assert_eq!(config.proxy_address.as_deref(), Some("121.124.241.211:3313"));
        assert_eq!(config.proxy_username(), Some("myuser"));
        assert_eq!(config.proxy_password(), Some("mypassword"));
    }

    #[test]
    fn default_config_has_no_skip_field() {
        let toml = toml::to_string_pretty(&Config::default()).expect("default config should serialize");
        assert!(!toml.contains("[skip]"));
        assert!(!toml.contains("min_profit"));
    }
}
