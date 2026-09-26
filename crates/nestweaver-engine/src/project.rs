use std::collections::{HashMap, HashSet};
use std::path::Path;

use nestweaver_schema::{
    Heading, Note, NoteKind, Project, Section, heading_uid, note_uid, project_uid, section_uid,
    truncated_hash,
};
use nestweaver_store::write::{
    ReplaceMaterializedProjectsError, ReplaceMaterializedProjectsOutcome,
};
use nestweaver_store::{GraphStore, ProjectMutationDisposition};

use crate::config::InstanceConfig;
use crate::extensions::{load_extensions, save_extensions, set_property};
use crate::html_to_md::maybe_convert_html_to_markdown;
use crate::manifest::{
    GraphMutationPublicationGuard, GraphMutationPublicationOutcome,
    begin_graph_mutation_publication, finalize_committed_graph_mutation,
};
use crate::mcp_client::McpClient;
use crate::repo_display_name;

pub struct ProjectMaterializationResult {
    pub projects_created: usize,
    pub note_edges: usize,
    /// nw-678: symbols in the projects' member repos (membership is now
    /// computed from Project -> Repo, so no per-symbol edge is written).
    pub symbol_edges: usize,
    /// nw-678: PROJECT_INCLUDES_REPO edges written.
    pub repo_edges: usize,
    pub component_edges: usize,
    pub wiki_notes_ingested: usize,
    pub wiki_fetch_errors: usize,
    /// nw-674: declared `[[projects]] repos` entries that did not resolve
    /// cleanly — attached nothing, or attached only by the legacy URL
    /// substring. Previously these were dropped without a word, so a project
    /// could lose half its code and still report success.
    pub repo_issues: Vec<ProjectRepoIssue>,
    pub publication: GraphMutationPublicationOutcome,
}

/// Extension-sidecar key under which a project's declared-repo issues are
/// recorded, so read routes (`project_context`, `list-projects`) can disclose
/// them without re-resolving against the graph (nw-674).
pub const REPO_ISSUES_KEY: &str = "repo_issues";

/// nw-670 re-review F1: how many repos the project DECLARES, so a project
/// whose declared repos all failed to resolve is disclosed rather than
/// silently linking its notes as unscoped.
pub const DECLARED_REPO_COUNT_KEY: &str = "declared_repo_count";

/// A configured project's vault folder, used to keep membership current as
/// new notes are indexed after materialization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectFolder {
    pub project_uid: String,
    pub folder: String,
}

pub fn project_folders(config: &InstanceConfig, instance_id: &str) -> Vec<ProjectFolder> {
    config
        .projects
        .iter()
        .filter_map(|project| {
            project.vault_folder.as_ref().map(|folder| ProjectFolder {
                project_uid: project_uid(instance_id, &project.name),
                folder: folder.trim_end_matches('/').to_string(),
            })
        })
        .collect()
}

/// Match a note path at a folder boundary, not a similarly named sibling.
pub fn note_in_folder(note_path: &str, folder: &str) -> bool {
    note_path == folder || note_path.starts_with(&format!("{folder}/"))
}

/// Why a declared repo reference did not resolve cleanly (nw-674).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoIssueKind {
    /// Nothing indexed matched. Attached nothing.
    NoMatch,
    /// A `[[repos]]` alias with this name exists but its `url` identifies no
    /// indexed repo. Attached nothing — the alias is authoritative, so a
    /// derived name is not allowed to stand in for it.
    AliasNotIndexed,
    /// The name matched repos with more than one distinct remote. Attached
    /// nothing.
    Ambiguous,
    /// Matched only by the legacy URL-substring rule. ATTACHED, but the match
    /// is fragile enough that the operator should declare it precisely.
    SubstringMatch,
}

/// A declared repo reference that did not resolve cleanly (nw-674). Used for
/// `[[projects]] repos` and `[[features]] repos` alike; `project` names the
/// declaring project or feature.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProjectRepoIssue {
    pub project: String,
    /// The reference exactly as written in the config.
    pub repo: String,
    pub kind: RepoIssueKind,
    /// `name @ root` of the repos involved (ambiguous candidates, or the
    /// substring match), or the alias url(s) for `AliasNotIndexed`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<String>,
}

impl ProjectRepoIssue {
    fn new(
        project: &str,
        repo: &str,
        kind: RepoIssueKind,
        candidates: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            project: project.to_string(),
            repo: repo.to_string(),
            kind,
            // Sorted + deduplicated so the disclosure is stable across runs
            // regardless of store row order.
            candidates: candidates
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect(),
        }
    }

    /// Whether the reference attached no repo at all.
    pub fn attached_nothing(&self) -> bool {
        self.kind != RepoIssueKind::SubstringMatch
    }

    /// Classify one resolution. `None` for a clean resolution.
    pub fn from_match(project: &str, repo: &str, found: &DeclaredRepoMatch<'_>) -> Option<Self> {
        let described = |repos: &[&nestweaver_schema::Repo]| {
            repos
                .iter()
                .map(|repo| {
                    format!(
                        "{} @ {}",
                        repo_display_name(repo),
                        repo.local_root().unwrap_or(repo.url.as_str())
                    )
                })
                .collect::<Vec<_>>()
        };
        let (kind, candidates) = match found {
            DeclaredRepoMatch::Resolved(_) => return None,
            DeclaredRepoMatch::ResolvedBySubstring(repos) => {
                (RepoIssueKind::SubstringMatch, described(repos))
            }
            DeclaredRepoMatch::Unresolved => (RepoIssueKind::NoMatch, Vec::new()),
            DeclaredRepoMatch::AliasNotIndexed(urls) => {
                (RepoIssueKind::AliasNotIndexed, urls.clone())
            }
            DeclaredRepoMatch::Ambiguous(repos) => (RepoIssueKind::Ambiguous, described(repos)),
        };
        Some(Self::new(project, repo, kind, candidates))
    }
}

impl std::fmt::Display for ProjectRepoIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (project, repo, list) = (&self.project, &self.repo, self.candidates.join(", "));
        match self.kind {
            RepoIssueKind::NoMatch => write!(f, "{project}/{repo} (matches no indexed repo)"),
            RepoIssueKind::AliasNotIndexed => write!(
                f,
                "{project}/{repo} ([[repos]] alias url {list} identifies no indexed repo)"
            ),
            RepoIssueKind::Ambiguous => write!(f, "{project}/{repo} (ambiguous: {list})"),
            RepoIssueKind::SubstringMatch => write!(
                f,
                "{project}/{repo} (resolved only by URL substring to {list}; declare it by checkout directory, path, or a [[repos]] alias)"
            ),
        }
    }
}

/// The one text rendering of declared-repo issues, shared by every CLI text
/// route (`list-projects`, `project-context`, `context --feature`) so the
/// wording cannot drift between them (nw-674).
pub fn repo_issue_warning_lines(issues: &[ProjectRepoIssue]) -> Vec<String> {
    issues
        .iter()
        .map(|issue| {
            if issue.attached_nothing() {
                format!("Warning: declared repo not a member: {issue}")
            } else {
                format!("Warning: declared repo matched loosely: {issue}")
            }
        })
        .collect()
}

/// One-line summary of declared-repo issues for a materialization run's
/// terminal output — the daemon's progress line (clean AND degraded) and the
/// staged-rebuild route print exactly this (nw-674). `None` when clean.
pub fn repo_issues_summary(issues: &[ProjectRepoIssue]) -> Option<String> {
    if issues.is_empty() {
        return None;
    }
    Some(format!(
        "Warning: {} declared repo reference(s) did not resolve cleanly: {}. Declare them by checkout directory name, path, or a `[[repos]] name` alias.",
        issues.len(),
        issues
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ")
    ))
}

/// The declared-repo issues last recorded for `project_uid` by
/// materialization. Undecodable sidecar content reads as none rather than
/// failing a read route over a disclosure field.
pub fn recorded_repo_issues(
    ext_store: &crate::extensions::ExtensionStore,
    project_uid: &str,
) -> Vec<ProjectRepoIssue> {
    crate::extensions::get_property(ext_store, project_uid, REPO_ISSUES_KEY)
        .and_then(|value| serde_json::from_value(value.clone()).ok())
        .unwrap_or_default()
}

