# TWM Releases

This repository is the public download home for TWM.

It is intentionally focused on distribution, not development. You should expect to find:

- release binaries
- loader binaries
- checksums
- release notes
- short end-user docs

The source repository is `__SOURCE_REPO__`.

## Start here

If you are downloading TWM for the first time, use the loader for your platform:

- Windows: `TWM-loader-windows-x86_64.exe`
- Linux: `TWM-loader-linux-x86_64`

The loader checks this repository for the latest release, downloads the matching main binary when needed, and then launches TWM.

If you do not want the loader, you can download the direct binaries instead:

- Windows: `twm-windows-x86_64.exe`
- Linux: `twm-linux-x86_64`

## Latest release

- [Open the latest release](__PUBLIC_RELEASE_REPO_URL__/releases/latest)
- [Browse all releases](__PUBLIC_RELEASE_REPO_URL__/releases)

Every release includes:

- main binaries for supported platforms
- loader binaries for supported platforms
- `SHA256SUMS.txt` for verification

## One-time migration from `froumes/escrow`

Older `TWM-loader` copies that were originally downloaded from `froumes/escrow` will keep checking that old repository.

That means if the old repo becomes private:

- old loaders will stop receiving updates
- they will usually still launch the already-installed local `twm` binary
- they will not automatically discover this repository

To migrate, download the latest loader from this repository once and replace your old loader with it.

Migration help:

- [Migrating from `froumes/escrow`](docs/MIGRATING_FROM_ESCROW.md)

## Verify your download

Before running a binary, compare its SHA-256 hash with `SHA256SUMS.txt`.

- [Verification guide](docs/VERIFY_DOWNLOADS.md)

Quick commands:

```powershell
certutil -hashfile .\TWM-loader-windows-x86_64.exe SHA256
certutil -hashfile .\twm-windows-x86_64.exe SHA256
```

```bash
sha256sum TWM-loader-linux-x86_64
sha256sum twm-linux-x86_64
```

## How updates work

The loader updates the main `twm` binary, not the loader itself.

In practice:

- keep using the loader for normal binary updates
- replace the loader manually if a release specifically mentions a loader change
- keep `twm` and `TWM-loader` in the same folder

On first successful update or launch, you will typically see files like:

- `TWM-loader.exe` or `TWM-loader`
- `twm.exe` or `twm`
- `.version`
- `config.toml`
- runtime logs and profit history files

## Support notes

This repository is a release mirror, not the main development home.

- release notes describe what changed
- troubleshooting should start with your local logs and config
- issues and pull requests may be disabled here

## Source and license

The source repository is `__SOURCE_REPO__`.

If TWM continues to be distributed under AGPL, corresponding source for each released binary must also be made available with equivalent access.
