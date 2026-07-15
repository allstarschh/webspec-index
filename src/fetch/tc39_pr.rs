// TC39 PR previews.
//
// TC39 has no equivalent of whatpr.org, so we resolve a PR via the GitHub API —
// head repo/SHA for the PR build, `compare` for the merge base — and fetch the
// committed spec HTML from raw.githubusercontent.com on each side.
//
// Which committed file we fetch depends on the repo (see `committed_spec_file`):
//   - Proposals (`tc39/proposal-*`) commit their *built* `index.html` (the
//     ecmarkup output deployed to tc39.es). Caveat: the preview reflects the
//     committed index.html, so a PR that edits the source without rebuilding
//     will preview the stale build.
//   - The standards themselves (ECMA-262, ECMA-402, …) do not commit a built
//     index.html — they commit the ecmarkup *source* `spec.html` and build it
//     in CI. We fetch that source; the parser indexes its `<emu-clause id>`
//     sections directly, and the diff stays self-consistent because both sides
//     are parsed the same way.

use super::github::str_field;
use super::pr::{PrPage, PrResolver, ResolvedPr};
use anyhow::{Context, Result};
use async_trait::async_trait;
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};

/// Characters to percent-encode inside a git ref before embedding it in a URL
/// path. Git refs may contain URL-significant characters (`#`, space, …); these
/// would otherwise split the path/query/fragment. `/` is intentionally left
/// unescaped — it is legal in branch names and GitHub's compare path expects it
/// raw (e.g. `main...user:feature/x`).
const REF_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'#')
    .add(b'?')
    .add(b'%')
    .add(b'"')
    .add(b'<')
    .add(b'>')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'^')
    .add(b'\\')
    .add(b'|');

/// Resolves TC39 proposal PRs via the GitHub API + committed `index.html`.
pub(crate) struct Tc39Resolver;

#[async_trait]
impl PrResolver for Tc39Resolver {
    async fn resolve(
        &self,
        _spec_name: &str,
        base_url: &str,
        pr_number: i64,
    ) -> Result<ResolvedPr> {
        let repo = repo_from_base_url(base_url)?;
        let meta = fetch_pr_meta(&repo, pr_number).await?;
        let merge_base_sha =
            fetch_merge_base(&repo, &meta.base_ref, &meta.head_owner, &meta.head_ref).await?;
        Ok(build_resolved(&repo, &meta, merge_base_sha))
    }
}

/// Build the `ResolvedPr` from resolved metadata: the PR build is the head
/// repo's committed spec file, the merge base is the base repo's at that SHA.
/// Both sides fetch the same filename (chosen from the base repo), since a head
/// fork mirrors the base repo's layout.
fn build_resolved(repo: &str, meta: &PrMeta, merge_base_sha: String) -> ResolvedPr {
    let file = committed_spec_file(repo);
    ResolvedPr {
        head_sha: meta.head_sha.clone(),
        base_html_url: raw_spec_url(repo, &merge_base_sha, file),
        merge_base_sha,
        pages: vec![PrPage {
            page_path: file.to_string(),
            url: raw_spec_url(&meta.head_repo, &meta.head_sha, file),
            diff_url: None,
        }],
    }
}

/// The committed spec file to fetch for a repo. Proposals commit the built
/// `index.html`; the standards (ECMA-262/402, …) commit only the ecmarkup
/// source `spec.html` and build in CI, so there is no committed index.html to
/// fetch there.
fn committed_spec_file(repo: &str) -> &'static str {
    let slug = repo.rsplit('/').next().unwrap_or(repo);
    if slug.starts_with("proposal-") {
        "index.html"
    } else {
        "spec.html"
    }
}

/// PR head/base metadata needed to locate both builds.
#[derive(Debug)]
struct PrMeta {
    /// Head repo `owner/name` (may be a fork).
    head_repo: String,
    /// Full head commit SHA.
    head_sha: String,
    /// Head repo owner login (for the cross-fork `compare` basehead).
    head_owner: String,
    /// Head branch name.
    head_ref: String,
    /// Base branch name.
    base_ref: String,
}

/// Derive the GitHub repo (`tc39/<slug>`) from a proposal base URL such as
/// `https://tc39.es/proposal-defer-import-eval`.
fn repo_from_base_url(base_url: &str) -> Result<String> {
    let url =
        url::Url::parse(base_url).with_context(|| format!("Invalid TC39 base URL: {base_url}"))?;
    let slug = url
        .path_segments()
        .and_then(|mut segs| segs.find(|s| !s.is_empty()))
        .with_context(|| format!("TC39 base URL has no path segment: {base_url}"))?;
    Ok(format!("tc39/{slug}"))
}