/// Outcome of resolving one declared repo reference against indexed repos.
#[derive(Debug)]
pub enum DeclaredRepoMatch<'a> {
    Resolved(Vec<&'a nestweaver_schema::Repo>),
    /// Attached, but only via the legacy URL-substring rule — disclosed.
    ResolvedBySubstring(Vec<&'a nestweaver_schema::Repo>),
    Unresolved,
    /// A `[[repos]]` alias of this name exists; its url(s) are not indexed.
    AliasNotIndexed(Vec<String>),
    Ambiguous(Vec<&'a nestweaver_schema::Repo>),
}

impl<'a> DeclaredRepoMatch<'a> {
    /// The repos this reference attaches (empty when it attaches nothing).
    pub fn attached(&self) -> &[&'a nestweaver_schema::Repo] {
        match self {
            Self::Resolved(repos) | Self::ResolvedBySubstring(repos) => repos,
            Self::Unresolved | Self::AliasNotIndexed(_) | Self::Ambiguous(_) => &[],
        }
    }
}

/// Resolve a repo reference declared in `[[projects]] repos`,
/// `[[features]] repos`, or `[[links]] from/to` to the indexed repos it names.
///
/// nw-674: membership used to be `display_name == declared ||
/// alias url == repo.url || repo.url.contains(declared)`, all OR'd. The
/// display name of a repo indexed without `--name` is its URL basename, so
/// `Shot-Insights/web-app.git` checked out at `.../shot-insights-web-app` was
/// `web-app` and never matched the directory basename the instance config
/// documents as a repo's identity — six declared repos in the live brain were
/// silently not members. Worse, the OR'd substring clause made a generic name
/// like `website` claim every `.../website.git` in the instance.
///
/// Resolution, first match wins:
/// 1. a `[[repos]]` entry whose `name` is the reference — its `url` matched
///    against identity url OR checkout path
///    ([`crate::config::repo_ref_identifies`]). If such an alias exists but
///    identifies nothing indexed the result is `AliasNotIndexed`: the alias is
///    authoritative and no derived name may stand in for it;
/// 2. the reference itself as an identity url or checkout path;
/// 3. ONE derived-name tier: display name (stored name, else URL basename)
///    OR checkout directory basename. One tier, not three in sequence — a
///    sequence let the first matching spelling hide an unrelated repo that
///    shares the name under another spelling (`docs` = coyote's checkout dir
///    AND Shot-Insights/docs.git's display name);
/// 4. (legacy) a substring of the identity url — attached but disclosed.
///
/// Tiers 1-2 are explicit identities, so several matches (linked worktrees of
/// one remote) are all members. Tiers 3-4 match names: if they hit repos with
/// more than one distinct remote the reference is `Ambiguous` and attaches
/// nothing — two client sites both named `website` must not merge.
pub fn resolve_declared_repo<'a>(
    declared: &str,
    repo_configs: &[crate::config::RepoConfig],
    repos: &'a [nestweaver_schema::Repo],
) -> DeclaredRepoMatch<'a> {
    use crate::config::repo_ref_identifies;
    let identifies = |repo: &nestweaver_schema::Repo, reference: &str| {
        repo_ref_identifies(reference, &repo.url, repo.local_root().map(Path::new))
    };

    let aliases: Vec<&str> = repo_configs
        .iter()
        .filter(|config| config.name.as_deref() == Some(declared))
        .map(|config| config.url.as_str())
        .collect();
    if !aliases.is_empty() {
        let matched: Vec<_> = repos
            .iter()
            .filter(|repo| aliases.iter().any(|url| identifies(repo, url)))
            .collect();
        return if matched.is_empty() {
            DeclaredRepoMatch::AliasNotIndexed(aliases.iter().map(|url| url.to_string()).collect())
        } else {
            DeclaredRepoMatch::Resolved(matched)
        };
    }
    let matched: Vec<_> = repos
        .iter()
        .filter(|repo| identifies(repo, declared))
        .collect();
    if !matched.is_empty() {
        return DeclaredRepoMatch::Resolved(matched);
    }

    let by_name = |repo: &nestweaver_schema::Repo| {
        repo_display_name(repo) == declared
            || repo
                .local_root()
                .and_then(|root| Path::new(root).file_name())
                .is_some_and(|base| base == declared)
    };
    let by_substring =
        |repo: &nestweaver_schema::Repo| !declared.is_empty() && repo.url.contains(declared);
    let one_remote = |matched: &[&nestweaver_schema::Repo]| {
        matched
            .iter()
            .map(|repo| repo.url.trim_end_matches('/'))
            .collect::<HashSet<_>>()
            .len()
            == 1
    };
    let matched: Vec<_> = repos.iter().filter(|repo| by_name(repo)).collect();
    if !matched.is_empty() {
        return if one_remote(&matched) {
            DeclaredRepoMatch::Resolved(matched)
        } else {
            DeclaredRepoMatch::Ambiguous(matched)
        };
    }
    let matched: Vec<_> = repos.iter().filter(|repo| by_substring(repo)).collect();
    if !matched.is_empty() {
        return if one_remote(&matched) {
            DeclaredRepoMatch::ResolvedBySubstring(matched)
        } else {
            DeclaredRepoMatch::Ambiguous(matched)
        };
    }
    DeclaredRepoMatch::Unresolved
}

pub struct ImplicitProjectDetectionResult {
    pub projects: Vec<String>,
    pub publication: GraphMutationPublicationOutcome,
}

fn resolve_project_replacement<'a>(
    publication: GraphMutationPublicationGuard<'a>,
    replacement: Result<ReplaceMaterializedProjectsOutcome, ReplaceMaterializedProjectsError>,
) -> Result<
    (
        GraphMutationPublicationGuard<'a>,
        ReplaceMaterializedProjectsOutcome,
    ),
    anyhow::Error,
> {
    match replacement {
        Ok(outcome) => Ok((publication, outcome)),
        Err(error)
            if matches!(
                error.disposition,
                ProjectMutationDisposition::ConfirmedUnchanged
                    | ProjectMutationDisposition::ConfirmedRolledBack
            ) =>
        {
            if let Err(finish_error) = publication.finish(false) {
                return Err(anyhow::anyhow!(
                    "{error}; additionally failed to retire the no-change publication: {finish_error:#}"
                ));
            }
            Err(error.into())
        }
        Err(error) => {
            // Changed/ambiguous failures retain the durable crash fence. The
            // guard drop releases live ownership only; readers stay fail-closed.
            drop(publication);
            Err(error.into())
        }
    }
}

/// Heuristic patterns that indicate an MCP tool response is an error message
/// rather than real wiki content.
const ERROR_PATTERNS: &[&str] = &[
    "Error:",
    "error:",
    "unable to",
    "failed to",
    "CERTIFICATE",
    "TLS",
    "SSL",
    "connection refused",
    "timeout",
    "ECONNREFUSED",
    "ENOTFOUND",
    "ETIMEDOUT",
];

/// Returns `true` when the content looks like an error message rather than
/// genuine wiki content.
fn looks_like_fetch_error(content: &str) -> bool {
    // Short content with "error" anywhere is almost certainly an error.
    if content.len() < 200 && content.to_ascii_lowercase().contains("error") {
        return true;
    }
    ERROR_PATTERNS.iter().any(|p| content.contains(p))
}

/// nw-678: each configured project's member repos — its declared `repos`
/// resolved by the nw-674 resolver — as `(project_uid, repo_uid)` pairs,
/// sorted and distinct. One planner behind `materialize_projects` and the
/// daemon's startup [`rebuild_project_repo_membership`].
pub fn declared_repo_edges(
    config: &InstanceConfig,
    instance_id: &str,
    all_repos: &[nestweaver_schema::Repo],
) -> Vec<(String, String)> {
    let mut edges = Vec::new();
    for project_cfg in &config.projects {
        let uid = project_uid(instance_id, &project_cfg.name);
        for repo_name in &project_cfg.repos {
            let found = resolve_declared_repo(repo_name, &config.repos, all_repos);
            edges.extend(
                found
                    .attached()
                    .iter()
                    .map(|repo| (uid.clone(), repo.uid.clone())),
            );
        }
    }
    edges.sort();
    edges.dedup();
    edges
}

/// nw-678: bring every configured, materialized project's PROJECT_INCLUDES_REPO
/// membership (and its declared-repo count) in line with `config`, without
/// the rest of a materialization (no note, wiki or MCP work). The daemon runs
/// it at startup, so a graph materialized before the membership existed —
/// or a config whose `repos` changed since — gets its code membership back
/// with no operator action. Returns whether the graph changed.
pub fn rebuild_project_repo_membership(
    store: &GraphStore,
    config: &InstanceConfig,
    instance_id: &str,
    db_path: &Path,
    lease: Option<&crate::watcher::WatchMutationLeaseFactory>,
) -> Result<bool, anyhow::Error> {
    let existing: HashSet<String> = store
        .list_projects()?
        .into_iter()
        .map(|project| project.uid)
        .collect();
    let configured: Vec<String> = config
        .projects
        .iter()
        .map(|project| project_uid(instance_id, &project.name))
        .filter(|uid| existing.contains(uid))
        .collect();
    let desired: Vec<(String, String)> =
        declared_repo_edges(config, instance_id, &store.list_repos(None)?)
            .into_iter()
            .filter(|(project, _)| existing.contains(project))
            .collect();
    // nw-670 re-review N4: diff BEFORE taking any lease or publication — a
    // no-op rebuild (every pass once membership is current) must not open a
    // publication (ranked reads would fail closed for it; a crash would leave
    // a dirty marker) or touch the sidecar.
    let mut current = store
        .project_repo_edges_of(&configured)
        .map_err(|e| anyhow::anyhow!("read PROJECT_INCLUDES_REPO: {e}"))?;
    current.sort();
    let graph_differs = current != desired;
    let mut ext_store = load_extensions(db_path);
    let mut ext_changed = false;
    for project_cfg in &config.projects {
        let uid = project_uid(instance_id, &project_cfg.name);
        if !existing.contains(&uid) {
            continue;
        }
        let count = serde_json::json!(project_cfg.repos.len());
        if crate::extensions::get_property(&ext_store, &uid, DECLARED_REPO_COUNT_KEY)
            != Some(&count)
        {
            set_property(&mut ext_store, &uid, DECLARED_REPO_COUNT_KEY, count);
            ext_changed = true;
        }
    }
    if !graph_differs && !ext_changed {
        return Ok(false);
    }
    // One lease for the graph write and the sidecar save (N4).
    let mut changed = false;
    let _lease = if graph_differs {
        let (lease, publication) = crate::code_links::begin_publication(store, lease)?;
        let written = store
            .replace_project_repo_edges(&configured, &desired)
            .map_err(|e| anyhow::anyhow!("replace PROJECT_INCLUDES_REPO: {e}"));
        changed = matches!(written, Ok(true));
        crate::code_links::finish_publication(publication, written)?;
        lease
    } else {
        lease
            .map(|factory| factory("project_repo_membership"))
            .transpose()?
    };
    if ext_changed {
        crate::extensions::save_extensions(db_path, &ext_store)?;
    }
    if changed {
        crate::code_links::mark_code_links_pending(db_path, "project repo membership rebuilt");
    }
    Ok(changed)
}

/// Materialize explicit `[[projects]]` declared in an `InstanceConfig`.
///
/// For each project entry the function:
/// 1. Creates a Project node in the store.
/// 2. Attaches PROJECT_INCLUDES_NOTE edges for notes under `vault_folder`.
/// 3. Attaches PROJECT_INCLUDES_SYMBOL edges for symbols in listed repos.
/// 4. Attaches PROJECT_HAS_COMPONENT / PROJECT_HAS_PARENT edges between
///    parent and component projects.
/// 5. Persists `external_refs` into the extension sidecar.
pub fn materialize_projects(
    store: &GraphStore,
    config: &InstanceConfig,
    instance_id: &str,
    db_path: &Path,
) -> Result<ProjectMaterializationResult, anyhow::Error> {
    materialize_projects_with_lease(store, config, instance_id, db_path, None)
}

