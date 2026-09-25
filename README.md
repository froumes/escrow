# TWM Source Repository

This is the TWM source tree, based on [Frikadellen BAF](https://github.com/TreXito/frikadellen-baf-121): a Rust Hypixel SkyBlock flipping client with Bazaar and Auction House automation, a web panel, Discord integrations, and a self-updating loader.

Public downloads are published from a separate repository. The intended setup is:

- keep this repository private
- build artifacts here in GitHub Actions
- publish binaries, checksums, and public-facing docs to a separate public repository

The release mirror flow in this repo is set up for that model. See [docs/release-mirror-setup.md](docs/release-mirror-setup.md) for the GitHub-side steps.

## Local build

The project pins Rust nightly in `rust-toolchain.toml` and targets Minecraft 26.1 (Azalea 0.16).

```bash
cargo build --release
```

Built binaries:

- `target/release/twm`
- `target/release/TWM-loader`
- Windows-target builds additionally produce `twm.exe` and `TWM-loader.exe`

Run either binary directly:

```bash
cargo +nightly run --release --bin twm
cargo +nightly run --release --bin TWM-loader
```

## Combined features

Upstream additions include finder feeds, optional central-backend control, per-account SOCKS5 proxies, safer auction/Bazaar order handling, rest-break recovery, and a password-protected HTTPS control panel. TWM retains its Seller tab, persistent flip history, read-only stats sharing, profit-recovery ledger, and dedicated public release channel. Optional backend and finder integrations require their own configuration.

The Seller browser-login flow captures a Discord user token and stores it locally in plaintext; use it only if you accept that credential risk. Do not expose the local config or Seller API without panel authentication. Anyone with a stats share token can view its read-only data.

## Release mirror

The release workflow now uses `froumes/twm-releases` by default and expects this GitHub Actions secret in the source repo:

- `PUBLIC_RELEASES_PAT`
  A token with `contents:write` access to the public mirror repo

Optional override:

- `PUBLIC_RELEASE_REPO`
  Format: `owner/repo`
  When unset, CI defaults to `froumes/twm-releases`

At build time, CI injects `TWM_RELEASE_REPO`, so the loader and in-app update checks point to the public releases repo instead of this one.

Public-repo collateral lives in [public-release-repo](public-release-repo). The workflow syncs that folder into the public mirror before it publishes a release.

Existing loaders compiled against the old repo do not automatically migrate. See [docs/release-mirror-setup.md](docs/release-mirror-setup.md) for the one-time migration note.

## Web panel

The web panel runs on port 8080 by default, serves HTTPS with a password, and includes Seller, account controls, flip history, and optional read-only stats sharing. Preserve an existing `web_gui_password` when migrating; consult the startup log for generated credentials on a fresh installation. The panel and share page link to the configured public releases repository.

## Pages

The GitHub Pages workflow is manual-only now. If you intend this source repo to stay private, also disable GitHub Pages in repository settings so an old public site does not linger.

This repository is intended to trigger release publishing into the public mirror repo rather than act as the public download surface itself.

## Important license note

`Cargo.toml` currently declares `AGPL-3.0`. If you publicly distribute binaries under AGPL, you still need to make the exact corresponding source for those binaries available with equivalent access. A private source repo plus a public binary-only repo is not sufficient on its own.
