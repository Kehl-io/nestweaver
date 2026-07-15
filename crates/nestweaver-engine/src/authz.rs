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

    /// Whether any policy is configured. No rules ⇒ disabled ⇒ everyone is
    /// [`VisibleRepos::All`].
    pub fn is_enabled(&self) -> bool {
        !self.rules.is_empty()
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
/// `repos` is required to resolve an [`OrgImpactItem`]'s `affected_repo` — a
/// *display label* (a repo's `url` or `uid`), not a `repo_uid` — back to a
/// concrete `repo_uid` before the visibility check.
pub fn redact_blast_radius_for_visibility(
    result: &mut BlastRadiusResult,
    visible: &VisibleRepos,
    repos: &[Repo],
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

    // Map every display label (url and uid) to its repo_uid so an
    // OrgImpactItem's `affected_repo` label can be resolved before the check.
    let label_to_uid: HashMap<&str, &str> = repos
        .iter()
        .flat_map(|r| {
            [
                (r.url.as_str(), r.uid.as_str()),
                (r.uid.as_str(), r.uid.as_str()),
            ]
        })
        .collect();

    // An org item is visible when its resolved repo_uid is allowed. A label we
    // cannot resolve to a known repo is dropped — under an enabled policy we
    // cannot prove it is visible, so we fail closed rather than leak it.
    let item_visible = |affected_repo: &str| -> bool {
        match label_to_uid.get(affected_repo) {
            Some(uid) => visible_only.allows(uid),
            None => false,
        }
    };

    if let Some(org) = result.org_wide.as_mut() {
        org.breaking.retain(|i| item_visible(&i.affected_repo));
        org.warnings.retain(|i| item_visible(&i.affected_repo));
        org.info.retain(|i| item_visible(&i.affected_repo));

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

    // affected_clusters, risk_level, summary, status, gate_state, blind_spots,
    // analysis_direction, notifications, traversal_truncated: left as-is. They
    // do not name other repos, and we deliberately add no notification that
    // would reveal a hidden repo's existence.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blast_radius::{
        AffectedSymbol, ChangedSymbol, Coverage, OrgImpactItem, OrgWideImpact, StaleRepo,
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
        OrgImpactItem {
            change_name: "c".to_string(),
            change_kind: "function".to_string(),
            affected_name: "a".to_string(),
            affected_repo: affected_repo.to_string(),
            affected_file: "f.rs".to_string(),
            affected_line: 1,
            severity: "warning".to_string(),
            reason: "r".to_string(),
        }
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
}
