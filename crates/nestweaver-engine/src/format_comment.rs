//! PR/MR comment formatting and posting for impact analysis results.
//!
//! Renders impact results as Markdown with a hidden HTML marker for
//! create-or-update semantics (SonarQube pattern). Optionally posts
//! to GitHub PRs or GitLab MRs via their respective APIs.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::atomic_changes::{ImpactResult, ImpactSeverity};

/// Maximum number of impacted symbols to show in the comment.
const MAX_IMPACTS: usize = 50;

/// Target character limit for the rendered comment (below GitHub's 65,536 limit).
const TARGET_CHAR_LIMIT: usize = 50_000;

/// Hard character limit — if still over after progressive truncation.
const HARD_CHAR_LIMIT: usize = 65_000;

/// Configuration for comment formatting.
pub struct FormatConfig {
    pub marker: String,
    pub artifact_url: Option<String>,
}

impl Default for FormatConfig {
    fn default() -> Self {
        Self {
            marker: "nestweaver-impact".to_string(),
            artifact_url: None,
        }
    }
}

/// Input structure matching the JSON output of `pre-push-impact --format json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImpactReport {
    pub changes: Option<usize>,
    pub impacts: Vec<ImpactResult>,
    #[serde(default)]
    pub total_impacted_files: Option<usize>,
    #[serde(default)]
    pub total_impacted_repos: Option<usize>,
    #[serde(default)]
    pub error: Option<String>,
}

/// A group of impacts for a single changed symbol.
struct ImpactGroup {
    symbol_name: String,
    change_kind: String,
    severity: ImpactSeverity,
    callers: Vec<ImpactResult>,
}

/// Render impact analysis results as a Markdown PR comment.
///
/// Includes a hidden HTML marker for create-or-update dedup, a severity
/// summary table, and collapsible details per changed symbol.
pub fn render_impact_markdown(impacts: &[ImpactResult], config: &FormatConfig) -> String {
    if impacts.is_empty() {
        return render_clean_pr(&config.marker);
    }

    // Group impacts by changed symbol (change_canonical_id)
    let groups = group_impacts(impacts);

    // Sort groups: BREAKING first, then WARNING, then INFO; within same severity by caller count desc
    let mut sorted_groups: Vec<ImpactGroup> = groups.into_values().collect();
    sorted_groups.sort_by(|a, b| {
        severity_ord(&b.severity)
            .cmp(&severity_ord(&a.severity))
            .then_with(|| b.callers.len().cmp(&a.callers.len()))
    });

    let total_groups = sorted_groups.len();
    let truncated = total_groups > MAX_IMPACTS;

    // Take top N
    if truncated {
        sorted_groups.truncate(MAX_IMPACTS);
    }

    let mut md = format!(
        "<!-- {} -->\n## NestWeaver Impact Analysis\n\n",
        config.marker
    );

    // Summary table
    let breaking_count = impacts
        .iter()
        .filter(|i| i.severity == ImpactSeverity::Breaking)
        .count();
    let warning_count = impacts
        .iter()
        .filter(|i| i.severity == ImpactSeverity::Warning)
        .count();
    let info_count = impacts
        .iter()
        .filter(|i| i.severity == ImpactSeverity::Info)
        .count();

    let _repos: std::collections::HashSet<&str> = impacts
        .iter()
        .filter(|i| !i.affected_repo_url.is_empty())
        .map(|i| extract_repo_name(&i.affected_repo_url))
        .collect();

    md.push_str("| Severity | Count | Repos Affected |\n");
    md.push_str("|----------|-------|----------------|\n");

    if breaking_count > 0 {
        let mut breaking_repos: Vec<&str> = impacts
            .iter()
            .filter(|i| i.severity == ImpactSeverity::Breaking && !i.affected_repo_url.is_empty())
            .map(|i| extract_repo_name(&i.affected_repo_url))
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        breaking_repos.sort_unstable();
        md.push_str(&format!(
            "| BREAKING | {} | {} |\n",
            breaking_count,
            breaking_repos.join(", ")
        ));
    }
    if warning_count > 0 {
        let mut warning_repos: Vec<&str> = impacts
            .iter()
            .filter(|i| i.severity == ImpactSeverity::Warning && !i.affected_repo_url.is_empty())
            .map(|i| extract_repo_name(&i.affected_repo_url))
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        warning_repos.sort_unstable();
        md.push_str(&format!(
            "| WARNING | {} | {} |\n",
            warning_count,
            warning_repos.join(", ")
        ));
    }
    if info_count > 0 {
        let mut info_repos: Vec<&str> = impacts
            .iter()
            .filter(|i| i.severity == ImpactSeverity::Info && !i.affected_repo_url.is_empty())
            .map(|i| extract_repo_name(&i.affected_repo_url))
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        info_repos.sort_unstable();
        md.push_str(&format!(
            "| INFO | {} | {} |\n",
            info_count,
            info_repos.join(", ")
        ));
    }

    md.push('\n');

    // Per-symbol collapsible details
    for group in sorted_groups.iter() {
        let severity_label = match group.severity {
            ImpactSeverity::Breaking => "BREAKING",
            ImpactSeverity::Warning => "WARNING",
            ImpactSeverity::Info => "INFO",
        };

        md.push_str(&format!(
            "<details>\n<summary>{}: <code>{}</code> — {} ({} caller{})</summary>\n\n",
            severity_label,
            group.symbol_name,
            format_change_kind(&group.change_kind),
            group.callers.len(),
            if group.callers.len() == 1 { "" } else { "s" }
        ));

        md.push_str("| Caller | File | Line | Issue |\n");
        md.push_str("|--------|------|------|-------|\n");

        for caller in &group.callers {
            let repo_name = extract_repo_name(&caller.affected_repo_url);
            md.push_str(&format!(
                "| `{}` | `{}` | {} | {} |\n",
                if repo_name.is_empty() {
                    &caller.affected_name
                } else {
                    repo_name
                },
                caller.affected_file,
                caller.affected_line,
                caller.reason
            ));
        }

        md.push_str("\n</details>\n\n");
    }

    // Truncation notice
    if truncated {
        md.push_str("---\n");
        if let Some(url) = &config.artifact_url {
            md.push_str(&format!(
                "**Showing top {} of {} impacted symbols.** [View full report]({})\n",
                MAX_IMPACTS, total_groups, url
            ));
        } else {
            md.push_str(&format!(
                "**Showing top {} of {} impacted symbols.** See full report artifact for details.\n",
                MAX_IMPACTS, total_groups
            ));
        }
    }

    // Character safety check
    enforce_char_limit(&mut md, total_groups, config);

    md
}