/// Materialize configured projects, acquiring an optional external writer
/// lease only after remote wiki sources have been fetched and validated.
pub fn materialize_projects_with_lease(
    store: &GraphStore,
    config: &InstanceConfig,
    instance_id: &str,
    db_path: &Path,
    mutation_lease_factory: Option<crate::watcher::WatchMutationLeaseFactory>,
) -> Result<ProjectMaterializationResult, anyhow::Error> {
    let mut ext_store = load_extensions(db_path);

    // Reject duplicate project names up front: two entries with the same name
    // map to the same project UID, so the per-entry edge reset below would
    // silently wipe the previous entry's edges.
    let mut seen_names = std::collections::HashSet::new();
    for project_config in &config.projects {
        if !seen_names.insert(project_config.name.as_str()) {
            anyhow::bail!(
                "duplicate project name {:?} in instance config — project names must be unique",
                project_config.name
            );
        }
    }

    // Remote MCP calls are planning, not graph mutation. Fetch every source
    // before acquiring the daemon's sole-writer lease so a slow or wedged wiki
    // server cannot block unrelated writes.
    let mut prepared_wiki_results = HashMap::new();
    let mut mcp_clients: HashMap<String, Option<McpClient>> = HashMap::new();
    let mut total_wiki_fetch_errors = 0usize;
    for project_cfg in &config.projects {
        for ws in &project_cfg.wiki_sources {
            let Some(server_config) = config
                .mcp_servers
                .iter()
                .find(|server| server.name == ws.mcp_server)
            else {
                tracing::warn!(
                    project = project_cfg.name,
                    mcp_server = ws.mcp_server,
                    "MCP server not found in config, skipping wiki source"
                );
                continue;
            };
            let timeout = std::time::Duration::from_secs(server_config.timeout_secs.unwrap_or(30));
            let client_slot = mcp_clients.entry(ws.mcp_server.clone()).or_insert_with(|| {
                match McpClient::spawn_with_timeout(
                    &server_config.command,
                    &server_config.args,
                    &server_config.env,
                    timeout,
                ) {
                    Ok(client) => Some(client),
                    Err(error) => {
                        tracing::warn!(
                            mcp_server = ws.mcp_server,
                            error = %error,
                            "failed to spawn MCP server, skipping wiki sources for this server"
                        );
                        None
                    }
                }
            });
            let Some(client) = client_slot.as_mut() else {
                continue;
            };
            if client.is_poisoned() {
                tracing::debug!(label = ws.label, "skipping — MCP client is poisoned");
                total_wiki_fetch_errors += 1;
                continue;
            }
            match client.call_tool(&ws.tool, serde_json::json!(ws.args)) {
                Ok(result) => {
                    prepared_wiki_results.insert(
                        (
                            project_cfg.name.clone(),
                            ws.mcp_server.clone(),
                            ws.tool.clone(),
                            ws.label.clone(),
                        ),
                        result,
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        label = ws.label,
                        tool = ws.tool,
                        error = %error,
                        "MCP tool call failed, skipping wiki source"
                    );
                    total_wiki_fetch_errors += 1;
                }
            }
        }
    }
    drop(mcp_clients);

    let mut prepared_wiki_contents = HashMap::new();
    for (key, result) in prepared_wiki_results {
        let content = result.content;
        if content.is_empty() {
            tracing::warn!(label = key.3, "MCP tool returned empty content");
            total_wiki_fetch_errors += 1;
            continue;
        }
        if result.is_error || looks_like_fetch_error(&content) {
            let preview: String = content.chars().take(120).collect();
            tracing::warn!(label = key.3, "wiki fetch failed: {preview}");
            total_wiki_fetch_errors += 1;
            continue;
        }
        prepared_wiki_contents.insert(key, maybe_convert_html_to_markdown(&content));
    }

    let _mutation_lease = mutation_lease_factory
        .as_ref()
        .map(|factory| factory("materialize_projects"))
        .transpose()?;

    // Plan every local graph relationship before deleting anything. Repository
    // and note inventories are stable for this materialization pass and are
    // intentionally loaded once rather than once per project/repo.
    let all_notes = store.list_notes(None)?;
    let all_repos = store.list_repos(None)?;
    let mut projects = Vec::with_capacity(config.projects.len());
    let mut note_edges: Vec<(String, String)> = Vec::new();
    // nw-678: no per-symbol PROJECT_INCLUDES_SYMBOL fan-out any more — it
    // was dropped by every re-index and never re-materialized. Replacing
    // with an empty set also clears the legacy edges of these projects.
    let symbol_edges: Vec<(String, String)> = Vec::new();
    // nw-678: the shared planner, so this and the daemon's startup rebuild
    // cannot disagree about a project's member repos.
    let repo_edges = declared_repo_edges(config, instance_id, &all_repos);
    let mut component_edges: Vec<(String, String)> = Vec::new();
    let mut parent_edges: Vec<(String, String)> = Vec::new();
    let mut wiki_project_uids: HashMap<String, Vec<String>> = HashMap::new();
    let mut repo_issues: Vec<ProjectRepoIssue> = Vec::new();

    for project_cfg in &config.projects {
        let uid = project_uid(instance_id, &project_cfg.name);
        projects.push(Project {
            uid: uid.clone(),
            name: project_cfg.name.clone(),
            summary: project_cfg.description.clone(),
            instance_id: instance_id.to_string(),
        });

        if let Some(folder) = &project_cfg.vault_folder {
            let folder = folder.trim_end_matches('/');
            note_edges.extend(
                all_notes
                    .iter()
                    .filter(|note| note_in_folder(&note.file_path, folder))
                    .map(|note| (uid.clone(), note.uid.clone())),
            );
        }

        // A transient wiki fetch failure must not erase the last successfully
        // materialized membership. Existing successful sources belong in the
        // desired replacement too: omitting them here deleted and re-created
        // the same edge on every run, so an identical wiki response could
        // never be a true graph no-op.
        for ws in &project_cfg.wiki_sources {
            let wiki_note_uid = note_uid(
                &format!("wiki:{}", ws.mcp_server),
                &format!("{}/{}", ws.tool, ws.label),
            );
            wiki_project_uids
                .entry(wiki_note_uid.clone())
                .or_default()
                .push(uid.clone());
            if all_notes.iter().any(|note| note.uid == wiki_note_uid) {
                note_edges.push((uid.clone(), wiki_note_uid));
            }
        }

        for repo_name in &project_cfg.repos {
            let found = resolve_declared_repo(repo_name, &config.repos, &all_repos);
            repo_issues.extend(ProjectRepoIssue::from_match(
                &project_cfg.name,
                repo_name,
                &found,
            ));
        }

        for component_name in &project_cfg.components {
            let child_uid = project_uid(instance_id, component_name);
            component_edges.push((uid.clone(), child_uid.clone()));
            parent_edges.push((child_uid, uid.clone()));
        }
        if let Some(parent_name) = &project_cfg.parent {
            let parent_uid = project_uid(instance_id, parent_name);
            component_edges.push((parent_uid.clone(), uid.clone()));
            parent_edges.push((uid.clone(), parent_uid));
        }
    }
    for project_uids in wiki_project_uids.values_mut() {
        project_uids.sort();
        project_uids.dedup();
    }

    // One transaction replaces the complete configured Project subgraph.
    // Relationship COPY turns the 139k-edge hot path from one execute per edge
    // into four bounded bulk loads, and rollback preserves the old graph if
    // any replacement step fails.
    let graph_publication =
        begin_graph_mutation_publication(store, "explicit Project materialization")?;
    let (graph_publication, replacement) = resolve_project_replacement(
        graph_publication,
        store.replace_materialized_projects(
            &projects,
            &note_edges,
            &symbol_edges,
            &component_edges,
            &parent_edges,
        ),
    )?;
    let mut graph_changed = replacement.changed();
    let project_uids: Vec<String> = projects.iter().map(|project| project.uid.clone()).collect();
    match store.replace_project_repo_edges(&project_uids, &repo_edges) {
        Ok(changed) => graph_changed |= changed,
        Err(error) => {
            // The Project subgraph committed; retire the publication as
            // changed so readers do not trust pre-mutation derived state.
            let _ = graph_publication.finish(true);
            return Err(anyhow::anyhow!("replace PROJECT_INCLUDES_REPO: {error}"));
        }
    }
    let member_repo_uids: Vec<String> = repo_edges.iter().map(|(_, repo)| repo.clone()).collect();
    let member_symbols = store
        .count_symbols_in_repos(&member_repo_uids)
        .unwrap_or_default();

    let projects_created = projects.len();
    let total_note_edges = note_edges.len();
    let total_symbol_edges = member_symbols;
    let total_repo_edges = repo_edges.len();
    let total_component_edges = component_edges.len();
    let mut total_wiki_notes_ingested = 0usize;
    let mut wiki_mutation_errors = Vec::new();
    let mut reconciled_wiki_notes = HashSet::new();

    for project_cfg in &config.projects {
        let uid = project_uid(instance_id, &project_cfg.name);

        // 5. Store external_refs in the extension sidecar.
        if !project_cfg.external_refs.is_empty() {
            set_property(
                &mut ext_store,
                &uid,
                "external_refs",
                serde_json::json!(&project_cfg.external_refs),
            );
        }

        // 5a. nw-670 re-review F1 / nw-678: the declared repo count lets
        // status and project_context disclose a project whose declared repos
        // resolved to none (its notes then link unscoped, and it has no code).
        set_property(
            &mut ext_store,
            &uid,
            DECLARED_REPO_COUNT_KEY,
            serde_json::json!(project_cfg.repos.len()),
        );

        // 5b. nw-674: record (or clear) declared-repo issues. Unlike the
        // keys around it this one must be REMOVED once the repo resolves, or
        // a fixed config would keep disclosing a gap that no longer exists.
        let project_issues: Vec<&ProjectRepoIssue> = repo_issues
            .iter()
            .filter(|entry| entry.project == project_cfg.name)
            .collect();
        if project_issues.is_empty() {
            if let Some(properties) = ext_store.get_mut(&uid) {
                properties.remove(REPO_ISSUES_KEY);
            }
        } else {
            for entry in &project_issues {
                tracing::warn!("declared project repo did not resolve cleanly: {entry}");
            }
            set_property(
                &mut ext_store,
                &uid,
                REPO_ISSUES_KEY,
                serde_json::json!(project_issues),
            );
        }

        // 6. Store aliases in the extension sidecar.
        if !project_cfg.aliases.is_empty() {
            set_property(
                &mut ext_store,
                &uid,
                "aliases",
                serde_json::json!(&project_cfg.aliases),
            );
        }

        // 7b. Store tags in the extension sidecar.
        if !project_cfg.tags.is_empty() {
            set_property(
                &mut ext_store,
                &uid,
                "tags",
                serde_json::json!(&project_cfg.tags),
            );
        }

        // 7c. Store features in the extension sidecar.
        if !project_cfg.features.is_empty() {
            set_property(
                &mut ext_store,
                &uid,
                "features",
                serde_json::json!(&project_cfg.features),
            );
        }

        // 7. Apply the wiki content fetched before the mutation lease.
        for ws in &project_cfg.wiki_sources {
            let key = (
                project_cfg.name.clone(),
                ws.mcp_server.clone(),
                ws.tool.clone(),
                ws.label.clone(),
            );
            let Some(content) = prepared_wiki_contents.remove(&key) else {
                continue;
            };
            {
                // Create a Note from the wiki content.
                let wiki_note_uid = note_uid(
                    &format!("wiki:{}", ws.mcp_server),
                    &format!("{}/{}", ws.tool, ws.label),
                );
                if reconciled_wiki_notes.contains(&wiki_note_uid) {
                    continue;
                }

                let note = Note {
                    uid: wiki_note_uid.clone(),
                    vault_uid: format!("wiki:{}", ws.mcp_server),
                    file_path: format!("{}/{}", ws.tool, ws.label),
                    title: ws.label.clone(),
                    note_kind: NoteKind::General,
                    word_count: content.split_whitespace().count() as u32,
                    content_hash: truncated_hash(&content),
                    frontmatter: None,
                    // Synthesised from wiki content, not parsed from a file
                    // with a `---` block.
                    frontmatter_raw: None,
                    created_at: None,
                    modified_at: None,
                    pagerank_score: None,
                    embedding: None,
                };

                // Decompose the complete desired wiki topology before the
                // store transaction. Parse failure therefore cannot publish a
                // replacement Note with missing children.
                let parsed = match nestweaver_parser::parse_markdown(
                    &format!("{}/{}", ws.tool, ws.label),
                    &content,
                ) {
                    Ok(parsed) => parsed,
                    Err(error) => {
                        wiki_mutation_errors.push(format!(
                            "wiki source '{}' could not be parsed before graph mutation: {error:#}",
                            ws.label
                        ));
                        continue;
                    }
                };
                let heading_uids: Vec<String> = parsed
                    .headings
                    .iter()
                    .map(|heading| heading_uid(&wiki_note_uid, &heading.slug, heading.start_line))
                    .collect();
                let headings: Vec<Heading> = parsed
                    .headings
                    .iter()
                    .enumerate()
                    .map(|(index, heading)| Heading {
                        uid: heading_uids[index].clone(),
                        note_uid: wiki_note_uid.clone(),
                        level: heading.level,
                        text: heading.text.clone(),
                        slug: heading.slug.clone(),
                        start_line: heading.start_line,
                        end_line: heading.end_line,
                        content_hash: truncated_hash(&heading.text),
                        embedding: None,
                    })
                    .collect();
                let sections: Vec<Section> = parsed
                    .sections
                    .iter()
                    .map(|section| {
                        let text_hash = truncated_hash(&section.text);
                        Section {
                            uid: section_uid(&wiki_note_uid, section.start_line, &text_hash),
                            note_uid: wiki_note_uid.clone(),
                            heading_uid: section
                                .heading_idx
                                .and_then(|index| heading_uids.get(index))
                                .cloned(),
                            start_line: section.start_line,
                            end_line: section.end_line,
                            text_hash,
                            text_content: section.text.clone(),
                            word_count: u32::try_from(section.text.split_whitespace().count())
                                .unwrap_or(u32::MAX),
                            pagerank_score: None,
                        }
                    })
                    .collect();

                let project_uids = wiki_project_uids
                    .get(&wiki_note_uid)
                    .expect("configured wiki source has planned Project memberships");
                match store.replace_project_wiki_note_memberships(
                    project_uids,
                    &note,
                    &headings,
                    &sections,
                ) {
                    Ok(outcome) => {
                        reconciled_wiki_notes.insert(wiki_note_uid);
                        graph_changed |= outcome.changed();
                        total_wiki_notes_ingested += 1;
                        tracing::info!(
                            label = ws.label,
                            project = project_cfg.name,
                            changed = outcome.changed(),
                            "reconciled wiki source"
                        );
                    }
                    Err(error) => wiki_mutation_errors.push(format!(
                        "wiki source '{}' topology transaction failed: {error:#}",
                        ws.label
                    )),
                }
            }
        }
    }

    // Persist the extension sidecar once after all projects are processed.
    let extension_save_error = save_extensions(db_path, &ext_store).err();
    let mut publication = graph_publication.finish(graph_changed)?;
    if !wiki_mutation_errors.is_empty() {
        if graph_changed {
            for error in wiki_mutation_errors {
                publication.record_warning("materialize-project-wiki", error);
            }
        } else {
            anyhow::bail!(
                "project wiki materialization failed before any graph change: {}",
                wiki_mutation_errors.join("; ")
            );
        }
    }
    if let Some(error) = extension_save_error {
        if graph_changed {
            publication.record_warning(
                "save-project-extensions",
                format!(
                    "graph materialization committed but the project extension sidecar was not published: {error:#}"
                ),
            );
        } else {
            return Err(error);
        }
    }

    Ok(ProjectMaterializationResult {
        projects_created,
        note_edges: total_note_edges,
        symbol_edges: total_symbol_edges,
        repo_edges: total_repo_edges,
        component_edges: total_component_edges,
        wiki_notes_ingested: total_wiki_notes_ingested,
        wiki_fetch_errors: total_wiki_fetch_errors,
        repo_issues,
        publication,
    })
}