/// Raw-content URL of a committed spec file at a given repo and ref.
fn raw_spec_url(repo: &str, sha: &str, file: &str) -> String {
    format!("https://raw.githubusercontent.com/{repo}/{sha}/{file}")
}

async fn fetch_pr_meta(repo: &str, pr_number: i64) -> Result<PrMeta> {
    let url = format!("https://api.github.com/repos/{repo}/pulls/{pr_number}");
    let json = super::github::get_json(&url)
        .await
        .with_context(|| format!("Failed to fetch PR #{pr_number} from {repo}"))?;
    parse_pr_meta(&json)
}

/// Extract the fields we need from a `GET /repos/.../pulls/N` response.
fn parse_pr_meta(json: &serde_json::Value) -> Result<PrMeta> {
    let head = &json["head"];
    Ok(PrMeta {
        head_repo: str_field(&head["repo"]["full_name"], "head.repo.full_name")?,
        head_sha: str_field(&head["sha"], "head.sha")?,
        head_owner: str_field(&head["repo"]["owner"]["login"], "head.repo.owner.login")?,
        head_ref: str_field(&head["ref"], "head.ref")?,
        base_ref: str_field(&json["base"]["ref"], "base.ref")?,
    })
}

/// Find the merge base of the PR via the GitHub `compare` API. Uses the
/// cross-fork `base...owner:branch` basehead so head branches on forks resolve.
async fn fetch_merge_base(
    repo: &str,
    base_ref: &str,
    head_owner: &str,
    head_ref: &str,
) -> Result<String> {
    let url = compare_url(repo, base_ref, head_owner, head_ref);
    let json = super::github::get_json(&url)
        .await
        .with_context(|| format!("Failed to compute merge base for PR on {repo}"))?;
    parse_merge_base(&json)
}

/// Build the GitHub `compare` API URL for the cross-fork basehead
/// `base...owner:branch`. Ref components are percent-encoded individually so a
/// URL-significant character in a branch name can't break the request; the
/// `...`/`:` separators stay literal.
fn compare_url(repo: &str, base_ref: &str, head_owner: &str, head_ref: &str) -> String {
    let enc = |s: &str| utf8_percent_encode(s, REF_ENCODE_SET).to_string();
    let basehead = format!("{}...{}:{}", enc(base_ref), enc(head_owner), enc(head_ref));
    format!("https://api.github.com/repos/{repo}/compare/{basehead}")
}

