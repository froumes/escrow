use anyhow::Result;
use once_cell::sync::Lazy;
use std::path::{Path, PathBuf};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{filter::LevelFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

pub fn init_logger() -> Result<()> {
    let logs_dir = get_logs_dir();
    std::fs::create_dir_all(&logs_dir)?;
    rotate_previous_latest_log(&logs_dir)?;
    // Clean up log files older than 7 days at startup
    cleanup_old_logs(&logs_dir, 7);

    // Create file appender
    let file_appender = RollingFileAppender::new(
        Rotation::NEVER,
        &logs_dir,
        "latest.log",
    );

    // Create filter with specific rules to suppress noise.
    // Azalea library crates emit harmless errors & warnings (e.g. set_equipment
    // "Unexpected enum variant 7", chunk entity warnings) that spam the console.
    // We suppress them entirely – any important azalea error will surface through
    // our own logging when we handle the event.
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| {
            EnvFilter::new("info")
                .add_directive("azalea_world=off".parse().unwrap())
                .add_directive("azalea_entity=off".parse().unwrap())
                .add_directive("azalea_client=off".parse().unwrap())
                .add_directive("azalea_protocol=off".parse().unwrap())
        });

    // Set up subscriber with both console and file output
    tracing_subscriber::registry()
        .with(filter)
        .with(
            fmt::layer()
                .with_writer(std::io::stdout)
                .with_ansi(true)
                .with_target(false)
                .with_filter(LevelFilter::WARN)
        )
        .with(
            fmt::layer()
                .with_writer(file_appender)
                .with_ansi(false)
                .with_target(true)
        )
        .init();

    tracing::info!("Logger initialized, writing to {:?}", logs_dir.join("latest.log"));
    Ok(())
}

pub fn get_logs_dir() -> PathBuf {
    // Use executable directory for log file
    // This allows multiple instances to run with separate logs
    let exe_dir = match std::env::current_exe() {
        Ok(exe_path) => {
            exe_path.parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| {
                    eprintln!("Warning: Could not get parent directory of executable, using current directory");
                    PathBuf::from(".")
                })
        }
        Err(e) => {
            eprintln!("Warning: Could not get executable path ({}), using current directory", e);
            PathBuf::from(".")
        }
    };

    exe_dir.join("logs")
}

fn rotate_previous_latest_log(logs_dir: &Path) -> Result<()> {
    let latest_log = logs_dir.join("latest.log");
    if !latest_log.exists() {
        return Ok(());
    }

    let session_start = std::fs::metadata(&latest_log)
        .and_then(|m| m.modified())
        .ok()
        .map(chrono::DateTime::<chrono::Utc>::from)
        .unwrap_or_else(chrono::Utc::now);

    let base_name = session_start
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d_%H-%M-%S")
        .to_string();

    let mut archived_log = logs_dir.join(format!("{}.log", base_name));
    let mut suffix = 1usize;
    while archived_log.exists() {
        archived_log = logs_dir.join(format!("{}_{}.log", base_name, suffix));
        suffix += 1;
    }
    std::fs::rename(latest_log, archived_log)?;
    Ok(())
}

/// Delete archived log files older than `max_age_days` days.
/// Only removes `*.log` files that are NOT `latest.log`.
pub fn cleanup_old_logs(logs_dir: &Path, max_age_days: u64) {
    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(max_age_days * 86400));
    let cutoff = match cutoff {
        Some(c) => c,
        None => return, // clock error, skip cleanup
    };

    let entries = match std::fs::read_dir(logs_dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    let mut removed = 0u32;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        // Only delete archived log files, never the active latest.log
        if name == "latest.log" || !name.ends_with(".log") {
            continue;
        }
        let modified = match entry.metadata().and_then(|m| m.modified()) {
            Ok(t) => t,
            Err(_) => continue,
        };
        if modified < cutoff {
            if let Err(e) = std::fs::remove_file(&path) {
                eprintln!("Warning: Failed to remove old log {}: {}", name, e);
            } else {
                removed += 1;
            }
        }
    }
    if removed > 0 {
        // Logger may not be initialized yet at startup, so use eprintln
        eprintln!("Cleaned up {} old log file(s) from {:?}", removed, logs_dir);
    }
}

