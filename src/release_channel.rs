/// Default repository used for releases when no explicit public mirror is
/// configured at build time.
pub const DEFAULT_RELEASE_REPO: &str = "froumes/twm-releases";

/// The repository that binaries should check for updates and downloads.
///
/// Private-source builds can override this at compile time by setting the
/// `TWM_RELEASE_REPO` environment variable in CI, for example:
/// `owner/twm-binaries`.
pub fn public_release_repo() -> &'static str {
    option_env!("TWM_RELEASE_REPO").unwrap_or(DEFAULT_RELEASE_REPO)
}

/// Canonical GitHub URL for the public release repository.
pub fn public_release_repo_url() -> String {
    format!("https://github.com/{}", public_release_repo())
}

/// Canonical GitHub Releases download URL prefix.
pub fn latest_release_download_base_url() -> String {
    format!("{}/releases/latest/download", public_release_repo_url())
}
