# TWM Source Repository

This repository is the private source tree for TWM: a Rust-based Hypixel SkyBlock flipping client with a built-in web panel, Discord webhooks, Bazaar and Auction House automation, and a self-updating loader.

If you want public downloads, do not publish them from this repository directly. The intended setup is:

- keep this repository private
- build artifacts here in GitHub Actions
- publish binaries, checksums, and public-facing docs to a separate public repository

The release mirror flow in this repo is set up for that model. See [docs/release-mirror-setup.md](docs/release-mirror-setup.md) for the GitHub-side steps.

## Local build

TWM currently targets Rust nightly.

```bash
rustup toolchain install nightly
cargo +nightly build --release
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

The built-in panel is still served by the app on `http://localhost:8080` by default. The panel and read-only share page now link to the configured public releases repo rather than the source repo.

## Pages

The GitHub Pages workflow is manual-only now. If you intend this source repo to stay private, also disable GitHub Pages in repository settings so an old public site does not linger.

This repository is intended to trigger release publishing into the public mirror repo rather than act as the public download surface itself.

## Important license note

`Cargo.toml` currently declares `AGPL-3.0`. If you publicly distribute binaries under AGPL, you still need to make the exact corresponding source for those binaries available with equivalent access. A private source repo plus a public binary-only repo is not sufficient on its own.