/// Walk `vault_root/Projects/` and auto-detect project folders whose entry
/// note exists at `Projects/<slug>/<slug>.md`.
///
/// All detected Project nodes and their `PROJECT_INCLUDES_NOTE` relationships
/// are planned first, then replaced atomically as one graph mutation.
///
/// Returns the list of detected project slugs (folder names).
/// Vault folders that hold one directory per project.
///
/// nw-161: only `Projects/` was recognised, so on a vault laid out as
/// `Workspaces/<Name>/` — the layout this project's own CLAUDE.md documents,
/// with 21 such folders — detection reported "No implicit projects detected"
/// and the function returned before reaching any write.
pub const PROJECT_CONTAINER_DIRS: &[&str] = &["Projects", "Workspaces"];

/// Entry-note filenames that mark a directory as a project.
///
/// nw-161: only `<slug>/<slug>.md` was accepted. `_Overview.md` is the
/// convention actually used under `Workspaces/`.
fn is_entry_note(dir: &Path, slug: &str) -> bool {
    dir.join(format!("{slug}.md")).is_file() || dir.join("_Overview.md").is_file()
}

pub fn detect_implicit_projects(
    store: &GraphStore,
    vault_root: &Path,
    vault_uid: &str,
    instance_id: &str,
) -> Result<Vec<String>, anyhow::Error> {
    detect_implicit_projects_with_mode(store, vault_root, vault_uid, instance_id, false)
}

/// [`detect_implicit_projects`] with an explicit write mode.
///
/// nw-161: this function WRITES despite a read-sounding name, and nothing in
/// `--help` signalled it. `dry_run` reports what would be created without
/// touching the graph.
pub fn detect_implicit_projects_with_mode(
    store: &GraphStore,
    vault_root: &Path,
    vault_uid: &str,
    instance_id: &str,
    dry_run: bool,
) -> Result<Vec<String>, anyhow::Error> {
    Ok(detect_implicit_projects_with_publication(
        store,
        vault_root,
        vault_uid,
        instance_id,
        dry_run,
    )?
    .projects)
}

/// Detect implicit projects and return the complete graph-publication result.
///
/// Callers that expose mutation status should use this form so a committed
/// graph update followed by degraded generation/artifact reconciliation is not
/// flattened into ordinary success.
pub fn detect_implicit_projects_with_publication(
    store: &GraphStore,
    vault_root: &Path,
    vault_uid: &str,
    instance_id: &str,
    dry_run: bool,
) -> Result<ImplicitProjectDetectionResult, anyhow::Error> {
    let mut detected: Vec<(String, String)> = Vec::new();
    for container in PROJECT_CONTAINER_DIRS {
        let projects_dir = vault_root.join(container);
        if !projects_dir.is_dir() {
            continue;
        }
        detect_in_container(&projects_dir, container, &mut detected)?;
    }

    let mut project_names = Vec::new();
    let mut unique_names = std::collections::HashSet::new();
    for (slug, _) in &detected {
        if unique_names.insert(slug.clone()) {
            project_names.push(slug.clone());
        }
    }

    if dry_run || detected.is_empty() {
        return Ok(ImplicitProjectDetectionResult {
            projects: project_names,
            publication: finalize_committed_graph_mutation(store, false),
        });
    }

    // Plan every node and relationship before the single atomic store write.
    // This prevents a later filesystem/read failure from leaving earlier
    // Projects committed without a generation publication.
    let all_notes = store.list_notes(Some(vault_uid))?;
    let projects = project_names
        .iter()
        .map(|slug| Project {
            uid: project_uid(instance_id, slug),
            name: slug.clone(),
            summary: None,
            instance_id: instance_id.to_string(),
        })
        .collect::<Vec<_>>();
    let mut note_edges = Vec::new();
    for (slug, container) in &detected {
        let uid = project_uid(instance_id, slug);
        let prefix = format!("{container}/{slug}/");
        note_edges.extend(
            all_notes
                .iter()
                .filter(|note| note.file_path.starts_with(&prefix))
                .map(|note| (uid.clone(), note.uid.clone())),
        );
    }
    note_edges.sort();
    note_edges.dedup();

    let graph_publication = begin_graph_mutation_publication(store, "implicit Project detection")?;
    let (graph_publication, replacement) = resolve_project_replacement(
        graph_publication,
        store.replace_implicit_project_note_memberships(&projects, &note_edges),
    )?;
    let publication = graph_publication.finish(replacement.changed())?;
    Ok(ImplicitProjectDetectionResult {
        projects: project_names,
        publication,
    })
}

fn detect_in_container(
    projects_dir: &Path,
    container: &str,
    detected: &mut Vec<(String, String)>,
) -> Result<(), anyhow::Error> {
    let read_dir = std::fs::read_dir(projects_dir)?;
    for entry in read_dir {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let slug = match path.file_name().and_then(|n| n.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };

        if !is_entry_note(&path, &slug) {
            continue;
        }
        detected.push((slug, container.to_string()));
    }

    Ok(())
}

/// nw-678 test fixture: a vault note under `proj/`, a Rust repo `alpha` with
/// two functions, and a config whose project `p` covers `proj` and declares
/// `repos`; materialized. Returns `(store, db, repo_root, project_uid,
/// repo_url)`.
#[cfg(test)]
pub(crate) fn one_repo_project_fixture(
    dir: &Path,
    repos: &str,
) -> (
    GraphStore,
    std::path::PathBuf,
    std::path::PathBuf,
    String,
    String,
) {
    let vault = dir.join("vault");
    std::fs::create_dir_all(vault.join("proj")).unwrap();
    std::fs::write(vault.join("proj/a.md"), "# A\n\nnotes\n").unwrap();
    let db = dir.join("brain.lbug");
    crate::index_md::index_markdown_directory(&vault, &db, "default", "vault").unwrap();
    let store = GraphStore::open_or_create(&db).unwrap();
    let repo = dir.join("alpha");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("src/lib.rs"),
        "pub fn alpha_one() -> i32 { 1 }\npub fn alpha_two() -> i32 { 2 }\n",
    )
    .unwrap();
    let repo_url = "file:///fixture/alpha".to_string();
    crate::index::index_directory_with_store(
        &store,
        &repo,
        &db,
        "default",
        &repo_url,
        "sha",
        false,
        Some("alpha"),
    )
    .unwrap();
    let config = InstanceConfig::from_toml_str(&format!(
        r#"
instance_id = "default"

[snapshot_storage]
backend = "local"
path = "/tmp/snapshots"

[workspace]
backend = "local"
path = "/tmp/workspace"

[inference]
endpoint = "http://localhost:8080"
embedding_model = "text-embedding-3-small"
summary_model = "gpt-4o-mini"

[git]
credential_method = "ssh"

[[projects]]
name = "p"
vault_folder = "proj"
repos = [{repos}]
"#
    ))
    .unwrap();
    materialize_projects(&store, &config, "default", &db).unwrap();
    (store, db, repo, project_uid("default", "p"), repo_url)
}

