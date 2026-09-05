//! Shared node-scope filtering helpers (nw-421).
//!
//! `crates/nestweaver-mcp/src/tools.rs` and `src/main.rs` each carried a
//! byte-identical copy of these seven items — `NodeOwner`, `node_owner`,
//! `resolve_repo_filter`, `resolve_vault_filter`, `retain_nodes_in_repos`,
//! `retain_nodes_in_vaults`, `retain_nodes_under_path_prefix` — because the
//! MCP copies were crate-private and the CLI could not call them. That is
//! exactly the class nw-217 names: two implementations of one contract
//! reliably drift, and the only fix that has ever held is making the second
//! implementation CALL the first. This module IS the first implementation;
//! both surfaces call it and neither retains a private copy.
//!
//! `resolve_repo_filter` takes an optional [`VisibleRepos`] so the MCP route
//! can pre-filter the candidate repo set to the caller's authorization scope
//! before resolving a selector against it — the CLI's direct path has no such
//! concept and passes `None`, which is a no-op (every repo is a candidate).

use std::collections::HashSet;

use anyhow::{Context, anyhow};

use crate::authz::VisibleRepos;
use crate::query::BrainNode;
use nestweaver_store::GraphStore;

/// The container a node UID names as its owner.
///
/// The UID is the authority because it is the only field on a `BrainNode`
/// that NAMES an owner: `sym:`/`file:`/`svc:` embed the whole
/// `repo:{inst}:{hash}`, and `note:`/`sec:`/`head:`/`tag:` embed the whole
/// `vlt:{inst}:{hash}`. `location` names neither — a symbol's `location` is
/// REPO-RELATIVE and so never contains its own repo name, which is why
/// `--repos website` used to return ZERO symbols from the repo it named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeOwner {
    /// Owned by this repo UID.
    Repo(String),
    /// Owned by this vault UID. Vault content carries no `repo_uid` at all —
    /// the same fact `RepoScope::NotRepoScoped` is written on.
    Vault(String),
    /// Nothing in the UID names an owner.
    Unattributable,
}

/// The owner of `uid`.
///
/// The match is over [`nestweaver_schema::uid::UidKind`] rather than an `if`
/// chain of `starts_with`, for nw-301's reason: an `if` chain cannot be
/// exhaustive, so a twelfth UID domain would fall silently into whatever the
/// trailing arm does instead of failing this build.
pub fn node_owner(uid: &str) -> NodeOwner {
    use nestweaver_schema::uid::UidKind;

    /// `{prefix}{owner_uid}:{…}` — an owner UID is always exactly three
    /// colon-separated components (`repo:{inst}:{hash}`, `vlt:{inst}:{hash}`),
    /// so take three and discard the node-specific tail.
    fn owner_head(rest: &str) -> Option<String> {
        let parts: Vec<&str> = rest.splitn(4, ':').collect();
        (parts.len() >= 3).then(|| format!("{}:{}:{}", parts[0], parts[1], parts[2]))
    }

    let Some(kind) = UidKind::of(uid) else {
        return NodeOwner::Unattributable;
    };
    let rest = &uid[kind.prefix().len()..];
    match kind {
        // The container node itself.
        UidKind::Repo => NodeOwner::Repo(uid.to_string()),
        UidKind::Vault => NodeOwner::Vault(uid.to_string()),
        // `{prefix}{repo_uid}:{…}`
        UidKind::File | UidKind::Service | UidKind::Symbol => {
            owner_head(rest).map_or(NodeOwner::Unattributable, NodeOwner::Repo)
        }
        // `{prefix}{vault_uid}:{…}`
        UidKind::Note | UidKind::Tag => {
            owner_head(rest).map_or(NodeOwner::Unattributable, NodeOwner::Vault)
        }
        // `sec:{note_uid}:{…}` / `head:{note_uid}:{…}`, and a note UID is
        // itself `note:{vault_uid}:{…}`, so the inner `note:` comes off too.
        UidKind::Section | UidKind::Heading => rest
            .strip_prefix(UidKind::Note.prefix())
            .and_then(owner_head)
            .map_or(NodeOwner::Unattributable, NodeOwner::Vault),
        // `proj:{instance}:{hash}` names an INSTANCE, not a repo, and a
        // contract UID carries no repo component at all. Neither can be
        // attributed, so neither survives a scope filter.
        UidKind::Project | UidKind::Contract => NodeOwner::Unattributable,
    }
}

