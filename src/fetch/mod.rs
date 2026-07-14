// Fetch orchestration: coordinate HTML fetching, parsing, and database writes
pub mod github;
pub mod pr;
pub mod snapshot;
pub mod whatpr;

use crate::db::{queries, write};
use crate::parse;
use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::Connection;
use sha2::{Digest, Sha256};

const CHECK_INTERVAL_HOURS: i64 = 24;

fn is_fresh(last_checked: &DateTime<Utc>, now: &DateTime<Utc>) -> bool {
    now.signed_duration_since(*last_checked).num_hours() < CHECK_INTERVAL_HOURS
}

/// Whether the cached index was produced by the current build. A new release
/// (bumped `INDEX_VERSION`) makes this false, forcing a re-parse even when the
/// upstream HTML is unchanged.
fn index_is_current(state: &queries::UpdateCheckState) -> bool {
    state.index_version.as_deref() == Some(parse::INDEX_VERSION)
}

/// Whether a cached spec can be served without re-syncing: it must be within the
/// freshness window AND have been produced by the current build. A version
/// upgrade invalidates the freshness window so the next query re-indexes even if
/// the 24h check has not elapsed.
fn cache_is_current(state: &queries::UpdateCheckState, now: &DateTime<Utc>) -> bool {
    is_fresh(&state.last_checked, now) && index_is_current(state)
}

