//! Browser-assisted Discord login for the Seller panel.
//!
//! Drives a visible Chromium-based browser window to Discord's login page and
//! intercepts the `Authorization` header of the very first authenticated
//! request after the user signs in.  This mirrors the legacy Python tool's
//! Selenium+Edge flow, but is implemented on top of CDP directly so we avoid
//! shipping a separate WebDriver binary.
//!
//! Flow:
//!   1. Locate a Chromium-based browser executable (Edge is preferred on
//!      Windows because it ships with the OS; Chrome/Brave work too).
//!   2. Launch it with a fresh, ephemeral profile so we never touch the
//!      user's real browser cookies.
//!   3. Register a `Page.addScriptToEvaluateOnNewDocument` hook that patches
//!      `fetch` + `XMLHttpRequest.setRequestHeader` and stashes the first
//!      `Authorization` header it sees on `window.__capturedToken`.
//!   4. Navigate to `https://discord.com/login` and poll that global until
//!      we capture a token or the timeout expires.
//!   5. Validate the captured token against `/users/@me` and return the
//!      display name alongside the token itself.
//!
//! The browser is always launched **visibly** — the user has to log in by
//! hand (optionally solving any CAPTCHA), which is the whole point.  Total
//! wall-clock timeout is five minutes, matching the original tool.

use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::page::AddScriptToEvaluateOnNewDocumentParams;
use futures_util::StreamExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::time::sleep;

/// Maximum time we'll keep the browser window open waiting for the user to
/// sign in.  Anything beyond this is almost certainly a user who wandered
/// away from the machine.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

/// How often we ask the page for `window.__capturedToken`.  Short enough
/// that the user sees the UI react quickly after their first authenticated
/// request, long enough that we don't spam the DevTools socket.
const POLL_INTERVAL: Duration = Duration::from_millis(1500);

/// JS injected into every page frame before any user scripts run.  Wraps
/// `fetch` and `XMLHttpRequest` so we can sniff the `Authorization` header
/// Discord attaches to its own API calls.  The token is the raw header
/// value (no `Bearer ` prefix) — exactly what the user-API expects.
const INTERCEPTOR_JS: &str = r#"
(function () {
  if (window.__twmTokenInjected) { return; }
  window.__twmTokenInjected = true;
  window.__capturedToken = null;

  var grab = function (headers) {
    try {
      if (!headers) { return null; }
      if (typeof headers.get === 'function') {
        var t = headers.get('authorization') || headers.get('Authorization');
        if (t) { return t; }
      } else if (Array.isArray(headers)) {
        for (var i = 0; i < headers.length; i++) {
          var pair = headers[i];
          if (pair && pair.length >= 2 && String(pair[0]).toLowerCase() === 'authorization') {
            return pair[1];
          }
        }
      } else if (typeof headers === 'object') {
        var keys = Object.keys(headers);
        for (var j = 0; j < keys.length; j++) {
          if (keys[j].toLowerCase() === 'authorization') { return headers[keys[j]]; }
        }
      }
    } catch (e) {}
    return null;
  };

  var store = function (t) {
    if (window.__capturedToken) { return; }
    if (typeof t !== 'string' || t.length < 20) { return; }
    // Discord user tokens are long base64-ish strings; OAuth bearer tokens
    // that the login page itself sends (e.g. for hcaptcha) are shorter or
    // explicitly prefixed with "Bearer ".  Skip those so we only grab the
    // real user token.
    if (/^bearer\s/i.test(t)) { return; }
    if (/^basic\s/i.test(t)) { return; }
    window.__capturedToken = t;
  };

  try {
    var origFetch = window.fetch;
    window.fetch = function (resource, init) {
      try {
        if (init && init.headers) {
          store(grab(init.headers));
        } else if (typeof Request !== 'undefined' && resource instanceof Request) {
          store(grab(resource.headers));
        }
      } catch (e) {}
      return origFetch.apply(this, arguments);
    };
  } catch (e) {}

  try {
    var origSet = XMLHttpRequest.prototype.setRequestHeader;
    XMLHttpRequest.prototype.setRequestHeader = function (key, value) {
      try {
        if (typeof key === 'string' && key.toLowerCase() === 'authorization') {
          store(value);
        }
      } catch (e) {}
      return origSet.apply(this, arguments);
    };
  } catch (e) {}
})();
"#;

/// Result of a successful browser login.
#[derive(Debug, Clone)]
pub struct LoginResult {
    pub token: String,
    pub display_name: String,
}

