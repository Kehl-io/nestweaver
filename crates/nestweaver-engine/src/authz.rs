//! Host-agnostic per-repo authorization core for Blast Radius (R9/R9b).
//!
//! This is the mechanism + enforcement primitive only — no MCP/daemon wiring.
//! It answers one question: *given a caller's identity, which repos may they
//! see, and how do we redact a blast-radius result down to that set?*
//!
//! ## Why key on `repo_uid`
//!
//! Visibility is resolved against NestWeaver's own `repo_uid`, never a host or
//! host-API concept. An operator writes glob patterns against a repo's `url` or
//! `uid` (e.g. `github.com/acme/*`, `*/billing-*`, or a raw repo_uid); those
//! patterns are resolved to concrete `repo_uid`s against the known repo set.
//! Enforcement then works purely in `repo_uid` space, so it is agnostic to
//! whichever forge or API the repos actually came from.
//!
//! ## Fail-closed
//!
//! When a policy is *enabled* (at least one rule exists), an unknown identity
//! resolves to [`VisibleRepos::Only`] over the empty set — the caller sees
//! nothing cross-repo. When no policy is configured the source is *disabled*
//! and everyone resolves to [`VisibleRepos::All`], preserving the historical
//! single-trust-domain behavior (redaction becomes a no-op → zero behavior
//! change).

use std::collections::{HashMap, HashSet};

use nestweaver_schema::Repo;

use crate::blast_radius::BlastRadiusResult;

/// The caller's identity, as resolved by the host before authorization runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identity {
    /// The admin token — sees everything.
    Admin,
    /// A query bearer token (its value is the identity key).
    Token(String),
    /// No/unknown credential.
    Anonymous,
}

/// The repos an identity may see, resolved against the known repo set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VisibleRepos {
    /// No scoping — the caller sees every repo (single-trust-domain default).
    All,
    /// The caller may see only these repo_uids.
    Only(HashSet<String>),
}

impl VisibleRepos {
    /// Whether a symbol/edge in `repo_uid` is visible.
    ///
    /// An empty `repo_uid` (unresolved / the local repo under review) is always
    /// kept — dropping it would break the primary result, and it reveals
    /// nothing cross-repo.
    pub fn allows(&self, repo_uid: &str) -> bool {
        match self {
            VisibleRepos::All => true,
            VisibleRepos::Only(s) => repo_uid.is_empty() || s.contains(repo_uid),
        }
    }
}

/// The permission source (policy decision point).
///
/// Implementations MUST fail closed: an unknown identity under an *enabled*
/// policy resolves to [`VisibleRepos::Only`] over the empty set.
pub trait PermissionSource: Send + Sync {
    /// The repos this identity may see, resolved against the known repo set.
    fn visible_repos(&self, identity: &Identity, repos: &[Repo]) -> VisibleRepos;

    /// Whether any policy is actually configured. A disabled source resolves
    /// every identity to [`VisibleRepos::All`], letting callers skip per-request
    /// work (e.g. listing repos and running redaction) entirely. Defaults to
    /// enabled; disabled sources override.
    fn is_enabled(&self) -> bool {
        true
    }
}

/// Match a single glob pattern against a candidate string.
///
/// Uses the same `globset` matcher the rest of the engine relies on. A pattern
/// that fails to compile matches nothing (an operator typo cannot silently
/// widen visibility).
fn pattern_matches(pattern: &str, candidate: &str) -> bool {
    match globset::Glob::new(pattern) {
        Ok(g) => g.compile_matcher().is_match(candidate),
        Err(_) => false,
    }
}

/// Whether any of `patterns` matches this repo's `url` or `uid`.
fn repo_matches_any(repo: &Repo, patterns: &[String]) -> bool {
    patterns
        .iter()
        .any(|p| pattern_matches(p, &repo.url) || pattern_matches(p, &repo.uid))
}