/// Render a full impact report, including top-level error metadata.
pub fn render_impact_report_markdown(report: &ImpactReport, config: &FormatConfig) -> String {
    if let Some(error) = report.error.as_deref() {
        return render_error_report(error, config);
    }

    render_impact_markdown(&report.impacts, config)
}

/// Render the "clean PR" comment (no impacts detected).
fn render_clean_pr(marker: &str) -> String {
    format!(
        "<!-- {} -->\n## NestWeaver Impact Analysis\n\nNo cross-repo impact detected. Changes are contained to this repository.\n",
        marker
    )
}

fn render_error_report(error: &str, config: &FormatConfig) -> String {
    let mut md = format!(
        "<!-- {} -->\n## NestWeaver Impact Analysis\n\n",
        config.marker
    );
    md.push_str("Impact analysis did not complete.\n\n");
    md.push_str(&format!("**Error:** `{}`\n\n", error));
    md.push_str(
        "No production-readiness conclusion was made from this run. Retry when the NestWeaver server is reachable.",
    );
    md
}

/// Enforce GitHub's character limit by progressively removing detail blocks.
fn enforce_char_limit(md: &mut String, _total_groups: usize, _config: &FormatConfig) {
    if md.len() <= TARGET_CHAR_LIMIT {
        return;
    }

    // Strategy 1: Collapse all details blocks beyond the top 10 into one-liners
    let mut result = String::with_capacity(md.len());

    // Simple approach: split on <details> blocks and keep first 10
    let parts: Vec<&str> = md.split("<details>").collect();
    if parts.len() > 11 {
        // Keep preamble + first 10 detail blocks
        result.push_str(parts[0]);
        for part in &parts[1..=10] {
            result.push_str("<details>");
            result.push_str(part);
        }

        // Summarize the rest
        for part in &parts[11..] {
            // Extract the summary line
            if let Some(start) = part.find("<summary>")
                && let Some(end) = part.find("</summary>")
            {
                let summary = &part[start + 9..end];
                result.push_str(&format!("- {}\n", summary));
            }
        }

        *md = result;
    }

    // Strategy 2: Hard truncation
    if md.len() > HARD_CHAR_LIMIT {
        md.truncate(HARD_CHAR_LIMIT - 20);
        md.push_str("\n... [truncated]\n");
    }
}

