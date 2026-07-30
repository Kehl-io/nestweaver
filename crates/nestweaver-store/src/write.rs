use std::path::Path;

use nestweaver_schema::{
    Contract, EdgeType, File, Heading, Note, Project, Repo, ResolvedEdge, Section, Service, Symbol,
    Tag, Vault,
    uid::{file_uid, project_uid, repo_uid, service_uid, symbol_uid, vault_uid},
};
use serde_json;

use crate::db::GraphStore;
use crate::error::StoreError;

/// What a classified destructive store mutation can prove about durable state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MutationDisposition {
    ConfirmedNoChange,
    CommittedComplete,
    CommittedPartial,
    Ambiguous,
}

/// A mutation-stage error retained on a structured partial or ambiguous result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MutationFailure {
    pub stage: String,
    pub message: String,
}

impl MutationFailure {
    fn new(stage: impl Into<String>, error: impl std::fmt::Display) -> Self {
        Self {
            stage: stage.into(),
            message: error.to_string(),
        }
    }
}

/// A destructive store result that never uses `Err` to imply rollback after a
/// possible durable mutation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MutationOutcome<T> {
    pub disposition: MutationDisposition,
    pub confirmed_changed: bool,
    pub value: T,
    pub primary_failure: Option<MutationFailure>,
    pub mutation_warnings: Vec<MutationFailure>,
}

/// Confirmed graph mutation performed by a vault cascade.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeleteVaultCascadeOutcome {
    pub notes_deleted: usize,
    pub changed: bool,
}

/// Snapshot-derived counts retained by a classified Repo cascade.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeleteRepoCascadeOutcome {
    pub repo_uid: String,
    pub files_deleted: usize,
    pub symbols_deleted: usize,
}

/// What the store can prove about a Project cascade after the transaction
/// attempt completes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectMutationDisposition {
    /// No graph mutation was attempted (including a missing Project).
    ConfirmedUnchanged,
    /// A mutation was attempted and a subsequent rollback succeeded.
    ConfirmedRolledBack,
    /// The transaction committed successfully.
    Changed,
    /// The database may have committed or a required rollback could not be
    /// confirmed. Callers must reconcile against graph liveness.
    Ambiguous,
}

/// Confirmed result of an atomic Project cascade.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeleteProjectCascadeOutcome {
    pub project_uid: String,
    pub project_name: Option<String>,
    pub disposition: ProjectMutationDisposition,
}

/// A failed Project cascade together with the strongest mutation guarantee
/// the store can make.
#[derive(Debug)]
pub struct DeleteProjectCascadeError {
    pub project_uid: String,
    pub project_name: Option<String>,
    pub disposition: ProjectMutationDisposition,
    pub primary: StoreError,
    pub rollback: Option<StoreError>,
}

impl std::fmt::Display for DeleteProjectCascadeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Project cascade deletion for {} ({:?}) failed: {}",
            self.project_uid, self.disposition, self.primary
        )?;
        if let Some(project_name) = &self.project_name {
            write!(formatter, "; project_name={project_name}")?;
        }
        if let Some(rollback) = &self.rollback {
            write!(formatter, "; rollback failed: {rollback}")?;
        }
        Ok(())
    }
}

impl std::error::Error for DeleteProjectCascadeError {}

#[derive(Clone, Copy)]
struct ProjectCascadeQueries {
    begin: &'static str,
    repeat_begin: bool,
    lookup: &'static str,
    delete: &'static str,
    omit_delete_params: bool,
    commit: &'static str,
    repeat_commit: bool,
    rollback: &'static str,
}

impl Default for ProjectCascadeQueries {
    fn default() -> Self {
        Self {
            begin: "BEGIN TRANSACTION",
            repeat_begin: false,
            lookup: "MATCH (p:Project {uid: $uid}) RETURN p.uid, p.name",
            delete: "MATCH (p:Project {uid: $uid}) DETACH DELETE p",
            omit_delete_params: false,
            commit: "COMMIT",
            repeat_commit: false,
            rollback: "ROLLBACK",
        }
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default)]
struct ProjectCascadeFaults {
    begin: bool,
    lookup: bool,
    lookup_uid_mismatch: bool,
    lookup_uid_malformed: bool,
    lookup_name_malformed: bool,
    before_mutation: bool,
    detach: bool,
    commit: bool,
    rollback: bool,
}

#[derive(Clone, Copy, Debug, Default)]
struct VaultCascadeFaults {
    before_delete: bool,
    commit_before: bool,
    commit_after: bool,
    probe: bool,
}

#[derive(Clone, Copy, Debug, Default)]
struct RepoCascadeFaults {
    bulk_commit_after: bool,
    after_bulk: bool,
    before_root: bool,
    root_ack_after: bool,
    probe: bool,
}

#[derive(Clone, Copy, Debug, Default)]
struct PurgeInstanceFaults {
    before_repo: Option<usize>,
    before_vault: Option<usize>,
    orphan_commit_after: Option<usize>,
    orphan_probe: bool,
}

#[derive(Clone, Copy, Debug, Default)]
struct MergeInstanceFaults {
    before_graph: bool,
    after_repo: Option<usize>,
    after_graph: bool,
    verify: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct PurgeOrphanTarget {
    label: &'static str,
    uid: String,
    code: bool,
}

#[derive(Clone, Debug, Default)]
struct PurgeInstancePlan {
    repos: Vec<Repo>,
    vaults: Vec<Vault>,
    projects: Vec<Project>,
    orphan_targets: Vec<PurgeOrphanTarget>,
    code_repo_uids: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct VaultDeletionSnapshot {
    vault_uids: std::collections::BTreeSet<String>,
    note_uids: std::collections::BTreeSet<String>,
    tag_uids: std::collections::BTreeSet<String>,
}

impl VaultDeletionSnapshot {
    fn is_empty(&self) -> bool {
        self.vault_uids.is_empty() && self.note_uids.is_empty() && self.tag_uids.is_empty()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExactSnapshotState {
    WhollyLive,
    WhollyAbsent,
    Mixed,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct RepoDeletionSnapshot {
    repo_uids: std::collections::BTreeSet<String>,
    file_uids: std::collections::BTreeSet<String>,
    symbol_uids: std::collections::BTreeSet<String>,
    service_uids: std::collections::BTreeSet<String>,
    contract_uids: std::collections::BTreeSet<String>,
}

impl RepoDeletionSnapshot {
    fn is_empty(&self) -> bool {
        self.repo_uids.is_empty()
            && self.file_uids.is_empty()
            && self.symbol_uids.is_empty()
            && self.service_uids.is_empty()
            && self.contract_uids.is_empty()
    }

    fn is_subset_of(&self, other: &Self) -> bool {
        self.repo_uids.is_subset(&other.repo_uids)
            && self.file_uids.is_subset(&other.file_uids)
            && self.symbol_uids.is_subset(&other.symbol_uids)
            && self.service_uids.is_subset(&other.service_uids)
            && self.contract_uids.is_subset(&other.contract_uids)
    }
}

/// One recorded unresolved wikilink:
/// `(uid, source_note_uid, source_path, source_title, wikilink_text)`.
pub type UnresolvedWikilinkRecord = (String, String, String, String, String);

/// A vault whose notes were discarded during a collision in instance merge.
/// When two instances have vaults at the same root_path, the vault with
/// fewer notes loses and its notes are cascade-deleted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscardedVault {
    pub root_path: String,
    pub notes_discarded: usize,
}

/// Result of [`GraphStore::merge_instance_ids`].
///
/// `repos_moved` lists the identifier (display name if set, else url) of
/// every Repo node that was re-minted under the target instance. Source code
/// graph rows are removed before each Repo is re-minted, so the caller must
/// force re-index every repo in this list — see
/// [`MergeResult::repos_need_reindex`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MergeResult {
    pub vaults: usize,
    pub repos: usize,
    pub projects: usize,
    pub discarded: Vec<DiscardedVault>,
    /// Identifiers of repos re-minted under the target instance. Their graph
    /// rows were removed and must be rebuilt by a forced re-index.
    pub repos_moved: Vec<String>,
    /// Source repo UIDs whose derived graph rows were deleted during migration.
    /// The daemon uses these keys to remove per-repo sidecar slices.
    pub repo_uids_removed: Vec<String>,
}

/// A deterministic source-to-destination UID mapping produced by an instance
/// merge. File and Symbol rows are removed by the merge and rebuilt by the
/// required re-index, but their target UIDs are still deterministic from the
/// graph rows present at merge preflight.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct InstanceUidRemap {
    pub source_uid: String,
    pub destination_uid: String,
}

/// Stable identity used to attach authored metadata after a merge-required
/// force re-index recreates a node under an actual (possibly line-shifted) UID.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum InstanceUidHandoffIdentity {
    File {
        destination_repo_uid: String,
        path: String,
    },
    Service {
        destination_repo_uid: String,
        name: String,
    },
    Symbol {
        destination_repo_uid: String,
        canonical_id: Option<String>,
        file_path: String,
        name: String,
        kind: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct InstanceUidHandoff {
    pub source_uid: String,
    pub predicted_destination_uid: String,
    pub identity: InstanceUidHandoffIdentity,
}

/// Durable Repo payload needed to resume the delete-before-insert crash window
/// in a non-transactional instance merge.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct InstanceRepoRecovery {
    pub source_uid: String,
    pub destination_uid: String,
    pub url: String,
    pub staleness_commits_behind: u32,
    pub name: Option<String>,
    pub root_path: Option<String>,
}

/// Durable Vault payload needed to resume the delete-before-insert crash
/// window in a non-transactional instance merge. Children are intentionally
/// not captured: a recovered empty Vault is reindexed after the merge.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct InstanceVaultRecovery {
    pub source_uid: String,
    pub destination_uid: String,
    pub name: String,
    pub root_path: String,
}

/// Durable Project payload needed to resume the delete-before-insert crash
/// window in a non-transactional instance merge.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct InstanceProjectRecovery {
    pub source_uid: String,
    pub destination_uid: String,
    pub name: String,
    pub summary: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InstanceUidMigrationPlan {
    pub remaps: Vec<InstanceUidRemap>,
    pub handoffs: Vec<InstanceUidHandoff>,
    pub repo_recoveries: Vec<InstanceRepoRecovery>,
    pub vault_recoveries: Vec<InstanceVaultRecovery>,
    pub project_recoveries: Vec<InstanceProjectRecovery>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstanceUidRemapPlanState {
    Prepared,
    PartiallyApplied,
    Applied,
}

#[derive(Clone, Debug)]
struct InstanceProjectMergePlan {
    winner: Project,
    winner_preexists: bool,
    recovery_source_uid: Option<String>,
    source_count: usize,
    remaps: Vec<InstanceUidRemap>,
}

impl MergeResult {
    /// True when the merge re-minted one or more Repo nodes. The caller should
    /// instruct the user to force re-index each repo listed in
    /// [`MergeResult::repos_moved`].
    pub fn repos_need_reindex(&self) -> bool {
        !self.repos_moved.is_empty()
    }
}

/// Result of [`GraphStore::reparent_vault`].
#[derive(Debug)]
pub struct ReparentVaultResult {
    pub notes_migrated: usize,
    pub headings_migrated: usize,
    pub sections_migrated: usize,
    pub tags_migrated: usize,
}

/// Result of [`GraphStore::purge_instance`]. Reports how many top-level
/// rows were cascade-deleted from the graph for the given instance,
/// plus a separate count for orphan nodes (Symbol/File/Service/Note/
/// Heading/Section/Tag rows whose UID prefix encodes the instance but
/// whose parent Repo or Vault no longer exists — typically left behind
/// by a partially-applied `instance merge`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PurgeInstanceResult {
    pub repos: usize,
    pub files: usize,
    pub symbols: usize,
    pub vaults: usize,
    pub notes: usize,
    pub projects: usize,
    pub orphans_swept: usize,
    /// Orphaned code rows removed without a top-level Repo registry entry.
    pub code_orphans_swept: usize,
    /// Repo UIDs referenced by code rows removed during the purge, including
    /// orphan-only code whose top-level Repo registry row was already absent.
    pub code_repo_uids: Vec<String>,
}

/// Encode a Symbol's `framework_hint` as the `"framework:role"` string the
/// `framework_hint` column stores. Returns an empty string when absent.
fn encode_framework_hint(symbol: &Symbol) -> String {
    match &symbol.framework_hint {
        Some(h) => format!("{}:{}", h.framework, h.role),
        None => String::new(),
    }
}

// ── CSV writers for COPY FROM bulk loading ──────────────────────────────────

/// Write Symbol rows to a CSV file (no header row).
/// Column order: uid, name, kind, repo_uid, file_path, start_line, end_line,
/// signature, summary, content_hash, pagerank_score, is_entry_point,
/// entry_point_kind, framework_hint
fn write_symbols_csv(symbols: &[Symbol], path: &Path) -> Result<(), StoreError> {
    let f = std::fs::File::create(path)
        .map_err(|e| StoreError::Query(format!("create symbols csv: {e}")))?;
    let mut wtr = csv::WriterBuilder::new().has_headers(false).from_writer(f);
    for s in symbols {
        let kind = s.kind.to_string();
        let start_line = s.start_line.to_string();
        let end_line = s.end_line.to_string();
        let summary = s.summary.clone().unwrap_or_default();
        let pagerank = s.pagerank_score.unwrap_or(0.0).to_string();
        let is_ep = if s.is_entry_point { "true" } else { "false" };
        let epk = s
            .entry_point_kind
            .map(|k| k.to_string())
            .unwrap_or_default();
        let fh = encode_framework_hint(s);
        let canonical = s.canonical_id.clone().unwrap_or_default();
        wtr.write_record([
            &s.uid,
            &s.name,
            &kind,
            &s.repo_uid,
            &s.file_path,
            &start_line,
            &end_line,
            &s.signature,
            &summary,
            &s.content_hash,
            &pagerank,
            is_ep,
            &epk,
            &fh,
            &canonical,
        ])
        .map_err(|e| StoreError::Query(format!("write symbol row: {e}")))?;
    }
    wtr.flush()
        .map_err(|e| StoreError::Query(format!("flush symbols csv: {e}")))?;
    Ok(())
}

/// Write File rows to a CSV file (no header row).
/// Column order: uid, path, repo_uid, content_hash
fn write_files_csv(files: &[File], path: &Path) -> Result<(), StoreError> {
    let f = std::fs::File::create(path)
        .map_err(|e| StoreError::Query(format!("create files csv: {e}")))?;
    let mut wtr = csv::WriterBuilder::new().has_headers(false).from_writer(f);
    for file in files {
        wtr.write_record([&file.uid, &file.path, &file.repo_uid, &file.content_hash])
            .map_err(|e| StoreError::Query(format!("write file row: {e}")))?;
    }
    wtr.flush()
        .map_err(|e| StoreError::Query(format!("flush files csv: {e}")))?;
    Ok(())
}

/// Write Service rows to a CSV file (no header row).
/// Column order: uid, name, repo_uid, summary, summary_hash
fn write_services_csv(services: &[Service], path: &Path) -> Result<(), StoreError> {
    let f = std::fs::File::create(path)
        .map_err(|e| StoreError::Query(format!("create services csv: {e}")))?;
    let mut wtr = csv::WriterBuilder::new().has_headers(false).from_writer(f);
    for svc in services {
        wtr.write_record([
            &svc.uid,
            &svc.name,
            &svc.repo_uid,
            svc.summary.as_deref().unwrap_or(""),
            svc.summary_hash.as_deref().unwrap_or(""),
        ])
        .map_err(|e| StoreError::Query(format!("write service row: {e}")))?;
    }
    wtr.flush()
        .map_err(|e| StoreError::Query(format!("flush services csv: {e}")))?;
    Ok(())
}

/// Write Contract rows to a CSV file (no header row).
/// Column order: uid, kind, verb, path, operation_id, repo_uid, source_path, confidence
fn write_contracts_csv(contracts: &[Contract], path: &Path) -> Result<(), StoreError> {
    let f = std::fs::File::create(path)
        .map_err(|e| StoreError::Query(format!("create contracts csv: {e}")))?;
    let mut wtr = csv::WriterBuilder::new().has_headers(false).from_writer(f);
    for c in contracts {
        let conf = c.confidence.to_string();
        wtr.write_record([
            &c.uid,
            &c.kind,
            c.verb.as_deref().unwrap_or(""),
            c.path.as_deref().unwrap_or(""),
            c.operation_id.as_deref().unwrap_or(""),
            &c.repo_uid,
            &c.source_path,
            &conf,
        ])
        .map_err(|e| StoreError::Query(format!("write contract row: {e}")))?;
    }
    wtr.flush()
        .map_err(|e| StoreError::Query(format!("flush contracts csv: {e}")))?;
    Ok(())
}

/// Write edge (from_pk, to_pk) pairs to a CSV file (no header row).
fn write_edge_pair_csv(edges: &[(&str, &str)], path: &Path) -> Result<(), StoreError> {
    let f = std::fs::File::create(path)
        .map_err(|e| StoreError::Query(format!("create edge csv: {e}")))?;
    let mut wtr = csv::WriterBuilder::new().has_headers(false).from_writer(f);
    for (from_pk, to_pk) in edges {
        wtr.write_record([from_pk, to_pk])
            .map_err(|e| StoreError::Query(format!("write edge row: {e}")))?;
    }
    wtr.flush()
        .map_err(|e| StoreError::Query(format!("flush edge csv: {e}")))?;
    Ok(())
}

fn exec_params(
    conn: &lbug::Connection<'_>,
    query: &str,
    params: Vec<(&str, lbug::Value)>,
) -> Result<(), StoreError> {
    let mut stmt = conn
        .prepare(query)
        .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
    conn.execute(&mut stmt, params)
        .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
    Ok(())
}

impl GraphStore {
    fn plan_instance_project_merges(
        &self,
        from: &str,
        to: &str,
    ) -> Result<Vec<InstanceProjectMergePlan>, StoreError> {
        let mut groups: std::collections::BTreeMap<String, (Vec<Project>, Vec<Project>)> =
            std::collections::BTreeMap::new();
        for project in self.list_projects()? {
            let entry = groups.entry(project.name.to_lowercase()).or_default();
            if project.instance_id == from {
                entry.0.push(project);
            } else if project.instance_id == to {
                entry.1.push(project);
            }
        }

        let mut plans = Vec::new();
        for (_casefolded_name, (mut sources, mut targets)) in groups {
            if sources.is_empty() {
                continue;
            }
            sources.sort_by(|left, right| {
                project_uid(to, &left.name)
                    .cmp(&project_uid(to, &right.name))
                    .then_with(|| left.uid.cmp(&right.uid))
            });
            targets.sort_by(|left, right| left.uid.cmp(&right.uid));

            let source_count = sources.len();
            let (winner, winner_preexists, recovery_source_uid) =
                if let Some(target) = targets.first() {
                    (target.clone(), true, None)
                } else {
                    let source = &sources[0];
                    (
                        Project {
                            uid: project_uid(to, &source.name),
                            name: source.name.clone(),
                            summary: source.summary.clone(),
                            instance_id: to.to_string(),
                        },
                        false,
                        Some(source.uid.clone()),
                    )
                };

            let mut remaps: Vec<InstanceUidRemap> = targets
                .into_iter()
                .skip(1)
                .chain(sources)
                .map(|project| InstanceUidRemap {
                    source_uid: project.uid,
                    destination_uid: winner.uid.clone(),
                })
                .collect();
            remaps.sort();
            plans.push(InstanceProjectMergePlan {
                winner,
                winner_preexists,
                recovery_source_uid,
                source_count,
                remaps,
            });
        }
        Ok(plans)
    }

    pub fn insert_repo(&self, repo: &Repo) -> Result<(), StoreError> {
        let conn = self.conn()?;
        exec_params(
            &conn,
            "CREATE (:Repo {uid: $uid, url: $url, indexed_sha: $sha, \
             staleness_commits_behind: $scb, instance_id: $iid, name: $name, \
             root_path: $root_path})",
            vec![
                ("uid", lbug::Value::String(repo.uid.clone())),
                ("url", lbug::Value::String(repo.url.clone())),
                ("sha", lbug::Value::String(repo.indexed_sha.clone())),
                (
                    "scb",
                    lbug::Value::Int64(repo.staleness_commits_behind as i64),
                ),
                ("iid", lbug::Value::String(repo.instance_id.clone())),
                (
                    "name",
                    lbug::Value::String(repo.name.clone().unwrap_or_default()),
                ),
                (
                    "root_path",
                    lbug::Value::String(repo.root_path.clone().unwrap_or_default()),
                ),
            ],
        )
    }

    pub fn insert_file(&self, file: &File) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::insert_file_on(&conn, file)
    }

    /// Insert a File node using an externally-provided connection (for transaction batching).
    ///
    /// Uses `MERGE` so that re-indexing a modified file upserts the node
    /// instead of failing with a duplicate primary-key error.
    pub fn insert_file_on(conn: &lbug::Connection<'_>, file: &File) -> Result<(), StoreError> {
        exec_params(
            conn,
            "MERGE (f:File {uid: $uid}) \
             ON CREATE SET f.path = $path, f.repo_uid = $repo, f.content_hash = $hash \
             ON MATCH SET f.path = $path, f.repo_uid = $repo, f.content_hash = $hash",
            vec![
                ("uid", lbug::Value::String(file.uid.clone())),
                ("path", lbug::Value::String(file.path.clone())),
                ("repo", lbug::Value::String(file.repo_uid.clone())),
                ("hash", lbug::Value::String(file.content_hash.clone())),
            ],
        )
    }

    pub fn insert_service(&self, service: &Service) -> Result<(), StoreError> {
        let conn = self.conn()?;
        exec_params(
            &conn,
            "CREATE (:Service {uid: $uid, name: $name, repo_uid: $repo, \
             summary: $summary, summary_hash: $shash})",
            vec![
                ("uid", lbug::Value::String(service.uid.clone())),
                ("name", lbug::Value::String(service.name.clone())),
                ("repo", lbug::Value::String(service.repo_uid.clone())),
                (
                    "summary",
                    lbug::Value::String(service.summary.clone().unwrap_or_default()),
                ),
                (
                    "shash",
                    lbug::Value::String(service.summary_hash.clone().unwrap_or_default()),
                ),
            ],
        )
    }

    pub fn insert_symbol(&self, symbol: &Symbol) -> Result<(), StoreError> {
        let conn = self.conn()?;
        self.insert_symbol_with_conn(&conn, symbol)
    }

    pub(crate) fn insert_symbol_with_conn(
        &self,
        conn: &lbug::Connection<'_>,
        symbol: &Symbol,
    ) -> Result<(), StoreError> {
        Self::insert_symbol_with_conn_static(conn, symbol)
    }

    /// Static version of `insert_symbol_with_conn` for use without `&self`.
    pub(crate) fn insert_symbol_with_conn_static(
        conn: &lbug::Connection<'_>,
        symbol: &Symbol,
    ) -> Result<(), StoreError> {
        exec_params(
            conn,
            "CREATE (:Symbol {uid: $uid, name: $name, kind: $kind, \
             repo_uid: $repo, file_path: $fp, start_line: $sl, end_line: $el, \
             signature: $sig, summary: $summary, content_hash: $hash, \
             pagerank_score: $pr, is_entry_point: $iep, entry_point_kind: $epk, \
             framework_hint: $fh, canonical_id: $cid})",
            vec![
                ("uid", lbug::Value::String(symbol.uid.clone())),
                ("name", lbug::Value::String(symbol.name.clone())),
                ("kind", lbug::Value::String(symbol.kind.to_string())),
                ("repo", lbug::Value::String(symbol.repo_uid.clone())),
                ("fp", lbug::Value::String(symbol.file_path.clone())),
                ("sl", lbug::Value::Int64(symbol.start_line as i64)),
                ("el", lbug::Value::Int64(symbol.end_line as i64)),
                ("sig", lbug::Value::String(symbol.signature.clone())),
                (
                    "summary",
                    lbug::Value::String(symbol.summary.clone().unwrap_or_default()),
                ),
                ("hash", lbug::Value::String(symbol.content_hash.clone())),
                (
                    "pr",
                    lbug::Value::Double(symbol.pagerank_score.unwrap_or(0.0)),
                ),
                (
                    "iep",
                    lbug::Value::String(
                        if symbol.is_entry_point {
                            "true"
                        } else {
                            "false"
                        }
                        .to_string(),
                    ),
                ),
                (
                    "epk",
                    lbug::Value::String(
                        symbol
                            .entry_point_kind
                            .map(|k| k.to_string())
                            .unwrap_or_default(),
                    ),
                ),
                ("fh", lbug::Value::String(encode_framework_hint(symbol))),
                (
                    "cid",
                    lbug::Value::String(symbol.canonical_id.clone().unwrap_or_default()),
                ),
            ],
        )
    }