/// Spawn a background task that cleans up old logs once per day.
pub fn spawn_periodic_log_cleanup() {
    let logs_dir = get_logs_dir();
    tokio::spawn(async move {
        loop {
            // Run cleanup every 24 hours
            tokio::time::sleep(tokio::time::Duration::from_secs(86400)).await;
            cleanup_old_logs(&logs_dir, 7);
        }
    });
}

/// Pre-compiled regex for stripping Minecraft §-color codes.  Compiled once on
/// first use instead of on every call, avoiding ~1 ms of regex compilation overhead
/// per invocation (significant during rapid chat/lore processing).
static COLOR_CODE_RE: Lazy<regex::Regex> =
    Lazy::new(|| regex::Regex::new(r"§[0-9a-fk-or]").unwrap());

/// Remove Minecraft color codes from a string
pub fn remove_color_codes(text: &str) -> String {
    COLOR_CODE_RE.replace_all(text, "").to_string()
}

/// Convert Minecraft color codes to ANSI color codes for terminal display
pub fn mc_to_ansi(text: &str) -> String {
    text.replace("§0", "\x1b[30m")     // Black
        .replace("§1", "\x1b[34m")     // Dark Blue
        .replace("§2", "\x1b[32m")     // Dark Green
        .replace("§3", "\x1b[36m")     // Dark Aqua
        .replace("§4", "\x1b[31m")     // Dark Red
        .replace("§5", "\x1b[35m")     // Dark Purple
        .replace("§6", "\x1b[33m")     // Gold
        .replace("§7", "\x1b[37m")     // Gray
        .replace("§8", "\x1b[90m")     // Dark Gray
        .replace("§9", "\x1b[94m")     // Blue
        .replace("§a", "\x1b[92m")     // Green
        .replace("§b", "\x1b[96m")     // Aqua
        .replace("§c", "\x1b[91m")     // Red
        .replace("§d", "\x1b[95m")     // Light Purple
        .replace("§e", "\x1b[93m")     // Yellow
        .replace("§f", "\x1b[97m")     // White
        .replace("§l", "\x1b[1m")      // Bold
        .replace("§m", "\x1b[9m")      // Strikethrough
        .replace("§n", "\x1b[4m")      // Underline
        .replace("§o", "\x1b[3m")      // Italic
        .replace("§r", "\x1b[0m")      // Reset
        + "\x1b[0m" // Always reset at the end
}

/// Print a Minecraft chat message to console (with color code processing)
pub fn print_mc_chat(message: &str) {
    let colored = mc_to_ansi(message);
    println!("{}", colored);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_remove_color_codes() {
        let text = "§f[§4BAF§f]: §aHello §eWorld";
        let clean = remove_color_codes(text);
        assert_eq!(clean, "[BAF]: Hello World");
    }

    #[test]
    fn test_mc_to_ansi() {
        let text = "§f[§4BAF§f]: §aTest";
        let ansi = mc_to_ansi(text);
        
        // Should contain ANSI escape codes
        assert!(ansi.contains("\x1b["));
        // Should end with reset
        assert!(ansi.ends_with("\x1b[0m"));
        // Original text parts should still be present (ANSI codes are inserted, not replacing text)
        assert!(ansi.contains("BAF"));
        assert!(ansi.contains("Test"));
    }

    #[test]
    fn test_mc_to_ansi_colors() {
        // Test all basic colors are converted
        assert!(mc_to_ansi("§0").contains("\x1b[30m")); // Black
        assert!(mc_to_ansi("§1").contains("\x1b[34m")); // Dark Blue
        assert!(mc_to_ansi("§2").contains("\x1b[32m")); // Dark Green
        assert!(mc_to_ansi("§4").contains("\x1b[31m")); // Dark Red
        assert!(mc_to_ansi("§a").contains("\x1b[92m")); // Green
        assert!(mc_to_ansi("§c").contains("\x1b[91m")); // Red
        assert!(mc_to_ansi("§e").contains("\x1b[93m")); // Yellow
        assert!(mc_to_ansi("§f").contains("\x1b[97m")); // White
        
        // Test formatting codes
        assert!(mc_to_ansi("§l").contains("\x1b[1m"));  // Bold
        assert!(mc_to_ansi("§r").contains("\x1b[0m"));  // Reset
    }
}
