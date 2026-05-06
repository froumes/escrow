# Private Source + Public Binary Mirror Setup

This repository now supports a split model:

- private source repo for code, issues, and CI
- public repo for release binaries, checksums, and end-user documentation

## 1. Create the public mirror repo

Create a separate public GitHub repository that will hold:

- release assets
- `README.md`
- checksum instructions
- any minimal public docs you want users to see

This repo should already exist before the workflow runs.

## 2. Add GitHub Actions configuration to the private source repo

In the source repository settings, add:

- Actions secret: `PUBLIC_RELEASES_PAT`
  Use a fine-grained PAT or app token with `contents:write` access to the public mirror repo

Optional:

- Actions variable: `PUBLIC_RELEASE_REPO`
  For this setup it can be omitted because the workflow defaults to `froumes/twm-releases`
  Set it only if you want a different public mirror repo

## 3. Make the source repo private

This change still has to be done in GitHub repository settings. It is not exposed through the tools available in this environment.

After switching visibility, confirm:

- collaborators still have access
- Actions still runs
- any external integrations still authenticate correctly

## 4. Disable Pages on the source repo

The workflow in this repo is manual-only now, but if GitHub Pages is already enabled it can keep serving old content.

Disable Pages in repository settings unless you intentionally want a public site that is separate from the binary mirror.

The existing `webpage/` landing page still has source-repo-specific links. If you ever re-enable Pages, update that content to point at the public mirror first.

## 5. Seed the public mirror content

The template folder [public-release-repo](../public-release-repo) is what CI syncs into the public repo before each release. Update that template if you want different copy, branding, or support links.

## 6. Trigger a release

The release workflow runs on:

- pushes to `main`
- tags matching `v*`
- merged pull requests into `main`

For each run it will:

1. build Linux and Windows binaries
2. compile the loader so it points at the public mirror repo
3. upload artifacts and generate `SHA256SUMS.txt`
4. sync the public repo template
5. create or update a GitHub Release in the public repo

## Existing loader migration

Older `TWM-loader` binaries do not self-update and do not switch repositories by themselves.

That means:

- any loader already downloaded from `froumes/escrow` will keep checking `froumes/escrow`
- once `froumes/escrow` becomes private, those loaders will fail the update check and then launch the already-installed `twm` binary
- users will need a one-time manual download of the new loader from `froumes/twm-releases`

After that one-time replacement, future loader runs will check `froumes/twm-releases` as intended.

## AGPL warning

This project metadata currently says `AGPL-3.0`.

If you keep distributing binaries under AGPL, you still need to provide the exact corresponding source with equivalent access. GNU's FAQ says that if object code is downloadable from a network server, the corresponding source must also be accessible at least as easily. Source: [GNU GPL FAQ](https://www.gnu.org/licenses/gpl-faq.en.html) and [GNU AGPL v3 section 6](https://www.gnu.org/licenses/agpl-3.0.en.html).

That means the technical split in this repo is workable, but a private source repo plus a public binary-only repo is not enough for AGPL compliance unless you also publish or otherwise provide the matching source for each binary release.