/// Extract the merge base SHA from a `GET /repos/.../compare/...` response.
fn parse_merge_base(json: &serde_json::Value) -> Result<String> {
    str_field(&json["merge_base_commit"]["sha"], "merge_base_commit.sha")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_from_proposal_url() {
        assert_eq!(
            repo_from_base_url("https://tc39.es/proposal-defer-import-eval").unwrap(),
            "tc39/proposal-defer-import-eval"
        );
        // Trailing slash tolerated.
        assert_eq!(
            repo_from_base_url("https://tc39.es/proposal-defer-import-eval/").unwrap(),
            "tc39/proposal-defer-import-eval"
        );
        assert_eq!(
            repo_from_base_url("https://tc39.es/ecma262").unwrap(),
            "tc39/ecma262"
        );
    }

    #[test]
    fn raw_spec_url_format() {
        assert_eq!(
            raw_spec_url("tc39/proposal-x", "abc123", "index.html"),
            "https://raw.githubusercontent.com/tc39/proposal-x/abc123/index.html"
        );
        assert_eq!(
            raw_spec_url("tc39/ecma262", "abc123", "spec.html"),
            "https://raw.githubusercontent.com/tc39/ecma262/abc123/spec.html"
        );
    }

    #[test]
    fn committed_spec_file_by_repo() {
        // Proposals commit the built index.html.
        assert_eq!(
            committed_spec_file("tc39/proposal-defer-import-eval"),
            "index.html"
        );
        assert_eq!(committed_spec_file("fork/proposal-x"), "index.html");
        // Standards commit only the ecmarkup source spec.html.
        assert_eq!(committed_spec_file("tc39/ecma262"), "spec.html");
        assert_eq!(committed_spec_file("tc39/ecma402"), "spec.html");
    }

    /// Shape of a `GET /repos/tc39/.../pulls/N` response, modeled on PR #85
    /// (a head branch on a fork). Only the fields we read are populated.
    fn sample_pr_json() -> serde_json::Value {
        serde_json::json!({
            "head": {
                "sha": "9f07ab45af56350d49a256fb04308b705f17ebe7",
                "ref": "fix-issue-84",
                "repo": {
                    "full_name": "caiolima/proposal-defer-import-eval",
                    "owner": { "login": "caiolima" }
                }
            },
            "base": { "ref": "main" }
        })
    }

    #[test]
    fn parse_pr_meta_extracts_fields() {
        let meta = parse_pr_meta(&sample_pr_json()).unwrap();
        assert_eq!(meta.head_repo, "caiolima/proposal-defer-import-eval");
        assert_eq!(meta.head_sha, "9f07ab45af56350d49a256fb04308b705f17ebe7");
        assert_eq!(meta.head_owner, "caiolima");
        assert_eq!(meta.head_ref, "fix-issue-84");
        assert_eq!(meta.base_ref, "main");
    }

    #[test]
    fn parse_pr_meta_missing_head_repo_errors() {
        // A deleted fork leaves head.repo null; surface a clear error rather
        // than panicking.
        let json = serde_json::json!({
            "head": { "sha": "abc", "ref": "x", "repo": null },
            "base": { "ref": "main" }
        });
        let err = parse_pr_meta(&json).unwrap_err().to_string();
        assert!(
            err.contains("head.repo.full_name"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn compare_url_percent_encodes_refs() {
        // Ordinary refs pass through unchanged.
        assert_eq!(
            compare_url("tc39/proposal-x", "main", "caiolima", "fix-issue-84"),
            "https://api.github.com/repos/tc39/proposal-x/compare/main...caiolima:fix-issue-84"
        );
        // '#' and space are escaped; '/' in a branch name is preserved.
        assert_eq!(
            compare_url("tc39/x", "main", "user", "fix/issue #84"),
            "https://api.github.com/repos/tc39/x/compare/main...user:fix/issue%20%2384"
        );
    }

    #[test]
    fn parse_merge_base_extracts_sha() {
        let json = serde_json::json!({
            "merge_base_commit": { "sha": "012cc74e4a8fa27ed017e7579e3087ac1278387b" }
        });
        assert_eq!(
            parse_merge_base(&json).unwrap(),
            "012cc74e4a8fa27ed017e7579e3087ac1278387b"
        );
    }

    #[test]
    fn build_resolved_maps_metadata_to_urls() {
        let meta = parse_pr_meta(&sample_pr_json()).unwrap();
        let resolved = build_resolved(
            "tc39/proposal-defer-import-eval",
            &meta,
            "012cc74e4a8fa27ed017e7579e3087ac1278387b".to_string(),
        );

        assert_eq!(resolved.head_sha, meta.head_sha);
        assert_eq!(
            resolved.merge_base_sha,
            "012cc74e4a8fa27ed017e7579e3087ac1278387b"
        );
        // Merge base build comes from the base (tc39) repo.
        assert_eq!(
            resolved.base_html_url,
            "https://raw.githubusercontent.com/tc39/proposal-defer-import-eval/012cc74e4a8fa27ed017e7579e3087ac1278387b/index.html"
        );
        // The single PR page is the head (fork) repo's committed index.html.
        assert_eq!(resolved.pages.len(), 1);
        assert_eq!(resolved.pages[0].page_path, "index.html");
        assert_eq!(
            resolved.pages[0].url,
            "https://raw.githubusercontent.com/caiolima/proposal-defer-import-eval/9f07ab45af56350d49a256fb04308b705f17ebe7/index.html"
        );
    }

    #[test]
    fn build_resolved_uses_spec_html_for_ecma262() {
        // ECMA-262 has no committed index.html; both sides fetch the source
        // spec.html instead.
        let meta = PrMeta {
            head_repo: "someuser/ecma262".to_string(),
            head_sha: "aaaa".to_string(),
            head_owner: "someuser".to_string(),
            head_ref: "patch-1".to_string(),
            base_ref: "main".to_string(),
        };
        let resolved = build_resolved("tc39/ecma262", &meta, "bbbb".to_string());

        assert_eq!(
            resolved.base_html_url,
            "https://raw.githubusercontent.com/tc39/ecma262/bbbb/spec.html"
        );
        assert_eq!(resolved.pages[0].page_path, "spec.html");
        assert_eq!(
            resolved.pages[0].url,
            "https://raw.githubusercontent.com/someuser/ecma262/aaaa/spec.html"
        );
    }
}