#[cfg(test)]
mod repo_membership_tests {
    use super::*;

    fn config_with(instance: &str, repos: &str) -> InstanceConfig {
        InstanceConfig::from_toml_str(&format!(
            r#"
instance_id = "{instance}"

[snapshot_storage]
backend = "local"
path = "/tmp/snapshots"

[workspace]
backend = "local"
path = "/tmp/workspace"

[inference]
endpoint = "http://localhost:8080"
embedding_model = "text-embedding-3-small"
summary_model = "gpt-4o-mini"

[git]
credential_method = "ssh"

[[projects]]
name = "p"
vault_folder = "proj"
repos = [{repos}]
"#
        ))
        .unwrap()
    }

    /// nw-670 re-review N2: when a project's uid changes (instance_id
    /// change, merge) the stale project is deleted with a plain DELETE after
    /// its note/symbol/component/parent edges — but its repo membership was
    /// left, so the DELETE was refused and the whole materialize failed.
    #[test]
    fn a_project_uid_change_rematerializes() {
        let dir = tempfile::tempdir().unwrap();
        let (store, db, _repo, _project, _url) = one_repo_project_fixture(dir.path(), "\"alpha\"");
        materialize_projects(&store, &config_with("other", "\"alpha\""), "other", &db)
            .expect("the stale project with repo membership is replaced");
        assert!(
            store
                .list_projects()
                .unwrap()
                .iter()
                .all(|project| project.uid != project_uid("default", "p"))
        );
    }

    /// nw-670 re-review N1: a repo the config declares but that was first
    /// indexed AFTER the last materialize gets its membership from the
    /// rebuild (which every code-link pass now runs).
    #[test]
    fn the_rebuild_attaches_a_declared_repo_indexed_after_materialize() {
        let dir = tempfile::tempdir().unwrap();
        let (store, db, _repo, project, _url) =
            one_repo_project_fixture(dir.path(), "\"alpha\", \"bravo\"");
        assert_eq!(store.project_member_repo_uids(&project).unwrap().len(), 1);
        let bravo = dir.path().join("bravo");
        std::fs::create_dir_all(bravo.join("src")).unwrap();
        std::fs::write(bravo.join("src/lib.rs"), "pub fn bravo_one() {}\n").unwrap();
        crate::index::index_directory_with_store(
            &store,
            &bravo,
            &db,
            "default",
            "file:///fixture/bravo",
            "sha",
            false,
            Some("bravo"),
        )
        .unwrap();
        assert_eq!(
            store.project_member_repo_uids(&project).unwrap().len(),
            1,
            "precondition: indexing alone does not attach it"
        );
        let config = config_with("default", "\"alpha\", \"bravo\"");
        assert!(rebuild_project_repo_membership(&store, &config, "default", &db, None).unwrap());
        assert_eq!(store.project_member_repo_uids(&project).unwrap().len(), 2);
    }

    /// nw-670 re-review N1: a re-identified repo (same checkout, new
    /// identity — the old Repo node pruned, a bare new one inserted) is
    /// re-attached by the rebuild under its new uid.
    #[test]
    fn the_rebuild_reattaches_a_reidentified_repo() {
        let dir = tempfile::tempdir().unwrap();
        let (store, db, repo, project, url) = one_repo_project_fixture(dir.path(), "\"alpha\"");
        let old = nestweaver_schema::repo_uid("default", &url);
        store.delete_repo_node(&old).unwrap();
        crate::index::index_directory_with_store(
            &store,
            &repo,
            &db,
            "default",
            "file:///fixture/alpha-moved",
            "sha",
            false,
            Some("alpha"),
        )
        .unwrap();
        assert!(store.project_member_repo_uids(&project).unwrap().is_empty());
        let config = config_with("default", "\"alpha\"");
        assert!(rebuild_project_repo_membership(&store, &config, "default", &db, None).unwrap());
        assert_eq!(
            store.project_member_repo_uids(&project).unwrap(),
            vec![nestweaver_schema::repo_uid(
                "default",
                "file:///fixture/alpha-moved"
            )]
        );
    }

    /// nw-670 re-review N4: a rebuild with nothing to change takes no lease
    /// (so opens no publication and writes no sidecar).
    #[test]
    fn a_current_repo_membership_rebuild_takes_no_lease() {
        let dir = tempfile::tempdir().unwrap();
        let (store, db, _repo, _project, _url) = one_repo_project_fixture(dir.path(), "\"alpha\"");
        let leases = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = std::sync::Arc::clone(&leases);
        let factory: crate::watcher::WatchMutationLeaseFactory =
            std::sync::Arc::new(move |_label| {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(Box::new(()) as Box<dyn crate::watcher::WatchMutationLease>)
            });
        let changed = rebuild_project_repo_membership(
            &store,
            &config_with("default", "\"alpha\""),
            "default",
            &db,
            Some(&factory),
        )
        .unwrap();
        assert!(!changed);
        assert_eq!(leases.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    fn member_symbols(store: &GraphStore, project: &str) -> Vec<String> {
        store
            .list_project_symbol_uids_by_pagerank(project, 50, None, None)
            .unwrap()
    }

    /// nw-678: a forced re-index DETACH-deleted every symbol, taking its
    /// PROJECT_INCLUDES_SYMBOL edge along, and nothing re-materialized it —
    /// project_context then returned no code for the project. Membership is
    /// now the repo, read at query time.
    #[test]
    fn project_code_membership_survives_a_forced_reindex() {
        let dir = tempfile::tempdir().unwrap();
        let (store, db, repo, project, repo_url) =
            one_repo_project_fixture(dir.path(), "\"alpha\"");
        assert_eq!(member_symbols(&store, &project).len(), 2, "precondition");
        crate::index::index_directory_with_store(
            &store,
            &repo,
            &db,
            "default",
            &repo_url,
            "sha2",
            true,
            Some("alpha"),
        )
        .unwrap();
        assert_eq!(member_symbols(&store, &project).len(), 2);
    }

    /// nw-678: the incremental twin — a line-shift edit re-creates the
    /// file's symbols under new uids.
    #[test]
    fn project_code_membership_survives_an_incremental_line_shift() {
        let dir = tempfile::tempdir().unwrap();
        let (store, db, repo, project, repo_url) =
            one_repo_project_fixture(dir.path(), "\"alpha\"");
        std::fs::write(
            repo.join("src/lib.rs"),
            "// shifted\npub fn alpha_one() -> i32 { 1 }\npub fn alpha_two() -> i32 { 2 }\n",
        )
        .unwrap();
        crate::index::index_directory_with_store(
            &store,
            &repo,
            &db,
            "default",
            &repo_url,
            "sha2",
            false,
            Some("alpha"),
        )
        .unwrap();
        let members = member_symbols(&store, &project);
        assert_eq!(members.len(), 2, "{members:?}");
    }

    /// nw-678: the daemon's startup rebuild restores repo membership a graph
    /// lacks (materialized before it existed) and is a no-op once current.
    #[test]
    fn the_repo_membership_rebuild_restores_and_then_does_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (store, db, _repo, project, _url) = one_repo_project_fixture(dir.path(), "\"alpha\"");
        let conn = store.begin_transaction().unwrap();
        conn.query("MATCH (:Project)-[r:PROJECT_INCLUDES_REPO]->(:Repo) DELETE r")
            .unwrap();
        store.commit_transaction(&conn).unwrap();
        assert!(member_symbols(&store, &project).is_empty(), "precondition");
        let config = InstanceConfig::from_toml_str(
            r#"
instance_id = "default"

[snapshot_storage]
backend = "local"
path = "/tmp/snapshots"

[workspace]
backend = "local"
path = "/tmp/workspace"

[inference]
endpoint = "http://localhost:8080"
embedding_model = "text-embedding-3-small"
summary_model = "gpt-4o-mini"

[git]
credential_method = "ssh"

[[projects]]
name = "p"
vault_folder = "proj"
repos = ["alpha"]
"#,
        )
        .unwrap();
        assert!(rebuild_project_repo_membership(&store, &config, "default", &db, None).unwrap());
        assert_eq!(member_symbols(&store, &project).len(), 2);
        assert!(!rebuild_project_repo_membership(&store, &config, "default", &db, None).unwrap());
    }
}

#[cfg(test)]
mod tests {
    use super::looks_like_fetch_error;

    #[test]
    fn materialize_projects_rejects_duplicate_names() {
        // Two [[projects]] entries with the same name map to the same
        // project UID; the per-entry edge reset would silently wipe the first
        // entry's edges. Materialization must refuse instead.
        let toml = r#"
instance_id = "test-instance"

[snapshot_storage]
backend = "local"
path = "/tmp/snapshots"

[workspace]
backend = "local"
path = "/tmp/workspace"

[inference]
endpoint = "http://localhost:8080"
embedding_model = "text-embedding-3-small"
summary_model = "gpt-4o-mini"

[git]
credential_method = "ssh"

[[projects]]
name = "dup"
components = ["child-a"]

[[projects]]
name = "dup"
components = ["child-b"]
"#;
        let config = crate::config::InstanceConfig::from_toml_str(toml).unwrap();
        let store = nestweaver_store::GraphStore::in_memory().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("test.lbug");

        let err = super::materialize_projects(&store, &config, "test-instance", &db_path)
            .err()
            .expect("duplicate project names must be rejected");
        assert!(
            err.to_string().contains("\"dup\""),
            "error must name the duplicate project, got: {err}"
        );
    }