/// Group impact results by changed symbol.
fn group_impacts(impacts: &[ImpactResult]) -> HashMap<String, ImpactGroup> {
    let mut groups: HashMap<String, ImpactGroup> = HashMap::new();

    for impact in impacts {
        let key = impact.change_canonical_id.clone();
        let group = groups.entry(key).or_insert_with(|| {
            // Determine the symbol name from the change_kind and affected info
            let symbol_name = extract_symbol_name_from_reason(&impact.reason)
                .unwrap_or_else(|| impact.affected_name.clone());
            ImpactGroup {
                symbol_name,
                change_kind: impact.change_kind.clone(),
                severity: impact.severity,
                callers: Vec::new(),
            }
        });

        // Promote severity: if any caller is BREAKING, the group is BREAKING
        if severity_ord(&impact.severity) > severity_ord(&group.severity) {
            group.severity = impact.severity;
        }

        group.callers.push(impact.clone());
    }

    groups
}

fn severity_ord(s: &ImpactSeverity) -> u8 {
    match s {
        ImpactSeverity::Breaking => 2,
        ImpactSeverity::Warning => 1,
        ImpactSeverity::Info => 0,
    }
}

fn extract_symbol_name_from_reason(reason: &str) -> Option<String> {
    // Try to extract function name from patterns like "foo(): parameter count changed"
    if let Some(idx) = reason.find("()") {
        let name = reason[..idx].trim();
        if !name.is_empty() && !name.contains(' ') {
            return Some(name.to_string());
        }
    }
    // Try pattern "'name' was removed"
    if let Some(stripped) = reason.strip_prefix('\'')
        && let Some(end) = stripped.find('\'')
    {
        return Some(stripped[..end].to_string());
    }
    None
}

/// Render a human label for a change_kind. The input is the SCREAMING_SNAKE
/// token the impact emitter (`analyze_impact` / the ImpactAnalysis RPC) writes
/// on the wire — keep these arms in lockstep with the emitted kinds in
/// `atomic_changes.rs`; the contract is exercised by
/// `change_kind_labels_match_emitted_screaming_snake`.
fn format_change_kind(kind: &str) -> &str {
    match kind {
        "SIGNATURE_CHANGED" => "signature changed",
        "SYMBOL_REMOVED" => "removed",
        "EXPORT_REMOVED" => "export removed",
        "SYMBOL_RENAMED" => "renamed",
        "SYMBOL_MOVED" => "moved",
        "SYMBOL_ADDED" => "added",
        "EXPORT_ADDED" => "export added",
        _ => kind,
    }
}

fn extract_repo_name(url: &str) -> &str {
    url.rsplit('/')
        .next()
        .unwrap_or(url)
        .trim_end_matches(".git")
}

/// Percent-encode a path component for URLs (simple implementation).
fn percent_encode_path(s: &str) -> String {
    let mut result = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(b as char);
            }
            _ => {
                result.push_str(&format!("%{:02X}", b));
            }
        }
    }
    result
}

/// Configuration for posting a comment to GitHub.
pub struct GitHubCommentConfig {
    pub owner: String,
    pub repo: String,
    pub pr_number: u64,
    pub marker: String,
}

/// Configuration for posting a comment to GitLab.
pub struct GitLabCommentConfig {
    pub project_id: String,
    pub mr_iid: u64,
    pub token: String,
    pub api_url: String,
    pub marker: String,
}

