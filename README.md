# TWM

TWM is a Rust-based Hypixel SkyBlock flipping client focused on Auction House and Bazaar automation, local control, and fast deployment on Linux.

It connects to Coflnet for flip data, logs in through Microsoft authentication, exposes a built-in web panel, and persists profit data so the UI can show both session profit and all-time profit separately.

## Warning

This project automates gameplay actions on Hypixel.

Use it at your own risk.

If you use automation on a live server, bans and account loss are possible.

## What TWM Does

- Executes Auction House BIN flip flows with low-latency optimizations.
- Manages Bazaar orders and Bazaar flip collection.
- Exposes a local web GUI on port `8080` by default.
- Supports Discord webhooks for purchases, sales, Bazaar events, and alerts.
- Persists profit history across restarts while still tracking the current session separately.
- Supports multi-account rotation and optional humanization/rest-break settings.
- Includes a self-updating loader for Linux releases.

## Release Targets

The GitHub release workflow currently publishes Linux `x86_64` binaries only.

Release assets:

- `twm-linux-x86_64`
- `TWM-loader-linux-x86_64`

The loader is the recommended download because it can update the main binary automatically.

## Quick Install On Linux

### Recommended: loader

```bash
curl -fL https://github.com/froumes/escrow/releases/latest/download/TWM-loader-linux-x86_64 -o TWM-loader && chmod +x TWM-loader
./TWM-loader
```

### Direct binary

```bash
curl -fL https://github.com/froumes/escrow/releases/latest/download/twm-linux-x86_64 -o twm && chmod +x twm
./twm
```

## First Run

On first launch, TWM will guide you through the normal setup flow:

1. Enter your Minecraft in-game name if it is not already configured.
2. Complete Microsoft authentication when prompted.
3. Let the client connect to Hypixel.
4. Open the local panel in your browser once the web server starts.

By default the panel runs at:

```text
http://localhost:8080
```

If you are running on a VPS, replace `localhost` with the server IP and set `web_gui_password` before exposing it publicly.

## Built-In Web Panel

The web GUI is served from the main `twm` process and includes:

- runtime status
- pause/resume controls
- inventory and active window inspection
- active auctions and Bazaar orders
- live chat stream and command sending
- config editing
- session profit and all-time profit charts

If `web_gui_password` is empty, the panel is open on the configured port.

If `web_gui_password` is set, the panel requires login and uses a local session cookie.

## Configuration

TWM writes a `config.toml` file next to the executable.

Useful settings include:

- `ingame_name`
  Comma-separated account list is supported, for example `"Account1,Account2"`.
- `multi_switch_time`
  Hours before rotating to the next configured account.
- `web_gui_port`
  Local control panel port. Default: `8080`.
- `web_gui_password`
  Password-protect the web panel.
- `enable_bazaar_flips`
  Enables or disables Bazaar flipping logic at startup.
- `fastbuy`
  Enables the more aggressive fast-buy confirm path. Defaults to enabled.
- `freemoney`
  Enables the more aggressive bed/grace-period path.
- `command_delay_ms`
  Delay between queued commands.
- `bed_spam_click_delay`
  Click cadence for bed/grace purchase logic.
- `bazaar_order_check_interval_seconds`
  How often Bazaar order management runs.
- `webhook_url`
  Main Discord webhook for notifications.
- `bazaar_webhook_url`
  Optional separate webhook for Bazaar-only events.
- `discord_id`
  Optional Discord user ID for pings.
- `hypixel_api_key`
  Optional Hypixel API key for auction lookups.
- `proxy_enabled`, `proxy_address`, `proxy_credentials`
  Proxy support for Minecraft and websocket connections.
- `humanization_*`
  Optional rest-break behavior.

## Files TWM Persists

TWM stores runtime files next to the executable.

Common files you will see:

- `config.toml`
  Main configuration.
- `session_times.json`
  Account/session runtime tracking.
- `profit_history.json`
  Persisted AH and Bazaar profit history.
- `ah_purchase_ledger.json`
  Purchase ledger used for sold-profit recovery across restarts.
- `logs/latest.log`
  Current log output.

## Updating

If you use `TWM-loader`, updates are the easiest:

```bash
./TWM-loader
```

If you use the main binary directly, replace it with the latest release asset.

## Building From Source

TWM currently targets Rust nightly.

```bash
rustup toolchain install nightly
cargo +nightly build --release
```

Built binaries:

- main app: `target/release/twm`
- loader: `target/release/TWM-loader`

Run the main app directly:

```bash
cargo +nightly run --release --bin twm
```

Run the loader directly:

```bash
cargo +nightly run --release --bin TWM-loader
```

## Source Tree Launcher

This repo also includes a convenience launcher script named [`twm`](./twm).

Inside the source tree it will:

- run `target/release/twm` if it already exists
- otherwise build the project
- then launch TWM with any arguments you pass through

Example:

```bash
chmod +x twm
./twm
```

## Development Notes

- The release workflow currently builds Linux artifacts only.
- The local panel tracks session profit separately from all-time profit so per-hour stats stay tied to the active runtime.
- The project uses a patched local `azalea-client` dependency to reduce client-side detection latency.

## Support

If you are debugging issues, start with:

- `logs/latest.log`
- your `config.toml`
- the local web panel

If the panel or runtime behavior looks wrong after a restart, check that `profit_history.json` and `ah_purchase_ledger.json` are present next to the binary.

## License

AGPL-3.0