    pub fn batch_insert_symbols(&self, symbols: &[Symbol]) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::batch_insert_symbols_on(&conn, symbols)
    }

    /// Insert symbols using an externally-provided connection (for transaction batching).
    pub fn batch_insert_symbols_on(
        conn: &lbug::Connection<'_>,
        symbols: &[Symbol],
    ) -> Result<(), StoreError> {
        if symbols.is_empty() {
            return Ok(());
        }
        let tmp_dir =
            tempfile::tempdir().map_err(|e| StoreError::Query(format!("tempdir: {e}")))?;
        let csv_path = tmp_dir.path().join("symbols.csv");
        write_symbols_csv(symbols, &csv_path)?;
        let csv_str = csv_path.display().to_string().replace('\\', "/");
        conn.query(&format!("COPY Symbol FROM '{csv_str}' (PARALLEL=FALSE)"))
            .map_err(|e| StoreError::Query(format!("COPY Symbol: {e}")))?;
        Ok(())
    }

    pub fn batch_insert_files(&self, files: &[File]) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::batch_insert_files_on(&conn, files)
    }

    /// Insert files using an externally-provided connection (for transaction batching).
    pub fn batch_insert_files_on(
        conn: &lbug::Connection<'_>,
        files: &[File],
    ) -> Result<(), StoreError> {
        if files.is_empty() {
            return Ok(());
        }
        let tmp_dir =
            tempfile::tempdir().map_err(|e| StoreError::Query(format!("tempdir: {e}")))?;
        let csv_path = tmp_dir.path().join("files.csv");
        write_files_csv(files, &csv_path)?;
        let csv_str = csv_path.display().to_string().replace('\\', "/");
        conn.query(&format!("COPY File FROM '{csv_str}' (PARALLEL=FALSE)"))
            .map_err(|e| StoreError::Query(format!("COPY File: {e}")))?;
        Ok(())
    }

    pub fn batch_insert_repo_file_edges(&self, edges: &[(&str, &str)]) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::batch_insert_repo_file_edges_on(&conn, edges)
    }

    /// Insert repo-file edges using an externally-provided connection.
    pub fn batch_insert_repo_file_edges_on(
        conn: &lbug::Connection<'_>,
        edges: &[(&str, &str)],
    ) -> Result<(), StoreError> {
        let mut stmt = conn
            .prepare(
                "MATCH (r:Repo {uid: $repo}), (f:File {uid: $file}) \
                 CREATE (r)-[:REPO_HAS_FILE]->(f)",
            )
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        for (repo_uid, file_uid) in edges {
            conn.execute(
                &mut stmt,
                vec![
                    ("repo", lbug::Value::String(repo_uid.to_string())),
                    ("file", lbug::Value::String(file_uid.to_string())),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        }
        Ok(())
    }

    pub fn batch_insert_file_symbol_edges(&self, edges: &[(&str, &str)]) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::batch_insert_file_symbol_edges_on(&conn, edges)
    }

    /// Insert file-symbol edges using an externally-provided connection.
    pub fn batch_insert_file_symbol_edges_on(
        conn: &lbug::Connection<'_>,
        edges: &[(&str, &str)],
    ) -> Result<(), StoreError> {
        if edges.is_empty() {
            return Ok(());
        }
        let tmp_dir =
            tempfile::tempdir().map_err(|e| StoreError::Query(format!("tempdir: {e}")))?;
        let csv_path = tmp_dir.path().join("file_has_symbol.csv");
        write_edge_pair_csv(edges, &csv_path)?;
        let csv_str = csv_path.display().to_string().replace('\\', "/");
        conn.query(&format!(
            "COPY FILE_HAS_SYMBOL FROM '{csv_str}' (PARALLEL=FALSE)"
        ))
        .map_err(|e| StoreError::Query(format!("COPY FILE_HAS_SYMBOL: {e}")))?;
        Ok(())
    }

    pub fn insert_repo_file_edge(&self, repo_uid: &str, file_uid: &str) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::insert_repo_file_edge_on(&conn, repo_uid, file_uid)
    }

    /// Insert a single repo-file edge using an externally-provided connection.
    pub fn insert_repo_file_edge_on(
        conn: &lbug::Connection<'_>,
        repo_uid: &str,
        file_uid: &str,
    ) -> Result<(), StoreError> {
        exec_params(
            conn,
            "MATCH (r:Repo {uid: $repo}), (f:File {uid: $file}) \
             CREATE (r)-[:REPO_HAS_FILE]->(f)",
            vec![
                ("repo", lbug::Value::String(repo_uid.to_string())),
                ("file", lbug::Value::String(file_uid.to_string())),
            ],
        )
    }

    pub fn insert_file_symbol_edge(
        &self,
        file_uid: &str,
        symbol_uid: &str,
    ) -> Result<(), StoreError> {
        let conn = self.conn()?;
        exec_params(
            &conn,
            "MATCH (f:File {uid: $file}), (s:Symbol {uid: $sym}) \
             CREATE (f)-[:FILE_HAS_SYMBOL]->(s)",
            vec![
                ("file", lbug::Value::String(file_uid.to_string())),
                ("sym", lbug::Value::String(symbol_uid.to_string())),
            ],
        )
    }

    pub fn insert_service_symbol_edge(
        &self,
        service_uid: &str,
        symbol_uid: &str,
    ) -> Result<(), StoreError> {
        let conn = self.conn()?;
        exec_params(
            &conn,
            "MATCH (svc:Service {uid: $svc}), (sym:Symbol {uid: $sym}) \
             CREATE (svc)-[:SERVICE_HAS_SYMBOL]->(sym)",
            vec![
                ("svc", lbug::Value::String(service_uid.to_string())),
                ("sym", lbug::Value::String(symbol_uid.to_string())),
            ],
        )
    }

    pub fn batch_insert_service_symbol_edges(
        &self,
        edges: &[(&str, &str)],
    ) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::batch_insert_service_symbol_edges_on(&conn, edges)
    }

    /// Insert service-symbol edges using an externally-provided connection.
    pub fn batch_insert_service_symbol_edges_on(
        conn: &lbug::Connection<'_>,
        edges: &[(&str, &str)],
    ) -> Result<(), StoreError> {
        if edges.is_empty() {
            return Ok(());
        }
        let tmp_dir =
            tempfile::tempdir().map_err(|e| StoreError::Query(format!("tempdir: {e}")))?;
        let csv_path = tmp_dir.path().join("service_has_symbol.csv");
        write_edge_pair_csv(edges, &csv_path)?;
        let csv_str = csv_path.display().to_string().replace('\\', "/");
        conn.query(&format!(
            "COPY SERVICE_HAS_SYMBOL FROM '{csv_str}' (PARALLEL=FALSE)"
        ))
        .map_err(|e| StoreError::Query(format!("COPY SERVICE_HAS_SYMBOL: {e}")))?;
        Ok(())
    }

    pub fn insert_edge(&self, edge: &ResolvedEdge) -> Result<(), StoreError> {
        let conn = self.conn()?;
        self.insert_edge_with_conn(&conn, edge)
    }

    pub(crate) fn insert_edge_with_conn(
        &self,
        conn: &lbug::Connection<'_>,
        edge: &ResolvedEdge,
    ) -> Result<(), StoreError> {
        let src = edge.source_uid.clone();
        let tgt = edge.target_uid.clone();
        let conf = edge.confidence as f64;
        let evidence_json = if edge.evidence.is_empty() {
            String::new()
        } else {
            serde_json::to_string(&edge.evidence).unwrap_or_default()
        };

        match edge.edge_type {
            EdgeType::Calls
            | EdgeType::Imports
            | EdgeType::Extends
            | EdgeType::Implements
            | EdgeType::Includes
            | EdgeType::Uses
            | EdgeType::Accesses
            | EdgeType::MemberOf => {
                let rel = edge.edge_type.rel_table_name();
                let q = format!(
                    "MATCH (a:Symbol {{uid: $src}}), (b:Symbol {{uid: $tgt}}) \
                     CREATE (a)-[:{rel} {{confidence: $conf, evidence: $ev}}]->(b)"
                );
                exec_params(
                    conn,
                    &q,
                    vec![
                        ("src", lbug::Value::String(src)),
                        ("tgt", lbug::Value::String(tgt)),
                        ("conf", lbug::Value::Double(conf)),
                        ("ev", lbug::Value::String(evidence_json)),
                    ],
                )
            }
            EdgeType::Contains => Err(StoreError::Query(
                "Use insert_repo_file_edge / insert_file_symbol_edge for CONTAINS edges"
                    .to_string(),
            )),
            EdgeType::CrossRepoLink => {
                let link_type = edge
                    .link_type
                    .map(|lt| format!("{lt:?}"))
                    .unwrap_or_default();
                exec_params(
                    conn,
                    "MATCH (a:Symbol {uid: $src}), (b:Symbol {uid: $tgt}) \
                     CREATE (a)-[:CROSS_REPO_LINK {confidence: $conf, link_type: $lt, evidence: $ev}]->(b)",
                    vec![
                        ("src", lbug::Value::String(src)),
                        ("tgt", lbug::Value::String(tgt)),
                        ("conf", lbug::Value::Double(conf)),
                        ("lt", lbug::Value::String(link_type)),
                        ("ev", lbug::Value::String(evidence_json)),
                    ],
                )
            }
            EdgeType::ImplementsContract => exec_params(
                conn,
                "MATCH (a:Symbol {uid: $src}), (b:Contract {uid: $tgt}) \
                 CREATE (a)-[:IMPLEMENTS_CONTRACT {confidence: $conf, evidence: $ev}]->(b)",
                vec![
                    ("src", lbug::Value::String(src)),
                    ("tgt", lbug::Value::String(tgt)),
                    ("conf", lbug::Value::Double(conf)),
                    ("ev", lbug::Value::String(evidence_json)),
                ],
            ),
            EdgeType::Supersedes
            | EdgeType::DependsOn
            | EdgeType::CausedBy
            | EdgeType::RelatesTo => {
                // F11 typed Note→Note relationships.
                let rel = edge.edge_type.rel_table_name();
                let q = format!(
                    "MATCH (a:Note {{uid: $src}}), (b:Note {{uid: $tgt}}) \
                     CREATE (a)-[:{rel} {{confidence: $conf, evidence: $ev}}]->(b)"
                );
                exec_params(
                    conn,
                    &q,
                    vec![
                        ("src", lbug::Value::String(src)),
                        ("tgt", lbug::Value::String(tgt)),
                        ("conf", lbug::Value::Double(conf)),
                        ("ev", lbug::Value::String(evidence_json)),
                    ],
                )
            }
            EdgeType::ProjectIncludesSymbol
            | EdgeType::ProjectIncludesNote
            | EdgeType::ProjectHasComponent
            | EdgeType::ProjectHasParent => Err(StoreError::Query(
                "Use batch_insert_project_symbol_edges / batch_insert_project_note_edges / \
                 insert_project_component_edge / insert_project_parent_edge for Project edges"
                    .to_string(),
            )),
        }
    }

    /// Perform all bulk inserts for a full index in a single transaction.
    /// This avoids per-statement WAL flushes and provides a major speedup.
    pub fn bulk_index_write(
        &self,
        files: &[File],
        symbols: &[Symbol],
        repo_file_edges: &[(&str, &str)],
        file_symbol_edges: &[(&str, &str)],
        services: &[Service],
        service_symbol_edges: &[(&str, &str)],
    ) -> Result<(), StoreError> {
        let conn = self.begin_transaction()?;
        Self::bulk_index_write_on(
            &conn,
            files,
            symbols,
            repo_file_edges,
            file_symbol_edges,
            services,
            service_symbol_edges,
        )?;
        self.commit_transaction(&conn)?;
        Ok(())
    }

    /// Like [`bulk_index_write`](Self::bulk_index_write) but operates on an
    /// existing connection/transaction.
    #[allow(clippy::too_many_arguments)]
    pub fn bulk_index_write_on(
        conn: &lbug::Connection<'_>,
        files: &[File],
        symbols: &[Symbol],
        repo_file_edges: &[(&str, &str)],
        file_symbol_edges: &[(&str, &str)],
        services: &[Service],
        service_symbol_edges: &[(&str, &str)],
    ) -> Result<(), StoreError> {
        // Insert file nodes.
        Self::batch_insert_files_on(conn, files)?;

        // Insert symbol nodes.
        Self::batch_insert_symbols_on(conn, symbols)?;

        // Insert REPO_HAS_FILE edges.
        Self::batch_insert_repo_file_edges_on(conn, repo_file_edges)?;

        // Insert FILE_HAS_SYMBOL edges.
        Self::batch_insert_file_symbol_edges_on(conn, file_symbol_edges)?;

        // Insert service nodes.
        if !services.is_empty() {
            let tmp_dir =
                tempfile::tempdir().map_err(|e| StoreError::Query(format!("tempdir: {e}")))?;
            let csv_path = tmp_dir.path().join("services.csv");
            write_services_csv(services, &csv_path)?;
            let csv_str = csv_path.display().to_string().replace('\\', "/");
            conn.query(&format!("COPY Service FROM '{csv_str}' (PARALLEL=FALSE)"))
                .map_err(|e| StoreError::Query(format!("COPY Service: {e}")))?;
        }

        // Insert SERVICE_HAS_SYMBOL edges.
        Self::batch_insert_service_symbol_edges_on(conn, service_symbol_edges)?;

        Ok(())
    }

    /// Atomically delete old repo data and insert the replacement in a single
    /// transaction. This prevents concurrent readers from seeing an empty repo
    /// between the delete and the insert (the concurrency bug where the
    /// `write_mutex` serialises writes but does not block reads).
    #[allow(clippy::too_many_arguments)]
    pub fn bulk_reindex_write(
        &self,
        repo_uid: &str,
        files: &[File],
        symbols: &[Symbol],
        repo_file_edges: &[(&str, &str)],
        file_symbol_edges: &[(&str, &str)],
        services: &[Service],
        service_symbol_edges: &[(&str, &str)],
    ) -> Result<(usize, usize), StoreError> {
        let conn = self.begin_transaction()?;

        // Delete old data within the transaction.
        let counts = Self::bulk_delete_repo_files_and_symbols_on(&conn, repo_uid)?;
        Self::clear_repo_derived_nodes_on(&conn, repo_uid)?;

        // Insert replacement data in the same transaction.
        Self::bulk_index_write_on(
            &conn,
            files,
            symbols,
            repo_file_edges,
            file_symbol_edges,
            services,
            service_symbol_edges,
        )?;

        self.commit_transaction(&conn)?;
        Ok(counts)
    }

    /// Wrap all markdown vault inserts in a single transaction.
    ///
    /// Accepts the full set of data produced by `index_into_store` (notes,
    /// headings, sections, structural edges, tags, and cross-reference edges)
    /// and writes everything atomically, avoiding per-statement WAL flushes.
    #[allow(clippy::too_many_arguments)]
    pub fn bulk_vault_write(
        &self,
        notes: &[Note],
        headings: &[Heading],
        sections: &[Section],
        vault_note_edges: &[(&str, &str)],
        note_heading_edges: &[(&str, &str)],
        note_section_edges: &[(&str, &str)],
        heading_section_edges: &[(&str, &str)],
        heading_parent_edges: &[(&str, &str)],
        tags: &[Tag],
        note_tag_edges: &[(&str, &str)],
        section_tag_edges: &[(&str, &str)],
        wikilink_to_note_edges: &[(&str, &str, f32, &str)],
        wikilink_to_heading_edges: &[(&str, &str, f32, &str)],
    ) -> Result<(), StoreError> {
        let conn = self.begin_transaction()?;
        Self::bulk_vault_write_on(
            &conn,
            notes,
            headings,
            sections,
            vault_note_edges,
            note_heading_edges,
            note_section_edges,
            heading_section_edges,
            heading_parent_edges,
            tags,
            note_tag_edges,
            section_tag_edges,
            wikilink_to_note_edges,
            wikilink_to_heading_edges,
        )?;
        self.commit_transaction(&conn)?;
        Ok(())
    }

    /// Write all vault nodes and edges on an externally-provided transaction
    /// connection, without opening or committing a transaction of its own.
    /// This lets the caller fold the writes into a larger transaction (e.g.
    /// [`Self::bulk_vault_reindex_write`], which pairs it with the cascade
    /// delete so the two are atomic for concurrent readers).
    #[allow(clippy::too_many_arguments)]
    pub fn bulk_vault_write_on(
        conn: &lbug::Connection<'_>,
        notes: &[Note],
        headings: &[Heading],
        sections: &[Section],
        vault_note_edges: &[(&str, &str)],
        note_heading_edges: &[(&str, &str)],
        note_section_edges: &[(&str, &str)],
        heading_section_edges: &[(&str, &str)],
        heading_parent_edges: &[(&str, &str)],
        tags: &[Tag],
        note_tag_edges: &[(&str, &str)],
        section_tag_edges: &[(&str, &str)],
        wikilink_to_note_edges: &[(&str, &str, f32, &str)],
        wikilink_to_heading_edges: &[(&str, &str, f32, &str)],
    ) -> Result<(), StoreError> {
        // Insert node tables first so edge MATCH clauses find their endpoints.
        Self::batch_insert_notes_on(conn, notes)?;
        Self::batch_insert_headings_on(conn, headings)?;
        Self::batch_insert_sections_on(conn, sections)?;

        // Structural containment edges.
        Self::batch_insert_vault_note_edges_on(conn, vault_note_edges)?;
        Self::batch_insert_note_heading_edges_on(conn, note_heading_edges)?;
        Self::batch_insert_note_section_edges_on(conn, note_section_edges)?;
        Self::batch_insert_heading_section_edges_on(conn, heading_section_edges)?;
        Self::batch_insert_heading_parent_edges_on(conn, heading_parent_edges)?;

        // Tags (nodes + edges). Tags may already exist from a previous index
        // run; the caller is responsible for deduplicating `tags` by uid before
        // passing them in.
        Self::batch_insert_tags_on(conn, tags)?;
        Self::batch_insert_note_tag_edges_on(conn, note_tag_edges)?;
        Self::batch_insert_section_tag_edges_on(conn, section_tag_edges)?;

        // Cross-reference wikilink edges.
        Self::batch_insert_wikilink_to_note_edges_on(conn, wikilink_to_note_edges)?;
        Self::batch_insert_wikilink_to_heading_edges_on(conn, wikilink_to_heading_edges)?;

        Ok(())
    }

    /// Atomically cascade-delete a vault's old data and insert the replacement
    /// in a SINGLE transaction. This prevents concurrent readers from seeing an
    /// empty vault between the delete and the insert — the vault-side analogue
    /// of [`Self::bulk_reindex_write`] for code repos.
    ///
    /// When `vault_existed` is true the old vault is cascade-deleted first
    /// (in the same transaction); otherwise the delete is skipped. The Vault
    /// node is then (re-)created and all nodes/edges written. Because delete,
    /// vault upsert, and inserts share one transaction, a reader always
    /// observes either the complete old vault or the complete new vault — never
    /// the empty intermediate — and any mid-write failure rolls the delete back
    /// so the old vault survives intact. Returns the number of notes deleted.
    #[allow(clippy::too_many_arguments)]
    pub fn bulk_vault_reindex_write(
        &self,
        vault: &Vault,
        vault_existed: bool,
        notes: &[Note],
        headings: &[Heading],
        sections: &[Section],
        vault_note_edges: &[(&str, &str)],
        note_heading_edges: &[(&str, &str)],
        note_section_edges: &[(&str, &str)],
        heading_section_edges: &[(&str, &str)],
        heading_parent_edges: &[(&str, &str)],
        tags: &[Tag],
        note_tag_edges: &[(&str, &str)],
        section_tag_edges: &[(&str, &str)],
        wikilink_to_note_edges: &[(&str, &str, f32, &str)],
        wikilink_to_heading_edges: &[(&str, &str, f32, &str)],
    ) -> Result<usize, StoreError> {
        let conn = self.begin_transaction()?;

        // Delete old vault data within the transaction (if it existed).
        let deleted = if vault_existed {
            Self::delete_vault_cascade_on(&conn, &vault.uid)?
        } else {
            0
        };

        // (Re-)create the Vault node before its edges MATCH it.
        exec_params(
            &conn,
            "CREATE (:Vault {uid: $uid, name: $name, root_path: $rp, instance_id: $iid})",
            vec![
                ("uid", lbug::Value::String(vault.uid.clone())),
                ("name", lbug::Value::String(vault.name.clone())),
                ("rp", lbug::Value::String(vault.root_path.clone())),
                ("iid", lbug::Value::String(vault.instance_id.clone())),
            ],
        )?;

        // Insert replacement data in the same transaction.
        Self::bulk_vault_write_on(
            &conn,
            notes,
            headings,
            sections,
            vault_note_edges,
            note_heading_edges,
            note_section_edges,
            heading_section_edges,
            heading_parent_edges,
            tags,
            note_tag_edges,
            section_tag_edges,
            wikilink_to_note_edges,
            wikilink_to_heading_edges,
        )?;

        self.commit_transaction(&conn)?;
        Ok(deleted)
    }

    pub fn batch_insert_edges(&self, edges: &[ResolvedEdge]) -> Result<(), StoreError> {
        let conn = self.begin_transaction()?;
        Self::batch_insert_edges_on(&conn, edges)?;
        self.commit_transaction(&conn)?;
        Ok(())
    }

    /// Insert resolved edges using an externally-provided connection (for transaction batching).
    pub fn batch_insert_edges_on(
        conn: &lbug::Connection<'_>,
        edges: &[ResolvedEdge],
    ) -> Result<(), StoreError> {
        // Group edges by their SQL query string so we prepare each statement only once.
        use std::collections::HashMap;

        // Collect (query_string, params) pairs grouped by query.
        let mut groups: HashMap<String, Vec<Vec<(&str, lbug::Value)>>> = HashMap::new();

        for edge in edges {
            let src = edge.source_uid.clone();
            let tgt = edge.target_uid.clone();
            let conf = edge.confidence as f64;
            let evidence_json = if edge.evidence.is_empty() {
                String::new()
            } else {
                serde_json::to_string(&edge.evidence).unwrap_or_default()
            };

            match edge.edge_type {
                EdgeType::Calls
                | EdgeType::Imports
                | EdgeType::Extends
                | EdgeType::Implements
                | EdgeType::Includes
                | EdgeType::Uses
                | EdgeType::Accesses
                | EdgeType::MemberOf => {
                    let rel = edge.edge_type.rel_table_name();
                    let key = format!(
                        "MATCH (a:Symbol {{uid: $src}}), (b:Symbol {{uid: $tgt}}) \
                         CREATE (a)-[:{rel} {{confidence: $conf, evidence: $ev}}]->(b)"
                    );
                    groups.entry(key).or_default().push(vec![
                        ("src", lbug::Value::String(src)),
                        ("tgt", lbug::Value::String(tgt)),
                        ("conf", lbug::Value::Double(conf)),
                        ("ev", lbug::Value::String(evidence_json)),
                    ]);
                }
                EdgeType::Contains => {
                    return Err(StoreError::Query(
                        "Use insert_repo_file_edge / insert_file_symbol_edge for CONTAINS edges"
                            .to_string(),
                    ));
                }
                EdgeType::CrossRepoLink => {
                    let link_type = edge
                        .link_type
                        .map(|lt| format!("{lt:?}"))
                        .unwrap_or_default();
                    let key = "MATCH (a:Symbol {uid: $src}), (b:Symbol {uid: $tgt}) \
                               CREATE (a)-[:CROSS_REPO_LINK {confidence: $conf, link_type: $lt, evidence: $ev}]->(b)"
                        .to_string();
                    groups.entry(key).or_default().push(vec![
                        ("src", lbug::Value::String(src)),
                        ("tgt", lbug::Value::String(tgt)),
                        ("conf", lbug::Value::Double(conf)),
                        ("lt", lbug::Value::String(link_type)),
                        ("ev", lbug::Value::String(evidence_json)),
                    ]);
                }
                EdgeType::ImplementsContract => {
                    let key = "MATCH (a:Symbol {uid: $src}), (b:Contract {uid: $tgt}) \
                               CREATE (a)-[:IMPLEMENTS_CONTRACT {confidence: $conf, evidence: $ev}]->(b)"
                        .to_string();
                    groups.entry(key).or_default().push(vec![
                        ("src", lbug::Value::String(src)),
                        ("tgt", lbug::Value::String(tgt)),
                        ("conf", lbug::Value::Double(conf)),
                        ("ev", lbug::Value::String(evidence_json)),
                    ]);
                }
                EdgeType::Supersedes
                | EdgeType::DependsOn
                | EdgeType::CausedBy
                | EdgeType::RelatesTo => {
                    // F11 typed Note→Note relationships.
                    let rel = edge.edge_type.rel_table_name();
                    let key = format!(
                        "MATCH (a:Note {{uid: $src}}), (b:Note {{uid: $tgt}}) \
                         CREATE (a)-[:{rel} {{confidence: $conf, evidence: $ev}}]->(b)"
                    );
                    groups.entry(key).or_default().push(vec![
                        ("src", lbug::Value::String(src)),
                        ("tgt", lbug::Value::String(tgt)),
                        ("conf", lbug::Value::Double(conf)),
                        ("ev", lbug::Value::String(evidence_json)),
                    ]);
                }
                EdgeType::ProjectIncludesSymbol
                | EdgeType::ProjectIncludesNote
                | EdgeType::ProjectHasComponent
                | EdgeType::ProjectHasParent => {
                    return Err(StoreError::Query(
                        "Use batch_insert_project_symbol_edges / batch_insert_project_note_edges / \
                         insert_project_component_edge / insert_project_parent_edge for Project edges"
                            .to_string(),
                    ));
                }
            }
        }

        for (query, param_sets) in &groups {
            let mut stmt = conn
                .prepare(query)
                .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
            for params in param_sets {
                conn.execute(&mut stmt, params.clone())
                    .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
            }
        }
        Ok(())
    }

    // ── Brain extension: markdown node inserts ──────────────────────────────

    pub fn insert_vault(&self, vault: &Vault) -> Result<(), StoreError> {
        let conn = self.conn()?;
        exec_params(
            &conn,
            "CREATE (:Vault {uid: $uid, name: $name, root_path: $rp, instance_id: $iid})",
            vec![
                ("uid", lbug::Value::String(vault.uid.clone())),
                ("name", lbug::Value::String(vault.name.clone())),
                ("rp", lbug::Value::String(vault.root_path.clone())),
                ("iid", lbug::Value::String(vault.instance_id.clone())),
            ],
        )
    }

    pub fn upsert_vault(&self, vault: &Vault) -> Result<(), StoreError> {
        let conn = self.conn()?;
        let _ = exec_params(
            &conn,
            "MATCH (v:Vault {uid: $uid}) DETACH DELETE v",
            vec![("uid", lbug::Value::String(vault.uid.clone()))],
        );
        self.insert_vault(vault)
    }

    pub fn insert_note(&self, note: &Note) -> Result<(), StoreError> {
        let conn = self.conn()?;
        exec_params(
            &conn,
            "CREATE (:Note {uid: $uid, vault_uid: $vid, file_path: $fp, title: $title, \
             note_kind: $nk, word_count: $wc, content_hash: $hash, frontmatter: $fm, \
             created_at: $ca, modified_at: $ma, pagerank_score: $pr})",
            vec![
                ("uid", lbug::Value::String(note.uid.clone())),
                ("vid", lbug::Value::String(note.vault_uid.clone())),
                ("fp", lbug::Value::String(note.file_path.clone())),
                ("title", lbug::Value::String(note.title.clone())),
                ("nk", lbug::Value::String(note.note_kind.to_string())),
                ("wc", lbug::Value::Int64(note.word_count as i64)),
                ("hash", lbug::Value::String(note.content_hash.clone())),
                (
                    "fm",
                    lbug::Value::String(note.frontmatter.clone().unwrap_or_default()),
                ),
                (
                    "ca",
                    lbug::Value::String(note.created_at.clone().unwrap_or_default()),
                ),
                (
                    "ma",
                    lbug::Value::String(note.modified_at.clone().unwrap_or_default()),
                ),
                (
                    "pr",
                    lbug::Value::Double(note.pagerank_score.unwrap_or(0.0)),
                ),
            ],
        )
    }

    pub fn batch_insert_notes(&self, notes: &[Note]) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::batch_insert_notes_on(&conn, notes)
    }

    /// Insert notes using an externally-provided connection (for transaction batching).
    pub fn batch_insert_notes_on(
        conn: &lbug::Connection<'_>,
        notes: &[Note],
    ) -> Result<(), StoreError> {
        let mut stmt = conn
            .prepare(
                "CREATE (:Note {uid: $uid, vault_uid: $vid, file_path: $fp, title: $title, \
                 note_kind: $nk, word_count: $wc, content_hash: $hash, frontmatter: $fm, \
                 created_at: $ca, modified_at: $ma, pagerank_score: $pr})",
            )
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        for note in notes {
            conn.execute(
                &mut stmt,
                vec![
                    ("uid", lbug::Value::String(note.uid.clone())),
                    ("vid", lbug::Value::String(note.vault_uid.clone())),
                    ("fp", lbug::Value::String(note.file_path.clone())),
                    ("title", lbug::Value::String(note.title.clone())),
                    ("nk", lbug::Value::String(note.note_kind.to_string())),
                    ("wc", lbug::Value::Int64(note.word_count as i64)),
                    ("hash", lbug::Value::String(note.content_hash.clone())),
                    (
                        "fm",
                        lbug::Value::String(note.frontmatter.clone().unwrap_or_default()),
                    ),
                    (
                        "ca",
                        lbug::Value::String(note.created_at.clone().unwrap_or_default()),
                    ),
                    (
                        "ma",
                        lbug::Value::String(note.modified_at.clone().unwrap_or_default()),
                    ),
                    (
                        "pr",
                        lbug::Value::Double(note.pagerank_score.unwrap_or(0.0)),
                    ),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        }
        Ok(())
    }

    pub fn insert_vault_note_edge(
        &self,
        vault_uid: &str,
        note_uid: &str,
    ) -> Result<(), StoreError> {
        let conn = self.conn()?;
        exec_params(
            &conn,
            "MATCH (v:Vault {uid: $vid}), (n:Note {uid: $nid}) \
             CREATE (v)-[:VAULT_HAS_NOTE]->(n)",
            vec![
                ("vid", lbug::Value::String(vault_uid.to_string())),
                ("nid", lbug::Value::String(note_uid.to_string())),
            ],
        )
    }

    pub fn batch_insert_vault_note_edges(&self, edges: &[(&str, &str)]) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::batch_insert_vault_note_edges_on(&conn, edges)
    }

    /// Insert vault-note edges using an externally-provided connection (for transaction batching).
    pub fn batch_insert_vault_note_edges_on(
        conn: &lbug::Connection<'_>,
        edges: &[(&str, &str)],
    ) -> Result<(), StoreError> {
        let mut stmt = conn
            .prepare(
                "MATCH (v:Vault {uid: $vid}), (n:Note {uid: $nid}) \
                 CREATE (v)-[:VAULT_HAS_NOTE]->(n)",
            )
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        for (vault_uid, note_uid) in edges {
            conn.execute(
                &mut stmt,
                vec![
                    ("vid", lbug::Value::String(vault_uid.to_string())),
                    ("nid", lbug::Value::String(note_uid.to_string())),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        }
        Ok(())
    }

    // ── Brain extension: Heading and Section inserts ────────────────────────

    pub fn insert_heading(&self, heading: &Heading) -> Result<(), StoreError> {
        let conn = self.conn()?;
        exec_params(
            &conn,
            "CREATE (:Heading {uid: $uid, note_uid: $nid, level: $lvl, text: $text, \
             slug: $slug, start_line: $sl, end_line: $el, content_hash: $hash})",
            vec![
                ("uid", lbug::Value::String(heading.uid.clone())),
                ("nid", lbug::Value::String(heading.note_uid.clone())),
                ("lvl", lbug::Value::Int64(heading.level as i64)),
                ("text", lbug::Value::String(heading.text.clone())),
                ("slug", lbug::Value::String(heading.slug.clone())),
                ("sl", lbug::Value::Int64(heading.start_line as i64)),
                ("el", lbug::Value::Int64(heading.end_line as i64)),
                ("hash", lbug::Value::String(heading.content_hash.clone())),
            ],
        )
    }

    pub fn batch_insert_headings(&self, headings: &[Heading]) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::batch_insert_headings_on(&conn, headings)
    }

    /// Insert headings using an externally-provided connection (for transaction batching).
    pub fn batch_insert_headings_on(
        conn: &lbug::Connection<'_>,
        headings: &[Heading],
    ) -> Result<(), StoreError> {
        let mut stmt = conn
            .prepare(
                "CREATE (:Heading {uid: $uid, note_uid: $nid, level: $lvl, text: $text, \
                 slug: $slug, start_line: $sl, end_line: $el, content_hash: $hash})",
            )
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        for h in headings {
            conn.execute(
                &mut stmt,
                vec![
                    ("uid", lbug::Value::String(h.uid.clone())),
                    ("nid", lbug::Value::String(h.note_uid.clone())),
                    ("lvl", lbug::Value::Int64(h.level as i64)),
                    ("text", lbug::Value::String(h.text.clone())),
                    ("slug", lbug::Value::String(h.slug.clone())),
                    ("sl", lbug::Value::Int64(h.start_line as i64)),
                    ("el", lbug::Value::Int64(h.end_line as i64)),
                    ("hash", lbug::Value::String(h.content_hash.clone())),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        }
        Ok(())
    }

    pub fn insert_section(&self, section: &Section) -> Result<(), StoreError> {
        let conn = self.conn()?;
        exec_params(
            &conn,
            "CREATE (:Section {uid: $uid, note_uid: $nid, heading_uid: $hid, \
             start_line: $sl, end_line: $el, text_hash: $th, text_content: $tc, \
             word_count: $wc, pagerank_score: $pr})",
            vec![
                ("uid", lbug::Value::String(section.uid.clone())),
                ("nid", lbug::Value::String(section.note_uid.clone())),
                (
                    "hid",
                    lbug::Value::String(section.heading_uid.clone().unwrap_or_default()),
                ),
                ("sl", lbug::Value::Int64(section.start_line as i64)),
                ("el", lbug::Value::Int64(section.end_line as i64)),
                ("th", lbug::Value::String(section.text_hash.clone())),
                ("tc", lbug::Value::String(section.text_content.clone())),
                ("wc", lbug::Value::Int64(section.word_count as i64)),
                (
                    "pr",
                    lbug::Value::Double(section.pagerank_score.unwrap_or(0.0)),
                ),
            ],
        )
    }

    pub fn batch_insert_sections(&self, sections: &[Section]) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::batch_insert_sections_on(&conn, sections)
    }

    /// Insert sections using an externally-provided connection (for transaction batching).
    pub fn batch_insert_sections_on(
        conn: &lbug::Connection<'_>,
        sections: &[Section],
    ) -> Result<(), StoreError> {
        let mut stmt = conn
            .prepare(
                "CREATE (:Section {uid: $uid, note_uid: $nid, heading_uid: $hid, \
                 start_line: $sl, end_line: $el, text_hash: $th, text_content: $tc, \
                 word_count: $wc, pagerank_score: $pr})",
            )
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        for s in sections {
            conn.execute(
                &mut stmt,
                vec![
                    ("uid", lbug::Value::String(s.uid.clone())),
                    ("nid", lbug::Value::String(s.note_uid.clone())),
                    (
                        "hid",
                        lbug::Value::String(s.heading_uid.clone().unwrap_or_default()),
                    ),
                    ("sl", lbug::Value::Int64(s.start_line as i64)),
                    ("el", lbug::Value::Int64(s.end_line as i64)),
                    ("th", lbug::Value::String(s.text_hash.clone())),
                    ("tc", lbug::Value::String(s.text_content.clone())),
                    ("wc", lbug::Value::Int64(s.word_count as i64)),
                    ("pr", lbug::Value::Double(s.pagerank_score.unwrap_or(0.0))),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        }
        Ok(())
    }

    pub fn batch_insert_note_heading_edges(
        &self,
        edges: &[(&str, &str)],
    ) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::batch_insert_note_heading_edges_on(&conn, edges)
    }

    /// Insert note-heading edges using an externally-provided connection (for transaction batching).
    pub fn batch_insert_note_heading_edges_on(
        conn: &lbug::Connection<'_>,
        edges: &[(&str, &str)],
    ) -> Result<(), StoreError> {
        let mut stmt = conn
            .prepare(
                "MATCH (n:Note {uid: $nid}), (h:Heading {uid: $hid}) \
                 CREATE (n)-[:NOTE_HAS_HEADING]->(h)",
            )
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        for (note_uid, heading_uid) in edges {
            conn.execute(
                &mut stmt,
                vec![
                    ("nid", lbug::Value::String(note_uid.to_string())),
                    ("hid", lbug::Value::String(heading_uid.to_string())),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        }
        Ok(())
    }

    pub fn batch_insert_note_section_edges(
        &self,
        edges: &[(&str, &str)],
    ) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::batch_insert_note_section_edges_on(&conn, edges)
    }

    /// Insert note-section edges using an externally-provided connection (for transaction batching).
    pub fn batch_insert_note_section_edges_on(
        conn: &lbug::Connection<'_>,
        edges: &[(&str, &str)],
    ) -> Result<(), StoreError> {
        let mut stmt = conn
            .prepare(
                "MATCH (n:Note {uid: $nid}), (s:Section {uid: $sid}) \
                 CREATE (n)-[:NOTE_HAS_SECTION]->(s)",
            )
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        for (note_uid, section_uid) in edges {
            conn.execute(
                &mut stmt,
                vec![
                    ("nid", lbug::Value::String(note_uid.to_string())),
                    ("sid", lbug::Value::String(section_uid.to_string())),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        }
        Ok(())
    }

    pub fn batch_insert_heading_section_edges(
        &self,
        edges: &[(&str, &str)],
    ) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::batch_insert_heading_section_edges_on(&conn, edges)
    }

    /// Insert heading-section edges using an externally-provided connection (for transaction batching).
    pub fn batch_insert_heading_section_edges_on(
        conn: &lbug::Connection<'_>,
        edges: &[(&str, &str)],
    ) -> Result<(), StoreError> {
        let mut stmt = conn
            .prepare(
                "MATCH (h:Heading {uid: $hid}), (s:Section {uid: $sid}) \
                 CREATE (h)-[:HEADING_HAS_SECTION]->(s)",
            )
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        for (heading_uid, section_uid) in edges {
            conn.execute(
                &mut stmt,
                vec![
                    ("hid", lbug::Value::String(heading_uid.to_string())),
                    ("sid", lbug::Value::String(section_uid.to_string())),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        }
        Ok(())
    }

    pub fn batch_insert_heading_parent_edges(
        &self,
        edges: &[(&str, &str)],
    ) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::batch_insert_heading_parent_edges_on(&conn, edges)
    }

    /// Insert heading-parent edges using an externally-provided connection (for transaction batching).
    pub fn batch_insert_heading_parent_edges_on(
        conn: &lbug::Connection<'_>,
        edges: &[(&str, &str)],
    ) -> Result<(), StoreError> {
        let mut stmt = conn
            .prepare(
                "MATCH (child:Heading {uid: $cid}), (parent:Heading {uid: $pid}) \
                 CREATE (child)-[:HEADING_PARENT]->(parent)",
            )
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        for (child_uid, parent_uid) in edges {
            conn.execute(
                &mut stmt,
                vec![
                    ("cid", lbug::Value::String(child_uid.to_string())),
                    ("pid", lbug::Value::String(parent_uid.to_string())),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        }
        Ok(())
    }

    // ── Brain extension: Tag + Project + cross-reference edges ──────────────

    pub fn insert_tag(&self, tag: &Tag) -> Result<(), StoreError> {
        let conn = self.conn()?;
        exec_params(
            &conn,
            "CREATE (:Tag {uid: $uid, vault_uid: $vid, name: $name})",
            vec![
                ("uid", lbug::Value::String(tag.uid.clone())),
                ("vid", lbug::Value::String(tag.vault_uid.clone())),
                ("name", lbug::Value::String(tag.name.clone())),
            ],
        )
    }

    pub fn batch_insert_tags(&self, tags: &[Tag]) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::batch_insert_tags_on(&conn, tags)
    }

    /// Insert tags using an externally-provided connection (for transaction batching).
    pub fn batch_insert_tags_on(
        conn: &lbug::Connection<'_>,
        tags: &[Tag],
    ) -> Result<(), StoreError> {
        let mut stmt = conn
            .prepare("CREATE (:Tag {uid: $uid, vault_uid: $vid, name: $name})")
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        for t in tags {
            conn.execute(
                &mut stmt,
                vec![
                    ("uid", lbug::Value::String(t.uid.clone())),
                    ("vid", lbug::Value::String(t.vault_uid.clone())),
                    ("name", lbug::Value::String(t.name.clone())),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        }
        Ok(())
    }

    pub fn insert_project(&self, project: &Project) -> Result<(), StoreError> {
        let conn = self.conn()?;
        exec_params(
            &conn,
            "CREATE (:Project {uid: $uid, name: $name, summary: $summary, instance_id: $iid})",
            vec![
                ("uid", lbug::Value::String(project.uid.clone())),
                ("name", lbug::Value::String(project.name.clone())),
                (
                    "summary",
                    lbug::Value::String(project.summary.clone().unwrap_or_default()),
                ),
                ("iid", lbug::Value::String(project.instance_id.clone())),
            ],
        )
    }

    /// Record a genuinely-unresolved wikilink (`[[Target]]` with no matching
    /// note) so the broken-links query can surface it. `uid` is derived from
    /// the source section + link text by the caller so re-indexing the same
    /// note replaces rather than duplicates. DETACH DELETE-by-uid first makes
    /// the insert idempotent. Table may not exist on older DBs — caller treats
    /// errors as best-effort.
    pub fn insert_unresolved_wikilink(
        &self,
        uid: &str,
        source_note_uid: &str,
        source_path: &str,
        source_title: &str,
        wikilink_text: &str,
    ) -> Result<(), StoreError> {
        let conn = self.conn()?;
        exec_params(
            &conn,
            "MATCH (u:UnresolvedWikilink {uid: $uid}) DETACH DELETE u",
            vec![("uid", lbug::Value::String(uid.to_string()))],
        )?;
        exec_params(
            &conn,
            "CREATE (:UnresolvedWikilink {uid: $uid, source_note_uid: $snu, \
             source_path: $sp, source_title: $st, wikilink_text: $wt})",
            vec![
                ("uid", lbug::Value::String(uid.to_string())),
                ("snu", lbug::Value::String(source_note_uid.to_string())),
                ("sp", lbug::Value::String(source_path.to_string())),
                ("st", lbug::Value::String(source_title.to_string())),
                ("wt", lbug::Value::String(wikilink_text.to_string())),
            ],
        )
    }

    /// Batch-insert unresolved wikilinks, reusing ONE connection and prepared
    /// statements. The per-row `insert_unresolved_wikilink` opened a fresh
    /// connection and ran two separate queries per row (~ms each), so a note with
    /// thousands of unresolved links (a big index/MOC note, or a note whose
    /// targets don't exist yet) took ~20ms/link — seconds to a hang. Records are
    /// `(uid, source_note_uid, source_path, source_title, wikilink_text)`.
    pub fn batch_insert_unresolved_wikilinks(
        &self,
        records: &[UnresolvedWikilinkRecord],
    ) -> Result<(), StoreError> {
        if records.is_empty() {
            return Ok(());
        }
        // One explicit transaction for all rows: KuzuDB auto-commits (WAL fsync)
        // per statement otherwise, which is what made the per-row insert take
        // ~ms/link. Prepared statements are reused across the batch.
        let conn = self.begin_transaction()?;
        Self::batch_insert_unresolved_wikilinks_on(&conn, records)?;
        self.commit_transaction(&conn)?;
        Ok(())
    }

    /// Insert unresolved wikilinks on a caller-provided connection, so a larger
    /// transaction (e.g. `reparent_vault`) can batch them with its other work.
    pub fn batch_insert_unresolved_wikilinks_on(
        conn: &lbug::Connection<'_>,
        records: &[UnresolvedWikilinkRecord],
    ) -> Result<(), StoreError> {
        if records.is_empty() {
            return Ok(());
        }
        let mut del = conn
            .prepare("MATCH (u:UnresolvedWikilink {uid: $uid}) DETACH DELETE u")
            .map_err(|e| StoreError::Query(format!("prepare delete unresolved: {e}")))?;
        let mut cre = conn
            .prepare(
                "CREATE (:UnresolvedWikilink {uid: $uid, source_note_uid: $snu, \
                 source_path: $sp, source_title: $st, wikilink_text: $wt})",
            )
            .map_err(|e| StoreError::Query(format!("prepare create unresolved: {e}")))?;
        for (uid, snu, sp, st, wt) in records {
            conn.execute(&mut del, vec![("uid", lbug::Value::String(uid.clone()))])
                .map_err(|e| StoreError::Query(format!("delete unresolved: {e}")))?;
            conn.execute(
                &mut cre,
                vec![
                    ("uid", lbug::Value::String(uid.clone())),
                    ("snu", lbug::Value::String(snu.clone())),
                    ("sp", lbug::Value::String(sp.clone())),
                    ("st", lbug::Value::String(st.clone())),
                    ("wt", lbug::Value::String(wt.clone())),
                ],
            )
            .map_err(|e| StoreError::Query(format!("create unresolved: {e}")))?;
        }
        Ok(())
    }

    /// Remove all recorded unresolved wikilinks originating from `note_uid`.
    /// Called from `delete_note_cascade` so stale rows do not linger after a
    /// note is re-indexed (e.g. once its target note appears). Best-effort:
    /// silently succeeds if the table does not exist.
    pub fn delete_unresolved_wikilinks_for_note(&self, note_uid: &str) -> Result<(), StoreError> {
        let conn = self.conn()?;
        if let Err(e) = exec_params(
            &conn,
            "MATCH (u:UnresolvedWikilink {source_note_uid: $uid}) DETACH DELETE u",
            vec![("uid", lbug::Value::String(note_uid.to_string()))],
        ) {
            tracing::trace!("delete_unresolved_wikilinks_for_note skipped: {e}");
        }
        Ok(())
    }

    /// Insert (or idempotently replace) a Contract node. Mirrors
    /// `insert_project`: DETACH DELETE by UID first so re-indexing a spec
    /// or handler does not accumulate duplicate Contract nodes.
    pub fn insert_contract(&self, contract: &Contract) -> Result<(), StoreError> {
        let conn = self.conn()?;
        exec_params(
            &conn,
            "MATCH (c:Contract {uid: $uid}) DETACH DELETE c",
            vec![("uid", lbug::Value::String(contract.uid.clone()))],
        )?;
        exec_params(
            &conn,
            "CREATE (:Contract {uid: $uid, kind: $kind, verb: $verb, path: $path, \
             operation_id: $op, repo_uid: $repo, source_path: $src, confidence: $conf})",
            vec![
                ("uid", lbug::Value::String(contract.uid.clone())),
                ("kind", lbug::Value::String(contract.kind.clone())),
                (
                    "verb",
                    lbug::Value::String(contract.verb.clone().unwrap_or_default()),
                ),
                (
                    "path",
                    lbug::Value::String(contract.path.clone().unwrap_or_default()),
                ),
                (
                    "op",
                    lbug::Value::String(contract.operation_id.clone().unwrap_or_default()),
                ),
                ("repo", lbug::Value::String(contract.repo_uid.clone())),
                ("src", lbug::Value::String(contract.source_path.clone())),
                ("conf", lbug::Value::Float(contract.confidence)),
            ],
        )
    }

    /// Delete all Contract nodes for a given repo in one query, so re-indexing
    /// starts clean without per-contract DELETE round-trips.
    pub fn clear_repo_contracts(&self, repo_uid: &str) -> Result<(), StoreError> {
        let conn = self.conn()?;
        exec_params(
            &conn,
            "MATCH (c:Contract) WHERE c.repo_uid = $repo DETACH DELETE c",
            vec![("repo", lbug::Value::String(repo_uid.to_string()))],
        )
    }

    /// Bulk-insert Contract nodes via COPY FROM CSV (one connection, one query).
    pub fn batch_insert_contracts(&self, contracts: &[Contract]) -> Result<(), StoreError> {
        if contracts.is_empty() {
            return Ok(());
        }
        let tmp_dir =
            tempfile::tempdir().map_err(|e| StoreError::Query(format!("tempdir: {e}")))?;
        let csv_path = tmp_dir.path().join("contracts.csv");
        write_contracts_csv(contracts, &csv_path)?;
        let csv_str = csv_path.display().to_string().replace('\\', "/");
        let conn = self.conn()?;
        conn.query(&format!("COPY Contract FROM '{csv_str}' (PARALLEL=FALSE)"))
            .map_err(|e| StoreError::Query(format!("COPY Contract: {e}")))?;
        Ok(())
    }

    pub fn batch_insert_wikilink_to_note_edges(
        &self,
        edges: &[(&str, &str, f32, &str)],
    ) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::batch_insert_wikilink_to_note_edges_on(&conn, edges)
    }

    /// Insert wikilink-to-note edges using an externally-provided connection (for transaction batching).
    pub fn batch_insert_wikilink_to_note_edges_on(
        conn: &lbug::Connection<'_>,
        edges: &[(&str, &str, f32, &str)],
    ) -> Result<(), StoreError> {
        let mut stmt = conn
            .prepare(
                "MATCH (s:Section {uid: $sid}), (n:Note {uid: $nid}) \
                 CREATE (s)-[:WIKILINK_TO_NOTE {confidence: $conf, display: $disp}]->(n)",
            )
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        for (sec_uid, note_uid, conf, display) in edges {
            conn.execute(
                &mut stmt,
                vec![
                    ("sid", lbug::Value::String(sec_uid.to_string())),
                    ("nid", lbug::Value::String(note_uid.to_string())),
                    ("conf", lbug::Value::Double(*conf as f64)),
                    ("disp", lbug::Value::String(display.to_string())),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        }
        Ok(())
    }

    pub fn batch_insert_wikilink_to_heading_edges(
        &self,
        edges: &[(&str, &str, f32, &str)],
    ) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::batch_insert_wikilink_to_heading_edges_on(&conn, edges)
    }

    /// Insert wikilink-to-heading edges using an externally-provided connection (for transaction batching).
    pub fn batch_insert_wikilink_to_heading_edges_on(
        conn: &lbug::Connection<'_>,
        edges: &[(&str, &str, f32, &str)],
    ) -> Result<(), StoreError> {
        let mut stmt = conn
            .prepare(
                "MATCH (s:Section {uid: $sid}), (h:Heading {uid: $hid}) \
                 CREATE (s)-[:WIKILINK_TO_HEADING {confidence: $conf, display: $disp}]->(h)",
            )
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        for (sec_uid, head_uid, conf, display) in edges {
            conn.execute(
                &mut stmt,
                vec![
                    ("sid", lbug::Value::String(sec_uid.to_string())),
                    ("hid", lbug::Value::String(head_uid.to_string())),
                    ("conf", lbug::Value::Double(*conf as f64)),
                    ("disp", lbug::Value::String(display.to_string())),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        }
        Ok(())
    }

    pub fn batch_insert_note_tag_edges(&self, edges: &[(&str, &str)]) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::batch_insert_note_tag_edges_on(&conn, edges)
    }

    /// Insert note-tag edges using an externally-provided connection (for transaction batching).
    pub fn batch_insert_note_tag_edges_on(
        conn: &lbug::Connection<'_>,
        edges: &[(&str, &str)],
    ) -> Result<(), StoreError> {
        let mut stmt = conn
            .prepare(
                "MATCH (n:Note {uid: $nid}), (t:Tag {uid: $tid}) \
                 CREATE (n)-[:NOTE_TAGGED_WITH]->(t)",
            )
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        for (note_uid, tag_uid) in edges {
            conn.execute(
                &mut stmt,
                vec![
                    ("nid", lbug::Value::String(note_uid.to_string())),
                    ("tid", lbug::Value::String(tag_uid.to_string())),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        }
        Ok(())
    }

    pub fn batch_insert_section_tag_edges(&self, edges: &[(&str, &str)]) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::batch_insert_section_tag_edges_on(&conn, edges)
    }

    /// Insert section-tag edges using an externally-provided connection (for transaction batching).
    pub fn batch_insert_section_tag_edges_on(
        conn: &lbug::Connection<'_>,
        edges: &[(&str, &str)],
    ) -> Result<(), StoreError> {
        let mut stmt = conn
            .prepare(
                "MATCH (s:Section {uid: $sid}), (t:Tag {uid: $tid}) \
                 CREATE (s)-[:SECTION_TAGGED_WITH]->(t)",
            )
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        for (section_uid, tag_uid) in edges {
            conn.execute(
                &mut stmt,
                vec![
                    ("sid", lbug::Value::String(section_uid.to_string())),
                    ("tid", lbug::Value::String(tag_uid.to_string())),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        }
        Ok(())
    }

    // ── Upsert helpers (delete-then-create) ──────────────────────────────
    //
    // LadybugDB/Kuzu doesn't support MERGE or SET for most node types.
    // The established pattern (see `update_repo_sha`) is: read → DETACH
    // DELETE → re-CREATE. These helpers formalize this for node types
    // that need idempotent re-insertion (e.g. project materialization).

    /// Upsert a Note node. Deletes the existing Note (cascading headings,
    /// sections, and all incident edges) then re-inserts it.
    pub fn upsert_note(&self, note: &Note) -> Result<(), StoreError> {
        // delete_note_cascade is a no-op when the UID does not exist.
        self.delete_note_cascade(&note.uid)?;
        self.insert_note(note)
    }

    /// Upsert a Project node. DETACH DELETEs any existing node with the
    /// same UID **and** any existing node with the same name
    /// (case-insensitive), then re-creates it. The name-based cleanup
    /// prevents duplicate Project nodes when `instance_id` changes
    /// between materializer runs (which changes the UID).
    pub fn upsert_project(&self, project: &Project) -> Result<(), StoreError> {
        let conn = self.conn()?;

        // Delete by exact UID (fast path — covers the common case).
        exec_params(
            &conn,
            "MATCH (p:Project {uid: $uid}) DETACH DELETE p",
            vec![("uid", lbug::Value::String(project.uid.clone()))],
        )?;

        // Also delete any project with the same name regardless of UID.
        // LadybugDB has no toLower(), so we list all projects and delete
        // matches by UID in a second pass.
        let all = self.list_projects()?;
        let needle = project.name.to_lowercase();
        for existing in &all {
            if existing.uid != project.uid && existing.name.to_lowercase() == needle {
                exec_params(
                    &conn,
                    "MATCH (p:Project {uid: $uid}) DETACH DELETE p",
                    vec![("uid", lbug::Value::String(existing.uid.clone()))],
                )?;
            }
        }

        self.insert_project(project)
    }

    /// Upsert a batch of sections. For each section, deletes it by UID
    /// (DETACH DELETE to remove incident edges) then re-inserts.
    pub fn batch_upsert_sections(&self, sections: &[Section]) -> Result<(), StoreError> {
        let conn = self.conn()?;

        // Delete existing sections.
        let mut del_stmt = conn
            .prepare("MATCH (s:Section {uid: $uid}) DETACH DELETE s")
            .map_err(|e| StoreError::Query(format!("prepare delete: {e}")))?;
        for s in sections {
            conn.execute(
                &mut del_stmt,
                vec![("uid", lbug::Value::String(s.uid.clone()))],
            )
            .map_err(|e| StoreError::Query(format!("execute delete: {e}")))?;
        }

        // Re-insert.
        let mut ins_stmt = conn
            .prepare(
                "CREATE (:Section {uid: $uid, note_uid: $nid, heading_uid: $hid, \
                 start_line: $sl, end_line: $el, text_hash: $th, text_content: $tc, \
                 word_count: $wc, pagerank_score: $pr})",
            )
            .map_err(|e| StoreError::Query(format!("prepare insert: {e}")))?;
        for s in sections {
            conn.execute(
                &mut ins_stmt,
                vec![
                    ("uid", lbug::Value::String(s.uid.clone())),
                    ("nid", lbug::Value::String(s.note_uid.clone())),
                    (
                        "hid",
                        lbug::Value::String(s.heading_uid.clone().unwrap_or_default()),
                    ),
                    ("sl", lbug::Value::Int64(s.start_line as i64)),
                    ("el", lbug::Value::Int64(s.end_line as i64)),
                    ("th", lbug::Value::String(s.text_hash.clone())),
                    ("tc", lbug::Value::String(s.text_content.clone())),
                    ("wc", lbug::Value::Int64(s.word_count as i64)),
                    ("pr", lbug::Value::Double(s.pagerank_score.unwrap_or(0.0))),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute insert: {e}")))?;
        }
        Ok(())
    }

    /// Delete a Note and everything that belongs to it: all Headings, all
    /// Sections, and every edge involving any of those nodes (both
    /// containment and cross-reference).
    ///
    /// LadybugDB's Cypher dialect supports `DETACH DELETE` which removes
    /// the node along with all its attached relationships in one shot —
    /// that's what makes this cascade tractable without enumerating every
    /// individual REL TABLE the new nodes participate in.
    ///
    /// This is the foundation of incremental updates: on every file
    /// modify, the watcher calls `delete_note_cascade(note_uid)` then
    /// re-inserts the freshly-parsed Note + descendants. UIDs are stable
    /// across edits (content_hash is in `Note.content_hash`, not in
    /// `Note.uid`), so any inbound wikilinks from other notes survive the
    /// cycle naturally — they get reattached to the same target_uid on
    /// the next reindex pass.
    pub fn delete_note_cascade(&self, note_uid: &str) -> Result<(), StoreError> {
        let conn = self.conn()?;

        // 1. Drop every Section whose ownership property references this
        //    note, including fragments whose NOTE_HAS_SECTION edge is missing.
        //    DETACH removes the
        //    NOTE_HAS_SECTION, HEADING_HAS_SECTION, WIKILINK_TO_NOTE
        //    (incoming) and SECTION_TAGGED_WITH edges along with it.
        exec_params(
            &conn,
            "MATCH (s:Section) WHERE s.note_uid = $uid DETACH DELETE s",
            vec![("uid", lbug::Value::String(note_uid.to_string()))],
        )?;

        // 2. Drop Sections whose ownership edge references this note. This
        //    independent pass also handles a missing or corrupt note_uid
        //    property.
        exec_params(
            &conn,
            "MATCH (n:Note {uid: $uid})-[:NOTE_HAS_SECTION]->(s:Section) DETACH DELETE s",
            vec![("uid", lbug::Value::String(note_uid.to_string()))],
        )?;

        // 3. Drop every Heading whose ownership property references this
        //    note, including fragments whose NOTE_HAS_HEADING edge is missing.
        //    DETACH removes
        //    NOTE_HAS_HEADING, HEADING_HAS_SECTION (already gone if its
        //    section was dropped above), HEADING_PARENT (both directions),
        //    and WIKILINK_TO_HEADING (incoming).
        exec_params(
            &conn,
            "MATCH (h:Heading) WHERE h.note_uid = $uid DETACH DELETE h",
            vec![("uid", lbug::Value::String(note_uid.to_string()))],
        )?;

        // 4. Drop Headings whose ownership edge references this note. This
        //    independent pass also handles a missing or corrupt note_uid
        //    property.
        exec_params(
            &conn,
            "MATCH (n:Note {uid: $uid})-[:NOTE_HAS_HEADING]->(h:Heading) DETACH DELETE h",
            vec![("uid", lbug::Value::String(note_uid.to_string()))],
        )?;

        // 5. Drop the Note itself. DETACH removes VAULT_HAS_NOTE,
        //    NOTE_TAGGED_WITH, PROJECT_INCLUDES_NOTE, and any incoming
        //    WIKILINK_TO_NOTE edges from other notes' sections.
        exec_params(
            &conn,
            "MATCH (n:Note {uid: $uid}) DETACH DELETE n",
            vec![("uid", lbug::Value::String(note_uid.to_string()))],
        )?;

        // 6. Drop any recorded unresolved-wikilink rows for this note so they
        //    do not linger after re-index (e.g. once the target note appears).
        self.delete_unresolved_wikilinks_for_note(note_uid)?;

        Ok(())
    }

    /// Cascade-delete a Vault and every Note belonging to it using bulk
    /// DETACH DELETE queries scoped by `vault_uid` — avoids the O(N) per-note
    /// query loop that was issuing 4 queries × N notes.
    ///
    /// Order of operations (within a single transaction):
    ///   1. Count notes (before deleting, so we can return the count).
    ///   2. Delete Sections by note_uid property and NOTE_HAS_SECTION edge.
    ///   3. Delete Headings by note_uid property and NOTE_HAS_HEADING edge.
    ///   4. Delete UnresolvedWikilinks via cross-node join on source_note_uid.
    ///   5. Delete all Note nodes (vault_uid property; DETACH removes
    ///      VAULT_HAS_NOTE, NOTE_TAGGED_WITH, PROJECT_INCLUDES_NOTE, and
    ///      all incoming WIKILINK_TO_NOTE / WIKILINK_TO_HEADING edges).
    ///   6. Delete Tag nodes belonging to this vault.
    ///   7. Delete the Vault node itself.
    ///
    /// `delete_note_cascade` is kept as-is for incremental single-note deletions.
    pub fn delete_vault_cascade(&self, vault_uid: &str) -> Result<usize, StoreError> {
        Ok(self
            .delete_vault_cascade_with_outcome(vault_uid)?
            .notes_deleted)
    }

    /// Cascade-delete a vault and report whether any row targeted by the
    /// cascade existed. This distinguishes a confirmed no-op from deletion of
    /// an empty Vault or orphan Tag rows, both of which still mutate the graph.
    pub fn delete_vault_cascade_with_outcome(
        &self,
        vault_uid: &str,
    ) -> Result<DeleteVaultCascadeOutcome, StoreError> {
        Self::legacy_mutation_result(self.delete_vault_cascade_with_classified_outcome(vault_uid))
    }

    /// Cascade-delete a vault while retaining commit ambiguity as data.
    ///
    /// The preflight and every post-error liveness check use exact Vault,
    /// Note, and Tag UIDs. The probe opens a connection distinct from the
    /// transaction connection, so a failed transaction handle is never reused
    /// as liveness evidence.
    pub fn delete_vault_cascade_with_classified_outcome(
        &self,
        vault_uid: &str,
    ) -> Result<MutationOutcome<DeleteVaultCascadeOutcome>, StoreError> {
        self.delete_vault_cascade_with_classified_outcome_inner(
            vault_uid,
            VaultCascadeFaults::default(),
        )
    }

    #[cfg(test)]
    fn delete_vault_cascade_with_classified_outcome_and_faults(
        &self,
        vault_uid: &str,
        faults: VaultCascadeFaults,
    ) -> Result<MutationOutcome<DeleteVaultCascadeOutcome>, StoreError> {
        self.delete_vault_cascade_with_classified_outcome_inner(vault_uid, faults)
    }

    fn delete_vault_cascade_with_classified_outcome_inner(
        &self,
        vault_uid: &str,
        faults: VaultCascadeFaults,
    ) -> Result<MutationOutcome<DeleteVaultCascadeOutcome>, StoreError> {
        let before = self.vault_deletion_snapshot(vault_uid)?;
        let value = DeleteVaultCascadeOutcome {
            notes_deleted: before.note_uids.len(),
            changed: !before.is_empty(),
        };
        if before.is_empty() {
            return Ok(MutationOutcome {
                disposition: MutationDisposition::ConfirmedNoChange,
                confirmed_changed: false,
                value,
                primary_failure: None,
                mutation_warnings: Vec::new(),
            });
        }

        let conn = self.begin_transaction()?;
        let mutation =
            Self::delete_vault_cascade_with_outcome_on_with_faults(&conn, vault_uid, faults);
        if let Err(primary) = mutation {
            return match self.rollback_transaction(&conn) {
                Ok(()) => Err(primary),
                Err(rollback) => self.classify_failed_vault_attempt(
                    vault_uid,
                    &before,
                    value,
                    primary,
                    Some(rollback),
                    faults,
                    false,
                ),
            };
        }

        let commit = if faults.commit_before {
            Err(StoreError::Query(
                "injected commit failure before commit acknowledgement".to_string(),
            ))
        } else {
            self.commit_transaction(&conn).and_then(|()| {
                if faults.commit_after {
                    Err(StoreError::Query(
                        "injected commit failure after durable commit".to_string(),
                    ))
                } else {
                    Ok(())
                }
            })
        };
        match commit {
            Ok(()) => Ok(MutationOutcome {
                disposition: MutationDisposition::CommittedComplete,
                confirmed_changed: true,
                value,
                primary_failure: None,
                mutation_warnings: Vec::new(),
            }),
            Err(primary) => {
                let rollback = self.rollback_transaction(&conn).err();
                self.classify_failed_vault_attempt(
                    vault_uid, &before, value, primary, rollback, faults, true,
                )
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn classify_failed_vault_attempt(
        &self,
        vault_uid: &str,
        before: &VaultDeletionSnapshot,
        value: DeleteVaultCascadeOutcome,
        primary: StoreError,
        rollback: Option<StoreError>,
        faults: VaultCascadeFaults,
        commit_stage: bool,
    ) -> Result<MutationOutcome<DeleteVaultCascadeOutcome>, StoreError> {
        let failure_stage = if commit_stage {
            "vault-commit"
        } else {
            "vault-delete"
        };
        let probe = if faults.probe {
            Err(StoreError::Query(
                "injected vault liveness probe failure".to_string(),
            ))
        } else {
            self.vault_deletion_snapshot(vault_uid)
        };
        match probe {
            Ok(after) => match Self::exact_snapshot_state(before, &after) {
                ExactSnapshotState::WhollyLive => {
                    let rollback_context = rollback
                        .map(|error| format!("; rollback failed: {error}"))
                        .unwrap_or_default();
                    Err(StoreError::Query(format!(
                        "{primary}{rollback_context}; exact vault snapshot remained live"
                    )))
                }
                ExactSnapshotState::WhollyAbsent => Ok(MutationOutcome {
                    disposition: MutationDisposition::CommittedComplete,
                    confirmed_changed: true,
                    value,
                    primary_failure: None,
                    mutation_warnings: vec![MutationFailure::new(failure_stage, primary)],
                }),
                ExactSnapshotState::Mixed => Ok(MutationOutcome {
                    disposition: MutationDisposition::Ambiguous,
                    confirmed_changed: false,
                    value,
                    primary_failure: Some(MutationFailure::new(
                        failure_stage,
                        format!(
                            "{primary}; exact atomic vault snapshot was mixed{}",
                            rollback
                                .map(|error| format!("; rollback failed: {error}"))
                                .unwrap_or_default()
                        ),
                    )),
                    mutation_warnings: Vec::new(),
                }),
            },
            Err(probe) => Ok(MutationOutcome {
                disposition: MutationDisposition::Ambiguous,
                confirmed_changed: false,
                value,
                primary_failure: Some(MutationFailure::new(
                    failure_stage,
                    format!(
                        "{primary}; vault liveness probe failed: {probe}{}",
                        rollback
                            .map(|error| format!("; rollback failed: {error}"))
                            .unwrap_or_default()
                    ),
                )),
                mutation_warnings: Vec::new(),
            }),
        }
    }

    fn vault_deletion_snapshot(
        &self,
        vault_uid: &str,
    ) -> Result<VaultDeletionSnapshot, StoreError> {
        let conn = self.conn()?;
        Ok(VaultDeletionSnapshot {
            vault_uids: Self::exact_uids_on(
                &conn,
                "MATCH (v:Vault) WHERE v.uid = $scope RETURN v.uid",
                "scope",
                vault_uid,
                "Vault",
            )?,
            note_uids: Self::exact_uids_on(
                &conn,
                "MATCH (n:Note) WHERE n.vault_uid = $scope RETURN n.uid",
                "scope",
                vault_uid,
                "Note",
            )?,
            tag_uids: Self::exact_uids_on(
                &conn,
                "MATCH (t:Tag) WHERE t.vault_uid = $scope RETURN t.uid",
                "scope",
                vault_uid,
                "Tag",
            )?,
        })
    }

    fn exact_uids_on(
        conn: &lbug::Connection<'_>,
        query: &str,
        parameter: &'static str,
        scope: &str,
        label: &str,
    ) -> Result<std::collections::BTreeSet<String>, StoreError> {
        let mut statement = conn.prepare(query).map_err(|error| {
            StoreError::Query(format!("prepare exact {label} snapshot: {error}"))
        })?;
        let rows = conn
            .execute(
                &mut statement,
                vec![(parameter, lbug::Value::String(scope.to_string()))],
            )
            .map_err(|error| {
                StoreError::Query(format!("execute exact {label} snapshot: {error}"))
            })?;
        let mut uids = std::collections::BTreeSet::new();
        for row in rows {
            let Some(lbug::Value::String(uid)) = row.first() else {
                return Err(StoreError::Query(format!(
                    "malformed exact {label} snapshot identity: {:?}",
                    row.first()
                )));
            };
            if !uids.insert(uid.clone()) {
                return Err(StoreError::Query(format!(
                    "duplicate exact {label} snapshot identity: {uid}"
                )));
            }
        }
        Ok(uids)
    }

    fn exact_snapshot_state(
        before: &VaultDeletionSnapshot,
        after: &VaultDeletionSnapshot,
    ) -> ExactSnapshotState {
        if before == after {
            ExactSnapshotState::WhollyLive
        } else if after.is_empty() {
            ExactSnapshotState::WhollyAbsent
        } else {
            ExactSnapshotState::Mixed
        }
    }

    fn legacy_mutation_result<T>(
        outcome: Result<MutationOutcome<T>, StoreError>,
    ) -> Result<T, StoreError> {
        let outcome = outcome?;
        match outcome.disposition {
            MutationDisposition::ConfirmedNoChange | MutationDisposition::CommittedComplete => {
                Ok(outcome.value)
            }
            MutationDisposition::CommittedPartial | MutationDisposition::Ambiguous => {
                let failure = outcome.primary_failure.ok_or_else(|| {
                    StoreError::Query(format!(
                        "{:?} mutation outcome omitted its primary failure",
                        outcome.disposition
                    ))
                })?;
                Err(StoreError::Query(format!(
                    "{}: {}",
                    failure.stage, failure.message
                )))
            }
        }
    }

    /// Cascade-delete a vault's data using an externally-provided transaction
    /// connection, without opening or committing a transaction of its own.
    ///
    /// This lets the caller fold the delete into the SAME transaction as the
    /// re-insert (see [`Self::bulk_vault_reindex_write`]) so concurrent readers
    /// never observe the empty intermediate between the delete and the insert.
    /// Returns the number of notes that were present before the delete.
    pub fn delete_vault_cascade_on(
        conn: &lbug::Connection<'_>,
        vault_uid: &str,
    ) -> Result<usize, StoreError> {
        Ok(Self::delete_vault_cascade_with_outcome_on(conn, vault_uid)?.notes_deleted)
    }

    fn delete_vault_cascade_with_outcome_on(
        conn: &lbug::Connection<'_>,
        vault_uid: &str,
    ) -> Result<DeleteVaultCascadeOutcome, StoreError> {
        Self::delete_vault_cascade_with_outcome_on_with_faults(
            conn,
            vault_uid,
            VaultCascadeFaults::default(),
        )
    }

    fn delete_vault_cascade_with_outcome_on_with_faults(
        conn: &lbug::Connection<'_>,
        vault_uid: &str,
        faults: VaultCascadeFaults,
    ) -> Result<DeleteVaultCascadeOutcome, StoreError> {
        let count_matches = |query: &str, context: &str| -> Result<usize, StoreError> {
            let mut stmt = conn
                .prepare(query)
                .map_err(|e| StoreError::Query(format!("prepare {context}: {e}")))?;
            let rows = conn
                .execute(
                    &mut stmt,
                    vec![("vid", lbug::Value::String(vault_uid.to_string()))],
                )
                .map_err(|e| StoreError::Query(format!("execute {context}: {e}")))?;
            Ok(rows
                .filter_map(|row| {
                    row.first().and_then(|v| match v {
                        lbug::Value::Int64(n) => Some(*n as usize),
                        _ => None,
                    })
                })
                .next()
                .unwrap_or(0))
        };
        let count = count_matches(
            "MATCH (n:Note) WHERE n.vault_uid = $vid RETURN count(n)",
            "note count",
        )?;
        let vault_count = count_matches(
            "MATCH (v:Vault) WHERE v.uid = $vid RETURN count(v)",
            "vault count",
        )?;
        let tag_count = count_matches(
            "MATCH (t:Tag) WHERE t.vault_uid = $vid RETURN count(t)",
            "tag count",
        )?;
        let changed = count > 0 || vault_count > 0 || tag_count > 0;

        if faults.before_delete {
            return Err(StoreError::Query(
                "injected vault failure before delete".to_string(),
            ));
        }

        // 1. Delete all Sections whose ownership property references notes in
        //    this vault, including fragments whose NOTE_HAS_SECTION edge is
        //    missing.
        exec_params(
            conn,
            "MATCH (n:Note), (s:Section) \
             WHERE n.vault_uid = $vid AND s.note_uid = n.uid \
             DETACH DELETE s",
            vec![("vid", lbug::Value::String(vault_uid.to_string()))],
        )?;

        // 2. Delete Sections whose ownership edge references notes in this
        //    vault. This independent pass also handles a missing or corrupt
        //    note_uid property.
        exec_params(
            conn,
            "MATCH (n:Note {vault_uid: $vid})-[:NOTE_HAS_SECTION]->(s:Section) DETACH DELETE s",
            vec![("vid", lbug::Value::String(vault_uid.to_string()))],
        )?;

        // 3. Delete all Headings whose ownership property references notes in
        //    this vault, including fragments whose NOTE_HAS_HEADING edge is
        //    missing.
        exec_params(
            conn,
            "MATCH (n:Note), (h:Heading) \
             WHERE n.vault_uid = $vid AND h.note_uid = n.uid \
             DETACH DELETE h",
            vec![("vid", lbug::Value::String(vault_uid.to_string()))],
        )?;

        // 4. Delete Headings whose ownership edge references notes in this
        //    vault. This independent pass also handles a missing or corrupt
        //    note_uid property.
        exec_params(
            conn,
            "MATCH (n:Note {vault_uid: $vid})-[:NOTE_HAS_HEADING]->(h:Heading) DETACH DELETE h",
            vec![("vid", lbug::Value::String(vault_uid.to_string()))],
        )?;

        // 5. Delete UnresolvedWikilinks whose source note belongs to this vault.
        //    Uses a cross-node join: LadybugDB supports `MATCH (a), (b) WHERE a.prop = b.prop`.
        //    Best-effort: silently skip if the table does not exist on older DBs.
        {
            let uwl_result = (|| -> Result<(), StoreError> {
                let mut stmt = conn
                    .prepare(
                        "MATCH (n:Note), (u:UnresolvedWikilink) \
                         WHERE n.vault_uid = $vid AND u.source_note_uid = n.uid \
                         DELETE u",
                    )
                    .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
                conn.execute(
                    &mut stmt,
                    vec![("vid", lbug::Value::String(vault_uid.to_string()))],
                )
                .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
                Ok(())
            })();
            if let Err(e) = uwl_result {
                tracing::trace!("delete_vault_cascade: UnresolvedWikilink delete skipped: {e}");
            }
        }

        // 6. Delete all Note nodes (DETACH removes VAULT_HAS_NOTE, NOTE_TAGGED_WITH,
        //    PROJECT_INCLUDES_NOTE, and any incoming/outgoing wikilink edges).
        exec_params(
            conn,
            "MATCH (n:Note {vault_uid: $vid}) DETACH DELETE n",
            vec![("vid", lbug::Value::String(vault_uid.to_string()))],
        )?;

        // 7. Delete Tag nodes belonging to this vault.
        exec_params(
            conn,
            "MATCH (t:Tag {vault_uid: $vid}) DETACH DELETE t",
            vec![("vid", lbug::Value::String(vault_uid.to_string()))],
        )?;

        // 8. Delete the Vault node itself.
        exec_params(
            conn,
            "MATCH (v:Vault {uid: $vid}) DETACH DELETE v",
            vec![("vid", lbug::Value::String(vault_uid.to_string()))],
        )?;

        Ok(DeleteVaultCascadeOutcome {
            notes_deleted: count,
            changed,
        })
    }

    /// Batch insert REFERENCES_CODE edges from Note → Symbol. Each tuple
    /// is (note_uid, symbol_uid, confidence, source) where `source` is a
    /// short tag (`"name-match"`, `"code-block"`, `"annotation"`).
    pub fn batch_insert_note_to_symbol_edges(
        &self,
        edges: &[(&str, &str, f32, &str)],
    ) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::batch_insert_note_to_symbol_edges_on(&conn, edges)
    }

    /// Insert note→symbol edges using an externally-provided connection
    /// (for transaction batching across many notes — avoids one fsync per call).
    pub fn batch_insert_note_to_symbol_edges_on(
        conn: &lbug::Connection<'_>,
        edges: &[(&str, &str, f32, &str)],
    ) -> Result<(), StoreError> {
        if edges.is_empty() {
            return Ok(());
        }
        let mut stmt = conn
            .prepare(
                "MATCH (n:Note {uid: $nid}), (s:Symbol {uid: $sid}) \
                 CREATE (n)-[:REFERENCES_CODE_NOTE_TO_SYMBOL \
                 {confidence: $conf, source: $source}]->(s)",
            )
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        for (n_uid, s_uid, conf, src) in edges {
            conn.execute(
                &mut stmt,
                vec![
                    ("nid", lbug::Value::String(n_uid.to_string())),
                    ("sid", lbug::Value::String(s_uid.to_string())),
                    ("conf", lbug::Value::Double(*conf as f64)),
                    ("source", lbug::Value::String(src.to_string())),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        }
        Ok(())
    }

    /// Batch insert REFERENCES_CODE edges from Section → Symbol.
    pub fn batch_insert_section_to_symbol_edges(
        &self,
        edges: &[(&str, &str, f32, &str)],
    ) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::batch_insert_section_to_symbol_edges_on(&conn, edges)
    }

    /// Insert section→symbol edges using an externally-provided connection
    /// (for transaction batching across many notes — avoids one fsync per call).
    pub fn batch_insert_section_to_symbol_edges_on(
        conn: &lbug::Connection<'_>,
        edges: &[(&str, &str, f32, &str)],
    ) -> Result<(), StoreError> {
        if edges.is_empty() {
            return Ok(());
        }
        let mut stmt = conn
            .prepare(
                "MATCH (sec:Section {uid: $sid}), (sym:Symbol {uid: $symid}) \
                 CREATE (sec)-[:REFERENCES_CODE_SECTION_TO_SYMBOL \
                 {confidence: $conf, source: $source}]->(sym)",
            )
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        for (sec_uid, sym_uid, conf, src) in edges {
            conn.execute(
                &mut stmt,
                vec![
                    ("sid", lbug::Value::String(sec_uid.to_string())),
                    ("symid", lbug::Value::String(sym_uid.to_string())),
                    ("conf", lbug::Value::Double(*conf as f64)),
                    ("source", lbug::Value::String(src.to_string())),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        }
        Ok(())
    }

    /// Delete all REFERENCES_CODE edges originating from a note and its
    /// sections. Called before re-emitting cross-domain edges to ensure
    /// idempotency.
    pub fn delete_cross_domain_edges_for_note(&self, note_uid: &str) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::delete_cross_domain_edges_for_note_on(&conn, note_uid)
    }

    /// Delete cross-domain edges for a note using an externally-provided
    /// connection (for transaction batching across many notes).
    pub fn delete_cross_domain_edges_for_note_on(
        conn: &lbug::Connection<'_>,
        note_uid: &str,
    ) -> Result<(), StoreError> {
        exec_params(
            conn,
            "MATCH (n:Note {uid: $uid})-[r:REFERENCES_CODE_NOTE_TO_SYMBOL]->() DELETE r",
            vec![("uid", lbug::Value::String(note_uid.to_string()))],
        )?;
        // Section-level edges: find sections belonging to this note and
        // delete their outgoing REFERENCES_CODE edges.
        let section_uids: Vec<String> = {
            // LadybugDB does not support parameterized compound
            // property-match queries. Sanitize user-derived UIDs by
            // escaping single quotes to prevent Cypher injection.
            let safe_note_uid = note_uid.replace('\'', "\\'");
            let rows = conn
                .query(&format!(
                    "MATCH (n:Note {{uid: '{safe_note_uid}'}})-[:NOTE_HAS_SECTION]->(s:Section) RETURN s.uid"
                ))
                .map_err(|e| StoreError::Query(format!("query sections: {e}")))?;
            rows.filter_map(|row| {
                row.first().and_then(|v| match v {
                    lbug::Value::String(s) => Some(s.clone()),
                    _ => None,
                })
            })
            .collect()
        };
        for s_uid in &section_uids {
            exec_params(
                conn,
                "MATCH (s:Section {uid: $uid})-[r:REFERENCES_CODE_SECTION_TO_SYMBOL]->() DELETE r",
                vec![("uid", lbug::Value::String(s_uid.clone()))],
            )?;
        }
        Ok(())
    }

    /// Delete all Symbol nodes that belong to a specific file (matching both
    /// `repo_uid` AND `file_path`). Uses `DETACH DELETE` so all incident edges
    /// (CALLS, IMPORTS, EXTENDS_SYM, IMPLEMENTS_SYM, USES, ACCESSES, MEMBER_OF,
    /// FILE_HAS_SYMBOL, CROSS_REPO_LINK, REFERENCES_CODE_*) are automatically
    /// removed. Returns the count of deleted symbols.
    pub fn delete_symbols_in_file(
        &self,
        repo_uid: &str,
        file_path: &str,
    ) -> Result<usize, StoreError> {
        let conn = self.conn()?;
        Self::delete_symbols_in_file_on(&conn, repo_uid, file_path)
    }

    /// Delete symbols in a file using an externally-provided connection (for transaction batching).
    pub fn delete_symbols_in_file_on(
        conn: &lbug::Connection<'_>,
        repo_uid: &str,
        file_path: &str,
    ) -> Result<usize, StoreError> {
        // LadybugDB does not support parameterized compound WHERE clauses.
        // Sanitize user-derived values by escaping single quotes.
        let safe_repo_uid = repo_uid.replace('\'', "\\'");
        let safe_file_path = file_path.replace('\'', "\\'");

        // Count first so we can report how many were deleted.
        let count: usize = {
            let rows = conn
                .query(&format!(
                    "MATCH (s:Symbol) WHERE s.repo_uid = '{safe_repo_uid}' AND s.file_path = '{safe_file_path}' RETURN count(s)"
                ))
                .map_err(|e| StoreError::Query(format!("count symbols: {e}")))?;
            rows.filter_map(|row| {
                row.first().and_then(|v| match v {
                    lbug::Value::Int64(n) => Some(*n as usize),
                    _ => None,
                })
            })
            .next()
            .unwrap_or(0)
        };

        if count > 0 {
            // Single bulk DETACH DELETE instead of per-UID queries.
            conn.query(&format!(
                "MATCH (s:Symbol) WHERE s.repo_uid = '{safe_repo_uid}' AND s.file_path = '{safe_file_path}' DETACH DELETE s"
            ))
            .map_err(|e| StoreError::Query(format!("delete symbols in file: {e}")))?;
        }

        Ok(count)
    }

    /// Delete all resolved (semantic) edges originating from symbols in a
    /// specific file. Used by incremental resolution to clear stale edges
    /// before re-resolving affected files.
    ///
    /// Edge types deleted: CALLS, IMPORTS, EXTENDS_SYM, IMPLEMENTS_SYM,
    /// INCLUDES_SYM, USES, ACCESSES, MEMBER_OF.
    pub fn delete_resolved_edges_for_file(
        &self,
        repo_uid: &str,
        file_path: &str,
    ) -> Result<(), StoreError> {
        let conn = self.conn()?;

        // LadybugDB does not support `type(e) IN [...]`, so we issue one
        // DELETE per relationship type. Each query is cheap because the
        // WHERE clause narrows to a single file's symbols.
        for rel in &[
            "CALLS",
            "IMPORTS",
            "EXTENDS_SYM",
            "IMPLEMENTS_SYM",
            "INCLUDES_SYM",
            "USES",
            "ACCESSES",
            "MEMBER_OF",
        ] {
            // The rel type must be interpolated (LadybugDB doesn't support
            // parameterized relationship types), but the WHERE values are
            // parameterized to avoid injection.
            let query = format!(
                "MATCH (s:Symbol)-[r:{rel}]->() \
                 WHERE s.repo_uid = $repo AND s.file_path = $path \
                 DELETE r"
            );
            exec_params(
                &conn,
                &query,
                vec![
                    ("repo", lbug::Value::String(repo_uid.to_string())),
                    ("path", lbug::Value::String(file_path.to_string())),
                ],
            )
            .map_err(|e| StoreError::Query(format!("delete {rel} edges for file: {e}")))?;
        }
        Ok(())
    }

    /// Delete a File node by its UID using `DETACH DELETE`, which removes all
    /// incident edges (REPO_HAS_FILE, FILE_HAS_SYMBOL) automatically.
    pub fn delete_file_node(&self, file_uid: &str) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::delete_file_node_on(&conn, file_uid)
    }

    /// Delete a File node using an externally-provided connection (for transaction batching).
    pub fn delete_file_node_on(
        conn: &lbug::Connection<'_>,
        file_uid: &str,
    ) -> Result<(), StoreError> {
        exec_params(
            conn,
            "MATCH (f:File {uid: $uid}) DETACH DELETE f",
            vec![("uid", lbug::Value::String(file_uid.to_string()))],
        )
    }

    /// Update `file_path` on every Symbol belonging to `repo_uid` that
    /// currently has `old_path`.  LadybugDB does not support `SET`, so each
    /// symbol is deleted and re-created with the new path while preserving all
    /// other fields.
    pub fn update_symbol_file_paths(
        &self,
        repo_uid: &str,
        old_path: &str,
        new_path: &str,
    ) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::update_symbol_file_paths_on(&conn, repo_uid, old_path, new_path)
    }

    /// Update symbol file paths using an externally-provided connection (for transaction batching).
    pub fn update_symbol_file_paths_on(
        conn: &lbug::Connection<'_>,
        repo_uid: &str,
        old_path: &str,
        new_path: &str,
    ) -> Result<(), StoreError> {
        use crate::read::row_to_symbol;

        let rows: Vec<_> = {
            let r = conn
                .query(&format!(
                    "MATCH (s:Symbol) WHERE s.repo_uid = '{}' AND s.file_path = '{}' RETURN \
                     s.uid, s.name, s.kind, s.repo_uid, s.file_path, s.start_line, \
                     s.signature, s.summary, s.content_hash, s.pagerank_score",
                    repo_uid.replace('\'', "''"),
                    old_path.replace('\'', "''"),
                ))
                .map_err(|e| StoreError::Query(format!("query symbols: {e}")))?;
            r.collect()
        };

        for row in rows {
            let mut sym = row_to_symbol(&row)?;
            let old_uid = sym.uid.clone();
            sym.file_path = new_path.to_string();

            exec_params(
                conn,
                "MATCH (s:Symbol {uid: $uid}) DETACH DELETE s",
                vec![("uid", lbug::Value::String(old_uid))],
            )?;

            Self::insert_symbol_with_conn_static(conn, &sym)?;
        }

        Ok(())
    }

    /// Update the `indexed_sha` field of a Repo node.
    pub fn update_repo_sha(&self, repo_uid: &str, new_sha: &str) -> Result<(), StoreError> {
        let txn = self.begin_transaction()?;
        Self::update_repo_sha_on(&txn, repo_uid, new_sha)?;
        self.commit_transaction(&txn)?;
        Ok(())
    }

    /// Update the `indexed_sha` field using an externally-provided connection
    /// (for transaction batching). Does NOT begin/commit its own transaction.
    pub fn update_repo_sha_on(
        conn: &lbug::Connection<'_>,
        repo_uid: &str,
        new_sha: &str,
    ) -> Result<(), StoreError> {
        let cols = "r.uid, r.url, r.indexed_sha, r.staleness_commits_behind, r.instance_id, \
                    r.name, r.root_path";
        let rows: Vec<_> = conn
            .query(&format!(
                "MATCH (r:Repo {{uid: '{}'}}) RETURN {cols}",
                repo_uid.replace('\'', "''"),
            ))
            .map_err(|e| StoreError::Query(format!("query repo: {e}")))?
            .collect();

        let row = rows.into_iter().next().ok_or(StoreError::NotFound)?;

        let uid = match row.first() {
            Some(lbug::Value::String(s)) => s.clone(),
            _ => return Err(StoreError::Query("repo uid missing".to_string())),
        };
        let url = match row.get(1) {
            Some(lbug::Value::String(s)) => s.clone(),
            _ => return Err(StoreError::Query("repo url missing".to_string())),
        };
        let staleness = match row.get(3) {
            Some(lbug::Value::Int64(n)) => *n,
            _ => 0,
        };
        let instance_id = match row.get(4) {
            Some(lbug::Value::String(s)) => s.clone(),
            _ => return Err(StoreError::Query("repo instance_id missing".to_string())),
        };
        let name = match row.get(5) {
            Some(lbug::Value::String(s)) if !s.is_empty() => s.clone(),
            _ => String::new(),
        };
        let root_path = match row.get(6) {
            Some(lbug::Value::String(s)) if !s.is_empty() => s.clone(),
            _ => String::new(),
        };

        exec_params(
            conn,
            "MATCH (r:Repo {uid: $uid}) DETACH DELETE r",
            vec![("uid", lbug::Value::String(uid.clone()))],
        )?;

        exec_params(
            conn,
            "CREATE (:Repo {uid: $uid, url: $url, indexed_sha: $sha, \
             staleness_commits_behind: $scb, instance_id: $iid, name: $name, \
             root_path: $root_path})",
            vec![
                ("uid", lbug::Value::String(uid)),
                ("url", lbug::Value::String(url)),
                ("sha", lbug::Value::String(new_sha.to_string())),
                ("scb", lbug::Value::Int64(staleness)),
                ("iid", lbug::Value::String(instance_id)),
                ("name", lbug::Value::String(name)),
                ("root_path", lbug::Value::String(root_path)),
            ],
        )?;

        Ok(())
    }

    /// Update the `root_path` field of a Repo node, leaving every other
    /// field (including the identity `url`) untouched. Used at index time
    /// to keep the on-disk location current for pre-existing rows.
    ///
    /// Follows the established read → DETACH DELETE → CREATE pattern
    /// (see `update_repo_sha`).
    pub fn update_repo_root_path(&self, repo_uid: &str, root_path: &str) -> Result<(), StoreError> {
        let txn = self.begin_transaction()?;
        {
            let conn = &txn;
            let cols = "r.uid, r.url, r.indexed_sha, r.staleness_commits_behind, \
                        r.instance_id, r.name";
            let rows: Vec<_> = conn
                .query(&format!(
                    "MATCH (r:Repo {{uid: '{}'}}) RETURN {cols}",
                    repo_uid.replace('\'', "''"),
                ))
                .map_err(|e| StoreError::Query(format!("query repo: {e}")))?
                .collect();

            let row = rows.into_iter().next().ok_or(StoreError::NotFound)?;

            let uid = match row.first() {
                Some(lbug::Value::String(s)) => s.clone(),
                _ => return Err(StoreError::Query("repo uid missing".to_string())),
            };
            let url = match row.get(1) {
                Some(lbug::Value::String(s)) => s.clone(),
                _ => return Err(StoreError::Query("repo url missing".to_string())),
            };
            let sha = match row.get(2) {
                Some(lbug::Value::String(s)) => s.clone(),
                _ => String::new(),
            };
            let staleness = match row.get(3) {
                Some(lbug::Value::Int64(n)) => *n,
                _ => 0,
            };
            let instance_id = match row.get(4) {
                Some(lbug::Value::String(s)) => s.clone(),
                _ => return Err(StoreError::Query("repo instance_id missing".to_string())),
            };
            let name = match row.get(5) {
                Some(lbug::Value::String(s)) if !s.is_empty() => s.clone(),
                _ => String::new(),
            };

            exec_params(
                conn,
                "MATCH (r:Repo {uid: $uid}) DETACH DELETE r",
                vec![("uid", lbug::Value::String(uid.clone()))],
            )?;

            exec_params(
                conn,
                "CREATE (:Repo {uid: $uid, url: $url, indexed_sha: $sha, \
                 staleness_commits_behind: $scb, instance_id: $iid, name: $name, \
                 root_path: $root_path})",
                vec![
                    ("uid", lbug::Value::String(uid)),
                    ("url", lbug::Value::String(url)),
                    ("sha", lbug::Value::String(sha)),
                    ("scb", lbug::Value::Int64(staleness)),
                    ("iid", lbug::Value::String(instance_id)),
                    ("name", lbug::Value::String(name)),
                    ("root_path", lbug::Value::String(root_path.to_string())),
                ],
            )?;
        }
        self.commit_transaction(&txn)?;
        Ok(())
    }

    /// Update the `embedding` field of a Note node.
    ///
    /// Persist a Note embedding to the sidecar `EmbeddingIndex`.
    ///
    /// The embedding is added to the in-memory index and immediately flushed
    /// to disk. For batch operations, prefer `add_embedding` +
    /// `flush_embedding_index` to avoid O(n^2) writes.
    pub fn update_note_embedding(&self, uid: &str, embedding: &[f32]) -> Result<(), StoreError> {
        // A dimension-guard rejection is logged by the index; nothing was
        // inserted, so skip the flush.
        if self.add_embedding(uid, embedding.to_vec()) {
            self.flush_embedding_index()?;
        }
        Ok(())
    }

    /// Persist a Heading embedding to the sidecar `EmbeddingIndex`.
    ///
    /// The embedding is added to the in-memory index and immediately flushed
    /// to disk. For batch operations, prefer `add_embedding` +
    /// `flush_embedding_index` to avoid O(n^2) writes.
    pub fn update_heading_embedding(&self, uid: &str, embedding: &[f32]) -> Result<(), StoreError> {
        if self.add_embedding(uid, embedding.to_vec()) {
            self.flush_embedding_index()?;
        }
        Ok(())
    }

    /// Persist a Symbol embedding to the sidecar `EmbeddingIndex`.
    ///
    /// The embedding is added to the in-memory index and immediately flushed
    /// to disk. For batch operations, prefer `add_embedding` +
    /// `flush_embedding_index` to avoid O(n^2) writes.
    pub fn update_symbol_embedding(&self, uid: &str, embedding: &[f32]) -> Result<(), StoreError> {
        if self.add_embedding(uid, embedding.to_vec()) {
            self.flush_embedding_index()?;
        }
        Ok(())
    }

    /// Bulk-delete all Symbol and File nodes belonging to `repo_uid` using two
    /// DETACH DELETE queries instead of one per file. Called by `delete_repo_all_data`
    /// before a forced full re-index. `DETACH DELETE` removes all incident edges
    /// (FILE_HAS_SYMBOL, REPO_HAS_FILE, CALLS, IMPORTS, etc.) automatically.
    ///
    /// Returns `(file_count, symbol_count)` for logging.
    pub fn delete_repo_cascade_with_outcome(
        &self,
        repo_uid: &str,
    ) -> Result<MutationOutcome<DeleteRepoCascadeOutcome>, StoreError> {
        self.delete_repo_cascade_with_outcome_inner(repo_uid, RepoCascadeFaults::default())
    }

    #[cfg(test)]
    fn delete_repo_cascade_with_outcome_and_faults(
        &self,
        repo_uid: &str,
        faults: RepoCascadeFaults,
    ) -> Result<MutationOutcome<DeleteRepoCascadeOutcome>, StoreError> {
        self.delete_repo_cascade_with_outcome_inner(repo_uid, faults)
    }

    fn delete_repo_cascade_with_outcome_inner(
        &self,
        repo_uid: &str,
        faults: RepoCascadeFaults,
    ) -> Result<MutationOutcome<DeleteRepoCascadeOutcome>, StoreError> {
        let before = self.repo_deletion_snapshot(repo_uid)?;
        if before.is_empty() {
            return Ok(MutationOutcome {
                disposition: MutationDisposition::ConfirmedNoChange,
                confirmed_changed: false,
                value: Self::repo_delete_value(repo_uid, &before, &before),
                primary_failure: None,
                mutation_warnings: Vec::new(),
            });
        }

        let mut mutation_warnings = Vec::new();
        let bulk = (|| {
            let conn = self.begin_transaction()?;
            let mutation = Self::bulk_delete_repo_files_and_symbols_on(&conn, repo_uid);
            let counts = match mutation {
                Ok(counts) => counts,
                Err(error) => {
                    let rollback = self.rollback_transaction(&conn);
                    return Err(match rollback {
                        Ok(()) => error,
                        Err(rollback) => StoreError::Query(format!(
                            "{error}; Repo bulk rollback failed: {rollback}"
                        )),
                    });
                }
            };
            self.commit_transaction(&conn).and_then(|()| {
                if faults.bulk_commit_after {
                    Err(StoreError::Query(
                        "injected Repo bulk commit acknowledgement failure".to_string(),
                    ))
                } else {
                    Ok(counts)
                }
            })
        })();

        let bulk_confirmed_changed = !before.file_uids.is_empty() || !before.symbol_uids.is_empty();
        if let Err(primary) = bulk {
            let after = match self.repo_deletion_probe(repo_uid, faults) {
                Ok(after) => after,
                Err(probe) => {
                    return Ok(MutationOutcome {
                        disposition: MutationDisposition::Ambiguous,
                        confirmed_changed: false,
                        value: DeleteRepoCascadeOutcome {
                            repo_uid: repo_uid.to_string(),
                            files_deleted: 0,
                            symbols_deleted: 0,
                        },
                        primary_failure: Some(MutationFailure::new(
                            "repo-bulk-delete",
                            format!("{primary}; exact Repo liveness probe failed: {probe}"),
                        )),
                        mutation_warnings,
                    });
                }
            };
            let other_unchanged = after.repo_uids == before.repo_uids
                && after.service_uids == before.service_uids
                && after.contract_uids == before.contract_uids;
            let bulk_live =
                after.file_uids == before.file_uids && after.symbol_uids == before.symbol_uids;
            let bulk_absent = after.file_uids.is_empty() && after.symbol_uids.is_empty();
            if other_unchanged && bulk_live {
                return Err(StoreError::Query(format!(
                    "{primary}; exact Repo File/Symbol snapshot remained live"
                )));
            }
            if other_unchanged && bulk_absent {
                mutation_warnings.push(MutationFailure::new("repo-bulk-delete", primary));
            } else {
                return Ok(MutationOutcome {
                    disposition: MutationDisposition::Ambiguous,
                    confirmed_changed: false,
                    value: Self::repo_delete_value(repo_uid, &before, &after),
                    primary_failure: Some(MutationFailure::new(
                        "repo-bulk-delete",
                        format!(
                            "{primary}; exact atomic File/Symbol snapshot was mixed or other Repo rows changed"
                        ),
                    )),
                    mutation_warnings,
                });
            }
        }

        if faults.after_bulk {
            return self.classify_repo_attempt_error(
                repo_uid,
                &before,
                StoreError::Query("injected failure after committed Repo bulk delete".to_string()),
                bulk_confirmed_changed,
                mutation_warnings,
                faults,
            );
        }

        if let Err(primary) = self.clear_repo_derived_nodes_strict(repo_uid) {
            return self.classify_repo_attempt_error(
                repo_uid,
                &before,
                primary,
                bulk_confirmed_changed,
                mutation_warnings,
                faults,
            );
        }

        let child_confirmed_changed = bulk_confirmed_changed
            || !before.service_uids.is_empty()
            || !before.contract_uids.is_empty();

        if faults.before_root {
            return self.classify_repo_attempt_error(
                repo_uid,
                &before,
                StoreError::Query("injected failure before Repo root delete".to_string()),
                child_confirmed_changed,
                mutation_warnings,
                faults,
            );
        }

        let root_delete = self.delete_repo_node(repo_uid).and_then(|()| {
            if faults.root_ack_after {
                Err(StoreError::Query(
                    "injected Repo root delete acknowledgement failure".to_string(),
                ))
            } else {
                Ok(())
            }
        });
        if let Err(primary) = root_delete {
            return self.classify_repo_attempt_error(
                repo_uid,
                &before,
                primary,
                child_confirmed_changed,
                mutation_warnings,
                faults,
            );
        }

        Ok(MutationOutcome {
            disposition: MutationDisposition::CommittedComplete,
            confirmed_changed: true,
            value: DeleteRepoCascadeOutcome {
                repo_uid: repo_uid.to_string(),
                files_deleted: before.file_uids.len(),
                symbols_deleted: before.symbol_uids.len(),
            },
            primary_failure: None,
            mutation_warnings,
        })
    }

    fn classify_repo_attempt_error(
        &self,
        repo_uid: &str,
        before: &RepoDeletionSnapshot,
        primary: StoreError,
        known_changed: bool,
        mutation_warnings: Vec<MutationFailure>,
        faults: RepoCascadeFaults,
    ) -> Result<MutationOutcome<DeleteRepoCascadeOutcome>, StoreError> {
        let after = match self.repo_deletion_probe(repo_uid, faults) {
            Ok(after) => after,
            Err(probe) => {
                return Ok(MutationOutcome {
                    disposition: MutationDisposition::Ambiguous,
                    confirmed_changed: known_changed,
                    value: DeleteRepoCascadeOutcome {
                        repo_uid: repo_uid.to_string(),
                        files_deleted: usize::from(known_changed) * before.file_uids.len(),
                        symbols_deleted: usize::from(known_changed) * before.symbol_uids.len(),
                    },
                    primary_failure: Some(MutationFailure::new(
                        "repo-delete",
                        format!("{primary}; exact Repo liveness probe failed: {probe}"),
                    )),
                    mutation_warnings,
                });
            }
        };
        if after == *before && !known_changed {
            return Err(StoreError::Query(format!(
                "{primary}; exact Repo snapshot remained live"
            )));
        }
        if !after.is_subset_of(before) {
            return Ok(MutationOutcome {
                disposition: MutationDisposition::Ambiguous,
                confirmed_changed: known_changed,
                value: Self::repo_delete_value(repo_uid, before, &after),
                primary_failure: Some(MutationFailure::new(
                    "repo-delete",
                    format!("{primary}; exact Repo probe observed unexpected rows"),
                )),
                mutation_warnings,
            });
        }
        if after.is_empty() {
            let mut mutation_warnings = mutation_warnings;
            mutation_warnings.push(MutationFailure::new("repo-delete", primary));
            return Ok(MutationOutcome {
                disposition: MutationDisposition::CommittedComplete,
                confirmed_changed: true,
                value: Self::repo_delete_value(repo_uid, before, &after),
                primary_failure: None,
                mutation_warnings,
            });
        }
        Ok(MutationOutcome {
            disposition: MutationDisposition::CommittedPartial,
            confirmed_changed: true,
            value: Self::repo_delete_value(repo_uid, before, &after),
            primary_failure: Some(MutationFailure::new("repo-delete", primary)),
            mutation_warnings,
        })
    }

    fn repo_deletion_probe(
        &self,
        repo_uid: &str,
        faults: RepoCascadeFaults,
    ) -> Result<RepoDeletionSnapshot, StoreError> {
        if faults.probe {
            Err(StoreError::Query(
                "injected Repo liveness probe failure".to_string(),
            ))
        } else {
            self.repo_deletion_snapshot(repo_uid)
        }
    }

    fn repo_deletion_snapshot(&self, repo_uid: &str) -> Result<RepoDeletionSnapshot, StoreError> {
        let conn = self.conn()?;
        Ok(RepoDeletionSnapshot {
            repo_uids: Self::exact_uids_on(
                &conn,
                "MATCH (r:Repo) WHERE r.uid = $scope RETURN r.uid",
                "scope",
                repo_uid,
                "Repo",
            )?,
            file_uids: Self::exact_uids_on(
                &conn,
                "MATCH (f:File) WHERE f.repo_uid = $scope RETURN f.uid",
                "scope",
                repo_uid,
                "File",
            )?,
            symbol_uids: Self::exact_uids_on(
                &conn,
                "MATCH (s:Symbol) WHERE s.repo_uid = $scope RETURN s.uid",
                "scope",
                repo_uid,
                "Symbol",
            )?,
            service_uids: Self::exact_uids_on(
                &conn,
                "MATCH (s:Service) WHERE s.repo_uid = $scope RETURN s.uid",
                "scope",
                repo_uid,
                "Service",
            )?,
            contract_uids: Self::exact_uids_on(
                &conn,
                "MATCH (c:Contract) WHERE c.repo_uid = $scope RETURN c.uid",
                "scope",
                repo_uid,
                "Contract",
            )?,
        })
    }

    fn repo_delete_value(
        repo_uid: &str,
        before: &RepoDeletionSnapshot,
        after: &RepoDeletionSnapshot,
    ) -> DeleteRepoCascadeOutcome {
        DeleteRepoCascadeOutcome {
            repo_uid: repo_uid.to_string(),
            files_deleted: before.file_uids.difference(&after.file_uids).count(),
            symbols_deleted: before.symbol_uids.difference(&after.symbol_uids).count(),
        }
    }

    fn clear_repo_derived_nodes_strict(&self, repo_uid: &str) -> Result<(), StoreError> {
        let conn = self.conn()?;
        exec_params(
            &conn,
            "MATCH (s:Service {repo_uid: $uid}) DETACH DELETE s",
            vec![("uid", lbug::Value::String(repo_uid.to_string()))],
        )?;
        exec_params(
            &conn,
            "MATCH (c:Contract {repo_uid: $uid}) DETACH DELETE c",
            vec![("uid", lbug::Value::String(repo_uid.to_string()))],
        )
    }

    pub fn bulk_delete_repo_files_and_symbols(
        &self,
        repo_uid: &str,
    ) -> Result<(usize, usize), StoreError> {
        let conn = self.begin_transaction()?;
        let counts = Self::bulk_delete_repo_files_and_symbols_on(&conn, repo_uid)?;
        self.commit_transaction(&conn)?;
        Ok(counts)
    }

    /// Like [`bulk_delete_repo_files_and_symbols`](Self::bulk_delete_repo_files_and_symbols)
    /// but operates on an existing connection/transaction.
    pub fn bulk_delete_repo_files_and_symbols_on(
        conn: &lbug::Connection<'_>,
        repo_uid: &str,
    ) -> Result<(usize, usize), StoreError> {
        let rid = lbug::Value::String(repo_uid.to_string());

        // Count before deleting so the caller can log what was removed.
        let sym_count: usize = {
            let mut stmt = conn
                .prepare("MATCH (s:Symbol) WHERE s.repo_uid = $rid RETURN count(s)")
                .map_err(|e| StoreError::Query(format!("prepare count symbols: {e}")))?;
            let rows = conn
                .execute(&mut stmt, vec![("rid", rid.clone())])
                .map_err(|e| StoreError::Query(format!("count symbols: {e}")))?;
            rows.filter_map(|row| {
                row.first().and_then(|v| match v {
                    lbug::Value::Int64(n) => Some(*n as usize),
                    _ => None,
                })
            })
            .next()
            .unwrap_or(0)
        };
        let file_count: usize = {
            let mut stmt = conn
                .prepare("MATCH (f:File) WHERE f.repo_uid = $rid RETURN count(f)")
                .map_err(|e| StoreError::Query(format!("prepare count files: {e}")))?;
            let rows = conn
                .execute(&mut stmt, vec![("rid", rid.clone())])
                .map_err(|e| StoreError::Query(format!("count files: {e}")))?;
            rows.filter_map(|row| {
                row.first().and_then(|v| match v {
                    lbug::Value::Int64(n) => Some(*n as usize),
                    _ => None,
                })
            })
            .next()
            .unwrap_or(0)
        };

        let mut stmt = conn
            .prepare("MATCH (s:Symbol) WHERE s.repo_uid = $rid DETACH DELETE s")
            .map_err(|e| StoreError::Query(format!("prepare delete symbols: {e}")))?;
        conn.execute(&mut stmt, vec![("rid", rid.clone())])
            .map_err(|e| StoreError::Query(format!("bulk delete symbols: {e}")))?;

        let mut stmt = conn
            .prepare("MATCH (f:File) WHERE f.repo_uid = $rid DETACH DELETE f")
            .map_err(|e| StoreError::Query(format!("prepare delete files: {e}")))?;
        conn.execute(&mut stmt, vec![("rid", rid)])
            .map_err(|e| StoreError::Query(format!("bulk delete files: {e}")))?;

        Ok((file_count, sym_count))
    }

    /// Delete a Repo node (and its REPO_HAS_FILE edges) by UID.
    pub fn delete_repo_node(&self, repo_uid: &str) -> Result<(), StoreError> {
        let conn = self.conn()?;
        exec_params(
            &conn,
            "MATCH (r:Repo {uid: $uid}) DETACH DELETE r",
            vec![("uid", lbug::Value::String(repo_uid.to_string()))],
        )?;
        Ok(())
    }

    /// Delete all repo-scoped graph nodes that are NOT keyed off a stable,
    /// re-derivable UID and therefore would collide on a forced full re-index.
    ///
    /// `bulk_index_write` plain-`CREATE`s `Service` nodes (whose UID is derived
    /// from `repo_uid` + directory), and the contracts pass creates `Contract`
    /// nodes. Re-running `index --force` regenerates the same UIDs, so without
    /// clearing them first the second run trips LadybugDB's primary-key
    /// uniqueness constraint (`Found duplicated primary key value svc:...`).
    ///
    /// `DETACH DELETE` also removes incident `SERVICE_HAS_SYMBOL`,
    /// `IMPLEMENTS_CONTRACT`, and `SUPERSEDES`/`DEPENDS_ON`/`CAUSED_BY`/
    /// `RELATES_TO` edges. `Symbol`/`File` nodes are cleared separately by the
    /// per-file `delete_symbols_in_file` / `delete_file_node` path. Idempotent:
    /// a no-op for repos with no services/contracts.
    pub fn clear_repo_derived_nodes(&self, repo_uid: &str) -> Result<(), StoreError> {
        let conn = self.conn()?;
        Self::clear_repo_derived_nodes_on(&conn, repo_uid)
    }

    /// Like [`clear_repo_derived_nodes`](Self::clear_repo_derived_nodes) but
    /// operates on an existing connection/transaction.
    pub fn clear_repo_derived_nodes_on(
        conn: &lbug::Connection<'_>,
        repo_uid: &str,
    ) -> Result<(), StoreError> {
        // Service nodes for this repo.
        exec_params(
            conn,
            "MATCH (s:Service {repo_uid: $uid}) DETACH DELETE s",
            vec![("uid", lbug::Value::String(repo_uid.to_string()))],
        )?;
        // Contract nodes for this repo (table may not exist on older DBs).
        if let Err(e) = exec_params(
            conn,
            "MATCH (c:Contract {repo_uid: $uid}) DETACH DELETE c",
            vec![("uid", lbug::Value::String(repo_uid.to_string()))],
        ) {
            tracing::trace!("clear_repo_derived_nodes: Contract delete skipped: {e}");
        }
        Ok(())
    }

    /// Cascade-delete every graph row whose `instance_id` matches `id`:
    /// all Repos (with their files, symbols, services, contracts), all
    /// Vaults (with their notes/headings/sections via
    /// `delete_vault_cascade`), and all Projects. Composes the same
    /// per-Repo cleanup that `index --force` uses, so no novel write
    /// paths are introduced. Idempotent: returns zero counts on a clean
    /// DB. Useful for recovering from a misconfigured `instance merge`
    /// that left an orphan instance ID behind.
    pub fn purge_instance(&self, id: &str) -> Result<PurgeInstanceResult, StoreError> {
        Self::legacy_mutation_result(self.purge_instance_with_outcome(id))
    }

    /// Purge one instance while retaining every confirmed count if a later
    /// stage fails. Discovery, including exact orphan UIDs, completes before
    /// the first destructive statement.
    pub fn purge_instance_with_outcome(
        &self,
        id: &str,
    ) -> Result<MutationOutcome<PurgeInstanceResult>, StoreError> {
        self.purge_instance_with_outcome_inner(id, PurgeInstanceFaults::default())
    }

    #[cfg(test)]
    fn purge_instance_with_outcome_and_faults(
        &self,
        id: &str,
        faults: PurgeInstanceFaults,
    ) -> Result<MutationOutcome<PurgeInstanceResult>, StoreError> {
        self.purge_instance_with_outcome_inner(id, faults)
    }

    fn purge_instance_with_outcome_inner(
        &self,
        id: &str,
        faults: PurgeInstanceFaults,
    ) -> Result<MutationOutcome<PurgeInstanceResult>, StoreError> {
        let plan = self.plan_purge_instance(id)?;
        let planned_orphans = plan
            .orphan_targets
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let mut result = PurgeInstanceResult {
            code_repo_uids: plan.code_repo_uids,
            ..PurgeInstanceResult::default()
        };
        let mut confirmed_changed = false;
        let mut mutation_warnings = Vec::new();

        for (index, repo) in plan.repos.iter().enumerate() {
            if faults.before_repo == Some(index) {
                return Self::purge_stage_error(
                    result,
                    confirmed_changed,
                    mutation_warnings,
                    "purge-repo",
                    StoreError::Query(format!("injected failure before purge Repo {}", repo.uid)),
                );
            }
            let child = match self.delete_repo_cascade_with_outcome(&repo.uid) {
                Ok(child) => child,
                Err(error) => {
                    return Self::purge_stage_error(
                        result,
                        confirmed_changed,
                        mutation_warnings,
                        "purge-repo",
                        error,
                    );
                }
            };
            mutation_warnings.extend(child.mutation_warnings);
            if child.confirmed_changed {
                confirmed_changed = true;
                result.files += child.value.files_deleted;
                result.symbols += child.value.symbols_deleted;
            }
            match child.disposition {
                MutationDisposition::ConfirmedNoChange => {}
                MutationDisposition::CommittedComplete => result.repos += 1,
                MutationDisposition::CommittedPartial | MutationDisposition::Ambiguous => {
                    return Ok(MutationOutcome {
                        disposition: child.disposition,
                        confirmed_changed,
                        value: result,
                        primary_failure: child.primary_failure,
                        mutation_warnings,
                    });
                }
            }
        }

        for (index, vault) in plan.vaults.iter().enumerate() {
            if faults.before_vault == Some(index) {
                return Self::purge_stage_error(
                    result,
                    confirmed_changed,
                    mutation_warnings,
                    "purge-vault",
                    StoreError::Query(format!("injected failure before purge Vault {}", vault.uid)),
                );
            }
            let child = match self.delete_vault_cascade_with_classified_outcome(&vault.uid) {
                Ok(child) => child,
                Err(error) => {
                    return Self::purge_stage_error(
                        result,
                        confirmed_changed,
                        mutation_warnings,
                        "purge-vault",
                        error,
                    );
                }
            };
            mutation_warnings.extend(child.mutation_warnings);
            if child.confirmed_changed {
                confirmed_changed = true;
                result.notes += child.value.notes_deleted;
            }
            match child.disposition {
                MutationDisposition::ConfirmedNoChange => {}
                MutationDisposition::CommittedComplete => result.vaults += 1,
                MutationDisposition::CommittedPartial | MutationDisposition::Ambiguous => {
                    return Ok(MutationOutcome {
                        disposition: child.disposition,
                        confirmed_changed,
                        value: result,
                        primary_failure: child.primary_failure,
                        mutation_warnings,
                    });
                }
            }
        }

        for project in &plan.projects {
            let child = match self.delete_project_cascade_classified(&project.uid) {
                Ok(child) => child,
                Err(error) => {
                    return Self::purge_stage_error(
                        result,
                        confirmed_changed,
                        mutation_warnings,
                        "purge-project",
                        error,
                    );
                }
            };
            mutation_warnings.extend(child.mutation_warnings);
            if child.confirmed_changed {
                confirmed_changed = true;
            }
            match child.disposition {
                MutationDisposition::ConfirmedNoChange => {}
                MutationDisposition::CommittedComplete => result.projects += 1,
                MutationDisposition::CommittedPartial | MutationDisposition::Ambiguous => {
                    return Ok(MutationOutcome {
                        disposition: child.disposition,
                        confirmed_changed,
                        value: result,
                        primary_failure: child.primary_failure,
                        mutation_warnings,
                    });
                }
            }
        }

        for (index, target) in plan.orphan_targets.iter().enumerate() {
            let child = match self.delete_exact_purge_orphan(
                target,
                faults.orphan_commit_after == Some(index),
                faults.orphan_probe,
            ) {
                Ok(child) => child,
                Err(error) => {
                    return Self::purge_stage_error(
                        result,
                        confirmed_changed,
                        mutation_warnings,
                        "purge-orphan",
                        error,
                    );
                }
            };
            mutation_warnings.extend(child.mutation_warnings);
            if child.confirmed_changed {
                confirmed_changed = true;
                result.orphans_swept += child.value;
                if target.code {
                    result.code_orphans_swept += child.value;
                }
            }
            match child.disposition {
                MutationDisposition::ConfirmedNoChange | MutationDisposition::CommittedComplete => {
                }
                MutationDisposition::CommittedPartial | MutationDisposition::Ambiguous => {
                    return Ok(MutationOutcome {
                        disposition: child.disposition,
                        confirmed_changed,
                        value: result,
                        primary_failure: child.primary_failure,
                        mutation_warnings,
                    });
                }
            }
        }

        let final_orphans = match self.list_exact_purge_orphans(id) {
            Ok(targets) => targets
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>(),
            Err(error) => {
                return Ok(MutationOutcome {
                    disposition: MutationDisposition::Ambiguous,
                    confirmed_changed,
                    value: result,
                    primary_failure: Some(MutationFailure::new(
                        "purge-final-probe",
                        format!("exact purge final probe failed: {error}"),
                    )),
                    mutation_warnings,
                });
            }
        };
        if !final_orphans.is_subset(&planned_orphans) || !final_orphans.is_empty() {
            return Ok(MutationOutcome {
                disposition: MutationDisposition::Ambiguous,
                confirmed_changed,
                value: result,
                primary_failure: Some(MutationFailure::new(
                    "purge-final-probe",
                    "exact purge final probe observed remaining or unexpected targets",
                )),
                mutation_warnings,
            });
        }

        Ok(MutationOutcome {
            disposition: if confirmed_changed {
                MutationDisposition::CommittedComplete
            } else {
                MutationDisposition::ConfirmedNoChange
            },
            confirmed_changed,
            value: result,
            primary_failure: None,
            mutation_warnings,
        })
    }

    fn purge_stage_error(
        value: PurgeInstanceResult,
        confirmed_changed: bool,
        mutation_warnings: Vec<MutationFailure>,
        stage: &'static str,
        error: StoreError,
    ) -> Result<MutationOutcome<PurgeInstanceResult>, StoreError> {
        if !confirmed_changed {
            return Err(error);
        }
        Ok(MutationOutcome {
            disposition: MutationDisposition::CommittedPartial,
            confirmed_changed: true,
            value,
            primary_failure: Some(MutationFailure::new(stage, error)),
            mutation_warnings,
        })
    }

    fn plan_purge_instance(&self, id: &str) -> Result<PurgeInstancePlan, StoreError> {
        let code_repo_uids = self.list_purge_code_repo_uids(id)?;
        let mut repos = self.list_repos(Some(id))?;
        let mut vaults = self.list_vaults(Some(id))?;
        let mut projects = self
            .list_projects()?
            .into_iter()
            .filter(|project| project.instance_id == id)
            .collect::<Vec<_>>();
        let orphan_targets = self.list_exact_purge_orphans(id)?;
        repos.sort_by(|left, right| left.uid.cmp(&right.uid));
        vaults.sort_by(|left, right| left.uid.cmp(&right.uid));
        projects.sort_by(|left, right| left.uid.cmp(&right.uid));
        Ok(PurgeInstancePlan {
            repos,
            vaults,
            projects,
            orphan_targets,
            code_repo_uids,
        })
    }

    fn list_exact_purge_orphans(&self, id: &str) -> Result<Vec<PurgeOrphanTarget>, StoreError> {
        let conn = self.conn()?;
        let mut targets = Vec::new();
        for (label, prefix, code) in [
            ("Symbol", format!("sym:repo:{id}:"), true),
            ("File", format!("file:repo:{id}:"), true),
            ("Service", format!("svc:repo:{id}:"), true),
            ("Note", format!("note:vlt:{id}:"), false),
            ("Heading", format!("head:note:vlt:{id}:"), false),
            ("Section", format!("sec:note:vlt:{id}:"), false),
            ("Tag", format!("tag:vlt:{id}:"), false),
            ("Repo", format!("repo:{id}:"), true),
            ("Vault", format!("vlt:{id}:"), false),
            ("Project", format!("proj:{id}:"), false),
        ] {
            let query = format!("MATCH (n:{label}) WHERE n.uid STARTS WITH $p RETURN n.uid");
            let mut statement = conn.prepare(&query).map_err(|error| {
                StoreError::Query(format!("prepare exact purge {label} plan: {error}"))
            })?;
            let rows = conn
                .execute(&mut statement, vec![("p", lbug::Value::String(prefix))])
                .map_err(|error| {
                    StoreError::Query(format!("execute exact purge {label} plan: {error}"))
                })?;
            for row in rows {
                let Some(lbug::Value::String(uid)) = row.first() else {
                    return Err(StoreError::Query(format!(
                        "malformed exact purge {label} identity: {:?}",
                        row.first()
                    )));
                };
                targets.push(PurgeOrphanTarget {
                    label,
                    uid: uid.clone(),
                    code,
                });
            }
        }

        let repo_prefix = format!("repo:{id}:");
        if let Ok(mut statement) =
            conn.prepare("MATCH (n:Contract) WHERE n.repo_uid STARTS WITH $p RETURN n.uid")
        {
            let rows = conn
                .execute(
                    &mut statement,
                    vec![("p", lbug::Value::String(repo_prefix))],
                )
                .map_err(|error| {
                    StoreError::Query(format!("execute exact purge Contract plan: {error}"))
                })?;
            for row in rows {
                let Some(lbug::Value::String(uid)) = row.first() else {
                    return Err(StoreError::Query(format!(
                        "malformed exact purge Contract identity: {:?}",
                        row.first()
                    )));
                };
                targets.push(PurgeOrphanTarget {
                    label: "Contract",
                    uid: uid.clone(),
                    code: true,
                });
            }
        }
        targets.sort();
        let original_len = targets.len();
        targets.dedup();
        if targets.len() != original_len {
            return Err(StoreError::Query(
                "duplicate identity in exact purge plan".to_string(),
            ));
        }
        Ok(targets)
    }

    fn delete_exact_purge_orphan(
        &self,
        target: &PurgeOrphanTarget,
        commit_after: bool,
        probe_fault: bool,
    ) -> Result<MutationOutcome<usize>, StoreError> {
        if !self.purge_orphan_target_exists(target, false)? {
            return Ok(MutationOutcome {
                disposition: MutationDisposition::ConfirmedNoChange,
                confirmed_changed: false,
                value: 0,
                primary_failure: None,
                mutation_warnings: Vec::new(),
            });
        }

        let transaction = self.begin_transaction()?;
        let query = format!("MATCH (n:{} {{uid: $uid}}) DETACH DELETE n", target.label);
        if let Err(primary) = exec_params(
            &transaction,
            &query,
            vec![("uid", lbug::Value::String(target.uid.clone()))],
        ) {
            return match self.rollback_transaction(&transaction) {
                Ok(()) => Err(primary),
                Err(rollback) => self.classify_exact_purge_orphan_error(
                    target,
                    primary,
                    Some(rollback),
                    probe_fault,
                ),
            };
        }

        let commit = self.commit_transaction(&transaction).and_then(|()| {
            if commit_after {
                Err(StoreError::Query(format!(
                    "injected commit acknowledgement failure for purge {} {}",
                    target.label, target.uid
                )))
            } else {
                Ok(())
            }
        });
        match commit {
            Ok(()) => Ok(MutationOutcome {
                disposition: MutationDisposition::CommittedComplete,
                confirmed_changed: true,
                value: 1,
                primary_failure: None,
                mutation_warnings: Vec::new(),
            }),
            Err(primary) => {
                self.classify_exact_purge_orphan_error(target, primary, None, probe_fault)
            }
        }
    }

    fn classify_exact_purge_orphan_error(
        &self,
        target: &PurgeOrphanTarget,
        primary: StoreError,
        rollback: Option<StoreError>,
        probe_fault: bool,
    ) -> Result<MutationOutcome<usize>, StoreError> {
        match self.purge_orphan_target_exists(target, probe_fault) {
            Ok(true) if rollback.is_none() => Err(primary),
            Ok(true) => Ok(MutationOutcome {
                disposition: MutationDisposition::Ambiguous,
                confirmed_changed: false,
                value: 0,
                primary_failure: Some(MutationFailure::new(
                    "purge-orphan",
                    format!(
                        "{primary}; rollback failed: {}",
                        rollback.expect("checked above")
                    ),
                )),
                mutation_warnings: Vec::new(),
            }),
            Ok(false) => Ok(MutationOutcome {
                disposition: MutationDisposition::CommittedComplete,
                confirmed_changed: true,
                value: 1,
                primary_failure: None,
                mutation_warnings: vec![MutationFailure::new("purge-orphan", primary)],
            }),
            Err(probe) => Ok(MutationOutcome {
                disposition: MutationDisposition::Ambiguous,
                confirmed_changed: false,
                value: 0,
                primary_failure: Some(MutationFailure::new(
                    "purge-orphan",
                    format!(
                        "{primary}; exact orphan liveness probe failed: {probe}{}",
                        rollback
                            .map(|error| format!("; rollback failed: {error}"))
                            .unwrap_or_default()
                    ),
                )),
                mutation_warnings: Vec::new(),
            }),
        }
    }

    fn purge_orphan_target_exists(
        &self,
        target: &PurgeOrphanTarget,
        fault: bool,
    ) -> Result<bool, StoreError> {
        if fault {
            return Err(StoreError::Query(
                "injected exact purge orphan probe failure".to_string(),
            ));
        }
        self.instance_merge_node_exists(target.label, &target.uid)
    }

    /// Return every repo UID whose registry row or code children would be
    /// affected by [`purge_instance`](Self::purge_instance). Daemon callers
    /// use this before the non-transactional purge so sidecars can still be
    /// reconciled if a later deletion returns an error.
    pub fn list_purge_code_repo_uids(&self, id: &str) -> Result<Vec<String>, StoreError> {
        let conn = self.conn()?;
        let prefix = format!("repo:{id}:");
        let mut uids = std::collections::HashSet::new();

        for label in ["Symbol", "File", "Service", "Contract"] {
            let query = format!(
                "MATCH (n:{label}) WHERE n.repo_uid STARTS WITH $p RETURN DISTINCT n.repo_uid"
            );
            let Ok(mut stmt) = conn.prepare(&query) else {
                // Optional/legacy tables (notably Contract) may not exist.
                continue;
            };
            let rows = conn
                .execute(&mut stmt, vec![("p", lbug::Value::String(prefix.clone()))])
                .map_err(|e| StoreError::Query(format!("list {label} purge repos: {e}")))?;
            for row in rows {
                if let Some(lbug::Value::String(uid)) = row.first() {
                    uids.insert(uid.clone());
                }
            }
        }

        let mut stmt = conn
            .prepare("MATCH (r:Repo) WHERE r.uid STARTS WITH $p RETURN r.uid")
            .map_err(|e| StoreError::Query(format!("prepare purge repo UIDs: {e}")))?;
        let rows = conn
            .execute(&mut stmt, vec![("p", lbug::Value::String(prefix))])
            .map_err(|e| StoreError::Query(format!("list purge repo UIDs: {e}")))?;
        for row in rows {
            if let Some(lbug::Value::String(uid)) = row.first() {
                uids.insert(uid.clone());
            }
        }

        let mut uids = uids.into_iter().collect::<Vec<_>>();
        uids.sort();
        Ok(uids)
    }

    /// Insert a single CROSS_REPO_LINK edge between two Symbol nodes.
    pub fn insert_cross_repo_link(
        &self,
        from_uid: &str,
        to_uid: &str,
        confidence: f32,
        link_type: &str,
    ) -> Result<(), StoreError> {
        let conn = self.conn()?;
        exec_params(
            &conn,
            "MATCH (a:Symbol {uid: $from}), (b:Symbol {uid: $to}) \
             CREATE (a)-[:CROSS_REPO_LINK {confidence: $conf, link_type: $lt}]->(b)",
            vec![
                ("from", lbug::Value::String(from_uid.to_string())),
                ("to", lbug::Value::String(to_uid.to_string())),
                ("conf", lbug::Value::Double(confidence as f64)),
                ("lt", lbug::Value::String(link_type.to_string())),
            ],
        )
    }

    pub fn batch_insert_project_note_edges(
        &self,
        edges: &[(&str, &str)],
    ) -> Result<(), StoreError> {
        let conn = self.conn()?;
        let mut stmt = conn
            .prepare(
                "MATCH (p:Project {uid: $pid}), (n:Note {uid: $nid}) \
                 CREATE (p)-[:PROJECT_INCLUDES_NOTE {confidence: 1.0}]->(n)",
            )
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        for (project_uid, note_uid) in edges {
            conn.execute(
                &mut stmt,
                vec![
                    ("pid", lbug::Value::String(project_uid.to_string())),
                    ("nid", lbug::Value::String(note_uid.to_string())),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        }
        Ok(())
    }

    pub fn batch_insert_project_symbol_edges(
        &self,
        project_uid: &str,
        symbol_uids: &[String],
        confidence: f32,
    ) -> Result<(), StoreError> {
        let conn = self.conn()?;
        let mut stmt = conn
            .prepare(
                "MATCH (p:Project {uid: $pid}), (s:Symbol {uid: $sid}) \
                 CREATE (p)-[:PROJECT_INCLUDES_SYMBOL {confidence: $conf}]->(s)",
            )
            .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
        for sym_uid in symbol_uids {
            conn.execute(
                &mut stmt,
                vec![
                    ("pid", lbug::Value::String(project_uid.to_string())),
                    ("sid", lbug::Value::String(sym_uid.clone())),
                    ("conf", lbug::Value::Double(confidence as f64)),
                ],
            )
            .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
        }
        Ok(())
    }

    pub fn insert_project_component_edge(
        &self,
        parent_uid: &str,
        child_uid: &str,
        confidence: f32,
    ) -> Result<(), StoreError> {
        let conn = self.conn()?;
        exec_params(
            &conn,
            "MATCH (p:Project {uid: $pid}), (c:Project {uid: $cid}) \
             CREATE (p)-[:PROJECT_HAS_COMPONENT {confidence: $conf}]->(c)",
            vec![
                ("pid", lbug::Value::String(parent_uid.to_string())),
                ("cid", lbug::Value::String(child_uid.to_string())),
                ("conf", lbug::Value::Double(confidence as f64)),
            ],
        )
    }

    pub fn insert_project_parent_edge(
        &self,
        child_uid: &str,
        parent_uid: &str,
        confidence: f32,
    ) -> Result<(), StoreError> {
        let conn = self.conn()?;
        exec_params(
            &conn,
            "MATCH (c:Project {uid: $cid}), (p:Project {uid: $pid}) \
             CREATE (c)-[:PROJECT_HAS_PARENT {confidence: $conf}]->(p)",
            vec![
                ("cid", lbug::Value::String(child_uid.to_string())),
                ("pid", lbug::Value::String(parent_uid.to_string())),
                ("conf", lbug::Value::Double(confidence as f64)),
            ],
        )
    }

    /// Delete all outgoing project edges for the given Project UID atomically.
    ///
    /// This is the rematerialization reset path, so it deliberately preserves
    /// the Project node and incoming parent/component links. Every prepare and
    /// execution error is surfaced and the transaction is explicitly rolled
    /// back. Returns the number of relationships that were present and deleted.
    pub fn delete_project_edges(&self, project_uid: &str) -> Result<usize, StoreError> {
        self.delete_project_edges_with_types(
            project_uid,
            &[
                "PROJECT_INCLUDES_NOTE",
                "PROJECT_INCLUDES_SYMBOL",
                "PROJECT_HAS_COMPONENT",
                "PROJECT_HAS_PARENT",
            ],
        )
    }

    fn delete_project_edges_with_types(
        &self,
        project_uid: &str,
        edge_types: &[&str],
    ) -> Result<usize, StoreError> {
        let conn = self.begin_transaction().map_err(|error| {
            StoreError::Query(format!(
                "begin Project edge deletion for {project_uid}: {error}"
            ))
        })?;
        let mutation = (|| {
            let mut deleted = 0usize;
            for edge_type in edge_types {
                let count_query =
                    format!("MATCH (p:Project {{uid: $uid}})-[r:{edge_type}]->() RETURN count(r)");
                let mut count_stmt = conn.prepare(&count_query).map_err(|error| {
                    StoreError::Query(format!(
                        "prepare count for Project edge {edge_type}: {error}"
                    ))
                })?;
                let mut rows = conn
                    .execute(
                        &mut count_stmt,
                        vec![("uid", lbug::Value::String(project_uid.to_string()))],
                    )
                    .map_err(|error| {
                        StoreError::Query(format!(
                            "execute count for Project edge {edge_type}: {error}"
                        ))
                    })?;
                deleted += rows
                    .next()
                    .and_then(|row| match row.first() {
                        Some(lbug::Value::Int64(count)) => usize::try_from(*count).ok(),
                        _ => None,
                    })
                    .unwrap_or_default();
                drop(rows);

                let delete_query =
                    format!("MATCH (p:Project {{uid: $uid}})-[r:{edge_type}]->() DELETE r");
                let mut delete_stmt = conn.prepare(&delete_query).map_err(|error| {
                    StoreError::Query(format!(
                        "prepare delete for Project edge {edge_type}: {error}"
                    ))
                })?;
                conn.execute(
                    &mut delete_stmt,
                    vec![("uid", lbug::Value::String(project_uid.to_string()))],
                )
                .map_err(|error| {
                    StoreError::Query(format!(
                        "execute delete for Project edge {edge_type}: {error}"
                    ))
                })?;
            }
            Ok(deleted)
        })();

        self.finish_project_transaction(&conn, mutation, "Project edge deletion")
    }

    /// Atomically delete a Project node and every incident relationship.
    /// `DETACH DELETE` covers incoming and outgoing edges, including any
    /// future relationship table that can be incident on Project.
    pub fn delete_project_cascade_with_outcome(
        &self,
        project_uid: &str,
    ) -> Result<DeleteProjectCascadeOutcome, DeleteProjectCascadeError> {
        self.delete_project_cascade_with_queries(project_uid, ProjectCascadeQueries::default())
    }

    /// Delete one Project and resolve every legacy transaction disposition to
    /// the common mutation outcome contract. Ambiguous transaction failures
    /// are probed through [`Self::project_exists`], which opens a connection
    /// distinct from the failed transaction connection.
    pub fn delete_project_cascade_classified(
        &self,
        project_uid: &str,
    ) -> Result<MutationOutcome<DeleteProjectCascadeOutcome>, StoreError> {
        let result = self.delete_project_cascade_with_outcome(project_uid);
        Self::classify_project_cascade_result_with_liveness(result, || {
            self.project_exists(project_uid)
        })
    }

    fn classify_project_cascade_result_with_liveness<F>(
        result: Result<DeleteProjectCascadeOutcome, DeleteProjectCascadeError>,
        liveness: F,
    ) -> Result<MutationOutcome<DeleteProjectCascadeOutcome>, StoreError>
    where
        F: FnOnce() -> Result<bool, StoreError>,
    {
        match result {
            Ok(value) => match value.disposition {
                ProjectMutationDisposition::ConfirmedUnchanged
                | ProjectMutationDisposition::ConfirmedRolledBack => Ok(MutationOutcome {
                    disposition: MutationDisposition::ConfirmedNoChange,
                    confirmed_changed: false,
                    value,
                    primary_failure: None,
                    mutation_warnings: Vec::new(),
                }),
                ProjectMutationDisposition::Changed => Ok(MutationOutcome {
                    disposition: MutationDisposition::CommittedComplete,
                    confirmed_changed: true,
                    value,
                    primary_failure: None,
                    mutation_warnings: Vec::new(),
                }),
                ProjectMutationDisposition::Ambiguous => {
                    let synthetic =
                        "Project cascade returned an ambiguous result without an error".to_string();
                    match liveness() {
                        Ok(false) => Ok(MutationOutcome {
                            disposition: MutationDisposition::CommittedComplete,
                            confirmed_changed: true,
                            value,
                            primary_failure: None,
                            mutation_warnings: vec![MutationFailure::new(
                                "project-delete",
                                synthetic,
                            )],
                        }),
                        Ok(true) => Ok(MutationOutcome {
                            disposition: MutationDisposition::ConfirmedNoChange,
                            confirmed_changed: false,
                            value,
                            primary_failure: None,
                            mutation_warnings: Vec::new(),
                        }),
                        Err(probe) => Ok(MutationOutcome {
                            disposition: MutationDisposition::Ambiguous,
                            confirmed_changed: false,
                            value,
                            primary_failure: Some(MutationFailure::new(
                                "project-delete",
                                format!("{synthetic}; Project liveness probe failed: {probe}"),
                            )),
                            mutation_warnings: Vec::new(),
                        }),
                    }
                }
            },
            Err(error) => {
                let value = DeleteProjectCascadeOutcome {
                    project_uid: error.project_uid.clone(),
                    project_name: error.project_name.clone(),
                    disposition: error.disposition,
                };
                let message = error.to_string();
                match error.disposition {
                    ProjectMutationDisposition::ConfirmedUnchanged
                    | ProjectMutationDisposition::ConfirmedRolledBack => {
                        Err(StoreError::Query(message))
                    }
                    ProjectMutationDisposition::Changed => Ok(MutationOutcome {
                        disposition: MutationDisposition::CommittedComplete,
                        confirmed_changed: true,
                        value,
                        primary_failure: None,
                        mutation_warnings: vec![MutationFailure::new("project-delete", message)],
                    }),
                    ProjectMutationDisposition::Ambiguous => match liveness() {
                        Ok(false) => Ok(MutationOutcome {
                            disposition: MutationDisposition::CommittedComplete,
                            confirmed_changed: true,
                            value,
                            primary_failure: None,
                            mutation_warnings: vec![MutationFailure::new(
                                "project-delete",
                                message,
                            )],
                        }),
                        Ok(true) => Err(StoreError::Query(format!(
                            "{message}; exact Project UID remained live"
                        ))),
                        Err(probe) => Ok(MutationOutcome {
                            disposition: MutationDisposition::Ambiguous,
                            confirmed_changed: false,
                            value,
                            primary_failure: Some(MutationFailure::new(
                                "project-delete",
                                format!("{message}; Project liveness probe failed: {probe}"),
                            )),
                            mutation_warnings: Vec::new(),
                        }),
                    },
                }
            }
        }
    }

    fn delete_project_cascade_with_queries(
        &self,
        project_uid: &str,
        queries: ProjectCascadeQueries,
    ) -> Result<DeleteProjectCascadeOutcome, DeleteProjectCascadeError> {
        let conn = self.conn().map_err(|error| DeleteProjectCascadeError {
            project_uid: project_uid.to_string(),
            project_name: None,
            disposition: ProjectMutationDisposition::ConfirmedUnchanged,
            primary: StoreError::Query(format!("open connection before Project cascade: {error}")),
            rollback: None,
        })?;
        if let Err(error) = conn.query(queries.begin) {
            return Err(DeleteProjectCascadeError {
                project_uid: project_uid.to_string(),
                project_name: None,
                disposition: ProjectMutationDisposition::ConfirmedUnchanged,
                primary: StoreError::Query(format!("begin Project cascade: {error}")),
                rollback: None,
            });
        }
        if queries.repeat_begin
            && let Err(error) = conn.query(queries.begin)
        {
            return Err(Self::project_cascade_pre_mutation_error(
                &conn,
                queries.rollback,
                project_uid,
                None,
                StoreError::Query(format!("begin Project cascade: {error}")),
            ));
        }

        let mut lookup = match conn.prepare(queries.lookup) {
            Ok(lookup) => lookup,
            Err(error) => {
                return Err(Self::project_cascade_pre_mutation_error(
                    &conn,
                    queries.rollback,
                    project_uid,
                    None,
                    StoreError::Query(format!("prepare Project lookup: {error}")),
                ));
            }
        };
        let mut rows = match conn.execute(
            &mut lookup,
            vec![("uid", lbug::Value::String(project_uid.to_string()))],
        ) {
            Ok(rows) => rows,
            Err(error) => {
                return Err(Self::project_cascade_pre_mutation_error(
                    &conn,
                    queries.rollback,
                    project_uid,
                    None,
                    StoreError::Query(format!("execute Project lookup: {error}")),
                ));
            }
        };
        let lookup_row = rows.next();
        drop(rows);

        let Some(lookup_row) = lookup_row else {
            let commit = conn.query(queries.commit).and_then(|_| {
                if queries.repeat_commit {
                    conn.query(queries.commit).map(|_| ())
                } else {
                    Ok(())
                }
            });
            return match commit {
                Ok(_) => Ok(DeleteProjectCascadeOutcome {
                    project_uid: project_uid.to_string(),
                    project_name: None,
                    disposition: ProjectMutationDisposition::ConfirmedUnchanged,
                }),
                Err(error) => Err(Self::project_cascade_pre_mutation_error(
                    &conn,
                    queries.rollback,
                    project_uid,
                    None,
                    StoreError::Query(format!("commit read-only Project lookup: {error}")),
                )),
            };
        };
        let returned_uid = match lookup_row.first() {
            Some(lbug::Value::String(returned_uid)) => returned_uid,
            value => {
                return Err(Self::project_cascade_pre_mutation_error(
                    &conn,
                    queries.rollback,
                    project_uid,
                    None,
                    StoreError::Query(format!(
                        "malformed Project lookup UID for {project_uid}: {value:?}"
                    )),
                ));
            }
        };
        if returned_uid != project_uid {
            return Err(Self::project_cascade_pre_mutation_error(
                &conn,
                queries.rollback,
                project_uid,
                None,
                StoreError::Query(format!(
                    "Project lookup UID mismatch: requested {project_uid}, returned {returned_uid}"
                )),
            ));
        }
        let project_name = match lookup_row.get(1) {
            Some(lbug::Value::String(project_name)) => Some(project_name.clone()),
            _ => None,
        };

        let mut delete = match conn.prepare(queries.delete) {
            Ok(delete) => delete,
            Err(error) => {
                return Err(Self::project_cascade_pre_mutation_error(
                    &conn,
                    queries.rollback,
                    project_uid,
                    project_name,
                    StoreError::Query(format!("prepare Project DETACH DELETE: {error}")),
                ));
            }
        };
        let delete_params = if queries.omit_delete_params {
            Vec::new()
        } else {
            vec![("uid", lbug::Value::String(project_uid.to_string()))]
        };
        if let Err(error) = conn.execute(&mut delete, delete_params) {
            let rollback = Self::project_cascade_rollback(&conn, queries.rollback);
            return Err(DeleteProjectCascadeError {
                project_uid: project_uid.to_string(),
                project_name,
                disposition: if rollback.is_none() {
                    ProjectMutationDisposition::ConfirmedRolledBack
                } else {
                    ProjectMutationDisposition::Ambiguous
                },
                primary: StoreError::Query(format!("execute Project DETACH DELETE: {error}")),
                rollback,
            });
        }

        let commit = conn.query(queries.commit).and_then(|_| {
            if queries.repeat_commit {
                conn.query(queries.commit).map(|_| ())
            } else {
                Ok(())
            }
        });
        match commit {
            Ok(_) => Ok(DeleteProjectCascadeOutcome {
                project_uid: project_uid.to_string(),
                project_name,
                disposition: ProjectMutationDisposition::Changed,
            }),
            Err(error) => Err(DeleteProjectCascadeError {
                project_uid: project_uid.to_string(),
                project_name,
                // A COMMIT error after a mutation is ambiguous even when a
                // best-effort rollback subsequently reports success.
                disposition: ProjectMutationDisposition::Ambiguous,
                primary: StoreError::Query(format!("commit Project cascade: {error}")),
                rollback: Self::project_cascade_rollback(&conn, queries.rollback),
            }),
        }
    }

    fn project_cascade_pre_mutation_error(
        conn: &lbug::Connection<'_>,
        rollback_query: &str,
        project_uid: &str,
        project_name: Option<String>,
        primary: StoreError,
    ) -> DeleteProjectCascadeError {
        DeleteProjectCascadeError {
            project_uid: project_uid.to_string(),
            project_name,
            disposition: ProjectMutationDisposition::ConfirmedUnchanged,
            primary,
            rollback: Self::project_cascade_rollback(conn, rollback_query),
        }
    }

    fn project_cascade_rollback(
        conn: &lbug::Connection<'_>,
        rollback_query: &str,
    ) -> Option<StoreError> {
        conn.query(rollback_query)
            .err()
            .map(|error| StoreError::Query(format!("rollback Project cascade: {error}")))
    }

    #[cfg(test)]
    fn delete_project_cascade_with_faults(
        &self,
        project_uid: &str,
        faults: ProjectCascadeFaults,
    ) -> Result<DeleteProjectCascadeOutcome, DeleteProjectCascadeError> {
        let mut queries = ProjectCascadeQueries::default();
        if faults.begin {
            queries.repeat_begin = true;
        }
        if faults.lookup {
            queries.lookup = "INJECTED LOOKUP FAILURE";
        }
        if faults.lookup_uid_mismatch {
            queries.lookup = "MATCH (p:Project {uid: $uid}) RETURN 'proj:txn:unexpected', p.name";
        }
        if faults.lookup_uid_malformed {
            queries.lookup = "MATCH (p:Project {uid: $uid}) RETURN 42, p.name";
        }
        if faults.lookup_name_malformed {
            queries.lookup = "MATCH (p:Project {uid: $uid}) RETURN p.uid, 42";
        }
        if faults.before_mutation {
            queries.delete = "INJECTED BEFORE MUTATION FAILURE";
        }
        queries.omit_delete_params = faults.detach;
        if faults.commit {
            queries.repeat_commit = true;
        }
        if faults.rollback {
            queries.rollback = "INJECTED ROLLBACK FAILURE";
        }
        self.delete_project_cascade_with_queries(project_uid, queries)
    }

    fn finish_project_transaction<T>(
        &self,
        conn: &lbug::Connection<'_>,
        mutation: Result<T, StoreError>,
        operation: &str,
    ) -> Result<T, StoreError> {
        match mutation {
            Ok(value) => match self.commit_transaction(conn) {
                Ok(()) => Ok(value),
                Err(error) => {
                    let commit_error = StoreError::Query(format!("{operation} commit: {error}"));
                    Err(Self::rollback_project_transaction(
                        conn,
                        commit_error,
                        operation,
                    ))
                }
            },
            Err(error) => Err(Self::rollback_project_transaction(conn, error, operation)),
        }
    }

    fn rollback_project_transaction(
        conn: &lbug::Connection<'_>,
        error: StoreError,
        operation: &str,
    ) -> StoreError {
        match conn.query("ROLLBACK") {
            Ok(_) => error,
            Err(rollback_error) => StoreError::Query(format!(
                "{error}; {operation} rollback failed: {rollback_error}"
            )),
        }
    }

    /// Delete the Project node itself (and any remaining edges).
    pub fn delete_project_node(&self, project_uid: &str) -> Result<(), StoreError> {
        self.delete_project_cascade_with_outcome(project_uid)
            .map_err(|error| StoreError::Query(error.to_string()))?;
        Ok(())
    }

    /// Count notes belonging to a vault.
    fn vault_note_count(&self, vault_uid: &str) -> Result<usize, StoreError> {
        let conn = self.conn()?;
        let safe_vid = vault_uid.replace('\'', "\\'");
        let rows = conn
            .query(&format!(
                "MATCH (n:Note) WHERE n.vault_uid = '{safe_vid}' RETURN count(n)"
            ))
            .map_err(|e| StoreError::Query(format!("count notes: {e}")))?;
        Ok(rows
            .filter_map(|row| {
                row.first().and_then(|v| match v {
                    lbug::Value::Int64(n) => Some(*n as usize),
                    _ => None,
                })
            })
            .next()
            .unwrap_or(0))
    }

    /// Migrate all child nodes (notes, headings, sections, tags) from one
    /// vault to a new vault with a different UID and instance_id. The old
    /// vault is cascade-deleted and a new vault is created in its place,
    /// preserving all node data and structural edges (NOTE_HAS_HEADING,
    /// NOTE_HAS_SECTION, NOTE_TAGGED_WITH, SECTION_TAGGED_WITH).
    ///
    /// Cross-domain edges (WIKILINK_TO_NOTE, WIKILINK_TO_HEADING,
    /// REFERENCES_CODE_*) are NOT preserved — they are rebuilt by
    /// `discover_cross_domain_links` / `index_markdown_directory` on the
    /// next `brain add` invocation.
    ///
    /// Uses the LadybugDB-compatible DETACH DELETE + re-CREATE pattern
    /// since SET is not supported for property updates.
    /// Read wikilink edges that ORIGINATE in `vault_uid`, as
    /// `(section_uid, target_uid, confidence, display)`.
    ///
    /// `rel` is the relationship name and `dst` the destination pattern, so one
    /// helper serves both WIKILINK_TO_NOTE and WIKILINK_TO_HEADING. Best-effort:
    /// a missing table yields an empty vec rather than an error, matching how
    /// the cascade treats these tables.
    fn wikilink_edges_for_vault(
        &self,
        vault_uid: &str,
        rel: &str,
        dst: &str,
    ) -> Result<Vec<(String, String, f32, String)>, StoreError> {
        let conn = self.conn()?;
        let q = format!(
            "MATCH (n:Note {{vault_uid: $vid}})-[:NOTE_HAS_SECTION]->(s:Section)-[r:{rel}]->({dst}) \
             RETURN s.uid, dst.uid, r.confidence, r.display"
        );
        let mut stmt = match conn.prepare(&q) {
            Ok(stmt) => stmt,
            Err(e) => {
                tracing::trace!("wikilink_edges_for_vault: prepare {rel} skipped: {e}");
                return Ok(Vec::new());
            }
        };
        let result = match conn.execute(
            &mut stmt,
            vec![("vid", lbug::Value::String(vault_uid.to_string()))],
        ) {
            Ok(result) => result,
            Err(e) => {
                tracing::trace!("wikilink_edges_for_vault: execute {rel} skipped: {e}");
                return Ok(Vec::new());
            }
        };
        Ok(result
            .filter_map(|row| {
                Some((
                    crate::read::extract_string(&row, 0).ok()?,
                    crate::read::extract_string(&row, 1).ok()?,
                    row.get(2)
                        .and_then(|v| match v {
                            lbug::Value::Double(d) => Some(*d as f32),
                            lbug::Value::Float(f) => Some(*f),
                            _ => None,
                        })
                        .unwrap_or(0.0),
                    crate::read::extract_string(&row, 3).unwrap_or_default(),
                ))
            })
            .collect())
    }

    /// Read every UnresolvedWikilink row as
    /// `(uid, source_note_uid, source_path, source_title, wikilink_text)`.
    /// Callers filter by source note. Best-effort on a missing table.
    fn all_unresolved_wikilinks(&self) -> Result<Vec<UnresolvedWikilinkRecord>, StoreError> {
        let conn = self.conn()?;
        let q = "MATCH (u:UnresolvedWikilink) \
                 RETURN u.uid, u.source_note_uid, u.source_path, u.source_title, u.wikilink_text";
        let result = match conn.query(q) {
            Ok(result) => result,
            Err(e) => {
                tracing::trace!("all_unresolved_wikilinks skipped: {e}");
                return Ok(Vec::new());
            }
        };
        Ok(result
            .filter_map(|row| {
                Some((
                    crate::read::extract_string(&row, 0).ok()?,
                    crate::read::extract_string(&row, 1).ok()?,
                    crate::read::extract_string(&row, 2).unwrap_or_default(),
                    crate::read::extract_string(&row, 3).unwrap_or_default(),
                    crate::read::extract_string(&row, 4).unwrap_or_default(),
                ))
            })
            .collect())
    }

    pub fn reparent_vault(
        &self,
        old_vault_uid: &str,
        new_vault_uid: &str,
        new_instance_id: &str,
    ) -> Result<ReparentVaultResult, StoreError> {
        // 1. Read the old vault metadata.
        let old_vault = self
            .list_vaults(None)?
            .into_iter()
            .find(|v| v.uid == old_vault_uid)
            .ok_or_else(|| StoreError::Query(format!("vault not found: {old_vault_uid}")))?;

        // 2. Read all children and edges before deletion.
        let notes = self.list_notes(Some(old_vault_uid))?;
        let headings = self.list_headings_by_vault(old_vault_uid)?;
        let sections = self.list_sections_by_vault(old_vault_uid)?;
        let tags = self.list_tags(Some(old_vault_uid))?;

        // Capture note-tag edges before cascade destroys them.
        let note_tag_edges: Vec<(String, String)> = {
            let conn = self.conn()?;
            let q = "MATCH (n:Note {vault_uid: $vid})-[:NOTE_TAGGED_WITH]->(t:Tag) \
                     RETURN n.uid, t.uid";
            let mut stmt = conn
                .prepare(q)
                .map_err(|e| StoreError::Query(format!("prepare: {e}")))?;
            let result = conn
                .execute(
                    &mut stmt,
                    vec![("vid", lbug::Value::String(old_vault_uid.to_string()))],
                )
                .map_err(|e| StoreError::Query(format!("execute: {e}")))?;
            result
                .filter_map(|row| {
                    let nuid = crate::read::extract_string(&row, 0).ok()?;
                    let tuid = crate::read::extract_string(&row, 1).ok()?;
                    Some((nuid, tuid))
                })
                .collect()
        };

        // Capture section-tag edges before cascade destroys them.
        let section_tag_edges: Vec<(String, String)> = {
            let conn = self.conn()?;
            let q = "MATCH (n:Note {vault_uid: $vid})-[:NOTE_HAS_SECTION]->(s:Section)-[:SECTION_TAGGED_WITH]->(t:Tag) \
                     RETURN s.uid, t.uid";
            let mut stmt = conn
                .prepare(q)
                .map_err(|e| StoreError::Query(format!("prepare section_tag edges: {e}")))?;
            let result = conn
                .execute(
                    &mut stmt,
                    vec![("vid", lbug::Value::String(old_vault_uid.to_string()))],
                )
                .map_err(|e| StoreError::Query(format!("execute section_tag edges: {e}")))?;
            result
                .filter_map(|row| {
                    let suid = crate::read::extract_string(&row, 0).ok()?;
                    let tuid = crate::read::extract_string(&row, 1).ok()?;
                    Some((suid, tuid))
                })
                .collect()
        };

        let result = ReparentVaultResult {
            notes_migrated: notes.len(),
            headings_migrated: headings.len(),
            sections_migrated: sections.len(),
            tags_migrated: tags.len(),
        };

        // Prepare all replacement rows before opening the transaction. The
        // delete and every insert below share one commit so a crash or write
        // error cannot leave both the old and new vault roots absent.
        let reparented_notes: Vec<Note> = notes
            .into_iter()
            .map(|n| Note {
                vault_uid: new_vault_uid.to_string(),
                ..n
            })
            .collect();
        let vault_note_edges: Vec<(&str, &str)> = reparented_notes
            .iter()
            .map(|n| (new_vault_uid, n.uid.as_str()))
            .collect();
        let note_heading_edges: Vec<(&str, &str)> = headings
            .iter()
            .map(|h| (h.note_uid.as_str(), h.uid.as_str()))
            .collect();
        let note_section_edges: Vec<(&str, &str)> = sections
            .iter()
            .map(|s| (s.note_uid.as_str(), s.uid.as_str()))
            .collect();
        let heading_section_edges: Vec<(&str, &str)> = sections
            .iter()
            .filter_map(|s| {
                s.heading_uid
                    .as_ref()
                    .map(|huid| (huid.as_str(), s.uid.as_str()))
            })
            .collect();
        // Capture the wikilink graph before the cascade destroys it (nw-112).
        //
        // Every other child and edge above is captured and re-inserted, but
        // wikilinks were not — so `instance merge` reparented a vault and
        // silently wiped the note-to-note link graph, 2,067 edges to 0 on the
        // real brain, while reporting success. Backlinks, broken-link detection
        // and the graph view all went empty with no warning.
        //
        // Edges are keyed on section/note/heading UIDs, none of which change
        // here, so restoring them verbatim is sufficient. A link whose TARGET
        // lives in another vault is preserved too: that node is untouched by
        // this cascade.
        let wikilink_to_note: Vec<(String, String, f32, String)> =
            self.wikilink_edges_for_vault(old_vault_uid, "WIKILINK_TO_NOTE", "dst:Note")?;
        let wikilink_to_heading: Vec<(String, String, f32, String)> =
            self.wikilink_edges_for_vault(old_vault_uid, "WIKILINK_TO_HEADING", "dst:Heading")?;

        // `delete_vault_cascade` removes UnresolvedWikilink rows whose source
        // note belongs to this vault (its step 5), so they need restoring as
        // well or `broken-links` comes back empty after a merge.
        // UIDs are unchanged by reparenting (only vault_uid moves), so the
        // reparented list identifies the same notes.
        let note_uid_set: std::collections::HashSet<&str> =
            reparented_notes.iter().map(|n| n.uid.as_str()).collect();
        let unresolved: Vec<UnresolvedWikilinkRecord> = self
            .all_unresolved_wikilinks()?
            .into_iter()
            .filter(|(_, source_note_uid, _, _, _)| note_uid_set.contains(source_note_uid.as_str()))
            .collect();

        let reparented_tags: Vec<Tag> = tags
            .into_iter()
            .map(|t| Tag {
                vault_uid: new_vault_uid.to_string(),
                ..t
            })
            .collect();
        let nt_edges: Vec<(&str, &str)> = note_tag_edges
            .iter()
            .map(|(nuid, tuid)| (nuid.as_str(), tuid.as_str()))
            .collect();
        let st_edges: Vec<(&str, &str)> = section_tag_edges
            .iter()
            .map(|(suid, tuid)| (suid.as_str(), tuid.as_str()))
            .collect();

        let txn = self.begin_transaction()?;
        let conn = &txn;
        // 3. Delete old vault and all its children.
        Self::delete_vault_cascade_on(conn, old_vault_uid)?;

        // 4. Create new vault with updated UID and instance_id.
        exec_params(
            conn,
            "CREATE (:Vault {uid: $uid, name: $name, root_path: $rp, instance_id: $iid})",
            vec![
                ("uid", lbug::Value::String(new_vault_uid.to_string())),
                ("name", lbug::Value::String(old_vault.name)),
                ("rp", lbug::Value::String(old_vault.root_path)),
                ("iid", lbug::Value::String(new_instance_id.to_string())),
            ],
        )?;

        // 5. Re-insert notes with updated vault_uid and restore their edges.
        Self::batch_insert_notes_on(conn, &reparented_notes)?;
        Self::batch_insert_vault_note_edges_on(conn, &vault_note_edges)?;

        // 6. Re-insert headings and their edges (note_uid stays the same).
        Self::batch_insert_headings_on(conn, &headings)?;
        Self::batch_insert_note_heading_edges_on(conn, &note_heading_edges)?;

        // 7. Re-insert sections and their edges (note_uid stays the same).
        Self::batch_insert_sections_on(conn, &sections)?;
        Self::batch_insert_note_section_edges_on(conn, &note_section_edges)?;
        if !heading_section_edges.is_empty() {
            Self::batch_insert_heading_section_edges_on(conn, &heading_section_edges)?;
        }

        // 8. Re-insert tags with updated vault_uid and restore tag edges.
        Self::batch_insert_tags_on(conn, &reparented_tags)?;
        if !nt_edges.is_empty() {
            Self::batch_insert_note_tag_edges_on(conn, &nt_edges)?;
        }
        if !st_edges.is_empty() {
            Self::batch_insert_section_tag_edges_on(conn, &st_edges)?;
        }

        // 9. Restore the wikilink graph. Runs last: the edges reference
        //    sections, notes and headings, all of which are back in place by
        //    now (nw-112).
        let wl_note: Vec<(&str, &str, f32, &str)> = wikilink_to_note
            .iter()
            .map(|(src, dst, conf, disp)| (src.as_str(), dst.as_str(), *conf, disp.as_str()))
            .collect();
        if !wl_note.is_empty() {
            Self::batch_insert_wikilink_to_note_edges_on(conn, &wl_note)?;
        }
        let wl_heading: Vec<(&str, &str, f32, &str)> = wikilink_to_heading
            .iter()
            .map(|(src, dst, conf, disp)| (src.as_str(), dst.as_str(), *conf, disp.as_str()))
            .collect();
        if !wl_heading.is_empty() {
            Self::batch_insert_wikilink_to_heading_edges_on(conn, &wl_heading)?;
        }
        if !unresolved.is_empty() {
            Self::batch_insert_unresolved_wikilinks_on(conn, &unresolved)?;
        }

        self.commit_transaction(&txn)?;

        Ok(result)
    }

    /// Enumerate every authored extension UID that changes when `from` is
    /// merged into `to`.
    ///
    /// Every instance-derived UID that the merge invalidates is represented:
    /// Repo, File, Service, Symbol, Vault, and Project. File, Service, and
    /// Symbol rows are deleted during the merge and require re-indexing, so
    /// their predicted target UIDs are computed before deletion. Sorting makes
    /// collision and dedup handling deterministic when multiple source rows
    /// map to one destination.
    pub fn plan_instance_uid_remaps(
        &self,
        from: &str,
        to: &str,
    ) -> Result<Vec<InstanceUidRemap>, StoreError> {
        Ok(self.plan_instance_uid_migration(from, to)?.remaps)
    }

    /// Plan UID remaps together with semantic handoffs for rows that indexing
    /// recreates. Handoffs are source-UID sorted and bind the predicted UID to
    /// an instance-independent identity used after publication.
    pub fn plan_instance_uid_migration(
        &self,
        from: &str,
        to: &str,
    ) -> Result<InstanceUidMigrationPlan, StoreError> {
        if from == to {
            return Err(StoreError::Query(format!(
                "source and target instance IDs must differ (both were {from:?})"
            )));
        }

        let mut remaps = Vec::new();
        let mut handoffs = Vec::new();
        let mut repo_recoveries = Vec::new();
        let mut vault_recoveries = Vec::new();
        let mut project_recoveries = Vec::new();
        // The LadybugDB Cypher subset does not support a MATCH subquery in an
        // `IN` predicate, so enumerate once and scope by each source Repo UID.
        let services = self.list_services(None)?;
        for repo in self.list_repos(Some(from))? {
            let destination_repo_uid = repo_uid(to, &repo.url);
            remaps.push(InstanceUidRemap {
                source_uid: repo.uid.clone(),
                destination_uid: destination_repo_uid.clone(),
            });
            repo_recoveries.push(InstanceRepoRecovery {
                source_uid: repo.uid.clone(),
                destination_uid: destination_repo_uid.clone(),
                url: repo.url.clone(),
                staleness_commits_behind: repo.staleness_commits_behind,
                name: repo.name.clone(),
                root_path: repo.root_path.clone(),
            });
            for (source_uid, path) in self.list_files_by_repo(&repo.uid)? {
                let destination_uid = file_uid(&destination_repo_uid, &path);
                remaps.push(InstanceUidRemap {
                    source_uid: source_uid.clone(),
                    destination_uid: destination_uid.clone(),
                });
                handoffs.push(InstanceUidHandoff {
                    source_uid,
                    predicted_destination_uid: destination_uid,
                    identity: InstanceUidHandoffIdentity::File {
                        destination_repo_uid: destination_repo_uid.clone(),
                        path,
                    },
                });
            }
            for symbol in self.lookup_symbols_by_repo(&repo.uid)? {
                let destination_uid = symbol_uid(
                    &destination_repo_uid,
                    &symbol.file_path,
                    &symbol.name,
                    symbol.start_line,
                );
                remaps.push(InstanceUidRemap {
                    source_uid: symbol.uid.clone(),
                    destination_uid: destination_uid.clone(),
                });
                handoffs.push(InstanceUidHandoff {
                    source_uid: symbol.uid,
                    predicted_destination_uid: destination_uid,
                    identity: InstanceUidHandoffIdentity::Symbol {
                        destination_repo_uid: destination_repo_uid.clone(),
                        canonical_id: symbol.canonical_id,
                        file_path: symbol.file_path,
                        name: symbol.name,
                        kind: symbol.kind.to_string(),
                    },
                });
            }
            for service in services
                .iter()
                .filter(|service| service.repo_uid == repo.uid)
            {
                let destination_uid = service_uid(&destination_repo_uid, &service.name);
                remaps.push(InstanceUidRemap {
                    source_uid: service.uid.clone(),
                    destination_uid: destination_uid.clone(),
                });
                handoffs.push(InstanceUidHandoff {
                    source_uid: service.uid.clone(),
                    predicted_destination_uid: destination_uid,
                    identity: InstanceUidHandoffIdentity::Service {
                        destination_repo_uid: destination_repo_uid.clone(),
                        name: service.name.clone(),
                    },
                });
            }
        }
        for vault in self.list_vaults(Some(from))? {
            let destination_uid = vault_uid(to, &vault.root_path);
            remaps.push(InstanceUidRemap {
                source_uid: vault.uid.clone(),
                destination_uid: destination_uid.clone(),
            });
            vault_recoveries.push(InstanceVaultRecovery {
                source_uid: vault.uid,
                destination_uid,
                name: vault.name,
                root_path: vault.root_path,
            });
        }
        for project_merge in self.plan_instance_project_merges(from, to)? {
            if let Some(source_uid) = project_merge.recovery_source_uid.clone() {
                project_recoveries.push(InstanceProjectRecovery {
                    source_uid,
                    destination_uid: project_merge.winner.uid.clone(),
                    name: project_merge.winner.name.clone(),
                    summary: project_merge.winner.summary.clone(),
                });
            }
            remaps.extend(project_merge.remaps);
        }
        remaps.sort();
        remaps.dedup();
        handoffs.sort();
        handoffs.dedup();
        repo_recoveries.sort();
        repo_recoveries.dedup();
        vault_recoveries.sort();
        vault_recoveries.dedup();
        project_recoveries.sort();
        project_recoveries.dedup();
        Ok(InstanceUidMigrationPlan {
            remaps,
            handoffs,
            repo_recoveries,
            vault_recoveries,
            project_recoveries,
        })
    }

    /// Restore target Repo roots whose source row was committed deleted before
    /// the corresponding target insert. The empty indexed SHA makes the
    /// recovered root explicitly require a full re-index.
    pub fn recover_missing_instance_repos(
        &self,
        to: &str,
        recoveries: &[InstanceRepoRecovery],
    ) -> Result<usize, StoreError> {
        let mut restored = 0;
        for recovery in recoveries {
            if repo_uid(to, &recovery.url) != recovery.destination_uid {
                return Err(StoreError::Query(format!(
                    "repo recovery destination is not deterministic: {}",
                    recovery.destination_uid
                )));
            }
            if self.lookup_repo(&recovery.source_uid)?.is_some()
                || self.lookup_repo(&recovery.destination_uid)?.is_some()
            {
                continue;
            }
            self.insert_repo(&Repo {
                uid: recovery.destination_uid.clone(),
                url: recovery.url.clone(),
                indexed_sha: String::new(),
                staleness_commits_behind: recovery.staleness_commits_behind,
                instance_id: to.to_string(),
                name: recovery.name.clone(),
                root_path: recovery.root_path.clone(),
            })?;
            restored += 1;
        }
        Ok(restored)
    }

    /// Restore target Repo, Vault, and Project roots whose source row was
    /// committed deleted before the corresponding target insert. Recovered
    /// code and Vault roots are intentionally empty and require re-indexing.
    pub fn recover_missing_instance_roots(
        &self,
        to: &str,
        repo_recoveries: &[InstanceRepoRecovery],
        vault_recoveries: &[InstanceVaultRecovery],
        project_recoveries: &[InstanceProjectRecovery],
    ) -> Result<usize, StoreError> {
        let mut restored = self.recover_missing_instance_repos(to, repo_recoveries)?;
        for recovery in vault_recoveries {
            if vault_uid(to, &recovery.root_path) != recovery.destination_uid {
                return Err(StoreError::Query(format!(
                    "vault recovery destination is not deterministic: {}",
                    recovery.destination_uid
                )));
            }
            if !self.list_vaults(None)?.iter().any(|vault| {
                vault.uid == recovery.source_uid || vault.uid == recovery.destination_uid
            }) {
                self.insert_vault(&Vault {
                    uid: recovery.destination_uid.clone(),
                    name: recovery.name.clone(),
                    root_path: recovery.root_path.clone(),
                    instance_id: to.to_string(),
                })?;
                restored += 1;
            }
        }
        for recovery in project_recoveries {
            if project_uid(to, &recovery.name) != recovery.destination_uid {
                return Err(StoreError::Query(format!(
                    "project recovery destination is not deterministic: {}",
                    recovery.destination_uid
                )));
            }
            if !self.list_projects()?.iter().any(|project| {
                project.uid == recovery.source_uid || project.uid == recovery.destination_uid
            }) {
                self.insert_project(&Project {
                    uid: recovery.destination_uid.clone(),
                    name: recovery.name.clone(),
                    summary: recovery.summary.clone(),
                    instance_id: to.to_string(),
                })?;
                restored += 1;
            }
        }
        Ok(restored)
    }

    fn instance_merge_node_exists(&self, label: &str, uid: &str) -> Result<bool, StoreError> {
        let conn = self.conn()?;
        let query = format!("MATCH (n:{label} {{uid: $uid}}) RETURN n.uid");
        let mut statement = conn
            .prepare(&query)
            .map_err(|error| StoreError::Query(format!("prepare {label} liveness: {error}")))?;
        let mut rows = conn
            .execute(
                &mut statement,
                vec![("uid", lbug::Value::String(uid.to_string()))],
            )
            .map_err(|error| StoreError::Query(format!("execute {label} liveness: {error}")))?;
        Ok(rows.next().is_some())
    }

    /// Prove whether an exact, previously journaled UID remap plan is still
    /// prepared, has a provably-applied prefix/subset, or is fully applied.
    /// Current mappings must be an exact subset of the journal, and every
    /// missing mapping must independently prove source absence plus a live
    /// destination (or destination root). Extras and contradictions fail closed.
    pub fn verify_instance_uid_remap_plan_state(
        &self,
        from: &str,
        to: &str,
        expected: &[InstanceUidRemap],
    ) -> Result<InstanceUidRemapPlanState, StoreError> {
        if expected.is_empty() {
            return Err(StoreError::Query(
                "cannot verify an empty instance UID remap plan".to_string(),
            ));
        }
        let current = self.plan_instance_uid_remaps(from, to)?;
        if current == expected {
            return Ok(InstanceUidRemapPlanState::Prepared);
        }
        let expected_set: std::collections::BTreeSet<_> = expected.iter().cloned().collect();
        if expected_set.len() != expected.len() {
            return Err(StoreError::Query(
                "journaled instance UID remap plan contains duplicates".to_string(),
            ));
        }
        for mapping in &current {
            if !expected_set.contains(mapping) {
                return Err(StoreError::Query(format!(
                    "current graph remap is extra or contradicts the journaled plan: {} -> {}",
                    mapping.source_uid, mapping.destination_uid
                )));
            }
        }

        let current_set: std::collections::BTreeSet<_> = current.iter().cloned().collect();
        let mut missing_root_destinations = std::collections::BTreeSet::new();
        for mapping in expected
            .iter()
            .filter(|mapping| !current_set.contains(*mapping))
        {
            let root_label = if mapping.source_uid.starts_with("repo:")
                && mapping.destination_uid.starts_with("repo:")
            {
                Some("Repo")
            } else if mapping.source_uid.starts_with("vlt:")
                && mapping.destination_uid.starts_with("vlt:")
            {
                Some("Vault")
            } else if mapping.source_uid.starts_with("proj:")
                && mapping.destination_uid.starts_with("proj:")
            {
                Some("Project")
            } else {
                None
            };
            if let Some(root_label) = root_label {
                let source_live =
                    self.instance_merge_node_exists(root_label, &mapping.source_uid)?;
                let destination_live =
                    self.instance_merge_node_exists(root_label, &mapping.destination_uid)?;
                if !source_live && !destination_live {
                    missing_root_destinations.insert(mapping.destination_uid.clone());
                }
            }
        }
        for mapping in expected
            .iter()
            .filter(|mapping| !current_set.contains(*mapping))
        {
            let (source_label, destination_label, destination_root) =
                if mapping.source_uid.starts_with("repo:")
                    && mapping.destination_uid.starts_with("repo:")
                {
                    ("Repo", "Repo", mapping.destination_uid.clone())
                } else if mapping.source_uid.starts_with("proj:")
                    && mapping.destination_uid.starts_with("proj:")
                {
                    ("Project", "Project", mapping.destination_uid.clone())
                } else if mapping.source_uid.starts_with("vlt:")
                    && mapping.destination_uid.starts_with("vlt:")
                {
                    ("Vault", "Vault", mapping.destination_uid.clone())
                } else if mapping.source_uid.starts_with("svc:repo:")
                    && mapping.destination_uid.starts_with("svc:repo:")
                {
                    let parts: Vec<&str> = mapping.destination_uid.split(':').collect();
                    if parts.len() != 5 {
                        return Err(StoreError::Query(
                            "journaled Service remap has invalid destination UID".to_string(),
                        ));
                    }
                    ("Service", "Repo", format!("repo:{}:{}", parts[2], parts[3]))
                } else if mapping.source_uid.starts_with("file:repo:")
                    && mapping.destination_uid.starts_with("file:repo:")
                {
                    let parts: Vec<&str> = mapping.destination_uid.split(':').collect();
                    if parts.len() != 5 {
                        return Err(StoreError::Query(
                            "journaled File remap has invalid destination UID".to_string(),
                        ));
                    }
                    ("File", "Repo", format!("repo:{}:{}", parts[2], parts[3]))
                } else if mapping.source_uid.starts_with("sym:repo:")
                    && mapping.destination_uid.starts_with("sym:repo:")
                {
                    let parts: Vec<&str> = mapping.destination_uid.split(':').collect();
                    if parts.len() != 7 {
                        return Err(StoreError::Query(
                            "journaled Symbol remap has invalid destination UID".to_string(),
                        ));
                    }
                    ("Symbol", "Repo", format!("repo:{}:{}", parts[2], parts[3]))
                } else {
                    return Err(StoreError::Query(
                        "journaled remap changes node kind or uses an unsupported UID".to_string(),
                    ));
                };
            if self.instance_merge_node_exists(source_label, &mapping.source_uid)? {
                return Err(StoreError::Query(format!(
                    "journaled source node still exists after non-matching plan: {}",
                    mapping.source_uid
                )));
            }
            if !self.instance_merge_node_exists(destination_label, &destination_root)?
                && !missing_root_destinations.contains(&destination_root)
            {
                return Err(StoreError::Query(format!(
                    "journaled destination root does not exist: {destination_root}"
                )));
            }
        }
        if current.is_empty() && missing_root_destinations.is_empty() {
            Ok(InstanceUidRemapPlanState::Applied)
        } else {
            Ok(InstanceUidRemapPlanState::PartiallyApplied)
        }
    }

    /// Rewrite `instance_id` on all Vault, Repo, and Project nodes that
    /// match `from` to `to`. Returns a [`MergeResult`] with counts and
    /// details about any vaults whose notes were discarded during collision
    /// resolution (when two instances have vaults at the same root_path,
    /// the vault with fewer notes loses).
    ///
    /// Uses [`reparent_vault`] to preserve notes in the winning vault.
    pub fn merge_instance_ids(&self, from: &str, to: &str) -> Result<MergeResult, StoreError> {
        Self::legacy_mutation_result(self.merge_instance_ids_with_outcome(from, to))
    }

    /// Merge two instance IDs and classify failures against the exact remap
    /// plan captured before the first graph mutation.
    pub fn merge_instance_ids_with_outcome(
        &self,
        from: &str,
        to: &str,
    ) -> Result<MutationOutcome<MergeResult>, StoreError> {
        if from == to {
            return Err(StoreError::Query(format!(
                "source and target instance IDs must differ (both were {from:?})"
            )));
        }
        let plan = self.plan_instance_uid_remaps(from, to)?;
        self.merge_instance_ids_with_plan_inner(from, to, &plan, MergeInstanceFaults::default())
    }

    /// Classified merge using the caller's durable exact remap plan. This is
    /// the recovery-safe entry point for journal-backed daemon operations.
    pub fn merge_instance_ids_with_plan_outcome(
        &self,
        from: &str,
        to: &str,
        expected_plan: &[InstanceUidRemap],
    ) -> Result<MutationOutcome<MergeResult>, StoreError> {
        self.merge_instance_ids_with_plan_inner(
            from,
            to,
            expected_plan,
            MergeInstanceFaults::default(),
        )
    }

    #[cfg(test)]
    fn merge_instance_ids_with_plan_and_faults(
        &self,
        from: &str,
        to: &str,
        expected_plan: &[InstanceUidRemap],
        faults: MergeInstanceFaults,
    ) -> Result<MutationOutcome<MergeResult>, StoreError> {
        self.merge_instance_ids_with_plan_inner(from, to, expected_plan, faults)
    }

    fn merge_instance_ids_with_plan_inner(
        &self,
        from: &str,
        to: &str,
        expected_plan: &[InstanceUidRemap],
        faults: MergeInstanceFaults,
    ) -> Result<MutationOutcome<MergeResult>, StoreError> {
        if from == to {
            return Err(StoreError::Query(format!(
                "source and target instance IDs must differ (both were {from:?})"
            )));
        }
        if expected_plan.is_empty() {
            if self.plan_instance_uid_remaps(from, to)?.is_empty() {
                return Ok(MutationOutcome {
                    disposition: MutationDisposition::ConfirmedNoChange,
                    confirmed_changed: false,
                    value: MergeResult::default(),
                    primary_failure: None,
                    mutation_warnings: Vec::new(),
                });
            }
            return Err(StoreError::Query(
                "durable instance merge plan is empty but source rows remain".to_string(),
            ));
        }

        let initial_state = match self.verify_instance_uid_remap_plan_state(from, to, expected_plan)
        {
            Ok(state) => state,
            Err(error) => {
                return Ok(MutationOutcome {
                    disposition: MutationDisposition::Ambiguous,
                    confirmed_changed: false,
                    value: MergeResult::default(),
                    primary_failure: Some(MutationFailure::new(
                        "merge-preflight",
                        format!("exact merge plan verification failed: {error}"),
                    )),
                    mutation_warnings: Vec::new(),
                });
            }
        };
        if initial_state == InstanceUidRemapPlanState::Applied {
            return Ok(MutationOutcome {
                disposition: MutationDisposition::CommittedComplete,
                confirmed_changed: true,
                value: MergeResult::default(),
                primary_failure: None,
                mutation_warnings: Vec::new(),
            });
        }
        let mut confirmed_changed = initial_state == InstanceUidRemapPlanState::PartiallyApplied;
        let mut value = MergeResult::default();
        let mut mutation_warnings = Vec::new();
        // Source Vaults deleted because the target won a root_path collision:
        // their plan remaps are intentionally left without a destination (the
        // discard IS the plan), so the final probe must not score them as
        // unapplied.
        let mut discarded_vault_uids = std::collections::BTreeSet::new();

        // Complete all graph discovery before the first mutation.
        let project_merges = match self.plan_instance_project_merges(from, to) {
            Ok(plan) => plan,
            Err(error) => {
                return self.classify_merge_error(
                    from,
                    to,
                    expected_plan,
                    value,
                    confirmed_changed,
                    mutation_warnings,
                    "merge-preflight",
                    error,
                    faults.verify,
                );
            }
        };
        let all_vaults = match self.list_vaults(None) {
            Ok(vaults) => vaults,
            Err(error) => {
                return self.classify_merge_error(
                    from,
                    to,
                    expected_plan,
                    value,
                    confirmed_changed,
                    mutation_warnings,
                    "merge-preflight",
                    error,
                    faults.verify,
                );
            }
        };
        let mut source_vaults = all_vaults
            .iter()
            .filter(|vault| vault.instance_id == from)
            .cloned()
            .collect::<Vec<_>>();
        let target_vaults = all_vaults
            .into_iter()
            .filter(|vault| vault.instance_id == to)
            .map(|vault| (vault.root_path.clone(), vault))
            .collect::<std::collections::HashMap<_, _>>();
        let mut source_repos = match self.list_repos(Some(from)) {
            Ok(repos) => repos,
            Err(error) => {
                return self.classify_merge_error(
                    from,
                    to,
                    expected_plan,
                    value,
                    confirmed_changed,
                    mutation_warnings,
                    "merge-preflight",
                    error,
                    faults.verify,
                );
            }
        };
        source_vaults.sort_by(|left, right| left.uid.cmp(&right.uid));
        source_repos.sort_by(|left, right| left.uid.cmp(&right.uid));

        if faults.before_graph {
            return self.classify_merge_error(
                from,
                to,
                expected_plan,
                value,
                confirmed_changed,
                mutation_warnings,
                "merge-graph",
                StoreError::Query("injected failure before graph merge".to_string()),
                faults.verify,
            );
        }

        for vault in source_vaults {
            let root_path = vault.root_path.clone();
            let new_uid = vault_uid(to, &root_path);
            if let Some(target) = target_vaults.get(&root_path) {
                let source_count = match self.vault_note_count(&vault.uid) {
                    Ok(count) => count,
                    Err(error) => {
                        return self.classify_merge_error(
                            from,
                            to,
                            expected_plan,
                            value,
                            confirmed_changed,
                            mutation_warnings,
                            "merge-vault",
                            error,
                            faults.verify,
                        );
                    }
                };
                let target_count = match self.vault_note_count(&target.uid) {
                    Ok(count) => count,
                    Err(error) => {
                        return self.classify_merge_error(
                            from,
                            to,
                            expected_plan,
                            value,
                            confirmed_changed,
                            mutation_warnings,
                            "merge-vault",
                            error,
                            faults.verify,
                        );
                    }
                };
                if source_count > target_count {
                    let deletion =
                        match self.delete_vault_cascade_with_classified_outcome(&target.uid) {
                            Ok(outcome) => outcome,
                            Err(error) => {
                                return self.classify_merge_error(
                                    from,
                                    to,
                                    expected_plan,
                                    value,
                                    confirmed_changed,
                                    mutation_warnings,
                                    "merge-vault",
                                    error,
                                    faults.verify,
                                );
                            }
                        };
                    mutation_warnings.extend(deletion.mutation_warnings);
                    if deletion.disposition == MutationDisposition::Ambiguous {
                        return Ok(MutationOutcome {
                            disposition: MutationDisposition::Ambiguous,
                            confirmed_changed: confirmed_changed || deletion.confirmed_changed,
                            value,
                            primary_failure: deletion.primary_failure,
                            mutation_warnings,
                        });
                    }
                    confirmed_changed |= deletion.confirmed_changed;
                    if let Err(error) = self.reparent_vault(&vault.uid, &new_uid, to) {
                        return self.classify_merge_error(
                            from,
                            to,
                            expected_plan,
                            value,
                            confirmed_changed,
                            mutation_warnings,
                            "merge-vault",
                            error,
                            faults.verify,
                        );
                    }
                    confirmed_changed = true;
                    if deletion.value.notes_deleted > 0 {
                        value.discarded.push(DiscardedVault {
                            root_path,
                            notes_discarded: deletion.value.notes_deleted,
                        });
                    }
                } else {
                    let deletion =
                        match self.delete_vault_cascade_with_classified_outcome(&vault.uid) {
                            Ok(outcome) => outcome,
                            Err(error) => {
                                return self.classify_merge_error(
                                    from,
                                    to,
                                    expected_plan,
                                    value,
                                    confirmed_changed,
                                    mutation_warnings,
                                    "merge-vault",
                                    error,
                                    faults.verify,
                                );
                            }
                        };
                    mutation_warnings.extend(deletion.mutation_warnings);
                    if deletion.disposition == MutationDisposition::Ambiguous {
                        return Ok(MutationOutcome {
                            disposition: MutationDisposition::Ambiguous,
                            confirmed_changed: confirmed_changed || deletion.confirmed_changed,
                            value,
                            primary_failure: deletion.primary_failure,
                            mutation_warnings,
                        });
                    }
                    confirmed_changed |= deletion.confirmed_changed;
                    discarded_vault_uids.insert(vault.uid.clone());
                    if deletion.value.notes_deleted > 0 {
                        value.discarded.push(DiscardedVault {
                            root_path,
                            notes_discarded: deletion.value.notes_deleted,
                        });
                    }
                }
            } else if let Err(error) = self.reparent_vault(&vault.uid, &new_uid, to) {
                return self.classify_merge_error(
                    from,
                    to,
                    expected_plan,
                    value,
                    confirmed_changed,
                    mutation_warnings,
                    "merge-vault",
                    error,
                    faults.verify,
                );
            } else {
                confirmed_changed = true;
            }
            value.vaults += 1;
        }

        for repo in source_repos {
            let repo_ident = repo.name.clone().unwrap_or_else(|| repo.url.clone());
            let source_uid = repo.uid.clone();
            let target_uid = repo_uid(to, &repo.url);
            let deletion = match self.delete_repo_cascade_with_outcome(&source_uid) {
                Ok(outcome) => outcome,
                Err(error) => {
                    return self.classify_merge_error(
                        from,
                        to,
                        expected_plan,
                        value,
                        confirmed_changed,
                        mutation_warnings,
                        "merge-repo",
                        error,
                        faults.verify,
                    );
                }
            };
            mutation_warnings.extend(deletion.mutation_warnings);
            if matches!(
                deletion.disposition,
                MutationDisposition::CommittedPartial | MutationDisposition::Ambiguous
            ) {
                if deletion.disposition == MutationDisposition::Ambiguous {
                    return Ok(MutationOutcome {
                        disposition: MutationDisposition::Ambiguous,
                        confirmed_changed: confirmed_changed || deletion.confirmed_changed,
                        value,
                        primary_failure: deletion.primary_failure,
                        mutation_warnings,
                    });
                }
                let failure = deletion
                    .primary_failure
                    .unwrap_or_else(|| MutationFailure::new("merge-repo", "partial Repo deletion"));
                return self.classify_merge_error(
                    from,
                    to,
                    expected_plan,
                    value,
                    confirmed_changed || deletion.confirmed_changed,
                    mutation_warnings,
                    &failure.stage,
                    StoreError::Query(failure.message),
                    faults.verify,
                );
            }
            confirmed_changed |= deletion.confirmed_changed;
            let target_exists = match self.lookup_repo(&target_uid) {
                Ok(repo) => repo.is_some(),
                Err(error) => {
                    return self.classify_merge_error(
                        from,
                        to,
                        expected_plan,
                        value,
                        confirmed_changed,
                        mutation_warnings,
                        "merge-repo",
                        error,
                        faults.verify,
                    );
                }
            };
            if !target_exists {
                if let Err(error) = self.insert_repo(&Repo {
                    uid: target_uid,
                    instance_id: to.to_string(),
                    ..repo
                }) {
                    return self.classify_merge_error(
                        from,
                        to,
                        expected_plan,
                        value,
                        confirmed_changed,
                        mutation_warnings,
                        "merge-repo",
                        error,
                        faults.verify,
                    );
                }
                confirmed_changed = true;
            }
            value.repos += 1;
            value.repos_moved.push(repo_ident);
            value.repo_uids_removed.push(source_uid);
            if faults.after_repo == Some(value.repos) {
                let repos_merged = value.repos;
                return self.classify_merge_error(
                    from,
                    to,
                    expected_plan,
                    value,
                    confirmed_changed,
                    mutation_warnings,
                    "merge-repo",
                    StoreError::Query(format!(
                        "injected failure after merged Repo {}",
                        repos_merged
                    )),
                    faults.verify,
                );
            }
        }

        for project_merge in project_merges {
            if !project_merge.winner_preexists {
                if let Err(error) = self.insert_project(&project_merge.winner) {
                    return self.classify_merge_error(
                        from,
                        to,
                        expected_plan,
                        value,
                        confirmed_changed,
                        mutation_warnings,
                        "merge-project",
                        error,
                        faults.verify,
                    );
                }
                confirmed_changed = true;
            }
            for mapping in &project_merge.remaps {
                let deletion = match self.delete_project_cascade_classified(&mapping.source_uid) {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        return self.classify_merge_error(
                            from,
                            to,
                            expected_plan,
                            value,
                            confirmed_changed,
                            mutation_warnings,
                            "merge-project",
                            error,
                            faults.verify,
                        );
                    }
                };
                mutation_warnings.extend(deletion.mutation_warnings);
                if deletion.disposition == MutationDisposition::Ambiguous {
                    return Ok(MutationOutcome {
                        disposition: MutationDisposition::Ambiguous,
                        confirmed_changed: confirmed_changed || deletion.confirmed_changed,
                        value,
                        primary_failure: deletion.primary_failure,
                        mutation_warnings,
                    });
                }
                confirmed_changed |= deletion.confirmed_changed;
            }
            value.projects += project_merge.source_count;
        }

        if faults.after_graph {
            return self.classify_merge_error(
                from,
                to,
                expected_plan,
                value,
                confirmed_changed,
                mutation_warnings,
                "merge-graph",
                StoreError::Query("injected failure after graph merge".to_string()),
                faults.verify,
            );
        }
        // Intentional collision discards satisfy their plan items by deleting
        // the source — the predicted destination is never created, so verify
        // the remaining plan only. An empty remainder means every planned
        // remap was an intentional discard: fully applied.
        let remaining_plan: Vec<InstanceUidRemap> = expected_plan
            .iter()
            .filter(|mapping| !discarded_vault_uids.contains(&mapping.source_uid))
            .cloned()
            .collect();
        let final_state = if remaining_plan.is_empty() {
            Ok(InstanceUidRemapPlanState::Applied)
        } else {
            self.verify_merge_plan(from, to, &remaining_plan, faults.verify)
        };
        match final_state {
            Ok(InstanceUidRemapPlanState::Applied) => Ok(MutationOutcome {
                disposition: MutationDisposition::CommittedComplete,
                confirmed_changed,
                value,
                primary_failure: None,
                mutation_warnings,
            }),
            Ok(state) => self.classify_merge_error(
                from,
                to,
                expected_plan,
                value,
                confirmed_changed,
                mutation_warnings,
                "merge-final-probe",
                StoreError::Query(format!(
                    "merge returned before exact plan reached Applied; state={state:?}"
                )),
                false,
            ),
            Err(error) => Ok(MutationOutcome {
                disposition: MutationDisposition::Ambiguous,
                confirmed_changed,
                value,
                primary_failure: Some(MutationFailure::new(
                    "merge-final-probe",
                    format!("exact merge plan verification failed: {error}"),
                )),
                mutation_warnings,
            }),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn classify_merge_error(
        &self,
        from: &str,
        to: &str,
        expected_plan: &[InstanceUidRemap],
        value: MergeResult,
        confirmed_changed: bool,
        mut mutation_warnings: Vec<MutationFailure>,
        stage: &str,
        primary: StoreError,
        verification_fault: bool,
    ) -> Result<MutationOutcome<MergeResult>, StoreError> {
        let state = match self.verify_merge_plan(from, to, expected_plan, verification_fault) {
            Ok(state) => state,
            Err(verification) => {
                return Ok(MutationOutcome {
                    disposition: MutationDisposition::Ambiguous,
                    confirmed_changed,
                    value,
                    primary_failure: Some(MutationFailure::new(
                        stage,
                        format!("{primary}; exact merge plan verification failed: {verification}"),
                    )),
                    mutation_warnings,
                });
            }
        };
        match state {
            InstanceUidRemapPlanState::Prepared if !confirmed_changed => Err(primary),
            InstanceUidRemapPlanState::Prepared | InstanceUidRemapPlanState::PartiallyApplied => {
                Ok(MutationOutcome {
                    disposition: MutationDisposition::CommittedPartial,
                    confirmed_changed: true,
                    value,
                    primary_failure: Some(MutationFailure::new(stage, primary)),
                    mutation_warnings,
                })
            }
            InstanceUidRemapPlanState::Applied => {
                mutation_warnings.push(MutationFailure::new(stage, primary));
                Ok(MutationOutcome {
                    disposition: MutationDisposition::CommittedComplete,
                    confirmed_changed: true,
                    value,
                    primary_failure: None,
                    mutation_warnings,
                })
            }
        }
    }

    fn verify_merge_plan(
        &self,
        from: &str,
        to: &str,
        expected_plan: &[InstanceUidRemap],
        fault: bool,
    ) -> Result<InstanceUidRemapPlanState, StoreError> {
        if fault {
            Err(StoreError::Query(
                "injected exact merge plan verification failure".to_string(),
            ))
        } else {
            self.verify_instance_uid_remap_plan_state(from, to, expected_plan)
        }
    }

    // ── DB-level metadata ───────────────────────────────────────────────────

    /// Persist the embedding model ID and vector dimension as a singleton
    /// `Meta` node in the database. lbug does not support MERGE or SET, so
    /// we use the established delete-then-create upsert pattern. The node
    /// is keyed by the fixed string `"embedding"` — only one such record
    /// can exist at a time. Calling this again replaces any previous value.
    pub fn set_embedding_metadata(&self, model_id: &str, dimension: u32) -> Result<(), StoreError> {
        let conn = self.conn()?;

        // Encode both fields into a single JSON string so we can use the
        // two-column Meta table without widening it. Serialize via serde so a
        // model_id containing quotes/backslashes/newlines (e.g. a local model
        // path) can't produce invalid JSON that fails to parse on read.
        let value = serde_json::json!({ "model_id": model_id, "dimension": dimension }).to_string();

        // Delete the existing singleton, if any. Best-effort: silently
        // ignore errors from tables that were never created (old DBs).
        let _ = exec_params(
            &conn,
            "MATCH (m:Meta {key: $k}) DETACH DELETE m",
            vec![("k", lbug::Value::String("embedding".to_string()))],
        );

        exec_params(
            &conn,
            "CREATE (:Meta {key: $k, value: $v})",
            vec![
                ("k", lbug::Value::String("embedding".to_string())),
                ("v", lbug::Value::String(value)),
            ],
        )
    }
}

#[cfg(test)]
mod copy_from_tests {
    use super::*;
    use std::io::Write as IoWrite;

    /// Verify that lbug 0.16 supports COPY FROM CSV for NODE tables (Symbol).
    ///
    /// Column order must match the CREATE NODE TABLE definition exactly:
    /// uid, name, kind, repo_uid, file_path, start_line, end_line,
    /// signature, summary, content_hash, pagerank_score, is_entry_point,
    /// entry_point_kind, framework_hint
    ///
    /// This test MUST pass — if COPY FROM CSV does not work for node tables
    /// the bulk-CSV indexing optimization cannot proceed.
    #[test]
    fn test_copy_from_csv_node_table() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test_copy_node.lbug");
        let store = GraphStore::create(&db_path).unwrap();

        // Write a CSV with a header row + 100 Symbol rows.
        // Column order matches CREATE NODE TABLE Symbol(...) exactly.
        let csv_path = dir.path().join("symbols.csv");
        {
            let mut f = std::fs::File::create(&csv_path).unwrap();
            // Header — COPY FROM with HEADER=true should skip this line.
            writeln!(
                f,
                "uid,name,kind,repo_uid,file_path,start_line,end_line,\
                 signature,summary,content_hash,pagerank_score,is_entry_point,\
                 entry_point_kind,framework_hint,canonical_id"
            )
            .unwrap();
            for i in 0..100 {
                writeln!(
                    f,
                    "sym:{i},sym_name_{i},function,repo:test,src/lib.rs,{i},{i},\
                     \"fn sym_{i}()\",\"summary {i}\",hash{i:04},0.0,false,,,",
                )
                .unwrap();
            }
        }

        let csv_str = csv_path.to_str().unwrap();

        // Try COPY FROM with HEADER=true first (Kùzu-standard syntax).
        let result_with_header = {
            let conn = store.conn().unwrap();
            conn.query(&format!("COPY Symbol FROM '{csv_str}' (HEADER=true)"))
        };

        let count_after = {
            let conn = store.conn().unwrap();
            let rows = conn
                .query("MATCH (s:Symbol) RETURN count(s)")
                .expect("count query failed");
            rows.filter_map(|row| {
                row.first().and_then(|v| match v {
                    lbug::Value::Int64(n) => Some(*n),
                    _ => None,
                })
            })
            .next()
            .unwrap_or(0)
        };

        if result_with_header.is_ok() && count_after == 100 {
            // success
        } else {
            // Try without HEADER option — lbug may treat the first row as data.
            // First clear any partial inserts.
            {
                let conn = store.conn().unwrap();
                let _ = conn.query("MATCH (s:Symbol) DETACH DELETE s");
            }

            // Rewrite CSV without header row.
            let csv_no_hdr_path = dir.path().join("symbols_no_header.csv");
            {
                let mut f = std::fs::File::create(&csv_no_hdr_path).unwrap();
                for i in 0..100 {
                    writeln!(
                        f,
                        "sym:{i},sym_name_{i},function,repo:test,src/lib.rs,{i},{i},\
                         \"fn sym_{i}()\",\"summary {i}\",hash{i:04},0.0,false,,,",
                    )
                    .unwrap();
                }
            }

            let csv_no_hdr_str = csv_no_hdr_path.to_str().unwrap();
            let result_no_header = {
                let conn = store.conn().unwrap();
                conn.query(&format!("COPY Symbol FROM '{csv_no_hdr_str}'"))
            };

            let count_no_hdr = {
                let conn = store.conn().unwrap();
                let rows = conn
                    .query("MATCH (s:Symbol) RETURN count(s)")
                    .expect("count query failed");
                rows.filter_map(|row| {
                    row.first().and_then(|v| match v {
                        lbug::Value::Int64(n) => Some(*n),
                        _ => None,
                    })
                })
                .next()
                .unwrap_or(0)
            };

            if result_no_header.is_ok() && count_no_hdr == 100 {
                // success
            } else {
                panic!(
                    "FAIL: COPY FROM CSV did not insert 100 symbols. \
                     with_header_result={result_with_header:?}, count={count_after}; \
                     no_header_result={result_no_header:?}, count={count_no_hdr}"
                );
            }
        }
    }

    /// Exploratory: verify whether lbug 0.16 supports COPY FROM CSV for REL
    /// (relationship/edge) tables. REPO_HAS_FILE has no properties so the CSV
    /// only needs (from_uid, to_uid) — the primary keys of Repo and File.
    ///
    /// If COPY FROM works: asserts the edges were created.
    /// If COPY FROM fails: prints the error and documents the limitation —
    /// does NOT panic, because this test is purely exploratory.
    #[test]
    fn test_copy_from_csv_rel_table() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test_copy_rel.lbug");
        let store = GraphStore::create(&db_path).unwrap();

        // Insert prerequisite Repo and File nodes via normal Cypher.
        {
            let conn = store.conn().unwrap();
            conn.query(
                "CREATE (:Repo {uid: 'repo:test', url: 'https://example.com', \
                 indexed_sha: 'abc', staleness_commits_behind: 0, \
                 instance_id: 'inst:test', name: 'testrepo'})",
            )
            .expect("insert Repo");

            for i in 0..10 {
                conn.query(&format!(
                    "CREATE (:File {{uid: 'file:{i}', path: 'src/file{i}.rs', \
                     repo_uid: 'repo:test', content_hash: 'hash{i}'}})"
                ))
                .expect("insert File");
            }
        }

        // Write a CSV with (repo_uid, file_uid) pairs for REPO_HAS_FILE.
        // Kùzu REL table COPY FROM expects (from_pk, to_pk) in column order.
        let csv_path = dir.path().join("repo_has_file.csv");
        {
            let mut f = std::fs::File::create(&csv_path).unwrap();
            for i in 0..10 {
                writeln!(f, "repo:test,file:{i}").unwrap();
            }
        }

        let csv_str = csv_path.to_str().unwrap();

        let result = {
            let conn = store.conn().unwrap();
            conn.query(&format!("COPY REPO_HAS_FILE FROM '{csv_str}'"))
        };

        match result {
            Ok(_) => {
                // Verify the edges landed.
                let conn = store.conn().unwrap();
                let rows = conn
                    .query("MATCH (r:Repo)-[:REPO_HAS_FILE]->(f:File) RETURN count(r)")
                    .expect("count edges");
                let edge_count: i64 = rows
                    .filter_map(|row| {
                        row.first().and_then(|v| match v {
                            lbug::Value::Int64(n) => Some(*n),
                            _ => None,
                        })
                    })
                    .next()
                    .unwrap_or(0);

                assert_eq!(
                    edge_count, 10,
                    "expected 10 REPO_HAS_FILE edges after COPY FROM CSV"
                );
            }
            Err(e) => {
                // Document the failure — do not panic. The optimization path
                // for edge tables will need a different approach (e.g. batch
                // Cypher MATCH+CREATE).
                eprintln!(
                    "INFO: COPY FROM CSV for REL table (REPO_HAS_FILE) is NOT supported \
                     in this lbug build. Error: {e}"
                );
                eprintln!("Edge COPY FROM will need a batch Cypher workaround.");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_classified_vault(store: &GraphStore, vault_uid: &str) {
        store
            .insert_vault(&Vault {
                uid: vault_uid.to_string(),
                name: "classified".to_string(),
                root_path: format!("/{vault_uid}"),
                instance_id: "classified".to_string(),
            })
            .unwrap();
        store
            .insert_note(&Note {
                uid: format!("note:{vault_uid}:one"),
                vault_uid: vault_uid.to_string(),
                file_path: "one.md".to_string(),
                title: "One".to_string(),
                note_kind: nestweaver_schema::NoteKind::General,
                word_count: 1,
                content_hash: "one".to_string(),
                frontmatter: None,
                created_at: None,
                modified_at: None,
                pagerank_score: None,
                embedding: None,
            })
            .unwrap();
        store
            .insert_tag(&Tag {
                uid: format!("tag:{vault_uid}:one"),
                vault_uid: vault_uid.to_string(),
                name: "one".to_string(),
            })
            .unwrap();
    }

    /// nw-112: a merge must CONSERVE the wikilink graph.
    ///
    /// `reparent_vault` captures notes, headings, sections, tags and their edges
    /// before the cascade delete, then re-inserts them — but it never captured
    /// wikilink edges, so the cascade destroyed them and nothing put them back.
    /// On the real brain this took wikilinks 2,067 -> 0 while the command
    /// printed success and exited 0: backlinks, broken-link detection and the
    /// graph view all went silently empty.
    #[test]
    fn merge_conserves_the_wikilink_graph() {
        let store = GraphStore::in_memory().expect("store");

        let vault_uid = "vlt:old:aaaa";
        store
            .insert_vault(&Vault {
                uid: vault_uid.to_string(),
                name: "brain".to_string(),
                root_path: "/brain".to_string(),
                instance_id: "old".to_string(),
            })
            .unwrap();

        // Two notes, each with one section, and a wikilink from A's section to B.
        for (n, title) in [("a", "Alpha"), ("b", "Beta")] {
            store
                .insert_note(&Note {
                    uid: format!("note:{vault_uid}:{n}"),
                    vault_uid: vault_uid.to_string(),
                    file_path: format!("{n}.md"),
                    title: title.to_string(),
                    note_kind: nestweaver_schema::NoteKind::General,
                    word_count: 1,
                    content_hash: n.to_string(),
                    frontmatter: None,
                    created_at: None,
                    modified_at: None,
                    pagerank_score: None,
                    embedding: None,
                })
                .unwrap();
            store
                .insert_section(&Section {
                    uid: format!("sec:{vault_uid}:{n}"),
                    note_uid: format!("note:{vault_uid}:{n}"),
                    heading_uid: None,
                    start_line: 1,
                    end_line: 2,
                    text_hash: n.to_string(),
                    text_content: format!("body {n}"),
                    word_count: 2,
                    pagerank_score: None,
                })
                .unwrap();
            let note_uid = format!("note:{vault_uid}:{n}");
            let sec_uid = format!("sec:{vault_uid}:{n}");
            store
                .batch_insert_note_section_edges(&[(note_uid.as_str(), sec_uid.as_str())])
                .unwrap();
            store
                .batch_insert_vault_note_edges(&[(vault_uid, note_uid.as_str())])
                .unwrap();
        }

        let src_sec = format!("sec:{vault_uid}:a");
        let dst_note = format!("note:{vault_uid}:b");
        store
            .batch_insert_wikilink_to_note_edges(&[(
                src_sec.as_str(),
                dst_note.as_str(),
                1.0f32,
                "Beta",
            )])
            .unwrap();

        let before = store.count_wikilink_edges().unwrap();
        assert_eq!(before, 1, "fixture must start with a wikilink");

        store.merge_instance_ids("old", "new").unwrap();

        let after = store.count_wikilink_edges().unwrap();
        assert_eq!(
            after, before,
            "merge destroyed the wikilink graph: {before} -> {after}"
        );
    }

    #[test]
    fn vault_cascade_outcome_pre_delete_failure_rolls_back() {
        let store = GraphStore::in_memory().unwrap();
        let vault_uid = "vlt:classified:rollback";
        seed_classified_vault(&store, vault_uid);

        let error = store
            .delete_vault_cascade_with_classified_outcome_and_faults(
                vault_uid,
                VaultCascadeFaults {
                    before_delete: true,
                    ..Default::default()
                },
            )
            .unwrap_err();

        assert!(error.to_string().contains("before delete"));
        assert!(
            store
                .list_vaults(None)
                .unwrap()
                .iter()
                .any(|v| v.uid == vault_uid)
        );
        assert_eq!(store.list_notes(Some(vault_uid)).unwrap().len(), 1);
        assert_eq!(store.list_tags(Some(vault_uid)).unwrap().len(), 1);
    }

    #[test]
    fn vault_cascade_outcome_commit_failure_wholly_live_is_proved_no_change() {
        let store = GraphStore::in_memory().unwrap();
        let vault_uid = "vlt:classified:commit-live";
        seed_classified_vault(&store, vault_uid);

        let error = store
            .delete_vault_cascade_with_classified_outcome_and_faults(
                vault_uid,
                VaultCascadeFaults {
                    commit_before: true,
                    ..Default::default()
                },
            )
            .unwrap_err();

        assert!(error.to_string().contains("commit"));
        assert!(
            store
                .list_vaults(None)
                .unwrap()
                .iter()
                .any(|v| v.uid == vault_uid)
        );
        assert_eq!(store.list_notes(Some(vault_uid)).unwrap().len(), 1);
        assert_eq!(store.list_tags(Some(vault_uid)).unwrap().len(), 1);
    }

    #[test]
    fn vault_cascade_outcome_commit_failure_wholly_absent_is_complete_with_warning() {
        let store = GraphStore::in_memory().unwrap();
        let vault_uid = "vlt:classified:commit-absent";
        seed_classified_vault(&store, vault_uid);

        let outcome = store
            .delete_vault_cascade_with_classified_outcome_and_faults(
                vault_uid,
                VaultCascadeFaults {
                    commit_after: true,
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(outcome.disposition, MutationDisposition::CommittedComplete);
        assert!(outcome.confirmed_changed);
        assert_eq!(outcome.value.notes_deleted, 1);
        assert_eq!(outcome.primary_failure, None);
        assert_eq!(outcome.mutation_warnings.len(), 1);
        assert!(
            !store
                .list_vaults(None)
                .unwrap()
                .iter()
                .any(|v| v.uid == vault_uid)
        );
        assert!(store.list_notes(Some(vault_uid)).unwrap().is_empty());
        assert!(store.list_tags(Some(vault_uid)).unwrap().is_empty());
    }

    #[test]
    fn vault_cascade_outcome_commit_failure_with_failed_probe_is_ambiguous() {
        let store = GraphStore::in_memory().unwrap();
        let vault_uid = "vlt:classified:probe";
        seed_classified_vault(&store, vault_uid);

        let outcome = store
            .delete_vault_cascade_with_classified_outcome_and_faults(
                vault_uid,
                VaultCascadeFaults {
                    commit_after: true,
                    probe: true,
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(outcome.disposition, MutationDisposition::Ambiguous);
        assert!(!outcome.confirmed_changed);
        assert!(outcome.primary_failure.is_some());
        assert!(outcome.mutation_warnings.is_empty());
        assert!(
            !store
                .list_vaults(None)
                .unwrap()
                .iter()
                .any(|v| v.uid == vault_uid)
        );
    }

    #[test]
    fn vault_cascade_outcome_missing_vault_is_confirmed_no_change() {
        let store = GraphStore::in_memory().unwrap();

        let outcome = store
            .delete_vault_cascade_with_classified_outcome("vlt:classified:missing")
            .unwrap();

        assert_eq!(outcome.disposition, MutationDisposition::ConfirmedNoChange);
        assert!(!outcome.confirmed_changed);
        assert_eq!(outcome.value.notes_deleted, 0);
        assert_eq!(outcome.primary_failure, None);
        assert!(outcome.mutation_warnings.is_empty());
    }

    fn seed_classified_repo(store: &GraphStore, repo_uid: &str) {
        store
            .insert_repo(&Repo {
                uid: repo_uid.to_string(),
                url: format!("https://example.invalid/{repo_uid}"),
                indexed_sha: "classified".to_string(),
                staleness_commits_behind: 0,
                instance_id: "classified".to_string(),
                name: Some(repo_uid.to_string()),
                root_path: None,
            })
            .unwrap();
        store
            .insert_file(&File {
                uid: format!("file:{repo_uid}:one"),
                path: "src/one.rs".to_string(),
                repo_uid: repo_uid.to_string(),
                content_hash: "one".to_string(),
            })
            .unwrap();
        store
            .insert_symbol(&Symbol {
                uid: format!("sym:{repo_uid}:one"),
                name: "one".to_string(),
                kind: nestweaver_schema::SymbolKind::Function,
                repo_uid: repo_uid.to_string(),
                file_path: "src/one.rs".to_string(),
                start_line: 1,
                end_line: 1,
                signature: "fn one()".to_string(),
                summary: None,
                content_hash: "one".to_string(),
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
        store
            .insert_service(&Service {
                uid: format!("svc:{repo_uid}:one"),
                name: "one".to_string(),
                repo_uid: repo_uid.to_string(),
                summary: None,
                summary_hash: None,
                embedding: None,
            })
            .unwrap();
        store
            .insert_contract(&Contract {
                uid: format!("contract:{repo_uid}:one"),
                kind: "rest-endpoint".to_string(),
                verb: Some("GET".to_string()),
                path: Some("/one".to_string()),
                operation_id: None,
                repo_uid: repo_uid.to_string(),
                source_path: "openapi.yaml".to_string(),
                confidence: 1.0,
            })
            .unwrap();
    }

    #[test]
    fn classified_repo_delete_empty_target_is_confirmed_no_change() {
        let store = GraphStore::in_memory().unwrap();

        let outcome = store
            .delete_repo_cascade_with_outcome("repo:classified:missing")
            .unwrap();

        assert_eq!(outcome.disposition, MutationDisposition::ConfirmedNoChange);
        assert!(!outcome.confirmed_changed);
        assert_eq!(outcome.value.files_deleted, 0);
        assert_eq!(outcome.value.symbols_deleted, 0);
        assert_eq!(outcome.primary_failure, None);
    }

    #[test]
    fn classified_repo_delete_bulk_commit_ack_failure_continues_from_exact_absence() {
        let store = GraphStore::in_memory().unwrap();
        let repo_uid = "repo:classified:bulk-ack";
        seed_classified_repo(&store, repo_uid);

        let outcome = store
            .delete_repo_cascade_with_outcome_and_faults(
                repo_uid,
                RepoCascadeFaults {
                    bulk_commit_after: true,
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(outcome.disposition, MutationDisposition::CommittedComplete);
        assert!(outcome.confirmed_changed);
        assert_eq!(outcome.value.files_deleted, 1);
        assert_eq!(outcome.value.symbols_deleted, 1);
        assert_eq!(outcome.mutation_warnings.len(), 1);
        assert!(store.lookup_repo(repo_uid).unwrap().is_none());
    }

    #[test]
    fn classified_repo_delete_failure_after_committed_bulk_retains_partial_counts() {
        let store = GraphStore::in_memory().unwrap();
        let repo_uid = "repo:classified:after-bulk";
        seed_classified_repo(&store, repo_uid);

        let outcome = store
            .delete_repo_cascade_with_outcome_and_faults(
                repo_uid,
                RepoCascadeFaults {
                    after_bulk: true,
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(outcome.disposition, MutationDisposition::CommittedPartial);
        assert!(outcome.confirmed_changed);
        assert_eq!(outcome.value.files_deleted, 1);
        assert_eq!(outcome.value.symbols_deleted, 1);
        assert!(outcome.primary_failure.is_some());
        assert!(store.list_files_by_repo(repo_uid).unwrap().is_empty());
        assert!(store.lookup_symbols_by_repo(repo_uid).unwrap().is_empty());
        assert!(store.lookup_repo(repo_uid).unwrap().is_some());
        assert_eq!(
            store
                .list_services(None)
                .unwrap()
                .into_iter()
                .filter(|service| service.repo_uid == repo_uid)
                .count(),
            1
        );
        assert_eq!(store.list_contracts(Some(repo_uid)).unwrap().len(), 1);
    }

    #[test]
    fn classified_repo_delete_root_failure_reports_children_absent_root_live() {
        let store = GraphStore::in_memory().unwrap();
        let repo_uid = "repo:classified:root";
        seed_classified_repo(&store, repo_uid);

        let outcome = store
            .delete_repo_cascade_with_outcome_and_faults(
                repo_uid,
                RepoCascadeFaults {
                    before_root: true,
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(outcome.disposition, MutationDisposition::CommittedPartial);
        assert!(outcome.confirmed_changed);
        assert!(store.lookup_repo(repo_uid).unwrap().is_some());
        assert!(store.list_files_by_repo(repo_uid).unwrap().is_empty());
        assert!(store.lookup_symbols_by_repo(repo_uid).unwrap().is_empty());
        assert!(store.list_contracts(Some(repo_uid)).unwrap().is_empty());
    }

    #[test]
    fn classified_repo_delete_root_only_failure_is_proved_no_change_error() {
        let store = GraphStore::in_memory().unwrap();
        let repo_uid = "repo:classified:root-only";
        store
            .insert_repo(&Repo {
                uid: repo_uid.to_string(),
                url: "https://example.invalid/root-only".to_string(),
                indexed_sha: "classified".to_string(),
                staleness_commits_behind: 0,
                instance_id: "classified".to_string(),
                name: None,
                root_path: None,
            })
            .unwrap();

        let error = store
            .delete_repo_cascade_with_outcome_and_faults(
                repo_uid,
                RepoCascadeFaults {
                    before_root: true,
                    ..Default::default()
                },
            )
            .unwrap_err();

        assert!(error.to_string().contains("before Repo root"));
        assert!(store.lookup_repo(repo_uid).unwrap().is_some());
    }

    #[test]
    fn classified_repo_delete_final_ack_failure_is_complete_with_warning() {
        let store = GraphStore::in_memory().unwrap();
        let repo_uid = "repo:classified:root-ack";
        seed_classified_repo(&store, repo_uid);

        let outcome = store
            .delete_repo_cascade_with_outcome_and_faults(
                repo_uid,
                RepoCascadeFaults {
                    root_ack_after: true,
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(outcome.disposition, MutationDisposition::CommittedComplete);
        assert!(outcome.confirmed_changed);
        assert_eq!(outcome.primary_failure, None);
        assert_eq!(outcome.mutation_warnings.len(), 1);
        assert!(store.lookup_repo(repo_uid).unwrap().is_none());
    }

    #[test]
    fn classified_repo_delete_probe_failure_is_ambiguous_after_confirmed_bulk() {
        let store = GraphStore::in_memory().unwrap();
        let repo_uid = "repo:classified:probe";
        seed_classified_repo(&store, repo_uid);

        let outcome = store
            .delete_repo_cascade_with_outcome_and_faults(
                repo_uid,
                RepoCascadeFaults {
                    after_bulk: true,
                    probe: true,
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(outcome.disposition, MutationDisposition::Ambiguous);
        assert!(outcome.confirmed_changed);
        assert!(outcome.primary_failure.is_some());
    }

    #[test]
    fn legacy_mutation_result_surfaces_partial_and_ambiguous_as_errors() {
        // The legacy `purge_instance` / `merge_instance_ids` wrappers route
        // through `legacy_mutation_result` (like `delete_vault_cascade`), so a
        // CommittedPartial or Ambiguous disposition must surface as `Err`
        // instead of being swallowed into a reported success.
        fn outcome(
            disposition: MutationDisposition,
            primary_failure: Option<MutationFailure>,
        ) -> MutationOutcome<usize> {
            MutationOutcome {
                disposition,
                confirmed_changed: true,
                value: 1,
                primary_failure,
                mutation_warnings: Vec::new(),
            }
        }

        let complete = GraphStore::legacy_mutation_result(Ok(outcome(
            MutationDisposition::CommittedComplete,
            None,
        )));
        assert_eq!(complete.unwrap(), 1);

        let no_change = GraphStore::legacy_mutation_result(Ok(outcome(
            MutationDisposition::ConfirmedNoChange,
            None,
        )));
        assert_eq!(no_change.unwrap(), 1);

        let partial = GraphStore::legacy_mutation_result(Ok(outcome(
            MutationDisposition::CommittedPartial,
            Some(MutationFailure::new("stage-partial", "partial failure")),
        )))
        .unwrap_err();
        assert!(partial.to_string().contains("partial failure"));

        let ambiguous = GraphStore::legacy_mutation_result(Ok(outcome(
            MutationDisposition::Ambiguous,
            Some(MutationFailure::new("stage-ambiguous", "ambiguous failure")),
        )))
        .unwrap_err();
        assert!(ambiguous.to_string().contains("ambiguous failure"));

        // A partial/ambiguous outcome without its primary failure is itself an
        // error, never a silent success.
        let missing_failure = GraphStore::legacy_mutation_result(Ok(outcome(
            MutationDisposition::CommittedPartial,
            None,
        )))
        .unwrap_err();
        assert!(
            missing_failure
                .to_string()
                .contains("omitted its primary failure")
        );
    }

    #[test]
    fn legacy_purge_and_merge_wrappers_succeed_on_clean_runs() {
        // Success-path behavior of the legacy wrappers is unchanged by the
        // legacy_mutation_result routing.
        let store = GraphStore::in_memory().unwrap();
        seed_purge_repo(&store, "legacy-ok", "one");

        let purged = store.purge_instance("legacy-ok").unwrap();
        assert_eq!(purged.repos, 1);

        seed_merge_repo(&store, "legacy-merge", "one");
        let merged = store
            .merge_instance_ids("legacy-merge", "legacy-target")
            .unwrap();
        assert_eq!(merged.repos, 1);
    }

    fn seed_purge_repo(store: &GraphStore, instance_id: &str, suffix: &str) -> String {
        let uid = format!("repo:{instance_id}:{suffix}");
        store
            .insert_repo(&Repo {
                uid: uid.clone(),
                url: format!("https://example.invalid/{instance_id}/{suffix}"),
                indexed_sha: "purge".to_string(),
                staleness_commits_behind: 0,
                instance_id: instance_id.to_string(),
                name: Some(suffix.to_string()),
                root_path: None,
            })
            .unwrap();
        uid
    }

    #[test]
    fn purge_instance_second_repo_failure_retains_first_repo_count() {
        let store = GraphStore::in_memory().unwrap();
        let first = seed_purge_repo(&store, "purge-repos", "a");
        let second = seed_purge_repo(&store, "purge-repos", "b");

        let outcome = store
            .purge_instance_with_outcome_and_faults(
                "purge-repos",
                PurgeInstanceFaults {
                    before_repo: Some(1),
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(outcome.disposition, MutationDisposition::CommittedPartial);
        assert!(outcome.confirmed_changed);
        assert_eq!(outcome.value.repos, 1);
        assert!(store.lookup_repo(&first).unwrap().is_none());
        assert!(store.lookup_repo(&second).unwrap().is_some());
    }

    #[test]
    fn purge_instance_vault_failure_after_repo_retains_repo_result() {
        let store = GraphStore::in_memory().unwrap();
        let repo_uid = seed_purge_repo(&store, "purge-vault", "repo");
        let vault_uid = "vlt:purge-vault:one";
        store
            .insert_vault(&Vault {
                uid: vault_uid.to_string(),
                name: "one".to_string(),
                root_path: "/purge-vault/one".to_string(),
                instance_id: "purge-vault".to_string(),
            })
            .unwrap();

        let outcome = store
            .purge_instance_with_outcome_and_faults(
                "purge-vault",
                PurgeInstanceFaults {
                    before_vault: Some(0),
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(outcome.disposition, MutationDisposition::CommittedPartial);
        assert_eq!(outcome.value.repos, 1);
        assert_eq!(outcome.value.vaults, 0);
        assert!(store.lookup_repo(&repo_uid).unwrap().is_none());
        assert!(
            store
                .list_vaults(None)
                .unwrap()
                .iter()
                .any(|vault| vault.uid == vault_uid)
        );
    }

    #[test]
    fn purge_instance_orphan_probe_failure_is_ambiguous_with_prior_exact_delta() {
        let store = GraphStore::in_memory().unwrap();
        let missing_repo = "repo:purge-orphans:missing";
        store
            .insert_symbol(&Symbol {
                uid: "sym:repo:purge-orphans:missing:one".to_string(),
                name: "one".to_string(),
                kind: nestweaver_schema::SymbolKind::Function,
                repo_uid: missing_repo.to_string(),
                file_path: "src/one.rs".to_string(),
                start_line: 1,
                end_line: 1,
                signature: "fn one()".to_string(),
                summary: None,
                content_hash: "one".to_string(),
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
        store
            .insert_file(&File {
                uid: "file:repo:purge-orphans:missing:one".to_string(),
                path: "src/one.rs".to_string(),
                repo_uid: missing_repo.to_string(),
                content_hash: "one".to_string(),
            })
            .unwrap();

        let outcome = store
            .purge_instance_with_outcome_and_faults(
                "purge-orphans",
                PurgeInstanceFaults {
                    orphan_commit_after: Some(1),
                    orphan_probe: true,
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(outcome.disposition, MutationDisposition::Ambiguous);
        assert!(outcome.confirmed_changed);
        assert_eq!(outcome.value.orphans_swept, 1);
        assert!(store.lookup_symbols_by_name("one").unwrap().is_empty());
        assert!(store.list_files_by_repo(missing_repo).unwrap().is_empty());
    }

    fn seed_merge_repo(store: &GraphStore, from: &str, suffix: &str) -> String {
        let url = format!("https://example.invalid/merge/{suffix}");
        let uid = repo_uid(from, &url);
        store
            .insert_repo(&Repo {
                uid: uid.clone(),
                url,
                indexed_sha: "merge".to_string(),
                staleness_commits_behind: 0,
                instance_id: from.to_string(),
                name: Some(suffix.to_string()),
                root_path: None,
            })
            .unwrap();
        uid
    }

    #[test]
    fn merge_instance_prepared_plan_failure_is_proved_no_change_error() {
        let store = GraphStore::in_memory().unwrap();
        seed_merge_repo(&store, "merge-prepared", "one");
        let plan = store
            .plan_instance_uid_remaps("merge-prepared", "merge-target")
            .unwrap();

        let error = store
            .merge_instance_ids_with_plan_and_faults(
                "merge-prepared",
                "merge-target",
                &plan,
                MergeInstanceFaults {
                    before_graph: true,
                    ..Default::default()
                },
            )
            .unwrap_err();

        assert!(error.to_string().contains("before graph"));
        assert_eq!(
            store
                .verify_instance_uid_remap_plan_state("merge-prepared", "merge-target", &plan)
                .unwrap(),
            InstanceUidRemapPlanState::Prepared
        );
    }

    #[test]
    fn merge_instance_failure_after_first_repo_retains_partial_result() {
        let store = GraphStore::in_memory().unwrap();
        seed_merge_repo(&store, "merge-partial", "a");
        seed_merge_repo(&store, "merge-partial", "b");
        let plan = store
            .plan_instance_uid_remaps("merge-partial", "merge-target")
            .unwrap();

        let outcome = store
            .merge_instance_ids_with_plan_and_faults(
                "merge-partial",
                "merge-target",
                &plan,
                MergeInstanceFaults {
                    after_repo: Some(1),
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(outcome.disposition, MutationDisposition::CommittedPartial);
        assert!(outcome.confirmed_changed);
        assert_eq!(outcome.value.repos, 1);
        assert!(outcome.primary_failure.is_some());
        assert_eq!(
            store
                .verify_instance_uid_remap_plan_state("merge-partial", "merge-target", &plan)
                .unwrap(),
            InstanceUidRemapPlanState::PartiallyApplied
        );
    }

    #[test]
    fn merge_instance_failure_after_graph_completion_is_complete_with_warning() {
        let store = GraphStore::in_memory().unwrap();
        seed_merge_repo(&store, "merge-applied", "a");
        seed_merge_repo(&store, "merge-applied", "b");
        let plan = store
            .plan_instance_uid_remaps("merge-applied", "merge-target")
            .unwrap();

        let outcome = store
            .merge_instance_ids_with_plan_and_faults(
                "merge-applied",
                "merge-target",
                &plan,
                MergeInstanceFaults {
                    after_graph: true,
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(outcome.disposition, MutationDisposition::CommittedComplete);
        assert_eq!(outcome.value.repos, 2);
        assert_eq!(outcome.primary_failure, None);
        assert_eq!(outcome.mutation_warnings.len(), 1);
        assert_eq!(
            store
                .verify_instance_uid_remap_plan_state("merge-applied", "merge-target", &plan)
                .unwrap(),
            InstanceUidRemapPlanState::Applied
        );
    }

    #[test]
    fn merge_instance_verification_failure_after_first_repo_is_ambiguous() {
        let store = GraphStore::in_memory().unwrap();
        seed_merge_repo(&store, "merge-ambiguous", "a");
        seed_merge_repo(&store, "merge-ambiguous", "b");
        let plan = store
            .plan_instance_uid_remaps("merge-ambiguous", "merge-target")
            .unwrap();

        let outcome = store
            .merge_instance_ids_with_plan_and_faults(
                "merge-ambiguous",
                "merge-target",
                &plan,
                MergeInstanceFaults {
                    after_repo: Some(1),
                    verify: true,
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(outcome.disposition, MutationDisposition::Ambiguous);
        assert!(outcome.confirmed_changed);
        assert_eq!(outcome.value.repos, 1);
        assert!(outcome.primary_failure.is_some());
    }

    #[test]
    fn merge_vault_collision_discard_is_complete_not_partial() {
        // Source and target vaults share a root_path; equal note counts let
        // the target win, so the source vault is intentionally discarded. The
        // discard satisfies the vault's plan remap (the predicted destination
        // is never created) — the final probe must report CommittedComplete,
        // not CommittedPartial.
        let store = GraphStore::in_memory().unwrap();
        for (uid, instance) in [
            ("vlt:merge-coll:source", "merge-coll-source"),
            ("vlt:merge-coll:target", "merge-coll-target"),
        ] {
            store
                .insert_vault(&Vault {
                    uid: uid.to_string(),
                    name: uid.to_string(),
                    root_path: "/shared/merge-coll".to_string(),
                    instance_id: instance.to_string(),
                })
                .unwrap();
            store
                .insert_note(&Note {
                    uid: format!("note:{uid}:one"),
                    vault_uid: uid.to_string(),
                    file_path: "one.md".to_string(),
                    title: "One".to_string(),
                    note_kind: nestweaver_schema::NoteKind::General,
                    word_count: 1,
                    content_hash: "one".to_string(),
                    frontmatter: None,
                    created_at: None,
                    modified_at: None,
                    pagerank_score: None,
                    embedding: None,
                })
                .unwrap();
        }

        let outcome = store
            .merge_instance_ids_with_outcome("merge-coll-source", "merge-coll-target")
            .unwrap();

        assert_eq!(
            outcome.disposition,
            MutationDisposition::CommittedComplete,
            "intentional collision discard must not read as a partial merge: {:?}",
            outcome.primary_failure
        );
        assert_eq!(outcome.value.discarded.len(), 1);
        assert_eq!(outcome.value.discarded[0].root_path, "/shared/merge-coll");
        assert_eq!(outcome.value.discarded[0].notes_discarded, 1);
        let vaults = store.list_vaults(None).unwrap();
        assert_eq!(vaults.len(), 1);
        assert_eq!(vaults[0].uid, "vlt:merge-coll:target");

        // The legacy wrapper surfaces the same clean result.
        store
            .insert_vault(&Vault {
                uid: "vlt:merge-coll:second".to_string(),
                name: "second".to_string(),
                root_path: "/shared/merge-coll".to_string(),
                instance_id: "merge-coll-source".to_string(),
            })
            .unwrap();
        let merged = store
            .merge_instance_ids("merge-coll-source", "merge-coll-target")
            .unwrap();
        assert_eq!(merged.vaults, 1);
    }

    #[test]
    fn project_cascade_classified_maps_every_existing_disposition() {
        let unchanged = DeleteProjectCascadeOutcome {
            project_uid: "proj:classified:unchanged".to_string(),
            project_name: None,
            disposition: ProjectMutationDisposition::ConfirmedUnchanged,
        };
        let unchanged =
            GraphStore::classify_project_cascade_result_with_liveness(Ok(unchanged), || Ok(true))
                .unwrap();
        assert_eq!(
            unchanged.disposition,
            MutationDisposition::ConfirmedNoChange
        );
        assert!(!unchanged.confirmed_changed);

        let changed = DeleteProjectCascadeOutcome {
            project_uid: "proj:classified:changed".to_string(),
            project_name: Some("Changed".to_string()),
            disposition: ProjectMutationDisposition::Changed,
        };
        let changed =
            GraphStore::classify_project_cascade_result_with_liveness(Ok(changed), || Ok(false))
                .unwrap();
        assert_eq!(changed.disposition, MutationDisposition::CommittedComplete);
        assert!(changed.confirmed_changed);

        for disposition in [
            ProjectMutationDisposition::ConfirmedUnchanged,
            ProjectMutationDisposition::ConfirmedRolledBack,
        ] {
            let error = DeleteProjectCascadeError {
                project_uid: "proj:classified:error".to_string(),
                project_name: None,
                disposition,
                primary: StoreError::Query("injected project failure".to_string()),
                rollback: None,
            };
            assert!(
                GraphStore::classify_project_cascade_result_with_liveness(Err(error), || Ok(true))
                    .is_err()
            );
        }

        let changed_error = DeleteProjectCascadeError {
            project_uid: "proj:classified:error-changed".to_string(),
            project_name: Some("Changed".to_string()),
            disposition: ProjectMutationDisposition::Changed,
            primary: StoreError::Query("lost acknowledgement".to_string()),
            rollback: None,
        };
        let changed_error =
            GraphStore::classify_project_cascade_result_with_liveness(Err(changed_error), || {
                Ok(false)
            })
            .unwrap();
        assert_eq!(
            changed_error.disposition,
            MutationDisposition::CommittedComplete
        );
        assert_eq!(changed_error.mutation_warnings.len(), 1);
    }

    #[test]
    fn project_cascade_classified_resolves_ambiguous_by_fresh_liveness() {
        let make_error = || DeleteProjectCascadeError {
            project_uid: "proj:classified:ambiguous".to_string(),
            project_name: Some("Ambiguous".to_string()),
            disposition: ProjectMutationDisposition::Ambiguous,
            primary: StoreError::Query("commit acknowledgement lost".to_string()),
            rollback: Some(StoreError::Query("rollback unavailable".to_string())),
        };

        let absent =
            GraphStore::classify_project_cascade_result_with_liveness(Err(make_error()), || {
                Ok(false)
            })
            .unwrap();
        assert_eq!(absent.disposition, MutationDisposition::CommittedComplete);
        assert!(absent.confirmed_changed);
        assert_eq!(absent.mutation_warnings.len(), 1);

        assert!(
            GraphStore::classify_project_cascade_result_with_liveness(Err(make_error()), || {
                Ok(true)
            })
            .is_err()
        );

        let failed_probe =
            GraphStore::classify_project_cascade_result_with_liveness(Err(make_error()), || {
                Err(StoreError::Query("probe unavailable".to_string()))
            })
            .unwrap();
        assert_eq!(failed_probe.disposition, MutationDisposition::Ambiguous);
        assert!(!failed_probe.confirmed_changed);
        assert!(failed_probe.primary_failure.is_some());
    }

    fn seed_project_component(store: &GraphStore) {
        for (uid, name) in [("proj:txn:parent", "Parent"), ("proj:txn:child", "Child")] {
            store
                .insert_project(&Project {
                    uid: uid.to_string(),
                    name: name.to_string(),
                    summary: None,
                    instance_id: "txn".to_string(),
                })
                .unwrap();
        }
        store
            .insert_project_component_edge("proj:txn:parent", "proj:txn:child", 1.0)
            .unwrap();
    }

    #[test]
    fn project_edge_delete_query_failure_before_mutation_preserves_edges() {
        let store = GraphStore::in_memory().unwrap();
        seed_project_component(&store);

        let error = store
            .delete_project_edges_with_types(
                "proj:txn:parent",
                &["NOT_A_PROJECT_EDGE", "PROJECT_HAS_COMPONENT"],
            )
            .unwrap_err();

        assert!(error.to_string().contains("NOT_A_PROJECT_EDGE"));
        assert_eq!(
            store
                .list_project_component_uids("proj:txn:parent")
                .unwrap(),
            vec!["proj:txn:child"]
        );
    }

    #[test]
    fn project_edge_delete_reports_confirmed_mutation_count() {
        let store = GraphStore::in_memory().unwrap();
        seed_project_component(&store);

        let deleted = store.delete_project_edges("proj:txn:parent").unwrap();

        assert_eq!(deleted, 1);
        assert!(
            store
                .list_project_component_uids("proj:txn:parent")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn project_edge_delete_mid_operation_failure_rolls_back() {
        let store = GraphStore::in_memory().unwrap();
        seed_project_component(&store);

        let error = store
            .delete_project_edges_with_types(
                "proj:txn:parent",
                &["PROJECT_HAS_COMPONENT", "NOT_A_PROJECT_EDGE"],
            )
            .unwrap_err();

        assert!(error.to_string().contains("NOT_A_PROJECT_EDGE"));
        assert_eq!(
            store
                .list_project_component_uids("proj:txn:parent")
                .unwrap(),
            vec!["proj:txn:child"],
            "the earlier edge delete must roll back with the failed transaction"
        );
    }

    #[test]
    fn project_cascade_fault_matrix_reports_transaction_disposition() {
        for (faults, expected, name_known) in [
            (
                ProjectCascadeFaults {
                    begin: true,
                    ..Default::default()
                },
                ProjectMutationDisposition::ConfirmedUnchanged,
                false,
            ),
            (
                ProjectCascadeFaults {
                    lookup: true,
                    ..Default::default()
                },
                ProjectMutationDisposition::ConfirmedUnchanged,
                false,
            ),
            (
                ProjectCascadeFaults {
                    before_mutation: true,
                    ..Default::default()
                },
                ProjectMutationDisposition::ConfirmedUnchanged,
                true,
            ),
            (
                ProjectCascadeFaults {
                    detach: true,
                    ..Default::default()
                },
                ProjectMutationDisposition::ConfirmedRolledBack,
                true,
            ),
            (
                ProjectCascadeFaults {
                    commit: true,
                    ..Default::default()
                },
                ProjectMutationDisposition::Ambiguous,
                true,
            ),
            (
                ProjectCascadeFaults {
                    detach: true,
                    rollback: true,
                    ..Default::default()
                },
                ProjectMutationDisposition::Ambiguous,
                true,
            ),
        ] {
            let store = GraphStore::in_memory().unwrap();
            seed_project_component(&store);

            let error = store
                .delete_project_cascade_with_faults("proj:txn:parent", faults)
                .unwrap_err();

            assert_eq!(error.project_uid, "proj:txn:parent");
            assert_eq!(error.disposition, expected, "{faults:?}");
            assert_eq!(error.project_name.is_some(), name_known, "{faults:?}");
            assert!(!error.primary.to_string().is_empty());
            if faults.rollback {
                assert!(
                    error.rollback.as_ref().is_some_and(|rollback| rollback
                        .to_string()
                        .to_ascii_lowercase()
                        .contains("injected")),
                    "rollback context missing: {error}"
                );
            }

            if matches!(
                expected,
                ProjectMutationDisposition::ConfirmedUnchanged
                    | ProjectMutationDisposition::ConfirmedRolledBack
            ) {
                assert!(
                    store
                        .list_projects()
                        .unwrap()
                        .iter()
                        .any(|project| project.uid == "proj:txn:parent"),
                    "confirmed failure removed the Project: {faults:?}"
                );
                assert_eq!(
                    store
                        .list_project_component_uids("proj:txn:parent")
                        .unwrap(),
                    vec!["proj:txn:child"],
                    "confirmed failure removed the edge: {faults:?}"
                );
            }
        }
    }

    #[test]
    fn project_cascade_lookup_identity_failures_are_not_reported_as_missing() {
        for (faults, expected_message) in [
            (
                ProjectCascadeFaults {
                    lookup_uid_mismatch: true,
                    ..Default::default()
                },
                "mismatch",
            ),
            (
                ProjectCascadeFaults {
                    lookup_uid_malformed: true,
                    ..Default::default()
                },
                "malformed",
            ),
        ] {
            let store = GraphStore::in_memory().unwrap();
            seed_project_component(&store);

            let error = store
                .delete_project_cascade_with_faults("proj:txn:parent", faults)
                .expect_err("an untrusted lookup identity must fail closed");

            assert_eq!(
                error.disposition,
                ProjectMutationDisposition::ConfirmedUnchanged
            );
            assert!(
                error
                    .primary
                    .to_string()
                    .to_ascii_lowercase()
                    .contains(expected_message),
                "unexpected error: {error}"
            );
            assert!(store.project_exists("proj:txn:parent").unwrap());
            assert_eq!(
                store
                    .list_project_component_uids("proj:txn:parent")
                    .unwrap(),
                vec!["proj:txn:child"]
            );
        }
    }

    #[test]
    fn project_cascade_non_string_name_is_optional_metadata() {
        let store = GraphStore::in_memory().unwrap();
        seed_project_component(&store);

        let outcome = store
            .delete_project_cascade_with_faults(
                "proj:txn:parent",
                ProjectCascadeFaults {
                    lookup_name_malformed: true,
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(outcome.disposition, ProjectMutationDisposition::Changed);
        assert_eq!(outcome.project_uid, "proj:txn:parent");
        assert_eq!(outcome.project_name, None);
        assert!(!store.project_exists("proj:txn:parent").unwrap());
        assert!(
            store
                .list_project_component_uids("proj:txn:parent")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn project_cascade_deletes_null_name_project_and_incident_edges() {
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_note(&Note {
                uid: "note:null-project-delete".to_string(),
                vault_uid: "vlt:null-project-delete".to_string(),
                file_path: "null-project-delete.md".to_string(),
                title: "Null project delete".to_string(),
                note_kind: nestweaver_schema::NoteKind::General,
                word_count: 3,
                content_hash: "null-project-delete-hash".to_string(),
                frontmatter: None,
                created_at: None,
                modified_at: None,
                pagerank_score: None,
                embedding: None,
            })
            .unwrap();
        let conn = store.conn().unwrap();
        conn.query(
            "CREATE (:Project {uid: 'proj:txn:null-name', name: NULL, summary: NULL, instance_id: 'txn'})",
        )
        .unwrap();
        store
            .batch_insert_project_note_edges(&[("proj:txn:null-name", "note:null-project-delete")])
            .unwrap();
        conn.query(
            "CREATE REL TABLE FUTURE_NULL_PROJECT_EDGE(FROM Project TO Note, marker STRING)",
        )
        .unwrap();
        conn.query(
            "MATCH (p:Project {uid: 'proj:txn:null-name'}), \
             (n:Note {uid: 'note:null-project-delete'}) \
             CREATE (p)-[:FUTURE_NULL_PROJECT_EDGE {marker: 'future'}]->(n)",
        )
        .unwrap();
        assert_eq!(
            store
                .list_projects()
                .unwrap()
                .into_iter()
                .find(|project| project.uid == "proj:txn:null-name")
                .unwrap()
                .name,
            ""
        );

        let outcome = store
            .delete_project_cascade_with_outcome("proj:txn:null-name")
            .unwrap();

        assert_eq!(outcome.disposition, ProjectMutationDisposition::Changed);
        assert_eq!(outcome.project_uid, "proj:txn:null-name");
        assert_eq!(outcome.project_name, None);
        assert!(!store.project_exists("proj:txn:null-name").unwrap());
        for edge_type in ["PROJECT_INCLUDES_NOTE", "FUTURE_NULL_PROJECT_EDGE"] {
            let count = conn
                .query(&format!("MATCH ()-[r:{edge_type}]->() RETURN r"))
                .unwrap()
                .count();
            assert_eq!(count, 0, "{edge_type} survived the Project delete");
        }
    }

    #[test]
    fn missing_project_commit_failure_is_confirmed_unchanged() {
        let store = GraphStore::in_memory().unwrap();

        let error = store
            .delete_project_cascade_with_faults(
                "proj:txn:missing",
                ProjectCascadeFaults {
                    commit: true,
                    ..Default::default()
                },
            )
            .unwrap_err();

        assert_eq!(
            error.disposition,
            ProjectMutationDisposition::ConfirmedUnchanged
        );
        assert_eq!(error.project_uid, "proj:txn:missing");
        assert_eq!(error.project_name, None);
    }

    #[test]
    fn embedding_metadata_round_trips_including_special_chars() {
        let dir = tempfile::tempdir().unwrap();
        let store = GraphStore::create(&dir.path().join("emb_meta.lbug")).unwrap();

        // Absent by default.
        assert_eq!(store.get_embedding_metadata().unwrap(), None);

        // Normal HuggingFace id round-trips, and the singleton is replaced on re-set.
        store
            .set_embedding_metadata("thenlper/gte-base", 768)
            .unwrap();
        assert_eq!(
            store.get_embedding_metadata().unwrap(),
            Some(("thenlper/gte-base".to_string(), 768))
        );

        // A model_id containing quotes/backslashes (e.g. a local model path) must still
        // round-trip. Naive JSON string interpolation would produce invalid JSON here, which
        // get_embedding_metadata would fail to parse → the daemon would fall back to the
        // default model and silently disable semantic search on a dimension mismatch.
        let weird = r#"/models/my "local"\model"#;
        store.set_embedding_metadata(weird, 384).unwrap();
        assert_eq!(
            store.get_embedding_metadata().unwrap(),
            Some((weird.to_string(), 384))
        );
    }

    #[test]
    fn test_update_repo_sha_is_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test_sha.lbug");
        let store = GraphStore::create(&db_path).unwrap();

        let repo = nestweaver_schema::Repo {
            uid: "repo:test".to_string(),
            url: "file:///tmp/test".to_string(),
            indexed_sha: "aaa".to_string(),
            staleness_commits_behind: 0,
            instance_id: "default".to_string(),
            name: Some("test-repo".to_string()),
            root_path: None,
        };
        store.insert_repo(&repo).unwrap();

        {
            let conn = store.conn().unwrap();
            conn.query(
                "CREATE (:File {uid: 'file:1', path: 'src/main.rs', \
                 repo_uid: 'repo:test', content_hash: 'hash1'})",
            )
            .unwrap();
            conn.query(
                "MATCH (r:Repo {uid: 'repo:test'}), (f:File {uid: 'file:1'}) \
                 CREATE (r)-[:REPO_HAS_FILE]->(f)",
            )
            .unwrap();
        }

        store.update_repo_sha("repo:test", "bbb").unwrap();

        let repos = store.list_repos(None).unwrap();
        let found = repos.iter().find(|r| r.uid == "repo:test").unwrap();
        assert_eq!(found.indexed_sha, "bbb");
        assert_eq!(found.url, "file:///tmp/test");
        assert_eq!(found.name, Some("test-repo".to_string()));

        let conn = store.conn().unwrap();
        let rows: Vec<_> = conn
            .query("MATCH (f:File {uid: 'file:1'}) RETURN f.uid")
            .unwrap()
            .collect();
        assert_eq!(rows.len(), 1);
    }
}