/// Environment variable users can set to point us at an exact browser
/// executable when auto-detection fails — useful for portable installs,
/// non-standard VPS setups, or Windows Store editions of Edge that live
/// under `C:\Program Files\WindowsApps\…` and aren't in the usual paths.
const BROWSER_ENV_VAR: &str = "TWM_SELLER_BROWSER";

/// Human-readable fallback message appended to browser-launch errors.
/// Surfaced verbatim in the Seller tab so users don't have to guess what
/// to try next.
fn browser_not_found_hint() -> String {
    format!(
        "No Chromium-based browser could be located automatically. \
         Install Google Chrome or Microsoft Edge, \
         or set the {BROWSER_ENV_VAR} environment variable to the full path \
         of an existing browser executable (for example \
         'C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe'). \
         Alternatively you can skip this step entirely by pasting your Discord \
         token into the field above — the browser login is just a convenience."
    )
}

/// Query the OS's "where" / "which" command for a named executable.
/// Returns the first hit if any.  We keep this as a short, blocking call
/// (it completes in tens of ms) so it's fine to invoke from async code.
fn resolve_on_path(name: &str) -> Option<PathBuf> {
    #[cfg(windows)]
    let tool = "where";
    #[cfg(not(windows))]
    let tool = "which";

    let output = std::process::Command::new(tool).arg(name).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let p = PathBuf::from(line.trim());
        if !p.as_os_str().is_empty() && p.exists() {
            return Some(p);
        }
    }
    None
}

/// Locate the best Chromium-based browser on the current OS.  We prefer
/// Edge on Windows because it's always installed; otherwise fall back to
/// Chrome/Brave/Chromium.  Returning `None` lets chromiumoxide do its own
/// detection — but in practice this function succeeds on every normal
/// install, so that fallback is only for truly exotic setups.
fn find_browser_executable() -> Option<PathBuf> {
    // 1. Explicit override wins over everything else.
    if let Ok(raw) = std::env::var(BROWSER_ENV_VAR) {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            let p = PathBuf::from(trimmed);
            if p.exists() {
                return Some(p);
            }
        }
    }

    // 2. Hardcoded install paths, OS-specific.
    for p in hardcoded_browser_paths() {
        if p.exists() {
            return Some(p);
        }
    }

    // 3. Ask the OS to resolve each known executable name via PATH.  Edge
    //    registers itself under the "App Paths" registry key on Windows
    //    which `where.exe` honours, so this picks up per-user installs,
    //    portable installs, and custom directories alike.
    #[cfg(windows)]
    let names: &[&str] = &[
        "msedge.exe",
        "chrome.exe",
        "brave.exe",
        "chromium.exe",
    ];
    #[cfg(target_os = "macos")]
    let names: &[&str] = &[
        "Google Chrome",
        "Microsoft Edge",
        "Brave Browser",
        "Chromium",
        "google-chrome",
        "chromium",
    ];
    #[cfg(all(unix, not(target_os = "macos")))]
    let names: &[&str] = &[
        "google-chrome",
        "google-chrome-stable",
        "chromium",
        "chromium-browser",
        "microsoft-edge",
        "microsoft-edge-stable",
        "brave-browser",
    ];

    for name in names {
        if let Some(p) = resolve_on_path(name) {
            return Some(p);
        }
    }

    None
}