/// v1 permission source — static, admin-declared, host-agnostic.
///
/// Rules map a query token to a list of repo glob patterns matched against each
/// repo's `url` OR `uid`, resolved to concrete `repo_uid`s against the passed
/// repo set. Empty rules ⇒ disabled ⇒ everyone sees [`VisibleRepos::All`]
/// (backward-compatible single trust domain).
pub struct StaticConfigPermissionSource {
    /// token -> list of repo glob patterns (matched against `Repo.url` and `Repo.uid`).
    rules: HashMap<String, Vec<String>>,
}

impl StaticConfigPermissionSource {
    /// Build a source from a token → glob-patterns map.
    pub fn new(rules: HashMap<String, Vec<String>>) -> Self {
        Self { rules }
    }

    /// Resolve a list of glob patterns to the concrete visible repo_uids.
    fn resolve(&self, patterns: &[String], repos: &[Repo]) -> VisibleRepos {
        let visible: HashSet<String> = repos
            .iter()
            .filter(|r| repo_matches_any(r, patterns))
            .map(|r| r.uid.clone())
            .collect();
        VisibleRepos::Only(visible)
    }
}

impl PermissionSource for StaticConfigPermissionSource {
    /// No rules ⇒ disabled ⇒ everyone is [`VisibleRepos::All`].
    fn is_enabled(&self) -> bool {
        !self.rules.is_empty()
    }

    fn visible_repos(&self, identity: &Identity, repos: &[Repo]) -> VisibleRepos {
        // Disabled (no rules) ⇒ All for everyone (backward-compatible).
        if !self.is_enabled() {
            return VisibleRepos::All;
        }
        match identity {
            Identity::Admin => VisibleRepos::All,
            Identity::Token(t) => match self.rules.get(t) {
                Some(patterns) => self.resolve(patterns, repos),
                // Fail closed: enabled policy + unknown token ⇒ nothing.
                None => VisibleRepos::Only(HashSet::new()),
            },
            Identity::Anonymous => VisibleRepos::Only(HashSet::new()),
        }
    }
}