/// Post or update a comment on a GitHub PR.
///
/// Uses the `GITHUB_TOKEN` env var for authentication. Searches existing
/// comments for the hidden marker, then PATCHes if found or POSTs if not.
pub async fn post_github_comment(
    config: &GitHubCommentConfig,
    body: &str,
) -> Result<(), anyhow::Error> {
    let token = std::env::var("GITHUB_TOKEN")
        .or_else(|_| std::env::var("GH_TOKEN"))
        .map_err(|_| {
            anyhow::anyhow!(
                "GITHUB_TOKEN or GH_TOKEN environment variable required for posting PR comments"
            )
        })?;

    let client = reqwest::Client::new();
    let base_url = format!(
        "https://api.github.com/repos/{}/{}/issues/{}/comments",
        config.owner, config.repo, config.pr_number
    );

    // Search existing comments for the marker
    let marker_tag = format!("<!-- {} -->", config.marker);
    let existing_id = find_github_comment(&client, &base_url, &token, &marker_tag).await?;

    if let Some(comment_id) = existing_id {
        // Update existing comment
        let url = format!(
            "https://api.github.com/repos/{}/{}/issues/comments/{}",
            config.owner, config.repo, comment_id
        );
        let resp = client
            .patch(&url)
            .header("Authorization", format!("Bearer {}", token))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "nestweaver-cli")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(&serde_json::json!({ "body": body }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("GitHub API PATCH failed ({}): {}", status, text);
        }
        tracing::info!(comment_id, "updated existing PR comment");
    } else {
        // Create new comment
        let resp = client
            .post(&base_url)
            .header("Authorization", format!("Bearer {}", token))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "nestweaver-cli")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(&serde_json::json!({ "body": body }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("GitHub API POST failed ({}): {}", status, text);
        }
        tracing::info!("created new PR comment");
    }

    Ok(())
}

/// Search GitHub PR comments for one containing the marker string.
async fn find_github_comment(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    marker: &str,
) -> Result<Option<u64>, anyhow::Error> {
    let mut page = 1u32;
    loop {
        let paged_url = format!("{}?per_page=100&page={}", url, page);
        let resp = client
            .get(&paged_url)
            .header("Authorization", format!("Bearer {}", token))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "nestweaver-cli")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await?;

        if !resp.status().is_success() {
            // If we can't list comments, just create a new one
            return Ok(None);
        }

        let comments: Vec<serde_json::Value> = resp.json().await?;
        if comments.is_empty() {
            break;
        }

        for comment in &comments {
            if let Some(body) = comment.get("body").and_then(|b| b.as_str())
                && body.contains(marker)
                && let Some(id) = comment.get("id").and_then(|i| i.as_u64())
            {
                return Ok(Some(id));
            }
        }

        page += 1;
        if page > 10 {
            break; // Safety limit
        }
    }

    Ok(None)
}