#[cfg(windows)]
fn hardcoded_browser_paths() -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = vec![
        PathBuf::from(r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe"),
        PathBuf::from(r"C:\Program Files\Microsoft\Edge\Application\msedge.exe"),
        PathBuf::from(r"C:\Program Files\Google\Chrome\Application\chrome.exe"),
        PathBuf::from(r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe"),
        PathBuf::from(r"C:\Program Files\BraveSoftware\Brave-Browser\Application\brave.exe"),
        PathBuf::from(
            r"C:\Program Files (x86)\BraveSoftware\Brave-Browser\Application\brave.exe",
        ),
        PathBuf::from(r"C:\Program Files\Chromium\Application\chrome.exe"),
    ];
    if let Some(local) = dirs::data_local_dir() {
        candidates.push(local.join("Microsoft/Edge/Application/msedge.exe"));
        candidates.push(local.join("Google/Chrome/Application/chrome.exe"));
        candidates.push(local.join("BraveSoftware/Brave-Browser/Application/brave.exe"));
        candidates.push(local.join("Chromium/Application/chrome.exe"));
    }
    if let Some(roam) = dirs::data_dir() {
        candidates.push(roam.join("Microsoft/Edge/Application/msedge.exe"));
    }
    candidates
}

#[cfg(target_os = "macos")]
fn hardcoded_browser_paths() -> Vec<PathBuf> {
    vec![
        PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
        PathBuf::from("/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge"),
        PathBuf::from("/Applications/Brave Browser.app/Contents/MacOS/Brave Browser"),
        PathBuf::from("/Applications/Chromium.app/Contents/MacOS/Chromium"),
    ]
}

#[cfg(all(unix, not(target_os = "macos")))]
fn hardcoded_browser_paths() -> Vec<PathBuf> {
    vec![
        PathBuf::from("/usr/bin/google-chrome"),
        PathBuf::from("/usr/bin/google-chrome-stable"),
        PathBuf::from("/usr/bin/chromium"),
        PathBuf::from("/usr/bin/chromium-browser"),
        PathBuf::from("/usr/bin/microsoft-edge"),
        PathBuf::from("/usr/bin/microsoft-edge-stable"),
        PathBuf::from("/usr/bin/brave-browser"),
        PathBuf::from("/snap/bin/chromium"),
        PathBuf::from("/snap/bin/brave"),
    ]
}

/// Spawn a visible browser window, let the user log in, and return the
/// captured token together with the account's display name.
///
/// This function takes control of one browser process; callers should
/// serialize concurrent invocations.  The overall wall-clock timeout is
/// [`LOGIN_TIMEOUT`].
pub async fn extract_token_via_login() -> Result<LoginResult, String> {
    let mut builder = BrowserConfig::builder()
        .with_head()
        .args([
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-blink-features=AutomationControlled",
            // A small, focused window fits next to the TWM panel on most
            // displays and nudges the user toward the login form.
            "--window-size=520,760",
        ]);

    if let Some(exe) = find_browser_executable() {
        builder = builder.chrome_executable(exe);
    }

    let config = builder
        .build()
        .map_err(|e| format!("{e}. {}", browser_not_found_hint()))?;

    let (mut browser, mut handler) = Browser::launch(config)
        .await
        .map_err(|e| format!("could not launch browser: {e}. {}", browser_not_found_hint()))?;

    // chromiumoxide requires us to continuously drive the CDP handler; if
    // the task stops polling, the websocket stalls and every request hangs.
    let handler_task = tokio::spawn(async move {
        while let Some(event) = handler.next().await {
            // Discarding events is intentional — we only care about
            // explicit replies to commands we issue.  Errors here usually
            // mean the socket closed (i.e. the browser shut down).
            if event.is_err() {
                break;
            }
        }
    });

    // Run the actual login flow, but always tear the browser down afterward
    // so a failed attempt doesn't leave a zombie window around.
    let outcome = run_login_flow(&mut browser).await;

    let _ = browser.close().await;
    let _ = browser.wait().await;
    handler_task.abort();

    outcome
}

/// Drive the browser to Discord, inject the interceptor, poll for a token.
async fn run_login_flow(browser: &mut Browser) -> Result<LoginResult, String> {
    // Creating a blank page first lets us register the document-start hook
    // before any Discord JS runs — otherwise the first few XHRs could slip
    // past us.
    let page = browser
        .new_page("about:blank")
        .await
        .map_err(|e| format!("could not open browser tab: {e}"))?;

    page.execute(AddScriptToEvaluateOnNewDocumentParams::new(
        INTERCEPTOR_JS.to_string(),
    ))
    .await
    .map_err(|e| format!("could not inject login hook: {e}"))?;

    page.goto("https://discord.com/login")
        .await
        .map_err(|e| format!("could not open Discord: {e}"))?;

    let deadline = Instant::now() + LOGIN_TIMEOUT;
    loop {
        if Instant::now() >= deadline {
            return Err(format!(
                "login timed out after {} seconds",
                LOGIN_TIMEOUT.as_secs()
            ));
        }

        sleep(POLL_INTERVAL).await;

        // `evaluate` can fail transiently during a navigation; treat that
        // as "try again next tick" rather than a hard error.
        let evaluated = match page.evaluate("window.__capturedToken || null").await {
            Ok(v) => v,
            Err(_) => continue,
        };

        let value = evaluated.value();
        let token_str = value
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && s != "null");

        let Some(token) = token_str else { continue };

        // Validate against /users/@me before handing the token back.  This
        // filters out non-user Authorization values (e.g. a captcha bearer
        // token) that snuck past the JS side's heuristics.
        match super::discord::validate_token(&token).await {
            Ok(display_name) => {
                return Ok(LoginResult {
                    token,
                    display_name,
                });
            }
            Err(_) => {
                // Clear the stored value and keep polling — the real token
                // should appear on the next authenticated request.
                let _ = page
                    .evaluate("window.__capturedToken = null; true")
                    .await;
                continue;
            }
        }
    }
}
