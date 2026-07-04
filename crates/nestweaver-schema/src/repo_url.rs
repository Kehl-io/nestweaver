//! Normalized repo identity — the single source of truth for collapsing a repo
//! clone URL to a scheme/credential/suffix/case-invariant identity key.
//!
//! Two NestWeaver instances (a LOCAL daemon and a SERVER) routinely index the
//! *same* repository under different clone-URL *forms* — an ssh remote
//! (`git@github.com:acme/api.git`) on one and the canonical https URL
//! (`https://github.com/acme/api`) on the other. Hashing the raw URL string
//! makes those two forms mint different `url_hash`es, so their `repo_uid`s
//! (and every symbol/file UID derived from them) never reconcile and the same
//! symbol shows up twice in merged results.
//!
//! [`normalized_repo_key`] collapses a clone URL to a scheme/credential/suffix/
//! case-invariant `host/owner/name` key (validated against Sourcegraph's
//! `MakeURI`, Kythe corpus, and SCIP repo-identity conventions), so equivalent
//! URL forms produce the same key. [`crate::uid::repo_uid`] and
//! [`crate::uid::canonical_symbol_id`] normalize through this function BEFORE
//! hashing, so equivalent URL forms mint identical UIDs.
//!
//! Note on `file://` URLs: a local, path-only repo (`file:///Users/me/api`)
//! keys on its FULL lowercased path because there is no host or owner to key
//! on. It therefore does NOT reconcile with a remote `host/owner/name` key —
//! which is the correct behaviour: a working copy with no matching remote is a
//! distinct identity. Keying on the full path (rather than the directory leaf)
//! keeps two unrelated local checkouts that share a basename distinct.
//! Reconciling a `file://` repo with its server counterpart requires resolving
//! the local checkout's `origin` remote, which is not available at this layer.
//!
//! Consequence for cross-boundary queries: because an origin-less local repo's
//! `canonical_symbol_id`s embed a path hash that can never equal a server's
//! `host/owner/name`-derived ids, `flow_trace` continuation — which resolves a
//! boundary strictly by `canonical_id` (`symbol_by_canonical_id`) — returns an
//! EMPTY continuation for such a boundary (no error, no stub). `ImpactAnalysis`
//! degrades more gracefully via a name+file fallback, but flow_trace has none.
//! This matches SCIP's treatment of package-less code (no cross-repo moniker →
//! no cross-repo navigation). A local repo WITH an `origin` remote reconciles
//! correctly and stitches as expected; give an indexed repo a remote to enable
//! cross-boundary flow_trace.

/// Collapse a repo clone URL to a normalized identity key that is invariant to
/// scheme, embedded credentials, a trailing `.git`, a trailing slash, and host/
/// path casing.
///
/// Examples that all map to `github.com/acme/api`:
/// - `https://github.com/acme/api`
/// - `https://github.com/acme/api.git`
/// - `https://GitHub.com/Acme/API/`
/// - `git@github.com:acme/api.git`
/// - `ssh://git@github.com/acme/api`
/// - `https://user:token@github.com/acme/api`
///
/// A bare local path or `file://` URL keys on its FULL lowercased path (NOT the
/// directory leaf): the local daemon indexes repos as `file://<absolute-path>`,
/// and two unrelated checkouts that share a basename (`.../work/api` vs
/// `.../personal/api`) are distinct repos — collapsing them to the leaf would
/// merge them in the store. A local path still never reconciles with a remote
/// `host/owner/name` key (correct: a checkout with no matching remote is a
/// distinct identity).
///
/// ⚠ Changing this normalization changes every stored hash (`repo_uid`,
/// `file_uid`, `symbol_uid`, `canonical_symbol_id`) — requires a full reindex.
pub fn normalized_repo_key(repo_url: &str) -> String {
    let repo = strip_git_suffix(strip_url_suffix(repo_url.trim()).trim_end_matches('/'));

    if let Some(path) = repo.strip_prefix("file://") {
        return path.trim_end_matches('/').to_ascii_lowercase();
    }

    if let Some((_, rest)) = repo.split_once("://") {
        return normalize_remote_path(rest);
    }

    if let Some((_, rest)) = repo.split_once('@')
        && let Some((host, path)) = rest.split_once(':')
    {
        return format!("{host}/{path}")
            .trim_matches('/')
            .to_ascii_lowercase();
    }

    if repo.starts_with('/') || repo.starts_with("./") || repo.starts_with("../") {
        return repo.trim_end_matches('/').to_ascii_lowercase();
    }

    repo.trim_matches('/').to_ascii_lowercase()
}

/// The repo's short name (final path segment of the normalized key), e.g.
/// `api` for `github.com/acme/api`. `None` when the key has no non-empty leaf.
pub fn repo_name(repo_url: &str) -> Option<String> {
    let key = normalized_repo_key(repo_url);
    key.rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .map(ToString::to_string)
}

