// Shared GitHub API helpers used by the PR-preview providers.

use anyhow::{Context, Result};
use std::sync::OnceLock;

/// Process-wide HTTP client. `reqwest::Client` owns a connection pool and TLS
/// config, so it is built once and reused (a PR resolve makes several dependent
/// calls) rather than reconstructed per request.
fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
}

/// GET a GitHub API URL and parse the JSON response.
pub(crate) async fn get_json(url: &str) -> Result<serde_json::Value> {
    let resp = client()
        .get(url)
        .header(
            "User-Agent",
            concat!("webspec-index/", env!("CARGO_PKG_VERSION")),
        )
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()
        .with_context(|| format!("GitHub API request failed: {url}"))?;
    Ok(resp.json().await?)
}

/// Extract a required string field from a JSON value, naming it in the error so
/// a missing/null field points at the exact response path.
pub(crate) fn str_field(value: &serde_json::Value, field: &str) -> Result<String> {
    value
        .as_str()
        .map(str::to_string)
        .with_context(|| format!("GitHub API response missing {field}"))
}

/// Resolve a short SHA to a full SHA via the GitHub API. A full SHA passed in
/// is echoed back unchanged.
pub(crate) async fn resolve_full_sha(repo: &str, sha: &str) -> Result<String> {
    let url = format!("https://api.github.com/repos/{repo}/commits/{sha}");
    let json = get_json(&url).await?;
    str_field(&json["sha"], "sha")
}
