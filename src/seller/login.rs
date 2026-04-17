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

/// Locate the best Chromium-based browser on the current OS.  We prefer
/// Edge on Windows because it's always installed; otherwise fall back to
/// Chrome.  Returning `None` lets chromiumoxide do its own detection.
#[cfg(windows)]
fn find_browser_executable() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = vec![
        PathBuf::from(r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe"),
        PathBuf::from(r"C:\Program Files\Microsoft\Edge\Application\msedge.exe"),
        PathBuf::from(r"C:\Program Files\Google\Chrome\Application\chrome.exe"),
        PathBuf::from(r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe"),
        PathBuf::from(r"C:\Program Files\BraveSoftware\Brave-Browser\Application\brave.exe"),
    ];
    if let Some(local) = dirs::data_local_dir() {
        candidates.push(local.join("Microsoft/Edge/Application/msedge.exe"));
        candidates.push(local.join("Google/Chrome/Application/chrome.exe"));
        candidates.push(local.join("BraveSoftware/Brave-Browser/Application/brave.exe"));
    }
    candidates.into_iter().find(|p| p.exists())
}

#[cfg(target_os = "macos")]
fn find_browser_executable() -> Option<PathBuf> {
    let candidates = [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
    ];
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn find_browser_executable() -> Option<PathBuf> {
    let candidates = [
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/usr/bin/microsoft-edge",
        "/usr/bin/brave-browser",
    ];
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
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
        .map_err(|e| format!("failed to configure browser: {e}"))?;

    let (mut browser, mut handler) = Browser::launch(config).await.map_err(|e| {
        format!(
            "could not launch browser: {e}. \
             Make sure Google Chrome, Microsoft Edge, or another Chromium-based browser is installed."
        )
    })?;

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