/// Resolve caller-supplied `repos:` / `--repos` entries to concrete repo
/// UIDs.
///
/// Resolution is [`crate::resolve_repo_selector`] — the SAME resolver
/// `--repo` uses everywhere else — so this filter cannot grow a second,
/// drifting notion of what a repo name means, and an ambiguous selector
/// (`website` under two orgs) FAILS naming both candidates instead of
/// quietly merging two tenants' code into one answer. An unresolvable entry
/// ERRORS: the old predicate matched nothing and returned a confident empty
/// result, which reads as "this repo has no relevant content" rather than
/// "you named a repo that is not here".
///
/// `visible` filters the candidate set first when scoping to an
/// authorization boundary (MCP callers under an `[authz]` policy). Without
/// it the resolver's own "not found" / "ambiguous, candidates are …"
/// messages would enumerate repos a repo-scoped caller cannot see. The CLI's
/// direct path has no such boundary and passes `None`, under which every
/// indexed repo is a candidate — a no-op relative to filtering nothing.
pub fn resolve_repo_filter(
    store: &GraphStore,
    selectors: &[String],
    visible: Option<&VisibleRepos>,
) -> Result<HashSet<String>, anyhow::Error> {
    let repos: Vec<nestweaver_schema::Repo> = store
        .list_repos(None)
        .context("listing repositories to resolve the repo filter")?
        .into_iter()
        .filter(|repo| visible.is_none_or(|scope| scope.allows(&repo.uid)))
        .collect();
    selectors
        .iter()
        .map(|selector| {
            crate::resolve_repo_selector(&repos, selector)
                .map(|repo| repo.uid.clone())
                // Flattened with `{error:#}` rather than `.context(…)`: an MCP
                // client renders `Error::to_string()`, which shows only the
                // OUTERMOST context — so a `.context()` here would hide the
                // resolver's own "ambiguous; use an exact UID: …" candidate
                // list, which is the only actionable part of the message.
                .map_err(|error| anyhow!("repo filter entry {selector:?}: {error:#}"))
        })
        .collect()
}