/// Redact a blast-radius result to the caller's visible repos (R9b leakage
/// suppression, the policy enforcement point).
///
/// Silent by design: naming a hidden repo — even to say it was redacted — would
/// itself leak its existence, so no notification is added. A no-op when
/// `visible` is [`VisibleRepos::All`] (the disabled-policy / single-trust-domain
/// path), which is what preserves zero behavior change for existing callers.
///
/// `repos` is retained in the signature for caller compatibility. Authorization
/// deliberately does not consult display labels: org items carry stable source
/// and destination repo UIDs and legacy/unattributed rows fail closed.
pub fn redact_blast_radius_for_visibility(
    result: &mut BlastRadiusResult,
    visible: &VisibleRepos,
    _repos: &[Repo],
) {
    // All ⇒ nothing is hidden; leave the result byte-for-byte unchanged.
    let visible_only = match visible {
        VisibleRepos::All => return,
        VisibleRepos::Only(_) => visible,
    };

    // Symbols carry their owning repo_uid directly.
    result
        .affected_symbols
        .retain(|s| visible_only.allows(&s.repo_uid));
    result
        .changed_symbols
        .retain(|s| visible_only.allows(&s.repo_uid));
    result.affected_symbol_count = result.affected_symbols.len();

    // Org items reference both a source/change repo and a destination/affected
    // repo. Authorize exclusively by their stable UIDs: display URLs/names may
    // collide and are presentation-only. Legacy items lack one or both UIDs and
    // therefore fail closed under an enabled policy.
    let item_visible = |item: &crate::blast_radius::OrgImpactItem| -> bool {
        !item.change_repo_uid.is_empty()
            && !item.affected_repo_uid.is_empty()
            && visible_only.allows(&item.change_repo_uid)
            && visible_only.allows(&item.affected_repo_uid)
    };

    if let Some(org) = result.org_wide.as_mut() {
        org.breaking.retain(&item_visible);
        org.warnings.retain(&item_visible);
        org.info.retain(&item_visible);

        // Recompute impacted_repos to the surviving visible display labels,
        // de-duplicated and order-preserving.
        let mut seen: HashSet<String> = HashSet::new();
        let mut impacted: Vec<String> = Vec::new();
        for item in org
            .breaking
            .iter()
            .chain(org.warnings.iter())
            .chain(org.info.iter())
        {
            if seen.insert(item.affected_repo.clone()) {
                impacted.push(item.affected_repo.clone());
            }
        }
        org.impacted_repos = impacted;

        // Everything empty ⇒ the org-wide view reveals nothing; drop it.
        if org.breaking.is_empty()
            && org.warnings.is_empty()
            && org.info.is_empty()
            && org.impacted_repos.is_empty()
        {
            result.org_wide = None;
        }
    }

    // Coverage names repos by repo_uid directly.
    result
        .coverage
        .repos_in_scope
        .retain(|uid| visible_only.allows(uid));
    result
        .coverage
        .repos_not_indexed
        .retain(|uid| visible_only.allows(uid));
    result
        .coverage
        .stale_repos
        .retain(|sr| visible_only.allows(&sr.repo_uid));

    // affected_clusters is a graph-wide (potentially cross-repo) aggregate: each
    // cluster's `total_count` is its full size and `name` can be derived from a
    // hidden repo's paths — both leak. We lack the clustering here to re-attribute
    // counts to the visible subset, so we suppress it entirely (fail closed)
    // rather than emit numbers/names that encode hidden-repo membership.
    result.affected_clusters.clear();

    // Co-change rows currently carry paths and cardinalities but no repo UID.
    // Until the sidecar format can prove ownership, fail closed under scoping.
    result.cochanged_files.clear();

    // Notification detail may contain symbol names, paths, or raw store errors.
    // Preserve the stable descriptor and severity for machine handling, retain
    // the detail in server-side logs, and expose only a fixed generic message.
    for notification in &mut result.notifications {
        tracing::debug!(
            descriptor = %notification.descriptor,
            level = ?notification.level,
            detail = %notification.message,
            "redacting blast-radius notification detail for a restricted response"
        );
        notification.message =
            "blast-radius analysis details withheld by repository visibility policy".to_string();
    }

    // Regenerate the human summary from the REDACTED vecs. The baked-in string
    // embedded pre-redaction counts (transitively-affected + clusters), which
    // both disagreed with the redacted `affected_symbols` and leaked the
    // magnitude of hidden-repo impact. Distinct changed files are recomputed from
    // the surviving changed symbols (the caller's own diff — not a leak).
    let changed_files = result
        .changed_symbols
        .iter()
        .map(|s| s.file_path.as_str())
        .collect::<HashSet<&str>>()
        .len();
    result.summary = crate::blast_radius::render_blast_summary(
        result.changed_symbols.len(),
        changed_files,
        result.affected_symbol_count,
        result.affected_clusters.len(),
        result.risk_level,
        result.status,
    );

    // risk_level, gate_state, status, blind_spots, analysis_direction, and
    // coverage.traversal_truncated are intentionally NOT redacted.
    //
    // The gate verdict must reflect the TRUE risk of the change. Recomputing it
    // from only the visible subset could report "safe" while real impact lives in
    // a hidden repo — reintroducing exactly the false-safe (D3) this system
    // exists to prevent. This is a deliberate, documented trade-off: risk_level /
    // gate_state carry a low-bandwidth signal of aggregate hidden risk in
    // exchange for never under-reporting danger. status/blind_spots/direction
    // name no repos; we add no notification that would reveal a hidden repo.
}

/// nw-043: how an enabled authz policy responds to the repo listing.
/// - `Resolve(repos)` → resolve visibility (an empty store legitimately
///   resolves to "nothing visible" — fail closed, unchanged).
/// - `FailLoud(_)`    → the caller must FAIL LOUD (Unavailable), never serve a
///   silently-redacted 200: a transient store error is indistinguishable from
///   the nw-043 isolation anomaly, and silent full redaction reads as a valid
///   empty result.
#[derive(Debug)]
pub enum AuthzRepoListing {
    /// Both/either listing attempt succeeded — resolve visibility against it.
    Resolve(Vec<Repo>),
    /// Both listing attempts errored — the boundary must fail the request.
    FailLoud(String),
}

