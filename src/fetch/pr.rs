// Provider-agnostic PR-preview orchestration.
//
// Each provider implements `PrResolver` to turn a PR into a `ResolvedPr` of
// concrete fetch URLs (WHATWG via whatpr.org). Everything below — caching,
// merge-base fetch/reuse, page fetching, snapshot storage — is shared and only
// ever fetches raw HTML by URL.

use crate::db::{queries, write};
use crate::model::ParsedSpec;
use crate::parse;
use anyhow::Result;
use async_trait::async_trait;
use rusqlite::Connection;

/// A single preview page to fetch and index.
#[derive(Debug, Clone)]
pub struct PrPage {
    pub page_path: String,
    pub url: String,
    pub diff_url: Option<String>,
}

/// A PR preview resolved to concrete fetch URLs, independent of provider.
pub(crate) struct ResolvedPr {
    /// Native head identifier stored in the PR snapshot key (short SHA for
    /// whatpr.org, full SHA for TC39). Compared with `ends_with` against the
    /// stored key, so either width works.
    pub head_sha: String,
    /// Full merge-base SHA, used as the commit-snapshot cache key.
    pub merge_base_sha: String,
    /// URL of the merge-base build to diff against.
    pub base_html_url: String,
    /// PR build page(s) to fetch and merge.
    pub pages: Vec<PrPage>,
}

/// Resolve a PR to concrete fetch URLs. Implemented per provider so the shared
/// orchestration in `ensure_pr_indexed` stays provider-agnostic.
#[async_trait]
pub(crate) trait PrResolver {
    async fn resolve(&self, spec_name: &str, base_url: &str, pr_number: i64) -> Result<ResolvedPr>;
}

/// Pick the resolver for a provider.
fn resolver_for(provider: &str) -> Result<Box<dyn PrResolver>> {
    match provider {
        "whatwg" => Ok(Box::new(super::whatpr::WhatwgResolver)),
        other => anyhow::bail!(
            "PR previews are not supported for provider '{other}' — only WHATWG specs"
        ),
    }
}

/// Merge multiple ParsedSpec results (from multi-page fetches) into one.
pub fn merge_parsed_specs(specs: Vec<ParsedSpec>) -> ParsedSpec {
    let mut seen_anchors = std::collections::HashSet::new();
    let mut sections = Vec::new();
    let mut references = Vec::new();
    let mut idl_definitions = Vec::new();
    for spec in specs {
        for section in spec.sections {
            if seen_anchors.insert(section.anchor.clone()) {
                sections.push(section);
            }
        }
        references.extend(spec.references);
        idl_definitions.extend(spec.idl_definitions);
    }
    ParsedSpec {
        sections,
        references,
        idl_definitions,
    }
}

/// Fetch and parse every preview page, merging them into one spec.
async fn fetch_pr_pages(
    pages: &[PrPage],
    spec_name: &str,
    base_url: &str,
    pr_number: i64,
) -> Result<ParsedSpec> {
    let mut parsed_pages = Vec::new();
    for page in pages {
        eprintln!("Fetching PR #{pr_number} page: {}", page.page_path);
        let html = super::fetch_raw_html(&page.url).await?;
        let parsed = parse::parse_spec(&html, spec_name, base_url)?;
        parsed_pages.push(parsed);
    }
    Ok(merge_parsed_specs(parsed_pages))
}

/// Whether a snapshot was produced by the current build. An `INDEX_VERSION`
/// bump makes cached snapshots (including reused merge bases) stale, so they
/// must be re-fetched and re-parsed. A legacy integer value written by an
/// earlier build fails the text read and is treated as stale.
fn snapshot_index_is_current(conn: &Connection, snapshot_id: i64) -> bool {
    conn.query_row(
        "SELECT index_version FROM snapshots WHERE id = ?1",
        [snapshot_id],
        |row| row.get::<_, Option<String>>(0),
    )
    .map(|v| v.as_deref() == Some(parse::INDEX_VERSION))
    .unwrap_or(false)
}

fn is_pr_snapshot_valid(conn: &Connection, snapshot_id: i64) -> bool {
    if !snapshot_index_is_current(conn, snapshot_id) {
        return false;
    }
    conn.query_row(
        "SELECT COUNT(*) FROM sections WHERE snapshot_id = ?1",
        [snapshot_id],
        |row| row.get::<_, i64>(0),
    )
    .map(|count| count > 0)
    .unwrap_or(false)
}