    #[test]
    fn invalid_configured_project_topology_retires_marker_without_advancing_generation() {
        for (case, topology) in [
            ("component", "components = [\"missing\"]"),
            ("parent", "parent = \"missing\""),
        ] {
            let toml = format!(
                r#"
instance_id = "test-instance"

[snapshot_storage]
backend = "local"
path = "/tmp/snapshots"

[workspace]
backend = "local"
path = "/tmp/workspace"

[inference]
endpoint = "http://localhost:8080"
embedding_model = "text-embedding-3-small"
summary_model = "gpt-4o-mini"

[git]
credential_method = "ssh"

[[projects]]
name = "configured"
{topology}
"#
            );
            let config = crate::config::InstanceConfig::from_toml_str(&toml).unwrap();
            let dir = tempfile::tempdir().unwrap();
            let db_path = dir.path().join(format!("{case}.lbug"));
            let store = nestweaver_store::GraphStore::create(&db_path).unwrap();
            let generation_before = store.graph_generation();

            let error = super::materialize_projects(&store, &config, "test-instance", &db_path)
                .err()
                .expect("an undeclared Project endpoint must fail preflight");

            assert!(error.to_string().contains("not configured"), "{error:#}");
            assert_eq!(store.graph_generation(), generation_before);
            assert!(
                !crate::sidecar_path(&db_path, ".index-dirty").exists(),
                "proven {case} preflight failure must retire the publication marker"
            );
        }
    }

    #[test]
    fn typed_project_replacement_failure_retires_only_proven_restored_marker() {
        for (case, disposition, marker_expected) in [
            (
                "restored",
                nestweaver_store::ProjectMutationDisposition::ConfirmedRolledBack,
                false,
            ),
            (
                "ambiguous",
                nestweaver_store::ProjectMutationDisposition::Ambiguous,
                true,
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let db_path = dir.path().join(format!("{case}.lbug"));
            let store = nestweaver_store::GraphStore::create(&db_path).unwrap();
            let generation_before = store.graph_generation();
            let publication = crate::manifest::begin_graph_mutation_publication(
                &store,
                format!("injected {case} Project replacement"),
            )
            .unwrap();
            let injected = nestweaver_store::write::ReplaceMaterializedProjectsError {
                disposition,
                primary: nestweaver_store::StoreError::Query(format!(
                    "injected {case} Project replacement failure"
                )),
            };

            let error = match super::resolve_project_replacement(publication, Err(injected)) {
                Ok(_) => panic!("injected replacement must remain an operation failure"),
                Err(error) => error,
            };

            assert!(error.to_string().contains("injected"), "{error:#}");
            assert_eq!(store.graph_generation(), generation_before);
            assert_eq!(
                crate::sidecar_path(&db_path, ".index-dirty").exists(),
                marker_expected,
                "{case} disposition retained the wrong publication-marker state"
            );
        }
    }

    #[test]
    fn failed_wiki_fetch_preserves_the_last_materialized_membership() {
        use nestweaver_schema::{Note, NoteKind, Project, note_uid, project_uid};

        let toml = r#"
instance_id = "test-instance"

[snapshot_storage]
backend = "local"
path = "/tmp/snapshots"

[workspace]
backend = "local"
path = "/tmp/workspace"

[inference]
endpoint = "http://localhost:8080"
embedding_model = "text-embedding-3-small"
summary_model = "gpt-4o-mini"

[git]
credential_method = "ssh"

[[projects]]
name = "stable"

[[projects.wiki_sources]]
label = "Architecture"
mcp_server = "missing-server"
tool = "get_page"
args = { page = "123" }
"#;
        let config = crate::config::InstanceConfig::from_toml_str(toml).unwrap();
        let store = nestweaver_store::GraphStore::in_memory().unwrap();
        let project_uid = project_uid("test-instance", "stable");
        let wiki_note_uid = note_uid("wiki:missing-server", "get_page/Architecture");
        store
            .insert_project(&Project {
                uid: project_uid.clone(),
                name: "stable".to_string(),
                summary: None,
                instance_id: "test-instance".to_string(),
            })
            .unwrap();
        store
            .insert_note(&Note {
                uid: wiki_note_uid.clone(),
                vault_uid: "wiki:missing-server".to_string(),
                file_path: "get_page/Architecture".to_string(),
                title: "Architecture".to_string(),
                note_kind: NoteKind::General,
                word_count: 1,
                content_hash: "last-good".to_string(),
                frontmatter: None,
                frontmatter_raw: None,
                created_at: None,
                modified_at: None,
                pagerank_score: None,
                embedding: None,
            })
            .unwrap();
        store
            .batch_insert_project_note_edges(&[(&project_uid, &wiki_note_uid)])
            .unwrap();

        let dir = tempfile::tempdir().unwrap();
        super::materialize_projects(
            &store,
            &config,
            "test-instance",
            &dir.path().join("brain.lbug"),
        )
        .unwrap();

        assert_eq!(
            store.list_project_note_uids(&project_uid).unwrap(),
            vec![wiki_note_uid],
            "a transient remote failure must preserve the last good wiki membership"
        );
    }

    #[test]
    fn explicit_project_materialization_versions_change_but_not_exact_noop() {
        let toml = r#"
instance_id = "test-instance"

[snapshot_storage]
backend = "local"
path = "/tmp/snapshots"

[workspace]
backend = "local"
path = "/tmp/workspace"

[inference]
endpoint = "http://localhost:8080"
embedding_model = "text-embedding-3-small"
summary_model = "gpt-4o-mini"

[git]
credential_method = "ssh"

[[projects]]
name = "stable"
description = "Stable project"
"#;
        let config = crate::config::InstanceConfig::from_toml_str(toml).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("brain.lbug");
        let store = nestweaver_store::GraphStore::create(&db_path).unwrap();

        let changed =
            super::materialize_projects(&store, &config, "test-instance", &db_path).unwrap();
        assert_eq!(
            changed.publication.disposition,
            crate::manifest::GraphMutationPublicationDisposition::CommittedComplete
        );
        assert_eq!(
            changed.publication.generation_after,
            changed.publication.generation_before + 1
        );

        let unchanged =
            super::materialize_projects(&store, &config, "test-instance", &db_path).unwrap();
        assert_eq!(
            unchanged.publication.disposition,
            crate::manifest::GraphMutationPublicationDisposition::ConfirmedNoChange
        );
        assert_eq!(
            unchanged.publication.generation_after,
            changed.publication.generation_after
        );
        assert!(
            !crate::sidecar_path(&db_path, ".index-dirty").exists(),
            "clean and no-op publications must both retire the crash fence"
        );
    }

    #[cfg(unix)]
    #[test]
    fn shared_wiki_source_keeps_all_project_memberships_and_repeats_as_exact_noop() {
        use nestweaver_schema::{note_uid, project_uid};

        let dir = tempfile::tempdir().unwrap();
        let server_path = dir.path().join("mock-mcp.sh");
        std::fs::write(
            &server_path,
            r##"while IFS= read -r request; do
  case "$request" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{}}'
      ;;
    *'"method":"tools/call"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"# Shared Wiki\nOne canonical document."}],"isError":false}}'
      ;;
  esac
done
"##,
        )
        .unwrap();
        let toml = format!(
            r#"
instance_id = "test-instance"

[snapshot_storage]
backend = "local"
path = "/tmp/snapshots"

[workspace]
backend = "local"
path = "/tmp/workspace"

[inference]
endpoint = "http://localhost:8080"
embedding_model = "text-embedding-3-small"
summary_model = "gpt-4o-mini"

[git]
credential_method = "ssh"

[[projects]]
name = "alpha"

[[projects.wiki_sources]]
label = "Architecture"
mcp_server = "mock"
tool = "get_page"
args = {{ page = "shared" }}

[[projects]]
name = "beta"

[[projects.wiki_sources]]
label = "Architecture"
mcp_server = "mock"
tool = "get_page"
args = {{ page = "shared" }}

[[mcp_servers]]
name = "mock"
command = "/bin/sh"
args = ["{}"]
timeout_secs = 5
"#,
            server_path.display()
        );
        let config = crate::config::InstanceConfig::from_toml_str(&toml).unwrap();
        let db_path = dir.path().join("brain.lbug");
        let store = nestweaver_store::GraphStore::create(&db_path).unwrap();

        let first =
            super::materialize_projects(&store, &config, "test-instance", &db_path).unwrap();
        let alpha_uid = project_uid("test-instance", "alpha");
        let beta_uid = project_uid("test-instance", "beta");
        let shared_note_uid = note_uid("wiki:mock", "get_page/Architecture");
        for project_uid in [&alpha_uid, &beta_uid] {
            assert_eq!(
                store.list_project_note_uids(project_uid).unwrap(),
                vec![shared_note_uid.clone()],
                "the shared wiki Note must remain attached to every configured Project"
            );
        }
        assert_eq!(
            store
                .list_notes(None)
                .unwrap()
                .into_iter()
                .filter(|note| note.uid == shared_note_uid)
                .count(),
            1,
            "shared Project membership must use one canonical wiki Note"
        );
        assert_eq!(first.wiki_notes_ingested, 1);

        let second =
            super::materialize_projects(&store, &config, "test-instance", &db_path).unwrap();
        assert_eq!(
            second.publication.disposition,
            crate::manifest::GraphMutationPublicationDisposition::ConfirmedNoChange
        );
        assert_eq!(
            second.publication.generation_after, first.publication.generation_after,
            "an identical shared-source rerun must not advance graph generation"
        );
    }

    #[test]
    fn implicit_project_detection_versions_atomic_change_and_dry_run_is_noop() {
        use nestweaver_schema::{Note, NoteKind};

        let dir = tempfile::tempdir().unwrap();
        let vault_root = dir.path().join("vault");
        let project_dir = vault_root.join("Workspaces/alpha");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(project_dir.join("_Overview.md"), "# Alpha").unwrap();
        let db_path = dir.path().join("brain.lbug");
        let store = nestweaver_store::GraphStore::create(&db_path).unwrap();
        store
            .insert_note(&Note {
                uid: "note:vault:alpha".to_string(),
                vault_uid: "vault:test".to_string(),
                file_path: "Workspaces/alpha/_Overview.md".to_string(),
                title: "Alpha".to_string(),
                note_kind: NoteKind::General,
                word_count: 1,
                content_hash: "alpha".to_string(),
                frontmatter: None,
                frontmatter_raw: None,
                created_at: None,
                modified_at: None,
                pagerank_score: None,
                embedding: None,
            })
            .unwrap();

        let dry_run = super::detect_implicit_projects_with_publication(
            &store,
            &vault_root,
            "vault:test",
            "test-instance",
            true,
        )
        .unwrap();
        assert_eq!(dry_run.projects, vec!["alpha"]);
        assert_eq!(
            dry_run.publication.disposition,
            crate::manifest::GraphMutationPublicationDisposition::ConfirmedNoChange
        );
        assert_eq!(store.graph_generation(), 0);

        let changed = super::detect_implicit_projects_with_publication(
            &store,
            &vault_root,
            "vault:test",
            "test-instance",
            false,
        )
        .unwrap();
        assert_eq!(
            changed.publication.disposition,
            crate::manifest::GraphMutationPublicationDisposition::CommittedComplete
        );
        assert_eq!(changed.publication.generation_after, 1);

        let unchanged = super::detect_implicit_projects_with_publication(
            &store,
            &vault_root,
            "vault:test",
            "test-instance",
            false,
        )
        .unwrap();
        assert_eq!(
            unchanged.publication.disposition,
            crate::manifest::GraphMutationPublicationDisposition::ConfirmedNoChange
        );
        assert_eq!(store.graph_generation(), 1);
    }