/// Classify the outcome of the per-request authz repo listing (nw-043).
///
/// Pure policy decision, unit-testable without fault-injecting the store. The
/// `retry` closure is invoked ONLY when the first attempt errored (a single
/// retry to ride out a transient store anomaly); for an `Ok` first attempt it
/// is never called — lazy evaluation is enforced by the `FnOnce` type. On a
/// double failure both errors are logged server-side and the client-facing
/// message stays generic (no store internals leak).
pub fn classify_repo_listing(
    result: Result<Vec<Repo>, nestweaver_store::StoreError>,
    retry: impl FnOnce() -> Result<Vec<Repo>, nestweaver_store::StoreError>,
) -> AuthzRepoListing {
    match result {
        Ok(repos) => AuthzRepoListing::Resolve(repos),
        Err(first) => match retry() {
            Ok(repos) => {
                tracing::error!(
                    "authz: list_repos failed then succeeded on retry (nw-043 anomaly \
                     candidate): first error: {first:#}"
                );
                AuthzRepoListing::Resolve(repos)
            }
            Err(second) => {
                tracing::error!(
                    "authz: repo listing failed twice; first: {first:#}; second: {second:#}"
                );
                AuthzRepoListing::FailLoud("authz repo listing unavailable".to_string())
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blast_radius::{
        AffectedCluster, AffectedSymbol, AnalysisStatus, ChangedSymbol, CoChangedFile, Coverage,
        Notification, NotificationLevel, OrgImpactItem, OrgWideImpact, StaleRepo,
    };
    use crate::process::RiskLevel;

    fn repo(uid: &str, url: &str) -> Repo {
        Repo {
            uid: uid.to_string(),
            url: url.to_string(),
            indexed_sha: String::new(),
            staleness_commits_behind: 0,
            instance_id: String::new(),
            name: None,
            root_path: None,
        }
    }

    fn only(uids: &[&str]) -> VisibleRepos {
        VisibleRepos::Only(uids.iter().map(|s| s.to_string()).collect())
    }

    // --- VisibleRepos::allows ------------------------------------------------

    #[test]
    fn allows_all_permits_everything_including_empty() {
        let v = VisibleRepos::All;
        assert!(v.allows("repo:a"));
        assert!(v.allows("repo:b"));
        assert!(v.allows(""));
    }

    #[test]
    fn allows_only_scopes_but_keeps_empty() {
        let v = only(&["repo:a"]);
        assert!(v.allows("repo:a"));
        assert!(!v.allows("repo:b"));
        // Empty repo_uid (unresolved / local repo under review) is always kept.
        assert!(v.allows(""));
    }

    // --- StaticConfigPermissionSource ---------------------------------------

    #[test]
    fn disabled_source_is_all_for_every_identity() {
        let src = StaticConfigPermissionSource::new(HashMap::new());
        assert!(!src.is_enabled());
        let repos = [repo("repo:a", "github.com/acme/billing")];
        assert_eq!(
            src.visible_repos(&Identity::Admin, &repos),
            VisibleRepos::All
        );
        assert_eq!(
            src.visible_repos(&Identity::Token("t".into()), &repos),
            VisibleRepos::All
        );
        assert_eq!(
            src.visible_repos(&Identity::Anonymous, &repos),
            VisibleRepos::All
        );
    }

    #[test]
    fn enabled_admin_sees_all() {
        let mut rules = HashMap::new();
        rules.insert("t".to_string(), vec!["github.com/acme/*".to_string()]);
        let src = StaticConfigPermissionSource::new(rules);
        assert!(src.is_enabled());
        let repos = [repo("repo:a", "github.com/acme/billing")];
        assert_eq!(
            src.visible_repos(&Identity::Admin, &repos),
            VisibleRepos::All
        );
    }

    #[test]
    fn known_token_resolves_glob_against_url() {
        let mut rules = HashMap::new();
        rules.insert("t".to_string(), vec!["github.com/acme/*".to_string()]);
        let src = StaticConfigPermissionSource::new(rules);
        let repos = [
            repo("repo:a", "github.com/acme/billing"),
            repo("repo:b", "github.com/other/svc"),
        ];
        let v = src.visible_repos(&Identity::Token("t".into()), &repos);
        assert_eq!(v, only(&["repo:a"]));
    }

    #[test]
    fn known_token_resolves_pattern_against_raw_uid() {
        let mut rules = HashMap::new();
        // A raw repo_uid as the pattern matches that repo's uid directly.
        rules.insert("t".to_string(), vec!["repo:b".to_string()]);
        let src = StaticConfigPermissionSource::new(rules);
        let repos = [
            repo("repo:a", "github.com/acme/billing"),
            repo("repo:b", "github.com/other/svc"),
        ];
        let v = src.visible_repos(&Identity::Token("t".into()), &repos);
        assert_eq!(v, only(&["repo:b"]));
    }

    #[test]
    fn unknown_token_fails_closed() {
        let mut rules = HashMap::new();
        rules.insert("known".to_string(), vec!["github.com/acme/*".to_string()]);
        let src = StaticConfigPermissionSource::new(rules);
        let repos = [repo("repo:a", "github.com/acme/billing")];
        let v = src.visible_repos(&Identity::Token("stranger".into()), &repos);
        assert_eq!(v, VisibleRepos::Only(HashSet::new()));
    }

    #[test]
    fn anonymous_fails_closed_when_enabled() {
        let mut rules = HashMap::new();
        rules.insert("t".to_string(), vec!["github.com/acme/*".to_string()]);
        let src = StaticConfigPermissionSource::new(rules);
        let repos = [repo("repo:a", "github.com/acme/billing")];
        let v = src.visible_repos(&Identity::Anonymous, &repos);
        assert_eq!(v, VisibleRepos::Only(HashSet::new()));
    }

    // --- classify_repo_listing (nw-043) --------------------------------------

    fn qerr() -> nestweaver_store::StoreError {
        nestweaver_store::StoreError::Query("boom".to_string())
    }

    #[test]
    fn authz_listing_error_twice_fails_loud() {
        let r = classify_repo_listing(Err(qerr()), || Err(qerr()));
        assert!(matches!(r, AuthzRepoListing::FailLoud(_)));
    }

    #[test]
    fn authz_listing_error_then_success_resolves_with_retry_result() {
        let r = classify_repo_listing(Err(qerr()), || {
            Ok(vec![repo("repo:t:aaaa", "github.com/t/aaaa")])
        });
        match r {
            AuthzRepoListing::Resolve(repos) => assert_eq!(repos.len(), 1),
            _ => panic!("retry success must resolve"),
        }
    }

    #[test]
    fn authz_listing_empty_store_still_fails_closed_quietly() {
        // A genuinely empty store under an enabled policy resolves to Only(∅) —
        // that behavior is intentional and unchanged.
        let r = classify_repo_listing(Ok(vec![]), || Ok(vec![]));
        assert!(matches!(r, AuthzRepoListing::Resolve(v) if v.is_empty()));
    }

    #[test]
    fn authz_listing_ok_first_never_invokes_retry() {
        // If a future edit consults the retry on an Ok first attempt, both
        // boundaries would resolve to a poisoned/empty listing → full silent
        // redaction — the exact bug class this fn exists to kill.
        let r = classify_repo_listing(Ok(vec![repo("repo:t:aaaa", "github.com/t/aaaa")]), || {
            panic!("retry must not be invoked when the first listing succeeded")
        });
        match r {
            AuthzRepoListing::Resolve(repos) => assert_eq!(repos.len(), 1),
            _ => panic!("Ok first attempt must resolve"),
        }
    }

    // --- redact_blast_radius_for_visibility ---------------------------------

    fn changed(uid: &str, repo_uid: &str) -> ChangedSymbol {
        ChangedSymbol {
            uid: uid.to_string(),
            name: uid.to_string(),
            file_path: "f.rs".to_string(),
            kind: "function".to_string(),
            pagerank_score: None,
            repo_uid: repo_uid.to_string(),
        }
    }

    fn affected(uid: &str, repo_uid: &str) -> AffectedSymbol {
        AffectedSymbol {
            uid: uid.to_string(),
            name: uid.to_string(),
            file_path: "f.rs".to_string(),
            kind: "function".to_string(),
            depth: 1,
            edge_type: "calls".to_string(),
            confidence: 1.0,
            start_line: 1,
            impact_score: 1.0,
            repo_uid: repo_uid.to_string(),
        }
    }

    fn org_item(affected_repo: &str) -> OrgImpactItem {
        let affected_repo_uid = match affected_repo {
            "github.com/acme/a" => "repo:a",
            "github.com/acme/b" => "repo:b",
            _ => "",
        };
        OrgImpactItem {
            change_name: "c".to_string(),
            change_kind: "function".to_string(),
            change_repo_uid: "repo:a".to_string(),
            affected_name: "a".to_string(),
            affected_repo_uid: affected_repo_uid.to_string(),
            affected_repo: affected_repo.to_string(),
            affected_file: "f.rs".to_string(),
            affected_line: 1,
            severity: "warning".to_string(),
            reason: "r".to_string(),
        }
    }

    fn org_item_with_uids(
        affected_repo: &str,
        change_repo_uid: &str,
        affected_repo_uid: &str,
    ) -> OrgImpactItem {
        let mut item = org_item(affected_repo);
        item.change_repo_uid = change_repo_uid.to_string();
        item.affected_repo_uid = affected_repo_uid.to_string();
        item
    }

    fn sample_result() -> BlastRadiusResult {
        BlastRadiusResult {
            changed_symbols: vec![
                changed("c:a", "repo:a"),
                changed("c:b", "repo:b"),
                changed("c:local", ""),
            ],
            affected_symbols: vec![
                affected("s:a", "repo:a"),
                affected("s:b", "repo:b"),
                affected("s:local", ""),
            ],
            affected_symbol_count: 3,
            affected_clusters: vec![],
            risk_level: RiskLevel::Medium,
            summary: "s".to_string(),
            org_wide: Some(OrgWideImpact {
                breaking: vec![org_item("github.com/acme/b")],
                warnings: vec![org_item("github.com/acme/a")],
                info: vec![],
                impacted_repos: vec![
                    "github.com/acme/a".to_string(),
                    "github.com/acme/b".to_string(),
                ],
                source_server: "up".to_string(),
            }),
            status: Default::default(),
            notifications: vec![],
            gate_state: Default::default(),
            coverage: Coverage {
                repos_in_scope: vec!["repo:a".to_string(), "repo:b".to_string()],
                repos_not_indexed: vec!["repo:b".to_string()],
                stale_repos: vec![StaleRepo {
                    repo_uid: "repo:b".to_string(),
                    commits_behind: 3,
                }],
                traversal_truncated: false,
            },
            cochanged_files: Vec::new(),
            blind_spots: vec![],
            analysis_direction: String::new(),
        }
    }

    fn sample_repos() -> Vec<Repo> {
        vec![
            repo("repo:a", "github.com/acme/a"),
            repo("repo:b", "github.com/acme/b"),
        ]
    }

    #[test]
    fn redact_all_is_a_no_op() {
        let original = sample_result();
        let mut result = original.clone();
        redact_blast_radius_for_visibility(&mut result, &VisibleRepos::All, &sample_repos());
        // Deep-equal: All must leave the result completely unchanged. Compare
        // by serialized form since BlastRadiusResult is not PartialEq.
        let a = serde_json::to_value(&original).unwrap();
        let b = serde_json::to_value(&result).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn redact_only_drops_hidden_repo_everywhere() {
        let mut result = sample_result();
        redact_blast_radius_for_visibility(&mut result, &only(&["repo:a"]), &sample_repos());

        // repo:b symbols dropped; repo:a and empty-repo_uid symbols kept.
        let changed_repos: Vec<&str> = result
            .changed_symbols
            .iter()
            .map(|s| s.repo_uid.as_str())
            .collect();
        assert_eq!(changed_repos, vec!["repo:a", ""]);
        let affected_repos: Vec<&str> = result
            .affected_symbols
            .iter()
            .map(|s| s.repo_uid.as_str())
            .collect();
        assert_eq!(affected_repos, vec!["repo:a", ""]);

        // org_wide: the repo:b breaking item is gone; the repo:a warning stays.
        let org = result.org_wide.as_ref().expect("org_wide should survive");
        assert!(org.breaking.is_empty());
        assert_eq!(org.warnings.len(), 1);
        assert_eq!(org.warnings[0].affected_repo, "github.com/acme/a");
        assert_eq!(org.impacted_repos, vec!["github.com/acme/a".to_string()]);

        // coverage lists only repo:a.
        assert_eq!(result.coverage.repos_in_scope, vec!["repo:a".to_string()]);
        assert!(result.coverage.repos_not_indexed.is_empty());
        assert!(result.coverage.stale_repos.is_empty());
    }

    #[test]
    fn redact_only_regenerates_summary_and_clears_clusters() {
        let mut result = sample_result();
        // A cluster set and a summary baked from PRE-redaction counts (3 affected,
        // 2 clusters) — both would otherwise leak hidden-repo magnitude/names.
        result.affected_clusters = vec![
            AffectedCluster {
                id: 1,
                name: "acme/b::payments".to_string(),
                affected_count: 3,
                total_count: 42,
                cohesion: 0.9,
            },
            AffectedCluster {
                id: 2,
                name: "acme/a::api".to_string(),
                affected_count: 1,
                total_count: 5,
                cohesion: 0.5,
            },
        ];
        result.summary = "3 changed symbol(s) in 3 file(s), 3 transitively \
             affected symbol(s), 2 cluster(s) touched. Risk: Medium."
            .to_string();

        redact_blast_radius_for_visibility(&mut result, &only(&["repo:a"]), &sample_repos());

        // Clusters are a cross-repo aggregate (total_count and name leak) →
        // suppressed under an Only-policy.
        assert!(
            result.affected_clusters.is_empty(),
            "affected_clusters must be cleared under scoping"
        );
        // Summary regenerated from the REDACTED vecs: repo:a + empty-uid survive
        // ⇒ 2 changed / 2 affected / 0 clusters. It must NOT echo the pre-redaction
        // counts (3 affected, 2 clusters) that leaked hidden-repo impact.
        assert!(
            result.summary.contains("2 changed symbol(s)")
                && result.summary.contains("2 transitively affected symbol(s)")
                && result.summary.contains("0 cluster(s) touched"),
            "summary must reflect redacted counts, got: {}",
            result.summary
        );
        assert!(
            !result.summary.contains("3 transitively affected")
                && !result.summary.contains("2 cluster(s)"),
            "summary must not leak pre-redaction counts, got: {}",
            result.summary
        );
        assert_eq!(result.affected_symbol_count, 2);
    }

    #[test]
    fn redact_only_clears_org_wide_when_it_empties() {
        let mut result = sample_result();
        // Nothing in repo:a's org buckets → visibility that only allows repo:a
        // still leaves a warning, so instead scope to a repo with no org items.
        result.org_wide.as_mut().unwrap().warnings.clear();
        result.org_wide.as_mut().unwrap().breaking = vec![org_item("github.com/acme/b")];
        result.org_wide.as_mut().unwrap().info.clear();
        result.org_wide.as_mut().unwrap().impacted_repos = vec!["github.com/acme/b".to_string()];

        redact_blast_radius_for_visibility(&mut result, &only(&["repo:a"]), &sample_repos());
        // The only org item was for repo:b (hidden) → org_wide collapses to None.
        assert!(result.org_wide.is_none());
    }

    #[test]
    fn redact_unresolvable_org_label_is_dropped() {
        let mut result = sample_result();
        // A label that maps to no known repo cannot be proven visible → dropped.
        result.org_wide.as_mut().unwrap().warnings = vec![org_item("mystery-label")];
        result.org_wide.as_mut().unwrap().breaking.clear();
        redact_blast_radius_for_visibility(&mut result, &only(&["repo:a"]), &sample_repos());
        assert!(result.org_wide.is_none());
    }

    #[test]
    fn redact_only_requires_visible_source_and_destination_uids() {
        let mut result = sample_result();
        let org = result.org_wide.as_mut().unwrap();
        org.breaking.clear();
        org.warnings = vec![org_item_with_uids("github.com/acme/a", "repo:b", "repo:a")];
        org.info.clear();
        org.impacted_repos = vec!["github.com/acme/a".to_string()];

        redact_blast_radius_for_visibility(&mut result, &only(&["repo:a"]), &sample_repos());

        assert!(
            result.org_wide.is_none(),
            "a visible destination must not expose a hidden source"
        );
    }

    #[test]
    fn redact_only_uses_destination_uid_when_repo_urls_collide() {
        let repos = vec![
            repo("repo:hidden", "github.com/acme/shared"),
            repo("repo:visible", "github.com/acme/shared"),
        ];
        let mut result = sample_result();
        let org = result.org_wide.as_mut().unwrap();
        org.breaking.clear();
        org.warnings = vec![org_item_with_uids(
            "github.com/acme/shared",
            "repo:visible",
            "repo:hidden",
        )];
        org.info.clear();
        org.impacted_repos = vec!["github.com/acme/shared".to_string()];

        redact_blast_radius_for_visibility(&mut result, &only(&["repo:visible"]), &repos);

        assert!(
            result.org_wide.is_none(),
            "a duplicate display URL must not authorize the hidden destination"
        );
    }

    #[test]
    fn redact_only_drops_legacy_unattributed_org_items() {
        let mut result = sample_result();
        let mut legacy_value = serde_json::to_value(org_item("github.com/acme/a")).unwrap();
        let legacy_object = legacy_value.as_object_mut().unwrap();
        legacy_object.remove("change_repo_uid");
        legacy_object.remove("affected_repo_uid");
        let legacy_item = serde_json::from_value(legacy_value).unwrap();
        let org = result.org_wide.as_mut().unwrap();
        org.breaking.clear();
        org.warnings = vec![legacy_item];
        org.info.clear();
        org.impacted_repos = vec!["github.com/acme/a".to_string()];

        redact_blast_radius_for_visibility(&mut result, &only(&["repo:a"]), &sample_repos());

        assert!(result.org_wide.is_none());
    }

    #[test]
    fn redact_only_clears_unqualified_cochange_rows() {
        let mut result = sample_result();
        result.cochanged_files = vec![CoChangedFile {
            file: "hidden/private.sql".to_string(),
            coupled_to: "hidden/source.rs".to_string(),
            cochange_count: 8675309,
            confidence: 0.99,
            note: "hidden/private.sql changed with hidden/source.rs 8675309 times".to_string(),
        }];

        redact_blast_radius_for_visibility(&mut result, &only(&["repo:a"]), &sample_repos());

        assert!(result.cochanged_files.is_empty());
    }

    #[test]
    fn redact_only_sanitizes_notification_messages_for_json_and_sarif() {
        let mut result = sample_result();
        result.status = AnalysisStatus::Degraded;
        result.notifications = vec![Notification {
            level: NotificationLevel::Error,
            message: "impact failed for HiddenTarget at hidden/private.rs: raw-store-secret"
                .to_string(),
            descriptor: "store.impact-failed".to_string(),
        }];

        redact_blast_radius_for_visibility(&mut result, &only(&["repo:a"]), &sample_repos());

        let notification = &result.notifications[0];
        assert_eq!(notification.level, NotificationLevel::Error);
        assert_eq!(notification.descriptor, "store.impact-failed");
        assert_eq!(
            notification.message,
            "blast-radius analysis details withheld by repository visibility policy"
        );
        let json = serde_json::to_string(&result).unwrap();
        assert!(!json.contains("HiddenTarget"));
        assert!(!json.contains("hidden/private.rs"));
        assert!(!json.contains("raw-store-secret"));

        let sarif = crate::blast_radius_sarif::blast_radius_to_sarif(&result, "test");
        let sarif_json = serde_json::to_string(&sarif).unwrap();
        assert!(!sarif_json.contains("HiddenTarget"));
        assert!(!sarif_json.contains("hidden/private.rs"));
        assert!(!sarif_json.contains("raw-store-secret"));
        assert_eq!(
            sarif["runs"][0]["invocations"][0]["toolExecutionNotifications"][0]["descriptor"]["id"],
            "store.impact-failed"
        );
        assert_eq!(
            sarif["runs"][0]["invocations"][0]["toolExecutionNotifications"][0]["level"],
            "error"
        );
    }
}