/// Post or update a comment on a GitLab MR.
///
/// Uses the GitLab Notes API with the provided token. Searches existing
/// notes for the hidden marker, then PUTs if found or POSTs if not.
pub async fn post_gitlab_comment(
    config: &GitLabCommentConfig,
    body: &str,
) -> Result<(), anyhow::Error> {
    let client = reqwest::Client::new();
    let base_url = format!(
        "{}/projects/{}/merge_requests/{}/notes",
        config.api_url,
        percent_encode_path(&config.project_id),
        config.mr_iid
    );

    let marker_tag = format!("<!-- {} -->", config.marker);

    // Search existing notes for the marker
    let existing_id = find_gitlab_note(&client, &base_url, &config.token, &marker_tag).await?;

    if let Some(note_id) = existing_id {
        let url = format!("{}/{}", base_url, note_id);
        let resp = client
            .put(&url)
            .header("PRIVATE-TOKEN", &config.token)
            .json(&serde_json::json!({ "body": body }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("GitLab API PUT failed ({}): {}", status, text);
        }
        tracing::info!(note_id, "updated existing MR note");
    } else {
        let resp = client
            .post(&base_url)
            .header("PRIVATE-TOKEN", &config.token)
            .json(&serde_json::json!({ "body": body }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("GitLab API POST failed ({}): {}", status, text);
        }
        tracing::info!("created new MR note");
    }

    Ok(())
}

/// Search GitLab MR notes for one containing the marker string.
async fn find_gitlab_note(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    marker: &str,
) -> Result<Option<u64>, anyhow::Error> {
    let mut page = 1u32;
    loop {
        let paged_url = format!("{}?per_page=100&page={}", url, page);
        let resp = client
            .get(&paged_url)
            .header("PRIVATE-TOKEN", token)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Ok(None);
        }

        let notes: Vec<serde_json::Value> = resp.json().await?;
        if notes.is_empty() {
            break;
        }

        for note in &notes {
            if let Some(body) = note.get("body").and_then(|b| b.as_str())
                && body.contains(marker)
                && let Some(id) = note.get("id").and_then(|i| i.as_u64())
            {
                return Ok(Some(id));
            }
        }

        page += 1;
        if page > 10 {
            break;
        }
    }

    Ok(None)
}

/// Read an impact report from a JSON file (or stdin if path is "-").
pub fn read_impact_report(path: &Path) -> Result<ImpactReport, anyhow::Error> {
    let content = if path.as_os_str() == "-" {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        buf
    } else {
        std::fs::read_to_string(path)?
    };

    let report: ImpactReport = serde_json::from_str(&content)?;
    Ok(report)
}

/// A single GitLab Code Quality (CodeClimate-format) entry.
#[derive(serde::Serialize)]
struct CodeQualityEntry {
    description: String,
    check_name: String,
    fingerprint: String,
    severity: String,
    location: CodeQualityLocation,
}

#[derive(serde::Serialize)]
struct CodeQualityLocation {
    path: String,
    lines: CodeQualityLines,
}

#[derive(serde::Serialize)]
struct CodeQualityLines {
    begin: u64,
}

/// Render impact results as a GitLab Code Quality (CodeClimate) JSON array for
/// `artifacts.reports.codequality` (MR-widget annotations). Severity maps
/// Breaking→critical, Warning→major, Info→info. The `fingerprint` is a stable
/// blake3 hash of `(affected_canonical_id, change_canonical_id, affected_file)`
/// so GitLab dedups/tracks the same finding across commits. Paths are normalized
/// repo-relative and `lines.begin` is clamped to ≥ 1 — GitLab silently drops
/// entries with `./`/absolute paths or `begin < 1`. Output is deterministic
/// (fixed struct field order, input order preserved) — byte-identical on re-run.
pub fn render_codequality_json(impacts: &[ImpactResult]) -> String {
    let entries: Vec<CodeQualityEntry> = impacts
        .iter()
        .map(|i| {
            let severity = match i.severity {
                ImpactSeverity::Breaking => "critical",
                ImpactSeverity::Warning => "major",
                ImpactSeverity::Info => "info",
            }
            .to_string();
            let path = i
                .affected_file
                .trim_start_matches("./")
                .trim_start_matches('/')
                .to_string();
            let key = format!(
                "{}\u{0}{}\u{0}{}",
                i.affected_canonical_id, i.change_canonical_id, i.affected_file
            );
            let fingerprint = blake3::hash(key.as_bytes()).to_hex().to_string();
            CodeQualityEntry {
                description: format!("{}: {}", i.affected_name, i.reason),
                check_name: "nestweaver/impact".to_string(),
                fingerprint,
                severity,
                location: CodeQualityLocation {
                    path,
                    lines: CodeQualityLines {
                        begin: (i.affected_line as u64).max(1),
                    },
                },
            }
        })
        .collect();
    serde_json::to_string(&entries).unwrap_or_else(|_| "[]".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_impact(severity: ImpactSeverity, name: &str, repo: &str) -> ImpactResult {
        ImpactResult {
            change_canonical_id: format!("change_{}", name),
            change_kind: "SIGNATURE_CHANGED".to_string(),
            affected_canonical_id: format!("affected_{}", name),
            affected_name: format!("caller_of_{}", name),
            affected_repo_url: format!("https://github.com/org/{}", repo),
            affected_file: format!("src/{}.rs", name),
            affected_line: 42,
            affected_signature: format!("fn {}()", name),
            severity,
            reason: format!("{}(): parameter count changed (2 -> 3)", name),
        }
    }

    #[test]
    fn render_codequality_json_schema_and_determinism() {
        let impacts = vec![
            make_impact(ImpactSeverity::Breaking, "alpha", "svc-a"),
            make_impact(ImpactSeverity::Warning, "beta", "svc-b"),
            make_impact(ImpactSeverity::Info, "gamma", "svc-c"),
        ];
        let out = render_codequality_json(&impacts);

        assert!(!out.starts_with('\u{feff}'), "must not emit a BOM");
        assert_eq!(
            out,
            render_codequality_json(&impacts),
            "must be byte-identical on repeat"
        );

        let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        let arr = v.as_array().expect("top-level array");
        assert_eq!(arr.len(), 3);

        let sev: Vec<&str> = arr
            .iter()
            .map(|e| e["severity"].as_str().unwrap())
            .collect();
        assert_eq!(
            sev,
            ["critical", "major", "info"],
            "Breaking->critical, Warning->major, Info->info"
        );

        for e in arr {
            assert_eq!(e["check_name"], "nestweaver/impact");
            assert!(!e["description"].as_str().unwrap().is_empty());
            let path = e["location"]["path"].as_str().unwrap();
            assert!(
                !path.starts_with("./") && !path.starts_with('/'),
                "location.path must be repo-relative, got {path:?}"
            );
            assert!(
                e["location"]["lines"]["begin"].as_u64().unwrap() >= 1,
                "lines.begin must be >= 1"
            );
            let fp = e["fingerprint"].as_str().unwrap();
            assert!(
                !fp.is_empty() && fp.chars().all(|c| c.is_ascii_hexdigit()),
                "fingerprint must be hex, got {fp:?}"
            );
        }
    }

    #[test]
    fn change_kind_labels_match_emitted_screaming_snake() {
        // The impact emitter (`analyze_impact` / the ImpactAnalysis RPC) writes
        // change_kind as SCREAMING_SNAKE on the wire. The formatter must render a
        // human label for every emitted kind; otherwise CI comments leak raw
        // machine tokens like "SIGNATURE_CHANGED". This crosses the emitter↔
        // formatter contract seam that the casing bug slipped through.
        let emitted = [
            "SIGNATURE_CHANGED",
            "SYMBOL_REMOVED",
            "SYMBOL_RENAMED",
            "SYMBOL_MOVED",
            "EXPORT_REMOVED",
            "EXPORT_ADDED",
            "SYMBOL_ADDED",
        ];
        for kind in emitted {
            let label = format_change_kind(kind);
            assert_ne!(
                label, kind,
                "format_change_kind left {kind} as a raw machine token; emitter \
                 and formatter casing disagree"
            );
        }
    }

    #[test]
    fn test_clean_pr_comment() {
        let config = FormatConfig::default();
        let md = render_impact_markdown(&[], &config);
        assert!(md.contains("<!-- nestweaver-impact -->"));
        assert!(md.contains("No cross-repo impact detected"));
    }

    #[test]
    fn test_basic_markdown_rendering() {
        let impacts = vec![
            make_impact(ImpactSeverity::Breaking, "processPayment", "billing"),
            make_impact(ImpactSeverity::Warning, "formatCurrency", "web-client"),
        ];

        let config = FormatConfig::default();
        let md = render_impact_markdown(&impacts, &config);

        assert!(md.contains("<!-- nestweaver-impact -->"));
        assert!(md.contains("NestWeaver Impact Analysis"));
        assert!(md.contains("BREAKING"));
        assert!(md.contains("<details>"));
        assert!(md.contains("processPayment"));
        // The human change-kind label must be rendered, not the raw wire token.
        assert!(md.contains("signature changed"));
        assert!(!md.contains("SIGNATURE_CHANGED"));
    }

    #[test]
    fn test_severity_ordering() {
        let impacts = vec![
            make_impact(ImpactSeverity::Info, "infoFn", "repo1"),
            make_impact(ImpactSeverity::Breaking, "breakFn", "repo2"),
            make_impact(ImpactSeverity::Warning, "warnFn", "repo3"),
        ];

        let config = FormatConfig::default();
        let md = render_impact_markdown(&impacts, &config);

        // BREAKING should appear before WARNING which should appear before INFO
        let breaking_pos = md.find("BREAKING").unwrap();
        let warning_pos = md.find("WARNING").unwrap();
        let info_pos = md.find("INFO").unwrap();

        // In the summary table, BREAKING comes first
        assert!(breaking_pos < warning_pos);
        assert!(warning_pos < info_pos);
    }

    #[test]
    fn test_truncation_with_many_impacts() {
        // Generate 100 unique impact groups
        let mut impacts = Vec::new();
        for i in 0..100 {
            impacts.push(ImpactResult {
                change_canonical_id: format!("change_{}", i),
                change_kind: "SIGNATURE_CHANGED".to_string(),
                affected_canonical_id: format!("affected_{}", i),
                affected_name: format!("caller_{}", i),
                affected_repo_url: format!("https://github.com/org/repo-{}", i % 5),
                affected_file: format!("src/mod_{}.rs", i),
                affected_line: i as u32,
                affected_signature: format!("fn func_{}()", i),
                severity: if i < 10 {
                    ImpactSeverity::Breaking
                } else if i < 30 {
                    ImpactSeverity::Warning
                } else {
                    ImpactSeverity::Info
                },
                reason: format!("func_{}(): parameter count changed (2 -> 3)", i),
            });
        }

        let config = FormatConfig {
            marker: "nestweaver-impact".to_string(),
            artifact_url: Some("https://example.com/run/123".to_string()),
        };
        let md = render_impact_markdown(&impacts, &config);

        // Should have truncation notice
        assert!(md.contains("Showing top 50 of 100 impacted symbols"));
        assert!(md.contains("https://example.com/run/123"));

        // Should be under the hard limit
        assert!(
            md.len() <= HARD_CHAR_LIMIT,
            "Comment is {} chars, exceeds limit",
            md.len()
        );
    }

    #[test]
    fn test_no_truncation_under_50() {
        let impacts = vec![
            make_impact(ImpactSeverity::Breaking, "foo", "repo1"),
            make_impact(ImpactSeverity::Warning, "bar", "repo2"),
        ];

        let config = FormatConfig::default();
        let md = render_impact_markdown(&impacts, &config);

        assert!(!md.contains("Showing top"));
    }

    #[test]
    fn test_large_pr_under_char_limit() {
        // Generate 500 unique groups with verbose reasons
        let mut impacts = Vec::new();
        for i in 0..500 {
            impacts.push(ImpactResult {
                change_canonical_id: format!("change_{}", i),
                change_kind: "SIGNATURE_CHANGED".to_string(),
                affected_canonical_id: format!("affected_{}", i),
                affected_name: format!("caller_of_very_long_function_name_{}", i),
                affected_repo_url: format!(
                    "https://github.com/my-organization/my-very-long-repo-name-{}",
                    i % 20
                ),
                affected_file: format!(
                    "src/deeply/nested/directory/structure/module_{}.rs",
                    i
                ),
                affected_line: i as u32,
                affected_signature: format!(
                    "fn very_long_function_name_{}(param1: String, param2: i32, param3: Option<Vec<HashMap<String, Value>>>)",
                    i
                ),
                severity: ImpactSeverity::Warning,
                reason: format!(
                    "very_long_function_name_{}(): parameter count changed from 3 to 5 — existing callers pass 3 arguments but function now requires 5",
                    i
                ),
            });
        }

        let config = FormatConfig::default();
        let md = render_impact_markdown(&impacts, &config);

        assert!(
            md.len() <= HARD_CHAR_LIMIT,
            "Comment is {} chars, exceeds {}",
            md.len(),
            HARD_CHAR_LIMIT
        );
    }

    #[test]
    fn test_extract_repo_name() {
        assert_eq!(
            extract_repo_name("https://github.com/org/my-repo.git"),
            "my-repo"
        );
        assert_eq!(
            extract_repo_name("https://github.com/org/my-repo"),
            "my-repo"
        );
        assert_eq!(extract_repo_name(""), "");
    }

    #[test]
    fn test_read_impact_report_from_json() {
        let json = r#"{"changes": 3, "impacts": [], "total_impacted_files": 0, "total_impacted_repos": 0}"#;
        let tmp_dir = std::env::temp_dir();
        let tmp_file = tmp_dir.join("test_impact_report.json");
        std::fs::write(&tmp_file, json).unwrap();

        let report = read_impact_report(&tmp_file).unwrap();
        assert_eq!(report.impacts.len(), 0);
        assert_eq!(report.changes, Some(3));

        std::fs::remove_file(tmp_file).ok();
    }

    #[test]
    fn test_server_unavailable_report_does_not_render_clean_pr() {
        let report = ImpactReport {
            changes: None,
            impacts: vec![],
            total_impacted_files: None,
            total_impacted_repos: None,
            error: Some("server_unavailable".to_string()),
        };
        let md = render_impact_report_markdown(&report, &FormatConfig::default());

        assert!(md.contains("Impact analysis did not complete"));
        assert!(md.contains("server_unavailable"));
        assert!(!md.contains("No cross-repo impact detected"));
    }
}