fn normalize_remote_path(remote: &str) -> String {
    let without_auth = remote
        .split_once('/')
        .map(|(host, path)| {
            let host = host.rsplit('@').next().unwrap_or(host);
            format!("{host}/{path}")
        })
        .unwrap_or_else(|| remote.to_string());
    without_auth.trim_matches('/').to_ascii_lowercase()
}

fn strip_url_suffix(repo_url: &str) -> &str {
    repo_url.split(['?', '#']).next().unwrap_or(repo_url)
}

fn strip_git_suffix(repo_url: &str) -> &str {
    repo_url.strip_suffix(".git").unwrap_or(repo_url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equivalent_url_forms_share_one_key() {
        let canonical = normalized_repo_key("https://github.com/acme/api");
        for form in [
            "https://github.com/acme/api",
            "https://github.com/acme/api.git",
            "https://github.com/acme/api/",
            "https://GitHub.com/Acme/API",
            "https://user:token@github.com/acme/api",
            "git@github.com:acme/api.git",
            "git@github.com:acme/api",
            "ssh://git@github.com/acme/api",
            "ssh://git@github.com/acme/api.git",
            "https://github.com/acme/api?ref=main",
        ] {
            assert_eq!(
                normalized_repo_key(form),
                canonical,
                "URL form `{form}` should normalize to `{canonical}`"
            );
        }
        assert_eq!(canonical, "github.com/acme/api");
    }

    #[test]
    fn distinct_repos_get_distinct_keys() {
        assert_ne!(
            normalized_repo_key("https://github.com/acme/api"),
            normalized_repo_key("https://github.com/other/api"),
        );
        assert_ne!(
            normalized_repo_key("https://github.com/acme/api"),
            normalized_repo_key("https://gitlab.com/acme/api"),
        );
    }

    #[test]
    fn file_url_keys_on_full_path_and_stays_distinct_from_remote() {
        // Documents the file://-origin gap: a path-only checkout keys on its
        // FULL path, which does NOT match the remote host/owner/name key.
        let file_key = normalized_repo_key("file:///Users/me/dev/api");
        assert_eq!(file_key, "/users/me/dev/api");
        assert_ne!(
            file_key,
            normalized_repo_key("https://github.com/acme/api"),
            "a file:// checkout must not silently reconcile with a remote"
        );
    }

    #[test]
    fn two_distinct_local_paths_same_basename_stay_distinct() {
        // The local daemon indexes repos as `file://<absolute-path>`. Two
        // unrelated local checkouts that happen to share a basename (`api`)
        // MUST NOT collide — collapsing them to the leaf would merge two
        // distinct repos in the store (silent graph corruption).
        let a = normalized_repo_key("file:///Users/me/work/api");
        let b = normalized_repo_key("file:///Users/me/personal/api");
        assert_ne!(
            a, b,
            "distinct local paths must not collapse to the basename"
        );
        assert_ne!(
            crate::uid::repo_uid("local", "file:///Users/me/work/api"),
            crate::uid::repo_uid("local", "file:///Users/me/personal/api"),
            "distinct local paths must mint distinct repo_uids"
        );
        // A local path still does not reconcile with a remote (correct: a
        // checkout with no matching remote is a distinct identity).
        assert_ne!(a, normalized_repo_key("https://github.com/acme/api"));
    }

    #[test]
    fn nested_subgroup_scp_and_https_reconcile() {
        assert_eq!(
            normalized_repo_key("git@gitlab.com:group/subgroup/repo.git"),
            normalized_repo_key("https://gitlab.com/group/subgroup/repo"),
        );
    }

    #[test]
    fn git_and_http_schemes_reconcile() {
        let canonical = normalized_repo_key("https://github.com/acme/api");
        assert_eq!(
            normalized_repo_key("git://github.com/acme/api.git"),
            canonical
        );
        assert_eq!(normalized_repo_key("http://github.com/acme/api"), canonical);
    }

    #[test]
    fn malformed_and_empty_inputs_do_not_panic() {
        for input in ["", "   ", "user@host", "file://", "https://", "://x", "@:"] {
            let _ = normalized_repo_key(input);
            let _ = repo_name(input);
        }
    }

    #[test]
    fn bare_local_path_normalizes_to_full_path() {
        assert_eq!(
            normalized_repo_key("/Users/me/dev/api"),
            "/users/me/dev/api"
        );
        assert_eq!(
            normalized_repo_key("/Users/me/dev/api/"),
            "/users/me/dev/api"
        );
    }

    #[test]
    fn repo_name_returns_leaf() {
        assert_eq!(
            repo_name("git@github.com:acme/api.git").as_deref(),
            Some("api")
        );
        assert_eq!(
            repo_name("https://github.com/acme/api/").as_deref(),
            Some("api")
        );
    }
}