/// Ensure a PR snapshot is indexed and fresh.
///
/// Returns (pr_snapshot_id, merge_base_snapshot_id). Resolves the PR through the
/// matching provider's `PrResolver`, then applies shared caching and storage.
///
/// If the PR is already indexed with the same head SHA (and was indexed within
/// the last 24h when `force` is false), returns cached IDs without hitting the
/// network.
pub async fn ensure_pr_indexed(
    conn: &Connection,
    spec_name: &str,
    base_url: &str,
    provider: &str,
    pr_number: i64,
    force: bool,
) -> Result<(i64, i64)> {
    let spec_id = write::insert_or_get_spec(conn, spec_name, base_url, provider)?;

    // Fast path: if not forcing, check 24h freshness before hitting the network.
    if !force {
        if let Some((pr_snap_id, stored_base_sha)) =
            queries::get_pr_snapshot(conn, spec_name, pr_number)?
        {
            if is_pr_snapshot_valid(conn, pr_snap_id) {
                let indexed_at: String = conn.query_row(
                    "SELECT indexed_at FROM snapshots WHERE id = ?1",
                    [pr_snap_id],
                    |row| row.get(0),
                )?;
                if let Ok(indexed) = chrono::DateTime::parse_from_rfc3339(&indexed_at) {
                    let indexed_utc = indexed.with_timezone(&chrono::Utc);
                    if super::is_fresh(&indexed_utc, &chrono::Utc::now()) {
                        if let Some(base_snap_id) =
                            queries::get_commit_snapshot(conn, spec_id, &stored_base_sha)?
                        {
                            return Ok((pr_snap_id, base_snap_id));
                        }
                    }
                }
            }
        }
    }

    // Resolve the PR to concrete fetch URLs via its provider.
    let resolved = resolver_for(provider)?
        .resolve(spec_name, base_url, pr_number)
        .await?;

    // Check if we already have this PR indexed with the same head SHA.
    if let Some((pr_snap_id, stored_base_sha)) =
        queries::get_pr_snapshot(conn, spec_name, pr_number)?
    {
        let pr_sha: String = conn.query_row(
            "SELECT sha FROM snapshots WHERE id = ?1",
            [pr_snap_id],
            |row| row.get(0),
        )?;
        if pr_sha.ends_with(&resolved.head_sha) && is_pr_snapshot_valid(conn, pr_snap_id) {
            // Still current — find the merge base snapshot.
            if let Some(base_snap_id) =
                queries::get_commit_snapshot(conn, spec_id, &stored_base_sha)?
            {
                return Ok((pr_snap_id, base_snap_id));
            }
        }
        // Stale — delete old PR data.
        write::delete_pr_data(conn, spec_id, pr_number)?;
    }

    // Fetch or reuse the merge base snapshot. Reuse it only if the current
    // parser produced it; otherwise re-fetch so it stays consistent with the PR
    // snapshot. The re-fetch happens BEFORE deleting the stale copy so that a
    // fetch failure leaves the existing (usable) base intact rather than
    // destroying it; deleting first also avoids the UNIQUE(spec_id, sha)
    // conflict a plain re-insert would hit.
    let existing_base = queries::get_commit_snapshot(conn, spec_id, &resolved.merge_base_sha)?;
    let base_snap_id = match existing_base {
        Some(id) if snapshot_index_is_current(conn, id) => id,
        maybe_stale => {
            eprintln!(
                "Fetching merge base {}: {}",
                spec_name,
                &resolved.base_html_url[..resolved.base_html_url.len().min(80)]
            );
            let html = super::fetch_raw_html(&resolved.base_html_url).await?;
            let base_parsed = parse::parse_spec(&html, spec_name, base_url)?;
            if let Some(stale_id) = maybe_stale {
                write::delete_commit_snapshot(conn, stale_id)?;
            }
            let commit_date = chrono::Utc::now().to_rfc3339();
            let id = write::insert_snapshot(conn, spec_id, &resolved.merge_base_sha, &commit_date)?;
            write::insert_sections_bulk(conn, id, &base_parsed.sections)?;
            write::insert_refs_bulk(conn, id, &base_parsed.references)?;
            write::insert_idl_defs_bulk(conn, id, &base_parsed.idl_definitions)?;
            id
        }
    };

    // Fetch and parse PR pages.
    let pr_parsed = fetch_pr_pages(&resolved.pages, spec_name, base_url, pr_number).await?;
    let pr_sha = format!("pr:{}:{}", pr_number, resolved.head_sha);
    let commit_date = chrono::Utc::now().to_rfc3339();
    let page_paths: Vec<String> = resolved.pages.iter().map(|p| p.page_path.clone()).collect();
    let pr_snap_id = write::insert_pr_snapshot(
        conn,
        spec_id,
        &pr_sha,
        &commit_date,
        pr_number,
        &resolved.merge_base_sha,
        &page_paths,
    )?;
    write::insert_sections_bulk(conn, pr_snap_id, &pr_parsed.sections)?;
    write::insert_refs_bulk(conn, pr_snap_id, &pr_parsed.references)?;
    write::insert_idl_defs_bulk(conn, pr_snap_id, &pr_parsed.idl_definitions)?;

    Ok((pr_snap_id, base_snap_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_pr_snapshot_not_treated_as_cached() {
        use crate::db;

        let conn = db::open_test_db().unwrap();
        let spec_id =
            write::insert_or_get_spec(&conn, "HTML", "https://html.spec.whatwg.org", "whatwg")
                .unwrap();

        write::insert_pr_snapshot(
            &conn,
            spec_id,
            "pr:99:deadbeef",
            "2026-01-01T00:00:00Z",
            99,
            "basesha",
            &[],
        )
        .unwrap();
        write::insert_snapshot(&conn, spec_id, "basesha", "2026-01-01T00:00:00Z").unwrap();

        let pr_snap_id: i64 = conn
            .query_row(
                "SELECT id FROM snapshots WHERE sha = 'pr:99:deadbeef'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!is_pr_snapshot_valid(&conn, pr_snap_id));
    }

    #[test]
    fn test_pr_snapshot_stale_parser_not_valid() {
        use crate::db;
        use crate::model::{ParsedSection, SectionType};

        let conn = db::open_test_db().unwrap();
        let spec_id =
            write::insert_or_get_spec(&conn, "HTML", "https://html.spec.whatwg.org", "whatwg")
                .unwrap();

        let pr_snap_id = write::insert_pr_snapshot(
            &conn,
            spec_id,
            "pr:99:deadbeef",
            "2026-01-01T00:00:00Z",
            99,
            "basesha",
            &[],
        )
        .unwrap();
        // Give it a section so the emptiness check is satisfied and parser
        // version is the only thing under test.
        write::insert_sections_bulk(
            &conn,
            pr_snap_id,
            &[ParsedSection {
                anchor: "sec-a".into(),
                title: Some("A".into()),
                content_text: None,
                section_type: SectionType::Heading,
                parent_anchor: None,
                prev_anchor: None,
                next_anchor: None,
                depth: Some(2),
            }],
        )
        .unwrap();

        // Freshly inserted -> stamped with the current build -> valid.
        assert!(is_pr_snapshot_valid(&conn, pr_snap_id));

        // Produced by an older build -> stale -> not valid.
        conn.execute(
            "UPDATE snapshots SET index_version = ?1 WHERE id = ?2",
            ("0.0.0", pr_snap_id),
        )
        .unwrap();
        assert!(!is_pr_snapshot_valid(&conn, pr_snap_id));

        // Pre-upgrade rows (NULL index_version) -> not valid either.
        conn.execute(
            "UPDATE snapshots SET index_version = NULL WHERE id = ?1",
            [pr_snap_id],
        )
        .unwrap();
        assert!(!is_pr_snapshot_valid(&conn, pr_snap_id));
    }

    #[test]
    fn test_merge_parsed_specs() {
        use crate::model::{ParsedReference, ParsedSection, ParsedSpec, SectionType};

        let spec_a = ParsedSpec {
            sections: vec![ParsedSection {
                anchor: "sec-a".into(),
                title: Some("A".into()),
                content_text: None,
                section_type: SectionType::Heading,
                parent_anchor: None,
                prev_anchor: None,
                next_anchor: None,
                depth: Some(2),
            }],
            references: vec![],
            idl_definitions: vec![],
        };
        let spec_b = ParsedSpec {
            sections: vec![ParsedSection {
                anchor: "sec-b".into(),
                title: Some("B".into()),
                content_text: None,
                section_type: SectionType::Heading,
                parent_anchor: None,
                prev_anchor: None,
                next_anchor: None,
                depth: Some(2),
            }],
            references: vec![ParsedReference {
                from_anchor: "sec-b".into(),
                to_spec: "DOM".into(),
                to_anchor: "concept-tree".into(),
            }],
            idl_definitions: vec![],
        };

        let merged = merge_parsed_specs(vec![spec_a, spec_b]);
        assert_eq!(merged.sections.len(), 2);
        assert_eq!(merged.references.len(), 1);
    }
}