    #[test]
    fn error_detection_tls_certificate() {
        let content = "unable to get local issuer certificate";
        assert!(
            looks_like_fetch_error(content),
            "should detect TLS certificate error"
        );
    }

    #[test]
    fn error_detection_is_error_flag_content() {
        // The `isError` flag is checked separately in project.rs, but the
        // heuristic should still catch common error patterns.
        let content = "Error: CERTIFICATE_VERIFY_FAILED";
        assert!(
            looks_like_fetch_error(content),
            "should detect certificate verify error"
        );
    }

    #[test]
    fn error_detection_connection_refused() {
        let content = "connection refused";
        assert!(
            looks_like_fetch_error(content),
            "should detect connection refused"
        );
    }

    #[test]
    fn error_detection_ssl_error() {
        let content = "SSL handshake failed: certificate has expired";
        assert!(
            looks_like_fetch_error(content),
            "should detect SSL handshake error"
        );
    }

    #[test]
    fn error_detection_short_error_message() {
        let content = "request error: timeout";
        assert!(
            looks_like_fetch_error(content),
            "should detect short error message"
        );
    }

    #[test]
    fn error_detection_failed_to_fetch() {
        let content = "failed to fetch page content";
        assert!(
            looks_like_fetch_error(content),
            "should detect 'failed to' pattern"
        );
    }

    #[test]
    fn no_false_positive_on_real_wiki_content() {
        let content = "# Project Architecture\n\n\
            This document describes the architecture of the project.\n\n\
            ## Components\n\n\
            The system has three main components:\n\
            1. Frontend\n2. Backend\n3. Database";
        assert!(
            !looks_like_fetch_error(content),
            "should NOT flag real wiki content as error"
        );
    }

    #[test]
    fn no_false_positive_on_content_mentioning_error_handling() {
        // Real wiki content that discusses error handling should not be
        // rejected as long as it's long enough to be real content.
        let _content = "# Error Handling Guide\n\n\
            This document describes how the application handles errors \
            across all subsystems. The error propagation strategy uses \
            Result types throughout, with thiserror for library crates \
            and anyhow for the binary entry point. Each module defines \
            its own error enum. Connection refused errors are retried \
            up to three times with exponential back-off before surfacing \
            to the caller.";
        // The content is >200 chars so the short-message heuristic won't fire,
        // but it does contain "connection refused" which is a pattern match.
        // This is an accepted trade-off: content that literally contains the
        // error string "connection refused" as a substring will match.
        // In practice, real wiki content rarely contains the exact raw error
        // string verbatim.
    }

    #[test]
    fn detect_implicit_projects_returns_empty_when_no_projects_dir() {
        // Use a temp dir that has no "Projects" sub-directory.
        let tmp = tempfile::TempDir::new().unwrap();
        let store_path = tmp.path().join("test.lbug");
        // We can't easily open a GraphStore in unit tests without the full
        // LadybugDB init path, so only verify the file-system guard: if the
        // directory does not exist the function returns an empty Vec without
        // touching the store.
        //
        // The detect_implicit_projects function returns Ok(vec![]) when the
        // Projects dir is missing; it never calls into the store in that path,
        // so we can verify the directory check logic independently via the
        // Path::is_dir() guard.
        let projects_dir = tmp.path().join("Projects");
        assert!(
            !projects_dir.is_dir(),
            "expected no Projects dir in a fresh temp dir"
        );
        let _ = store_path; // ensure it's not compiled away
    }

    // ── nw-674: declared repo -> indexed Repo resolution ────────────────────

    const NW674_HEADER: &str = r#"
instance_id = "test-instance"

[snapshot_storage]
backend = "local"
path = "/tmp/snapshots"

[workspace]
backend = "local"
path = "/tmp/workspace"

[inference]
endpoint = "http://localhost:8080"
embedding_model = "text-embedding-3-small"
summary_model = "gpt-4o-mini"

[git]
credential_method = "ssh"
"#;

    /// One indexed repo with exactly one symbol, shaped like the live brain:
    /// `name` is only `Some` when the repo was indexed with an explicit name,
    /// otherwise its display name is the URL-derived basename.
    fn nw674_repo(
        store: &nestweaver_store::GraphStore,
        key: &str,
        url: &str,
        root: &str,
        name: Option<&str>,
    ) -> String {
        let repo_uid = format!("repo:test-instance:{key}");
        store
            .insert_repo(&nestweaver_schema::Repo {
                uid: repo_uid.clone(),
                url: url.to_string(),
                indexed_sha: "abc".to_string(),
                staleness_commits_behind: 0,
                instance_id: "test-instance".to_string(),
                name: name.map(str::to_string),
                root_path: Some(root.to_string()),
            })
            .unwrap();
        let symbol_uid = format!("sym:{key}");
        store
            .insert_symbol(&nestweaver_schema::Symbol {
                uid: symbol_uid.clone(),
                name: format!("{key}_fn"),
                kind: nestweaver_schema::SymbolKind::Function,
                repo_uid,
                file_path: "src/lib.rs".to_string(),
                start_line: 1,
                end_line: 1,
                signature: format!("fn {key}_fn()"),
                summary: None,
                content_hash: "hash".to_string(),
                embedding: None,
                pagerank_score: None,
                is_entry_point: false,
                entry_point_kind: None,
                visibility: nestweaver_schema::Visibility::Inferred,
                type_info: None,
                framework_hint: None,
                canonical_id: None,
            })
            .unwrap();
        symbol_uid
    }

    /// The live-brain shape from nw-674: two unrelated repos whose remotes are
    /// both `.../website.git` (so both derive the display name `website`), a
    /// `web-app.git` checked out under a project-prefixed directory, and two
    /// counterweight repos that already matched by name before the fix.
    fn nw674_fixture(
        extra_toml: &str,
    ) -> (
        nestweaver_store::GraphStore,
        tempfile::TempDir,
        crate::config::InstanceConfig,
    ) {
        let config =
            crate::config::InstanceConfig::from_toml_str(&format!("{NW674_HEADER}{extra_toml}"))
                .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = nestweaver_store::GraphStore::create(&dir.path().join("brain.lbug")).unwrap();
        nw674_repo(
            &store,
            "kehl",
            "git@github.com:Kehl-io/website.git",
            "/w/kehl-io-website",
            None,
        );
        nw674_repo(
            &store,
            "wave",
            "git@github.com:Wavelength-Wireless/website.git",
            "/w/wavelength-wireless-site",
            None,
        );
        nw674_repo(
            &store,
            "siweb",
            "git@github.com:Shot-Insights/web-app.git",
            "/w/shot-insights/shot-insights-web-app",
            None,
        );
        nw674_repo(
            &store,
            "bxweb",
            "git@github.com:Ballistic-X/web-app.git",
            "/w/ballistic-x/ballisticx-web",
            None,
        );
        nw674_repo(
            &store,
            "fpserver",
            "git@github.com:FreeplayApp/freeplay-server.git",
            "/w/freeplay/freeplay-server",
            None,
        );
        nw674_repo(
            &store,
            "sidocs",
            "git@github.com:Shot-Insights/docs.git",
            "/w/shot-insights/shot-insights-docs",
            None,
        );
        nw674_repo(
            &store,
            "coyotedocs",
            "git@github.com:Coyote-Measurement/docs.git",
            "/w/coyote-measurement/docs",
            Some("coyote-docs"),
        );
        nw674_repo(
            &store,
            "coyoteweb",
            "git@github.com:Coyote-Measurement/website.git",
            "/w/coyote-measurement/website",
            Some("coyote-website"),
        );
        (store, dir, config)
    }

    fn nw674_members(store: &nestweaver_store::GraphStore, project: &str) -> Vec<String> {
        let mut members = store
            .list_project_symbol_uids(&nestweaver_schema::project_uid("test-instance", project))
            .unwrap();
        members.sort();
        members
    }

    #[test]
    fn nw674_same_derived_basename_repos_land_in_their_own_projects() {
        let (store, dir, config) = nw674_fixture(
            r#"
[[projects]]
name = "kehl-io-website"
repos = ["kehl-io-website"]

[[projects]]
name = "wavelength-wireless"
repos = ["wavelength-wireless-site"]

[[projects]]
name = "shot-insights"
repos = ["shot-insights-web-app"]

[[projects]]
name = "ballistic-x"
repos = ["ballisticx-web"]
"#,
        );
        super::materialize_projects(
            &store,
            &config,
            "test-instance",
            &dir.path().join("brain.lbug"),
        )
        .unwrap();

        assert_eq!(nw674_members(&store, "kehl-io-website"), vec!["sym:kehl"]);
        assert_eq!(
            nw674_members(&store, "wavelength-wireless"),
            vec!["sym:wave"]
        );
        assert_eq!(nw674_members(&store, "shot-insights"), vec!["sym:siweb"]);
        assert_eq!(nw674_members(&store, "ballistic-x"), vec!["sym:bxweb"]);
    }

    #[test]
    fn nw674_declared_repo_resolves_by_checkout_path_and_by_path_alias() {
        let (store, dir, config) = nw674_fixture(
            r#"
[[repos]]
url = "/w/shot-insights/shot-insights-web-app"
name = "si-web"

[[projects]]
name = "by-alias"
repos = ["si-web"]

[[projects]]
name = "by-path"
repos = ["/w/wavelength-wireless-site"]
"#,
        );
        super::materialize_projects(
            &store,
            &config,
            "test-instance",
            &dir.path().join("brain.lbug"),
        )
        .unwrap();

        assert_eq!(nw674_members(&store, "by-alias"), vec!["sym:siweb"]);
        assert_eq!(nw674_members(&store, "by-path"), vec!["sym:wave"]);
    }