fn hash_html(html: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(html.as_bytes());
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn store_update_check(
    conn: &Connection,
    spec_id: i64,
    now: &DateTime<Utc>,
    last_indexed: Option<&DateTime<Utc>>,
    content_hash: Option<&str>,
) -> Result<()> {
    let checked = now.to_rfc3339();
    let indexed = last_indexed.map(|t| t.to_rfc3339());
    write::record_update_check(
        conn,
        spec_id,
        &checked,
        indexed.as_deref(),
        content_hash,
        Some(parse::INDEX_VERSION),
    )
}

#[allow(clippy::too_many_arguments)]
fn sync_from_html(
    conn: &Connection,
    spec_id: i64,
    spec_name: &str,
    base_url: &str,
    provider_name: &str,
    html: String,
    previous_snapshot_id: Option<i64>,
    state: Option<queries::UpdateCheckState>,
    now: &DateTime<Utc>,
) -> Result<(i64, bool)> {
    let content_hash = hash_html(&html);

    if let (Some(snapshot_id), Some(state)) = (previous_snapshot_id, state.as_ref()) {
        let content_unchanged = state.content_hash.as_deref() == Some(content_hash.as_str());
        if content_unchanged && index_is_current(state) {
            store_update_check(
                conn,
                spec_id,
                now,
                state.last_indexed.as_ref(),
                Some(&content_hash),
            )?;
            return Ok((snapshot_id, false));
        }
    }

    let parsed = parse::parse_spec(&html, spec_name, base_url)?;
    write::delete_spec_data(conn, spec_id)?;

    let synthetic_sha = format!("hash:{content_hash}");
    let commit_date = now.to_rfc3339();
    let spec_id_reloaded = write::insert_or_get_spec(conn, spec_name, base_url, provider_name)?;
    let snapshot_id = write::insert_snapshot(conn, spec_id_reloaded, &synthetic_sha, &commit_date)?;
    write::insert_sections_bulk(conn, snapshot_id, &parsed.sections)?;
    write::insert_refs_bulk(conn, snapshot_id, &parsed.references)?;
    write::insert_idl_defs_bulk(conn, snapshot_id, &parsed.idl_definitions)?;

    store_update_check(conn, spec_id_reloaded, now, Some(now), Some(&content_hash))?;
    Ok((snapshot_id, true))
}

fn is_respec_source(html: &str) -> bool {
    html.contains("respec-w3c") || html.contains("respec.js") || html.contains("/respec/")
}

async fn render_via_spec_generator(url: &str) -> Result<String> {
    let api_url = format!(
        "https://www.w3.org/publications/spec-generator/?type=respec&url={}",
        url::form_urlencoded::byte_serialize(url.as_bytes()).collect::<String>()
    );
    let html = fetch_raw_html(&api_url).await?;
    if html.trim_start().starts_with('{') {
        anyhow::bail!(
            "spec-generator returned error: {}",
            &html[..html.len().min(200)]
        );
    }
    Ok(html)
}

pub(crate) async fn fetch_raw_html(url: &str) -> Result<String> {
    let client = reqwest::Client::new();
    let response = client
        .get(url)
        .header(
            "User-Agent",
            concat!("webspec-index/", env!("CARGO_PKG_VERSION")),
        )
        .send()
        .await?;
    if !response.status().is_success() {
        anyhow::bail!("Failed to fetch {}: HTTP {}", url, response.status());
    }
    Ok(response.text().await?)
}

async fn fetch_live_html(base_url: &str) -> Result<String> {
    let url = if base_url.ends_with(".html") || base_url.ends_with(".txt") {
        base_url.to_string()
    } else {
        format!("{}/", base_url.trim_end_matches('/'))
    };
    let html = fetch_raw_html(&url).await?;

    if is_respec_source(&html) {
        eprintln!(
            "note: {} is a live ReSpec document; rendering via W3C spec-generator",
            url
        );
        match render_via_spec_generator(&url).await {
            Ok(rendered) => return Ok(rendered),
            Err(e) => eprintln!("warning: spec-generator failed ({}), using raw HTML", e),
        }
    }

    Ok(html)
}

/// Outcome of a live-fetch attempt during a sync.
enum FetchOutcome {
    /// Fresh HTML that should be (re)parsed.
    Fresh(String),
    /// The fetch failed but a cached snapshot exists; serve it instead of failing.
    ServeCached(i64),
}

/// Decide how to proceed after attempting a live fetch. On failure, fall back to
/// the cached snapshot only when `allow_fallback` is set (best-effort refresh)
/// and a cached snapshot exists; otherwise propagate the fetch error. A forced
/// refresh disallows the fallback so failures surface instead of silently
/// serving stale data.
fn resolve_fetch(
    fetched: Result<String>,
    previous_snapshot_id: Option<i64>,
    allow_fallback: bool,
) -> Result<FetchOutcome> {
    match fetched {
        Ok(html) => Ok(FetchOutcome::Fresh(html)),
        Err(e) => match previous_snapshot_id {
            Some(snapshot_id) if allow_fallback => Ok(FetchOutcome::ServeCached(snapshot_id)),
            _ => Err(e),
        },
    }
}

async fn sync_known_spec(
    conn: &Connection,
    spec_name: &str,
    base_url: &str,
    provider_name: &str,
    force: bool,
) -> Result<(i64, bool)> {
    let spec_id = write::insert_or_get_spec(conn, spec_name, base_url, provider_name)?;
    let previous_snapshot_id = queries::get_snapshot(conn, spec_name)?;
    let state = queries::get_update_check(conn, spec_id)?;
    let now = Utc::now();

    if !force {
        if let (Some(snapshot_id), Some(sync_state)) = (previous_snapshot_id, state.as_ref()) {
            if cache_is_current(sync_state, &now) {
                return Ok((snapshot_id, false));
            }
        }
    }

    // A forced refresh must surface fetch failures; otherwise fall back to the
    // cached snapshot when offline.
    match resolve_fetch(
        fetch_live_html(base_url).await,
        previous_snapshot_id,
        !force,
    )? {
        FetchOutcome::ServeCached(snapshot_id) => Ok((snapshot_id, false)),
        FetchOutcome::Fresh(html) => sync_from_html(
            conn,
            spec_id,
            spec_name,
            base_url,
            provider_name,
            html,
            previous_snapshot_id,
            state,
            &now,
        ),
    }
}

/// Same as [`sync_known_spec`], for ad-hoc URL-based specs whose provider is
/// always "dynamic".
async fn sync_dynamic_spec(
    conn: &Connection,
    spec_name: &str,
    base_url: &str,
    force: bool,
) -> Result<(i64, bool)> {
    sync_known_spec(conn, spec_name, base_url, "dynamic", force).await
}

/// Ensure an ad-hoc URL-based spec is indexed.
///
/// This supports domains accepted by `SpecRegistry::resolve_url()` auto resolution.
pub async fn ensure_indexed_dynamic(
    conn: &Connection,
    spec_name: &str,
    base_url: &str,
) -> Result<i64> {
    let (snapshot_id, _) = sync_dynamic_spec(conn, spec_name, base_url, false).await?;
    Ok(snapshot_id)
}

/// Ensure a spec is indexed and reasonably fresh.
///
/// Uses a 24h freshness window based on `update_checks.last_checked`.
/// When refreshing, fetches live HTML and re-indexes only if content hash changed.
pub async fn ensure_indexed(
    conn: &Connection,
    spec_name: &str,
    base_url: &str,
    provider_name: &str,
) -> Result<i64> {
    let (snapshot_id, _) = sync_known_spec(conn, spec_name, base_url, provider_name, false).await?;
    Ok(snapshot_id)
}

/// Update a spec if needed.
///
/// Returns `Some(snapshot_id)` only when content changed and was re-indexed.
pub async fn update_if_needed(
    conn: &Connection,
    spec_name: &str,
    base_url: &str,
    provider_name: &str,
    force: bool,
) -> Result<Option<i64>> {
    let (snapshot_id, updated) =
        sync_known_spec(conn, spec_name, base_url, provider_name, force).await?;
    Ok(updated.then_some(snapshot_id))
}

/// Update all specs in the registry.
/// Returns vector of (spec_name, Option<snapshot_id>) pairs.
pub async fn update_all_specs(
    conn: &Connection,
    specs: &[(String, String, String)], // (name, base_url, provider)
    force: bool,
) -> Vec<(String, Result<Option<i64>>)> {
    let mut results = Vec::new();

    for (name, base_url, provider) in specs {
        let result = update_if_needed(conn, name, base_url, provider, force).await;
        results.push((name.clone(), result));
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn fixed_now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn state_with(index_version: Option<&str>, last_checked: &str) -> queries::UpdateCheckState {
        queries::UpdateCheckState {
            last_checked: DateTime::parse_from_rfc3339(last_checked)
                .unwrap()
                .with_timezone(&Utc),
            last_indexed: None,
            content_hash: Some("hash".to_string()),
            index_version: index_version.map(str::to_string),
        }
    }

    // On fetch failure, resolve_fetch falls back to a cached snapshot only when
    // fallback is allowed and a snapshot exists; otherwise it propagates the error.
    #[test]
    fn test_resolve_fetch() {
        // Success -> parse the fresh HTML regardless of cache/fallback state.
        match resolve_fetch(Ok("<html></html>".to_string()), None, true).unwrap() {
            FetchOutcome::Fresh(html) => assert_eq!(html, "<html></html>"),
            FetchOutcome::ServeCached(_) => panic!("expected Fresh on success"),
        }

        // Failure, fallback allowed, cached snapshot present -> serve the cache.
        match resolve_fetch(Err(anyhow::anyhow!("offline")), Some(42), true).unwrap() {
            FetchOutcome::ServeCached(id) => assert_eq!(id, 42),
            FetchOutcome::Fresh(_) => panic!("expected ServeCached on failure with cache"),
        }

        // Failure with no cached snapshot -> propagate the error.
        assert!(resolve_fetch(Err(anyhow::anyhow!("offline")), None, true).is_err());

        // Failure with a cache but fallback disallowed (e.g. --force) -> propagate.
        assert!(resolve_fetch(Err(anyhow::anyhow!("offline")), Some(42), false).is_err());
    }

    // The freshness gate must require BOTH a fresh timestamp and a matching
    // index version before serving cached data without a re-sync.
    #[test]
    fn test_cache_is_current() {
        let now = fixed_now();

        // Fresh + current build -> serve cache.
        assert!(cache_is_current(
            &state_with(Some(parse::INDEX_VERSION), "2026-01-01T00:00:00Z"),
            &now
        ));

        // Fresh but produced by an older build (or pre-upgrade NULL) -> re-sync.
        assert!(!cache_is_current(
            &state_with(Some("0.0.0"), "2026-01-01T00:00:00Z"),
            &now
        ));
        assert!(!cache_is_current(
            &state_with(None, "2026-01-01T00:00:00Z"),
            &now
        ));

        // Current build but stale (older than the 24h window) -> re-sync.
        assert!(!cache_is_current(
            &state_with(Some(parse::INDEX_VERSION), "2020-01-01T00:00:00Z"),
            &now
        ));
    }

    // A version change (bumped INDEX_VERSION) must force a re-parse of an already
    // cached spec even when the upstream HTML is byte-for-byte unchanged.
    #[test]
    fn test_sync_reparses_on_version_bump() {
        let conn = db::open_test_db().unwrap();
        let spec_id =
            write::insert_or_get_spec(&conn, "TEST", "https://example.test", "test").unwrap();
        let html = "<h2 id=\"intro\">Intro</h2>".to_string();
        let now = fixed_now();

        // First index: no previous snapshot -> parse.
        let (snap1, updated1) = sync_from_html(
            &conn,
            spec_id,
            "TEST",
            "https://example.test",
            "test",
            html.clone(),
            None,
            None,
            &now,
        )
        .unwrap();
        assert!(updated1, "first index should parse");

        // Second: identical content, state stamped with the current index
        // version -> skip re-parsing.
        let state = queries::get_update_check(&conn, spec_id).unwrap();
        assert_eq!(
            state.as_ref().and_then(|s| s.index_version.as_deref()),
            Some(parse::INDEX_VERSION)
        );
        let (snap2, updated2) = sync_from_html(
            &conn,
            spec_id,
            "TEST",
            "https://example.test",
            "test",
            html.clone(),
            Some(snap1),
            state,
            &now,
        )
        .unwrap();
        assert!(!updated2, "unchanged content + current parser should skip");
        assert_eq!(snap2, snap1);

        // Third: identical content, but the stored index was produced by an older
        // build version -> must re-parse.
        let mut stale = queries::get_update_check(&conn, spec_id).unwrap().unwrap();
        stale.index_version = Some("0.0.0".to_string());
        let (_snap3, updated3) = sync_from_html(
            &conn,
            spec_id,
            "TEST",
            "https://example.test",
            "test",
            html.clone(),
            Some(snap2),
            Some(stale),
            &now,
        )
        .unwrap();
        assert!(
            updated3,
            "index version change should force re-parse despite unchanged content"
        );
    }
}