/// Resolve caller-supplied `vaults:` / `--vaults` entries to concrete vault
/// UIDs.
///
/// The mirror of [`resolve_repo_filter`], written separately because there is
/// no engine-side vault selector to reuse: `--repo` is a first-class CLI
/// selector and `--vault` is not. The precedence deliberately matches
/// `resolve_repo_selector`'s (exact UID, then case-insensitive exact name,
/// then exact root path), and it is exact-only — no substring leg.
pub fn resolve_vault_filter(
    store: &GraphStore,
    selectors: &[String],
) -> Result<HashSet<String>, anyhow::Error> {
    let vaults = store
        .list_vaults(None)
        .context("listing vaults to resolve the vault filter")?;
    selectors
        .iter()
        .map(|selector| {
            let needle = selector.to_lowercase();
            let matches: Vec<&nestweaver_schema::Vault> = vaults
                .iter()
                .filter(|vault| {
                    vault.uid == *selector
                        || vault.name.to_lowercase() == needle
                        || vault.root_path == *selector
                })
                .collect();
            match matches.as_slice() {
                [vault] => Ok(vault.uid.clone()),
                // An unresolvable entry ERRORS. The old predicate matched
                // nothing and returned a confident empty result, which reads
                // as "this vault has no relevant content" rather than "you
                // named a vault that is not here".
                [] => {
                    let known: Vec<&str> = vaults.iter().map(|vault| vault.name.as_str()).collect();
                    Err(anyhow!(
                        "vault filter entry {selector:?} matches no indexed vault; \
                         known vaults: {}",
                        if known.is_empty() {
                            "(none indexed)".to_string()
                        } else {
                            known.join(", ")
                        }
                    ))
                }
                ambiguous => Err(anyhow!(
                    "vault filter entry {selector:?} is ambiguous; use an exact UID: {}",
                    ambiguous
                        .iter()
                        .map(|vault| format!("{} ({})", vault.name, vault.uid))
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            }
        })
        .collect()
}

/// Keep only nodes owned by one of `repo_uids`.
///
/// **VAULT NODES ARE DROPPED.** This is nw-405's required recorded decision.
/// A Note, Section, Heading or Tag belongs to a VAULT and carries no
/// `repo_uid`, so it is not "in repo X" under any reading — keeping it is
/// exactly the measured over-include where `repos:["clientA"]` returned
/// clientB's notes because their PATH happened to contain the string. Vault
/// content is scoped with `vaults:` / `--vaults`, which is the parameter that
/// can actually answer the question.
///
/// A node whose UID names no owner is dropped for the reason nw-403's
/// redactor drops one: "I cannot tell what owns this" is not a reason to
/// return it under a scope argument.
pub fn retain_nodes_in_repos(nodes: &mut Vec<BrainNode>, repo_uids: &HashSet<String>) {
    nodes.retain(
        |node| matches!(node_owner(&node.uid), NodeOwner::Repo(uid) if repo_uids.contains(&uid)),
    );
}

/// Keep only nodes owned by one of `vault_uids`.
///
/// The mirror of [`retain_nodes_in_repos`]: Symbol/File/Service nodes belong
/// to a repo, so they cannot satisfy a vault scope and are dropped.
pub fn retain_nodes_in_vaults(nodes: &mut Vec<BrainNode>, vault_uids: &HashSet<String>) {
    nodes.retain(
        |node| matches!(node_owner(&node.uid), NodeOwner::Vault(uid) if vault_uids.contains(&uid)),
    );
}

/// Apply a path prefix filter, exempting nodes that have no path at all.
///
/// nw-406. The predicate was the bare
/// `nodes.retain(|n| n.location.starts_with(prefix))`, and a Tag node carries
/// `location: ""`: `"".starts_with("Workspaces/")` is false, so
/// `--kinds Tag --path-prefix Workspaces/` measured 25 Tag nodes -> 0 on a
/// vault whose 606 tags ALL live under `Workspaces/`. A confident zero with no
/// disclosure.
///
/// A tag is not "outside" the prefix — it has no path concept for the prefix
/// to test, which is the same unhandled-kind omission as the `tags`-vs-Symbol
/// carve-out. An empty location is therefore EXEMPT rather than excluded: a
/// filter that cannot decide a kind must not silently delete all of it.
pub fn retain_nodes_under_path_prefix(nodes: &mut Vec<BrainNode>, prefix: &str) {
    nodes.retain(|node| node.location.is_empty() || node.location.starts_with(prefix));
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPO_A: &str = "repo:default:aaaaaaaaaaaa";
    const REPO_B: &str = "repo:default:bbbbbbbbbbbb";
    const VAULT: &str = "vlt:default:cccccccccccc";

    fn node(uid: &str, location: &str) -> BrainNode {
        BrainNode {
            uid: uid.to_string(),
            kind: "Symbol".to_string(),
            title: uid.to_string(),
            location: location.to_string(),
            relevance: 1.0,
            inline_body: None,
            body_complete: true,
        }
    }

    #[test]
    fn node_owner_classifies_every_kind() {
        assert_eq!(node_owner("proj:default:abc"), NodeOwner::Unattributable);
        assert_eq!(node_owner("banana"), NodeOwner::Unattributable);
        assert_eq!(
            node_owner(&format!("sym:{REPO_A}:f:n:1")),
            NodeOwner::Repo(REPO_A.to_string())
        );
        assert_eq!(
            node_owner(&format!("head:note:{VAULT}:n1:h:3")),
            NodeOwner::Vault(VAULT.to_string())
        );
    }

    /// COUNTERWEIGHT: invert the predicate (keep only nodes whose owner is
    /// NOT in the requested set) and confirm the test fails, proving the
    /// assertion actually depends on which repo owns the node rather than
    /// passing regardless.
    #[test]
    fn retain_nodes_in_repos_drops_other_repos_and_vault_nodes() {
        let mut nodes = vec![
            node(&format!("sym:{REPO_A}:f:n:1"), "a.rs"),
            node(&format!("sym:{REPO_B}:f:n:2"), "b.rs"),
            node(&format!("note:{VAULT}:n1"), "Workspaces/x.md"),
        ];
        retain_nodes_in_repos(&mut nodes, &HashSet::from([REPO_A.to_string()]));
        assert_eq!(nodes.len(), 1);
        assert!(nodes[0].uid.starts_with("sym:"));
        assert!(nodes[0].uid.contains(REPO_A));
    }

    #[test]
    fn retain_nodes_under_path_prefix_exempts_empty_location() {
        let mut nodes = vec![
            node("tag:default:t1", ""),
            node(&format!("sym:{REPO_A}:f:n:1"), "Workspaces/a.rs"),
            node(&format!("sym:{REPO_A}:f:n:2"), "other/b.rs"),
        ];
        retain_nodes_under_path_prefix(&mut nodes, "Workspaces/");
        assert_eq!(nodes.len(), 2);
        assert!(nodes.iter().any(|n| n.uid.starts_with("tag:")));
        assert!(nodes.iter().any(|n| n.location == "Workspaces/a.rs"));
    }
}