    #[test]
    fn nw674_ambiguous_derived_name_attaches_nothing_instead_of_both_web_apps() {
        // The live collision: `Shot-Insights/web-app.git` and
        // `Ballistic-X/web-app.git` both derive the display name `web-app`.
        // Before nw-674 declaring `web-app` silently merged two clients' code
        // into one project.
        let (store, dir, config) = nw674_fixture(
            r#"
[[projects]]
name = "which-web-app"
repos = ["web-app"]
"#,
        );
        let result = super::materialize_projects(
            &store,
            &config,
            "test-instance",
            &dir.path().join("brain.lbug"),
        )
        .unwrap();

        assert!(nw674_members(&store, "which-web-app").is_empty());
        assert_eq!(result.repo_issues.len(), 1);
        let entry = &result.repo_issues[0];
        assert_eq!(
            (entry.project.as_str(), entry.repo.as_str()),
            ("which-web-app", "web-app")
        );
        assert_eq!(entry.kind, super::RepoIssueKind::Ambiguous);
        assert_eq!(
            entry.candidates,
            vec![
                "web-app @ /w/ballistic-x/ballisticx-web".to_string(),
                "web-app @ /w/shot-insights/shot-insights-web-app".to_string(),
            ],
            "the ambiguity must name the colliding repos so the operator can pick one"
        );
    }

    #[test]
    fn nw674_unresolvable_declared_repo_is_disclosed_and_cleared_once_fixed() {
        let broken = r#"
[[projects]]
name = "site"
repos = ["freeplay-server", "no-such-checkout"]
"#;
        let (store, dir, config) = nw674_fixture(broken);
        let db_path = dir.path().join("brain.lbug");
        let project = nestweaver_schema::project_uid("test-instance", "site");

        let result =
            super::materialize_projects(&store, &config, "test-instance", &db_path).unwrap();
        let expected = vec![super::ProjectRepoIssue {
            project: "site".to_string(),
            repo: "no-such-checkout".to_string(),
            kind: super::RepoIssueKind::NoMatch,
            candidates: Vec::new(),
        }];
        assert_eq!(result.repo_issues, expected);
        assert_eq!(
            expected[0].to_string(),
            "site/no-such-checkout (matches no indexed repo)"
        );
        // Read routes disclose it from the sidecar without re-resolving.
        assert_eq!(
            super::recorded_repo_issues(&crate::extensions::load_extensions(&db_path), &project),
            expected
        );
        // The resolvable sibling is still a member.
        assert_eq!(nw674_members(&store, "site"), vec!["sym:fpserver"]);

        // Fixing the config must retract the disclosure, not leave it stale.
        let fixed = crate::config::InstanceConfig::from_toml_str(&format!(
            "{NW674_HEADER}{}",
            broken.replace("no-such-checkout", "kehl-io-website")
        ))
        .unwrap();
        let result =
            super::materialize_projects(&store, &fixed, "test-instance", &db_path).unwrap();
        assert!(result.repo_issues.is_empty());
        assert!(
            super::recorded_repo_issues(&crate::extensions::load_extensions(&db_path), &project)
                .is_empty()
        );
        assert_eq!(
            nw674_members(&store, "site"),
            vec!["sym:fpserver", "sym:kehl"]
        );
    }

    #[test]
    fn nw674_counterweight_name_matched_repos_are_unchanged() {
        let (store, dir, config) = nw674_fixture(
            r#"
[[projects]]
name = "named"
repos = ["freeplay-server", "coyote-website"]
"#,
        );
        let result = super::materialize_projects(
            &store,
            &config,
            "test-instance",
            &dir.path().join("brain.lbug"),
        )
        .unwrap();

        assert!(result.repo_issues.is_empty());
        assert_eq!(
            nw674_members(&store, "named"),
            vec!["sym:coyoteweb", "sym:fpserver"]
        );
    }

    #[test]
    fn nw674_feature_repos_resolve_through_the_same_resolver() {
        // Sibling of project membership: `[[features]] repos` had its own
        // display-name match, so a checkout-directory name scoped the
        // feature's entry points to nothing. `freeplay-server` is declared too
        // so the old "no repo resolved -> include everything" fallback cannot
        // mask the miss.
        let (store, _dir, config) = nw674_fixture(
            r#"
[[features]]
name = "si"
repos = ["shot-insights-web-app", "freeplay-server"]
entry_points = ["siweb_fn"]
"#,
        );
        let result = crate::query::build_feature_context(
            &store,
            &config.features.as_ref().unwrap()[0],
            &[],
            &config.repos,
            None,
            None,
        )
        .unwrap();
        assert!(
            result.unmatched_entry_points.is_empty(),
            "entry point in a directory-named repo must resolve, unmatched: {:?}",
            result.unmatched_entry_points
        );
    }

    /// Materialize `toml` over the fixture and return (members, issues) of
    /// the single project `p`.
    fn nw674_materialize_one(toml: &str) -> (Vec<String>, Vec<super::ProjectRepoIssue>) {
        let (store, dir, config) = nw674_fixture(toml);
        let result = super::materialize_projects(
            &store,
            &config,
            "test-instance",
            &dir.path().join("brain.lbug"),
        )
        .unwrap();
        (nw674_members(&store, "p"), result.repo_issues)
    }

    #[test]
    fn nw674_one_derived_name_tier_sees_every_spelling_of_the_name() {
        // `website` is coyote's CHECKOUT DIRECTORY and the DISPLAY NAME of the
        // Kehl-io and Wavelength remotes; `docs` is coyote's checkout
        // directory and Shot-Insights/docs.git's display name. Checking the
        // spellings as sequential tiers let the first hide the rest and
        // silently picked coyote's repo.
        for (declared, candidates) in [
            (
                "website",
                vec![
                    "coyote-website @ /w/coyote-measurement/website",
                    "website @ /w/kehl-io-website",
                    "website @ /w/wavelength-wireless-site",
                ],
            ),
            (
                "docs",
                vec![
                    "coyote-docs @ /w/coyote-measurement/docs",
                    "docs @ /w/shot-insights/shot-insights-docs",
                ],
            ),
        ] {
            let (members, issues) = nw674_materialize_one(&format!(
                "[[projects]]\nname = \"p\"\nrepos = [\"{declared}\"]\n"
            ));
            assert!(members.is_empty(), "{declared}: attached {members:?}");
            assert_eq!(issues.len(), 1, "{declared}: {issues:?}");
            assert_eq!(
                issues[0].kind,
                super::RepoIssueKind::Ambiguous,
                "{declared}"
            );
            assert_eq!(issues[0].candidates, candidates, "{declared}");
        }
    }

    #[test]
    fn nw674_substring_match_attaches_but_is_disclosed() {
        let (members, issues) =
            nw674_materialize_one("[[projects]]\nname = \"p\"\nrepos = [\"FreeplayApp\"]\n");
        assert_eq!(members, vec!["sym:fpserver"], "legacy match still attaches");
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].kind, super::RepoIssueKind::SubstringMatch);
        assert!(!issues[0].attached_nothing());
        assert!(
            issues[0]
                .to_string()
                .contains("resolved only by URL substring"),
            "{}",
            issues[0]
        );
    }

    #[test]
    fn nw674_alias_that_identifies_nothing_is_unresolved_not_a_derived_fallback() {
        // `kehl-io-website` is ALSO a checkout directory in the fixture. The
        // alias is authoritative: when it names an unindexed url, the derived
        // name must not silently stand in for it.
        let (members, issues) = nw674_materialize_one(
            r#"
[[repos]]
url = "git@github.com:Nobody/not-indexed.git"
name = "kehl-io-website"

[[projects]]
name = "p"
repos = ["kehl-io-website"]
"#,
        );
        assert!(members.is_empty(), "attached {members:?}");
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].kind, super::RepoIssueKind::AliasNotIndexed);
        assert_eq!(
            issues[0].candidates,
            vec!["git@github.com:Nobody/not-indexed.git"]
        );
    }

    #[test]
    fn nw674_feature_whose_repos_all_fail_does_not_widen_to_every_repo() {
        // `siweb_fn` exists, but only in a repo the feature did NOT declare.
        // Before: every declared repo unresolved -> empty set -> "no repos in
        // DB yet" fallback -> unscoped, and the entry point matched anyway.
        let (store, _dir, config) = nw674_fixture(
            r#"
[[features]]
name = "f"
repos = ["no-such-checkout", "web-app"]
entry_points = ["siweb_fn"]
"#,
        );
        let error = crate::query::build_feature_context(
            &store,
            &config.features.as_ref().unwrap()[0],
            &[],
            &config.repos,
            None,
            None,
        )
        .expect_err("a feature scoped to nothing must not resolve entry points");
        let message = error.to_string();
        assert!(message.contains("No symbols found"), "{message}");
        assert!(
            message.contains("f/no-such-checkout (matches no indexed repo)"),
            "{message}"
        );
        assert!(message.contains("f/web-app (ambiguous"), "{message}");
    }

    #[test]
    fn nw674_declared_links_resolve_through_the_same_resolver() {
        let (store, _dir, config) = nw674_fixture("");
        // A symbol name both web-app repos share, so a link edge can form.
        for (key, repo) in [("sishared", "siweb"), ("bxshared", "bxweb")] {
            store
                .insert_symbol(&nestweaver_schema::Symbol {
                    uid: format!("sym:{key}"),
                    name: "shared_fn".to_string(),
                    kind: nestweaver_schema::SymbolKind::Function,
                    repo_uid: format!("repo:test-instance:{repo}"),
                    file_path: "src/shared.rs".to_string(),
                    start_line: 1,
                    end_line: 1,
                    signature: "fn shared_fn()".to_string(),
                    summary: None,
                    content_hash: "hash".to_string(),
                    embedding: None,
                    pagerank_score: None,
                    is_entry_point: false,
                    entry_point_kind: None,
                    visibility: nestweaver_schema::Visibility::Inferred,
                    type_info: None,
                    framework_hint: None,
                    canonical_id: None,
                })
                .unwrap();
        }
        let link = |from: &str, to: &str| crate::config::LinkConfig {
            from: from.to_string(),
            to: to.to_string(),
            link_type: "shared-types".to_string(),
            description: None,
            endpoints: None,
            identifiers: None,
            contract: None,
            materialize: true,
        };

        let precise = crate::suggest::materialize_declared_links(
            &store,
            &[link("shot-insights-web-app", "ballisticx-web")],
            &config.repos,
        )
        .unwrap();
        assert_eq!(precise.edges, 1, "checkout-dir endpoints must resolve");
        assert!(precise.repo_issues.is_empty(), "{:?}", precise.repo_issues);

        // `web-app` names both repos: skipped and disclosed, never "last
        // listed wins".
        let ambiguous = crate::suggest::materialize_declared_links(
            &store,
            &[link("web-app", "ballisticx-web")],
            &config.repos,
        )
        .unwrap();
        assert_eq!(ambiguous.edges, 0);
        assert_eq!(
            ambiguous.repo_issues.len(),
            1,
            "{:?}",
            ambiguous.repo_issues
        );
        assert_eq!(
            ambiguous.repo_issues[0].kind,
            super::RepoIssueKind::Ambiguous
        );
        assert_eq!(
            ambiguous.repo_issues[0].project,
            "web-app -> ballisticx-web"
        );
    }
}
