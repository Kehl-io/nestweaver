use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::content_reader::ContentReader;

pub(crate) const MANIFEST_ARTIFACT_KIND: &str = "repo_manifest";
pub(crate) const MANIFEST_ARTIFACT_SCHEMA_VERSION: u32 = 2;
pub(crate) const MANIFEST_ALGORITHM_FINGERPRINT: &str = "nestweaver-repo-manifest-v2";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ManifestInfo {
    pub package_name: Option<String>,
    pub dependencies: Vec<String>,
    /// Repo-relative file paths declared as package entry points by every
    /// `package.json` found in the repo at any depth (root or nested;
    /// `node_modules` and the other shared skip-dirs are excluded, and
    /// `.gitignore` is respected — see [`discover_package_json_entry_files`]).
    /// Extracted from `main`, `bin` (string or object), `exports` (a string,
    /// an array fallback list, or a nested condition/subpath map — every
    /// string target at any depth is collected), and `browser` only when its
    /// value is a string (the object form is a bundler replacement map, not
    /// an entry point). Each raw path is rebased onto the directory
    /// containing its own manifest and normalized to a repo-relative,
    /// `/`-joined path (`.`/`..` resolved); an entry is dropped when it is
    /// empty/whitespace-only, contains a `*` pattern, is absolute (`/...`),
    /// contains `\` or a `://` scheme, ends in `/` (a directory reference),
    /// or would escape the repo root — see
    /// [`rebase_package_json_entry`] for the exact rules. A non-JSON manifest
    /// format (e.g. CMake) may also contribute its own entries here. These
    /// are entry points for the package(s) and their symbols should not be
    /// flagged as dead code.
    #[serde(default)]
    pub entry_files: Vec<String>,
}

/// Parse the manifest file(s) found in `repo_path` and return extracted
/// package name and dependency list. The first recognized manifest format
/// wins for `package_name`/`dependencies`.
///
/// `entry_files` is independent of that first-format-wins choice: every
/// `package.json` in the repo, at any depth and including the root, is
/// discovered and unioned in (see [`discover_package_json_entry_files`]), so
/// a root manifest of a different format (e.g. `Cargo.toml`) does not hide a
/// nested `package.json`'s entry points, and a root `package.json` missing a
/// `name` field still contributes its entries even though it cannot win the
/// name/dependencies choice above.
pub fn parse_manifest(reader: &dyn ContentReader) -> ManifestInfo {
    let mut info = parse_package_json(reader)
        .or_else(|| parse_go_mod(reader))
        .or_else(|| parse_cargo_toml(reader))
        .or_else(|| parse_pyproject_toml(reader))
        .or_else(|| parse_requirements_txt(reader))
        .or_else(|| parse_composer_json(reader))
        .or_else(|| parse_gemfile(reader))
        .or_else(|| parse_pubspec_yaml(reader))
        .or_else(|| parse_package_swift(reader))
        .or_else(|| parse_csproj(reader))
        .or_else(|| parse_build_gradle_kts(reader))
        .or_else(|| parse_cmake(reader))
        .unwrap_or_default();

    for entry in discover_package_json_entry_files(reader) {
        if !info.entry_files.contains(&entry) {
            info.entry_files.push(entry);
        }
    }
    info
}

/// Persist a `HashMap<repo_uid, ManifestInfo>` as a JSON sidecar file.
pub fn save_manifest_cache(
    manifests: &HashMap<String, ManifestInfo>,
    path: &Path,
) -> Result<(), anyhow::Error> {
    let json = serde_json::to_string(manifests)?;
    atomic_replace_file(path, |file| file.write_all(json.as_bytes()))
}

/// Canonical manifest sidecar path for a database.
pub fn manifest_cache_path(db_path: &Path) -> PathBuf {
    crate::sidecar_path(db_path, ".manifests.json")
}

/// Load the canonical manifest sidecar, migrating the legacy replacement-
/// extension path when it is the only copy present.
pub fn load_manifest_cache_for_db(
    store: &nestweaver_store::GraphStore,
    db_path: &Path,
) -> Result<HashMap<String, ManifestInfo>, anyhow::Error> {
    Ok(crate::artifact_sidecar::load_json(
        store,
        &manifest_cache_path(db_path),
        MANIFEST_ARTIFACT_KIND,
        MANIFEST_ARTIFACT_SCHEMA_VERSION,
        MANIFEST_ALGORITHM_FINGERPRINT,
    )?
    .unwrap_or_default())
}

/// Publication status for a graph mutation that has already committed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphMutationPublicationDisposition {
    /// The store proved that no graph state changed.
    ConfirmedNoChange,
    /// The mutation and every required derived-artifact reconciliation step
    /// completed.
    CommittedComplete,
    /// The graph mutation committed, but one or more reconciliation steps
    /// failed and require operator repair.
    CommittedDegraded,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphMutationPublicationWarning {
    pub stage: String,
    pub message: String,
}

/// Complete publication result for a graph mutation.
///
/// A committed graph write is never turned back into a plain `Err` merely
/// because generation persistence or sidecar reconciliation subsequently
/// failed. Callers can therefore report the truthful committed-but-degraded
/// state instead of implying that the graph rolled back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphMutationPublicationOutcome {
    pub disposition: GraphMutationPublicationDisposition,
    pub generation_before: u64,
    pub generation_after: u64,
    pub warnings: Vec<GraphMutationPublicationWarning>,
}

/// Crash fence for a non-index graph mutation.
///
/// Unlike a full index publication this guard does not reserve dirty N+1 and
/// clean N+2 generations: the materialization contract requires exactly one
/// successor generation. The durable marker still brackets the graph commit,
/// so interruption before post-commit reconciliation leaves every ranked read
/// and snapshot fail-closed until normal abandoned-publication repair runs.
pub struct GraphMutationPublicationGuard<'a> {
    lease: Option<nestweaver_store::IndexPublicationLease<'a>>,
    marker_path: Option<PathBuf>,
    operation: String,
}

impl<'a> GraphMutationPublicationGuard<'a> {
    /// Finish a bracketed operation. A confirmed no-op retires the marker
    /// without advancing generation; a commit publishes exactly once.
    pub fn finish(
        mut self,
        changed: bool,
    ) -> Result<GraphMutationPublicationOutcome, anyhow::Error> {
        let store = self
            .lease
            .as_ref()
            .expect("publication guard always owns its lease until finish")
            .store();
        let mut outcome = finalize_committed_graph_mutation(store, changed);

        if !outcome.is_degraded()
            && let Some(marker_path) = &self.marker_path
            && let Err(error) = store.with_index_publication_rank_barrier(|| {
                nestweaver_store::durable_sidecar::remove_file_durable_if_exists(marker_path)
            })
        {
            if changed {
                outcome.record_warning(
                    "retire-graph-publication-marker",
                    format!(
                        "{} committed clean state but could not retire {}: {error}",
                        self.operation,
                        marker_path.display()
                    ),
                );
            } else {
                anyhow::bail!(
                    "{} made no graph change but could not retire publication marker {}: {error}",
                    self.operation,
                    marker_path.display()
                );
            }
        }

        if let Some(lease) = self.lease.take()
            && let Err(error) = lease.release()
        {
            if changed {
                outcome.record_warning(
                    "release-graph-publication-lease",
                    format!(
                        "{} committed but could not release its publication lease: {error}",
                        self.operation
                    ),
                );
            } else {
                return Err(anyhow::anyhow!(
                    "{} made no graph change but could not release its publication lease: {error}",
                    self.operation
                ));
            }
        }
        Ok(outcome)
    }
}

/// Durably mark a graph publication dirty before its first possible write.
///
/// The returned guard must span the graph commit and post-commit finalizer.
/// Dropping it early intentionally releases only live ownership; the durable
/// marker remains so a crash or ambiguous write cannot make old derived data
/// authoritative.
pub fn begin_graph_mutation_publication<'a>(
    store: &'a nestweaver_store::GraphStore,
    operation: impl Into<String>,
) -> Result<GraphMutationPublicationGuard<'a>, anyhow::Error> {
    let operation = operation.into();
    let lease = store
        .acquire_index_publication_lease()
        .map_err(|error| anyhow::anyhow!("{operation}: acquire publication lease: {error}"))?;
    lease.ensure_clean_for_snapshot().map_err(|error| {
        anyhow::anyhow!("{operation}: refusing to overwrite a dirty publication: {error}")
    })?;

    let marker_path = if let Some(db_path) = store.db_path() {
        lease.preflight_generation().map_err(|error| {
            anyhow::anyhow!("{operation}: preflight successor generation: {error}")
        })?;
        let marker_path = crate::sidecar_path(db_path, ".index-dirty");
        let payload = nestweaver_store::index_publication::format_marker_payload(
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            None,
        );
        store
            .with_index_publication_rank_barrier(|| {
                nestweaver_store::durable_sidecar::atomic_replace_file(&marker_path, |file| {
                    file.write_all(payload.as_bytes())
                })
            })
            .map_err(|error| {
                anyhow::anyhow!(
                    "{operation}: publish graph mutation marker {}: {error}",
                    marker_path.display()
                )
            })?;
        Some(marker_path)
    } else {
        lease.preflight_transient_generation().map_err(|error| {
            anyhow::anyhow!("{operation}: preflight in-memory successor generation: {error}")
        })?;
        None
    };

    Ok(GraphMutationPublicationGuard {
        lease: Some(lease),
        marker_path,
        operation,
    })
}

impl GraphMutationPublicationOutcome {
    pub fn changed(&self) -> bool {
        self.disposition != GraphMutationPublicationDisposition::ConfirmedNoChange
    }

    pub fn is_degraded(&self) -> bool {
        self.disposition == GraphMutationPublicationDisposition::CommittedDegraded
    }

    pub fn record_warning(&mut self, stage: impl Into<String>, message: impl Into<String>) {
        self.warnings.push(GraphMutationPublicationWarning {
            stage: stage.into(),
            message: message.into(),
        });
        if self.disposition == GraphMutationPublicationDisposition::CommittedComplete {
            self.disposition = GraphMutationPublicationDisposition::CommittedDegraded;
        }
    }
}

/// Publish one already-committed graph mutation.
///
/// A confirmed no-op leaves every generation and derived artifact untouched.
/// A change invalidates both live and durable PageRank, advances generation
/// exactly once, persists it, and rebinds a valid repository-manifest cache to
/// the successor generation. Post-commit failures are accumulated instead of
/// being returned as rollback-looking errors.
pub fn finalize_committed_graph_mutation(
    store: &nestweaver_store::GraphStore,
    changed: bool,
) -> GraphMutationPublicationOutcome {
    let generation_before = store.graph_generation();
    if !changed {
        return GraphMutationPublicationOutcome {
            disposition: GraphMutationPublicationDisposition::ConfirmedNoChange,
            generation_before,
            generation_after: generation_before,
            warnings: Vec::new(),
        };
    }

    let db_path = store.db_path().map(Path::to_path_buf);
    let mut outcome = GraphMutationPublicationOutcome {
        disposition: GraphMutationPublicationDisposition::CommittedComplete,
        generation_before,
        generation_after: generation_before,
        warnings: Vec::new(),
    };
    let carried_manifests = db_path.as_ref().and_then(|db_path| {
        let path = manifest_cache_path(db_path);
        if !path.exists() {
            return None;
        }
        match load_manifest_cache_for_db(store, db_path) {
            Ok(manifests) => Some(manifests),
            Err(error) => {
                // nw-459. The manifest cache is REBUILDABLE derived data. An
                // upgrade invalidates it by construction (the artifact records
                // the producing version), and the next index regenerates it.
                // Recording that as a publication WARNING flipped the
                // disposition to `CommittedDegraded`, which the daemon and CLI
                // both surface as a hard error — so the first index after every
                // upgrade exited non-zero on a graph that had committed
                // perfectly, telling the user to "repair the named stage(s)"
                // for a stage that needed no repair.
                //
                // A genuine incompatibility (different algorithm, different
                // schema, foreign identity, corrupt payload) is NOT rebuildable
                // and still degrades the publication loudly.
                let rendered = format!("{error:#}");
                if nestweaver_store::artifact_envelope::is_rebuildable_artifact(&rendered) {
                    tracing::debug!(
                        error = %rendered,
                        "dropping a rebuildable manifest cache; the next index regenerates it"
                    );
                } else {
                    outcome.record_warning(
                        "load-manifest-cache",
                        format!(
                            "could not carry the repository manifest cache across publication: {rendered}"
                        ),
                    );
                }
                None
            }
        }
    });

    if let Err(error) = store.reconcile_embedding_index() {
        outcome.record_warning(
            "embedding-index",
            format!("graph committed; embedding reconciliation must be retried: {error:#}"),
        );
    }

    // Clear live scores before exposing the new generation. The durable copy
    // is removed as well, so a process restart cannot reload pre-mutation
    // ranking even if a later publication step is degraded.
    store.invalidate_pagerank();
    if let Some(db_path) = &db_path {
        let pagerank_path = crate::sidecar_path(db_path, ".pagerank.json");
        if let Err(error) =
            nestweaver_store::durable_sidecar::remove_file_durable_if_exists(&pagerank_path)
        {
            outcome.record_warning(
                "invalidate-pagerank-sidecar",
                format!(
                    "could not durably remove stale PageRank sidecar {}: {error}",
                    pagerank_path.display()
                ),
            );
        }
    }

    let generation_after = match store.try_bump_graph_generation() {
        Ok(generation) => generation,
        Err(error) => {
            outcome.record_warning(
                "advance-graph-generation",
                format!("committed graph mutation could not advance generation: {error}"),
            );
            outcome.generation_after = store.graph_generation();
            return outcome;
        }
    };
    outcome.generation_after = generation_after;

    let generation_persisted = if let Some(db_path) = &db_path {
        let generation_path = crate::sidecar_path(db_path, ".generation");
        match store.save_graph_generation(&generation_path) {
            Ok(()) => true,
            Err(error) => {
                outcome.record_warning(
                    "persist-graph-generation",
                    format!(
                        "generation {generation_after} is live but could not be durably published to {}: {error}",
                        generation_path.display()
                    ),
                );
                false
            }
        }
    } else {
        true
    };

    if generation_persisted
        && let (Some(db_path), Some(manifests)) = (&db_path, carried_manifests)
        && let Err(error) = save_manifest_cache_for_db(&manifests, store, db_path)
    {
        outcome.record_warning(
            "rebind-manifest-cache",
            format!(
                "could not rebind the repository manifest cache to generation {generation_after}: {error:#}"
            ),
        );
    }

    for warning in &outcome.warnings {
        tracing::warn!(
            stage = %warning.stage,
            message = %warning.message,
            generation_before,
            generation_after = outcome.generation_after,
            "graph mutation committed with degraded publication reconciliation"
        );
    }
    outcome
}

/// Advance the graph generation while carrying the manifest cache across the
/// boundary.
///
/// nw-289, deeper property. `.manifests.json` is identity- AND
/// generation-bound: its envelope records `source_graph_generation`, and
/// [`load_manifest_cache_for_db`] refuses to decode it once that no longer
/// matches the live graph. Only the DELETION path reconciled that; every other
/// generation advance left the artifact orphaned.
///
/// Measured before the fix: a code index leaves graph and manifest cache both
/// at generation 2; a subsequent `brain add` moves the graph to 3, the
/// manifest cache stays at 2, and from then on
/// `load_manifest_cache_for_db` fails with "stale artifact generation" until a
/// code index happens to run. Every CLI consumer loads it with
/// `.unwrap_or_default()`, so the graph does not report an error — `dead-code`
/// quietly loses its manifest-driven entry points and `suggest-links` its
/// cross-repo signal.
///
/// REPUBLISH, not invalidate. A markdown index cannot change a code manifest,
/// so the payload is still correct and only its binding went stale; deleting
/// the artifact would force a full code re-index to recover data that was
/// never wrong. The deletion path is the one case whose payload genuinely must
/// be FILTERED first, and it still does that separately.
///
/// Taking the advance as a closure is the point: the read must happen before
/// it and the write after, and the deletion path's own comment records what
/// the other ordering costs — "saving at N and then advancing to N+1 makes a
/// freshly written, identity-bound artifact stale immediately". A helper that
/// only did the write could be called in the wrong place; this one cannot be.
///
/// Read failures are ignored by design. An absent cache is the normal state
/// for a graph with no code repos, and an ALREADY-stale or corrupt one must
/// not be re-blessed against a generation it was never derived from — both
/// simply skip the republish. A WRITE failure is logged rather than returned,
/// because the graph mutation is already committed and the caller cannot undo
/// it; the artifact is left stale, which is the pre-existing behaviour.
pub(crate) fn advancing_generation_rebinding_manifests<T>(
    store: &nestweaver_store::GraphStore,
    advance: impl FnOnce() -> T,
) -> T {
    let carried = store.db_path().and_then(|db_path| {
        load_manifest_cache_for_db(store, db_path)
            .ok()
            .filter(|manifests| !manifests.is_empty())
            .map(|manifests| (db_path.to_path_buf(), manifests))
    });
    let outcome = advance();
    if let Some((db_path, manifests)) = carried
        && let Err(error) = save_manifest_cache_for_db(&manifests, store, &db_path)
    {
        tracing::warn!(
            "could not rebind the manifest cache to the published generation: {error:#};              it stays bound to the previous one and a code re-index will restore it"
        );
    }
    outcome
}

/// Persist the canonical manifest sidecar and retire the legacy copy only
/// after the replacement has been durably flushed and renamed into place.
pub fn save_manifest_cache_for_db(
    manifests: &HashMap<String, ManifestInfo>,
    store: &nestweaver_store::GraphStore,
    db_path: &Path,
) -> Result<(), anyhow::Error> {
    let canonical_path = manifest_cache_path(db_path);
    crate::artifact_sidecar::save_json(
        store,
        &canonical_path,
        MANIFEST_ARTIFACT_KIND,
        MANIFEST_ARTIFACT_SCHEMA_VERSION,
        MANIFEST_ALGORITHM_FINGERPRINT,
        manifests,
    )?;

    let legacy_path = db_path.with_extension("manifests.json");
    if legacy_path != canonical_path {
        nestweaver_store::durable_sidecar::remove_file_durable_if_exists(&legacy_path).map_err(
            |error| {
                anyhow::anyhow!(
                    "durably remove legacy manifest sidecar {}: {error}",
                    legacy_path.display()
                )
            },
        )?;
    }
    Ok(())
}

pub(crate) fn atomic_replace_file(
    path: &Path,
    write: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> Result<(), anyhow::Error> {
    nestweaver_store::durable_sidecar::atomic_replace_file(path, write).map_err(Into::into)
}

/// Load a `HashMap<repo_uid, ManifestInfo>` from a JSON sidecar file.
///
/// Returns an empty map when the file does not exist.
pub fn load_manifest_cache(path: &Path) -> Result<HashMap<String, ManifestInfo>, anyhow::Error> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let json = std::fs::read_to_string(path)?;
    let map = serde_json::from_str(&json)?;
    Ok(map)
}

// ── per-format parsers ────────────────────────────────────────────────────────

fn parse_package_json(reader: &dyn ContentReader) -> Option<ManifestInfo> {
    let content = reader.read_file(Path::new("package.json")).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;

    let package_name = json.get("name")?.as_str().map(String::from);

    let mut deps = Vec::new();
    for field in ["dependencies", "devDependencies", "peerDependencies"] {
        if let Some(obj) = json.get(field).and_then(|v| v.as_object()) {
            deps.extend(obj.keys().cloned());
        }
    }

    // `entry_files` is populated separately by `discover_package_json_entry_files`,
    // which walks every package.json in the repo (root included) independent of
    // whether it has a `name` — see that function's doc comment.
    Some(ManifestInfo {
        package_name,
        dependencies: deps,
        entry_files: Vec::new(),
    })
}

/// Recursively collect string values from the `exports` field of package.json.
/// The `exports` field can be a string, an array (Node's documented fallback
/// list — the first entry a resolver understands wins at runtime, but every
/// entry is a candidate entry point, and over-rooting a symbol is the safe
/// direction for a deletion aid), an object with condition keys mapping to
/// strings/arrays/nested objects, or an object with subpath keys.
fn collect_export_paths(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Object(obj) => {
            for v in obj.values() {
                collect_export_paths(v, out);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                collect_export_paths(v, out);
            }
        }
        _ => {}
    }
}

/// Extract the raw (as-written, not yet rebased) entry-point strings from one
/// parsed `package.json` document: `main`, `bin` (string or object), `exports`
/// (string or nested condition/subpath map), and `browser` only when it is a
/// string — the object form is a bundler replacement map, not a documented
/// npm/Node entry point.
fn collect_package_json_raw_entries(json: &serde_json::Value) -> Vec<String> {
    let mut entries = Vec::new();
    if let Some(main) = json.get("main").and_then(|v| v.as_str()) {
        entries.push(main.to_string());
    }
    if let Some(bin) = json.get("bin") {
        match bin {
            serde_json::Value::String(s) => entries.push(s.clone()),
            serde_json::Value::Object(obj) => {
                for v in obj.values() {
                    if let Some(s) = v.as_str() {
                        entries.push(s.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(exports) = json.get("exports") {
        collect_export_paths(exports, &mut entries);
    }
    if let Some(browser) = json.get("browser").and_then(|v| v.as_str()) {
        entries.push(browser.to_string());
    }
    entries
}

/// Rebase one raw `package.json` entry path onto the repo-root-relative
/// directory containing that manifest, producing a `/`-joined, lexically
/// normalized path (`.` dropped, `..` resolved against what came before it).
///
/// Returns `None` for anything that cannot be a literal repo-relative file
/// path:
/// - empty or whitespace-only (on a nested manifest this would otherwise
///   rebase to the manifest's own directory, which is not a file);
/// - contains a `*` (a subpath-pattern target, not a literal file —
///   expanding it is out of scope);
/// - starts with `/` (an absolute filesystem path, never repo-relative —
///   rebasing it under the manifest's directory would be silently wrong,
///   not merely inert);
/// - contains `\` or a `://` scheme (a Windows-style path or a URL; neither
///   is ever a repo-relative path here — Windows paths are unsupported);
/// - ends with `/` (a directory reference, e.g. `"lib/"`; npm resolves a
///   directory `main`/`exports` target via its own `index.js`/`package.json`
///   lookup, which this function does not implement — filed as follow-up
///   nw-499, not silently mis-rooted as the literal directory name);
/// - a leading `..` that would walk outside the repo root (nothing left to
///   pop).
fn rebase_package_json_entry(manifest_dir: &Path, entry: &str) -> Option<String> {
    let entry = entry.trim();
    if entry.is_empty()
        || entry.contains('*')
        || entry.starts_with('/')
        || entry.contains('\\')
        || entry.contains("://")
        || entry.ends_with('/')
    {
        return None;
    }
    let mut segments: Vec<&str> = manifest_dir
        .iter()
        .filter_map(|c| c.to_str())
        .filter(|s| !s.is_empty())
        .collect();
    for part in entry.split('/') {
        match part {
            "" | "." => continue,
            ".." => {
                segments.pop()?;
            }
            other => segments.push(other),
        }
    }
    if segments.is_empty() {
        return None;
    }
    Some(segments.join("/"))
}

/// Discover every `package.json` in the repo, at any depth and including the
/// root, and return the union of their rebased, repo-relative entry-point
/// paths.
///
/// Reuses `reader.list_files()`, the same repo-relative file listing the
/// index's own file walk uses, so `node_modules` and the other shared
/// skip-dirs, plus `.gitignore`/`.git/info/exclude`, are already applied —
/// no separate exclusion logic is needed here. The root is included
/// deliberately (not skipped as "already covered" by [`parse_package_json`]):
/// a root `package.json` without a `name` field is invisible to
/// `parse_package_json`, and this is the only place its entries are
/// recovered.
fn discover_package_json_entry_files(reader: &dyn ContentReader) -> Vec<String> {
    let Ok(files) = reader.list_files() else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for path in files {
        if path.file_name().and_then(|n| n.to_str()) != Some("package.json") {
            continue;
        }
        let Ok(content) = reader.read_file(&path) else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) else {
            continue;
        };
        let manifest_dir = path.parent().unwrap_or(Path::new(""));
        for raw in collect_package_json_raw_entries(&json) {
            if let Some(rebased) = rebase_package_json_entry(manifest_dir, &raw)
                && !entries.contains(&rebased)
            {
                entries.push(rebased);
            }
        }
    }
    entries
}

fn parse_go_mod(reader: &dyn ContentReader) -> Option<ManifestInfo> {
    let content = reader.read_file(Path::new("go.mod")).ok()?;

    let mut package_name = None;
    let mut deps = Vec::new();
    let mut in_require = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("module ") {
            package_name = Some(trimmed.strip_prefix("module ")?.trim().to_string());
        } else if trimmed == "require (" {
            in_require = true;
        } else if trimmed == ")" {
            in_require = false;
        } else if in_require && !trimmed.is_empty() && !trimmed.starts_with("//") {
            // "github.com/pkg/errors v0.9.1" or "… // indirect"
            let module_path = trimmed.split_whitespace().next()?;
            deps.push(module_path.to_string());
        } else if trimmed.starts_with("require ") && !trimmed.contains('(') {
            // Single-line require: "require github.com/pkg/errors v0.9.1"
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            if parts.len() >= 2 {
                deps.push(parts[1].to_string());
            }
        }
    }

    Some(ManifestInfo {
        package_name,
        dependencies: deps,
        entry_files: vec![],
    })
}

fn parse_cargo_toml(reader: &dyn ContentReader) -> Option<ManifestInfo> {
    let content = reader.read_file(Path::new("Cargo.toml")).ok()?;
    let toml: toml::Value = toml::from_str(&content).ok()?;

    let package_name = toml.get("package")?.get("name")?.as_str().map(String::from);

    let mut deps = Vec::new();
    for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(table) = toml.get(section).and_then(|v| v.as_table()) {
            deps.extend(table.keys().cloned());
        }
    }

    Some(ManifestInfo {
        package_name,
        dependencies: deps,
        entry_files: vec![],
    })
}

fn parse_pyproject_toml(reader: &dyn ContentReader) -> Option<ManifestInfo> {
    let content = reader.read_file(Path::new("pyproject.toml")).ok()?;
    let toml: toml::Value = toml::from_str(&content).ok()?;

    let package_name = toml.get("project")?.get("name")?.as_str().map(String::from);

    let mut deps = Vec::new();
    if let Some(dep_list) = toml
        .get("project")
        .and_then(|p| p.get("dependencies"))
        .and_then(|d| d.as_array())
    {
        for dep in dep_list {
            if let Some(s) = dep.as_str() {
                // PEP 508: "package-name>=1.0" — extract name before version specifier
                let name = s
                    .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_' && c != '.')
                    .next()
                    .unwrap_or(s);
                deps.push(name.to_string());
            }
        }
    }

    Some(ManifestInfo {
        package_name,
        dependencies: deps,
        entry_files: vec![],
    })
}

fn parse_requirements_txt(reader: &dyn ContentReader) -> Option<ManifestInfo> {
    let content = reader.read_file(Path::new("requirements.txt")).ok()?;

    let mut deps = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('-') {
            continue;
        }
        let name = trimmed
            .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_' && c != '.')
            .next()
            .unwrap_or(trimmed);
        if !name.is_empty() {
            deps.push(name.to_string());
        }
    }

    if deps.is_empty() {
        return None;
    }
    Some(ManifestInfo {
        package_name: None,
        dependencies: deps,
        entry_files: vec![],
    })
}

fn parse_composer_json(reader: &dyn ContentReader) -> Option<ManifestInfo> {
    let content = reader.read_file(Path::new("composer.json")).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;

    let package_name = json.get("name").and_then(|v| v.as_str()).map(String::from);

    let mut deps = Vec::new();
    for field in ["require", "require-dev"] {
        if let Some(obj) = json.get(field).and_then(|v| v.as_object()) {
            for key in obj.keys() {
                if key != "php" && !key.starts_with("ext-") {
                    deps.push(key.clone());
                }
            }
        }
    }

    Some(ManifestInfo {
        package_name,
        dependencies: deps,
        entry_files: vec![],
    })
}

fn parse_gemfile(reader: &dyn ContentReader) -> Option<ManifestInfo> {
    let content = reader.read_file(Path::new("Gemfile")).ok()?;

    let mut deps = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(after) = trimmed
            .strip_prefix("gem ")
            .or_else(|| trimmed.strip_prefix("gem("))
        {
            let name = after
                .trim_start_matches(['\'', '"'])
                .split(['\'', '"'])
                .next()
                .unwrap_or("");
            if !name.is_empty() {
                deps.push(name.to_string());
            }
        }
    }

    if deps.is_empty() {
        return None;
    }
    Some(ManifestInfo {
        package_name: None,
        dependencies: deps,
        entry_files: vec![],
    })
}

fn parse_pubspec_yaml(reader: &dyn ContentReader) -> Option<ManifestInfo> {
    let content = reader.read_file(Path::new("pubspec.yaml")).ok()?;
    let yaml: serde_yaml::Value = serde_yaml::from_str(&content).ok()?;

    let package_name = yaml.get("name").and_then(|v| v.as_str()).map(String::from);

    let mut deps = Vec::new();
    for field in ["dependencies", "dev_dependencies"] {
        if let Some(mapping) = yaml.get(field).and_then(|v| v.as_mapping()) {
            for key in mapping.keys() {
                if let Some(name) = key
                    .as_str()
                    .filter(|n| *n != "flutter" && *n != "flutter_test")
                {
                    deps.push(name.to_string());
                }
            }
        }
    }

    Some(ManifestInfo {
        package_name,
        dependencies: deps,
        entry_files: vec![],
    })
}

fn parse_package_swift(reader: &dyn ContentReader) -> Option<ManifestInfo> {
    let content = reader.read_file(Path::new("Package.swift")).ok()?;

    let package_name = content.lines().find_map(|line| {
        let trimmed = line.trim();
        if let Some(after) = trimmed.strip_prefix("name:") {
            let name = after
                .trim()
                .trim_matches(|c: char| c == '"' || c == ',' || c == ' ');
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
        None
    });

    let mut deps = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.contains(".package(url:")
            && let Some(url_start) = trimmed.find("url:")
        {
            let after_url = &trimmed[url_start + 4..];
            let url = after_url
                .trim()
                .trim_start_matches([' ', '"'])
                .split('"')
                .next()
                .unwrap_or("");
            if let Some(last_segment) = url.rsplit('/').next() {
                let name = last_segment.trim_end_matches(".git");
                if !name.is_empty() {
                    deps.push(name.to_string());
                }
            }
        }
    }

    Some(ManifestInfo {
        package_name,
        dependencies: deps,
        entry_files: vec![],
    })
}

/// Find the first `.csproj` file in the repo root or one level of
/// subdirectories, using the reader's file listing instead of `read_dir`.
fn find_csproj(reader: &dyn ContentReader) -> Option<std::path::PathBuf> {
    let files = reader.list_files().ok()?;
    // Prefer root-level csproj files, then one-level subdirectory files.
    let mut root_level: Option<std::path::PathBuf> = None;
    let mut subdir_level: Option<std::path::PathBuf> = None;
    for f in &files {
        if f.extension().is_some_and(|ext| ext == "csproj") {
            let depth = f.components().count();
            if depth == 1 && root_level.is_none() {
                root_level = Some(f.clone());
            } else if depth == 2 && subdir_level.is_none() {
                subdir_level = Some(f.clone());
            }
        }
    }
    root_level.or(subdir_level)
}

fn parse_csproj(reader: &dyn ContentReader) -> Option<ManifestInfo> {
    let csproj_rel = find_csproj(reader)?;
    let content = reader.read_file(&csproj_rel).ok()?;

    let mut deps = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.contains("PackageReference")
            && let Some(start) = trimmed.find("Include=\"")
        {
            let after = &trimmed[start + 9..];
            if let Some(end) = after.find('"') {
                let name = &after[..end];
                if !name.is_empty() {
                    deps.push(name.to_string());
                }
            }
        }
    }

    if deps.is_empty() {
        return None;
    }
    Some(ManifestInfo {
        package_name: None,
        dependencies: deps,
        entry_files: vec![],
    })
}

fn parse_build_gradle_kts(reader: &dyn ContentReader) -> Option<ManifestInfo> {
    let content = reader.read_file(Path::new("build.gradle.kts")).ok()?;

    let mut deps = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        for prefix in [
            "implementation(\"",
            "api(\"",
            "testImplementation(\"",
            "runtimeOnly(\"",
            "compileOnly(\"",
            "implementation('",
            "api('",
            "testImplementation('",
            "runtimeOnly('",
            "compileOnly('",
        ] {
            if let Some(after) = trimmed.strip_prefix(prefix) {
                let dep_str = after.split(['"', '\'']).next().unwrap_or("");
                let parts: Vec<&str> = dep_str.split(':').collect();
                if parts.len() >= 2 {
                    deps.push(format!("{}:{}", parts[0], parts[1]));
                }
            }
        }
    }

    if deps.is_empty() {
        return None;
    }
    Some(ManifestInfo {
        package_name: None,
        dependencies: deps,
        entry_files: vec![],
    })
}

fn parse_cmake(reader: &dyn ContentReader) -> Option<ManifestInfo> {
    let content = reader.read_file(Path::new("CMakeLists.txt")).ok()?;

    let mut package_name = None;
    let mut deps = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(after) = trimmed.strip_prefix("project(") {
            let name = after.split([')', ' ']).next().unwrap_or("");
            if !name.is_empty() {
                package_name = Some(name.to_string());
            }
        }
        if let Some(after) = trimmed.strip_prefix("find_package(") {
            let name = after.split([')', ' ']).next().unwrap_or("");
            if !name.is_empty() {
                deps.push(name.to_string());
            }
        }
    }

    Some(ManifestInfo {
        package_name,
        dependencies: deps,
        entry_files: cmake_executable_sources(&content),
    })
}

/// Collect the source files named by every `add_executable(...)` in a
/// CMakeLists.txt.
///
/// nw-351: `parse_cmake` returned `entry_files: vec![]` while `Cargo.toml` and
/// `package.json` both contribute entry files, so a CMake project fed NOTHING
/// to `dead_code`'s manifest-driven seeding (`dead_code.rs:388`). Combined with
/// `detect_cpp` recognising only `main` and a handful of test macros, that is
/// how a real C++ corpus reached zero entry points and was reported 100% dead.
///
/// `add_executable` and not `add_library`, deliberately: a library's surface is
/// called from outside the corpus and is not modelled as an entry point today,
/// while an executable's sources genuinely are the program's roots.
///
/// The scan is over the whole file rather than line-by-line because the call is
/// conventionally wrapped across lines. Tokens that are CMake keywords, that
/// carry a `$` (an unresolved variable or generator expression), or that have
/// no file extension are dropped; the first token is the target NAME, never a
/// source.
fn cmake_executable_sources(content: &str) -> Vec<String> {
    const KEYWORDS: [&str; 4] = ["WIN32", "MACOSX_BUNDLE", "EXCLUDE_FROM_ALL", "IMPORTED"];
    let mut out = Vec::new();
    let mut rest = content;
    while let Some(at) = rest.find("add_executable(") {
        rest = &rest[at + "add_executable(".len()..];
        let Some(close) = rest.find(')') else { break };
        let (args, tail) = rest.split_at(close);
        rest = tail;
        for (index, token) in args.split_whitespace().enumerate() {
            let token = token.trim_matches('"');
            // The first token is the target name.
            if index == 0 || token.is_empty() {
                continue;
            }
            if KEYWORDS.contains(&token) || token.contains('$') {
                continue;
            }
            if Path::new(token)
                .extension()
                .is_none_or(|ext| ext.is_empty())
            {
                continue;
            }
            let token = token.to_string();
            if !out.contains(&token) {
                out.push(token);
            }
        }
    }
    out
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content_reader::FilesystemReader;

    /// nw-351: a CMake project contributed no entry files at all, while
    /// `Cargo.toml` and `package.json` both do — so `dead_code`'s
    /// manifest-driven seeding had nothing to work with on any C++ corpus.
    #[test]
    fn parse_cmake_extracts_executable_sources_as_entry_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("CMakeLists.txt"),
            "project(demo)\n\
             find_package(Threads REQUIRED)\n\
             add_executable(demo_cli\n\
             \x20   src/main.cpp\n\
             \x20   src/cli.cpp)\n\
             add_executable(tool WIN32 tools/tool.cpp ${GENERATED_SRC})\n\
             add_library(demo_core src/core.cpp)\n",
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert_eq!(info.package_name.as_deref(), Some("demo"));
        assert_eq!(
            info.entry_files,
            vec![
                "src/main.cpp".to_string(),
                "src/cli.cpp".to_string(),
                "tools/tool.cpp".to_string(),
            ],
            "the target NAME, CMake keywords, unresolved `${{...}}` variables \
             and every add_library source must all stay out"
        );
    }

    #[test]
    fn parse_package_json_extracts_name_and_deps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{
                "name": "@myorg/api-client",
                "dependencies": { "axios": "^1.0.0", "@myorg/shared-types": "^2.0.0" },
                "devDependencies": { "jest": "^29.0.0" }
            }"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert_eq!(info.package_name.as_deref(), Some("@myorg/api-client"));
        assert!(info.dependencies.contains(&"axios".to_string()));
        assert!(
            info.dependencies
                .contains(&"@myorg/shared-types".to_string())
        );
        assert!(info.dependencies.contains(&"jest".to_string()));
    }

    #[test]
    fn parse_go_mod_extracts_module_and_requires() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("go.mod"),
            "module github.com/myorg/service\n\ngo 1.21\n\nrequire (\n\tgithub.com/myorg/shared v1.0.0\n\tgithub.com/pkg/errors v0.9.1\n)\n",
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert_eq!(
            info.package_name.as_deref(),
            Some("github.com/myorg/service")
        );
        assert!(
            info.dependencies
                .contains(&"github.com/myorg/shared".to_string())
        );
    }

    #[test]
    fn parse_cargo_toml_extracts_name_and_deps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            r#"
[package]
name = "my-crate"
version = "0.1.0"

[dependencies]
serde = "1.0"
my-shared = { path = "../shared" }
"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert_eq!(info.package_name.as_deref(), Some("my-crate"));
        assert!(info.dependencies.contains(&"serde".to_string()));
        assert!(info.dependencies.contains(&"my-shared".to_string()));
    }

    #[test]
    fn parse_returns_default_for_no_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(info.package_name.is_none());
        assert!(info.dependencies.is_empty());
    }

    #[test]
    fn parse_pyproject_extracts_name_and_deps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pyproject.toml"),
            r#"
[project]
name = "myservice"
dependencies = ["requests>=2.28", "pydantic>=2.0"]
"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert_eq!(info.package_name.as_deref(), Some("myservice"));
        assert!(info.dependencies.contains(&"requests".to_string()));
        assert!(info.dependencies.contains(&"pydantic".to_string()));
    }

    #[test]
    fn parse_requirements_txt_extracts_deps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("requirements.txt"),
            "# comment\nrequests==2.28.0\npydantic>=2.0\n-r other.txt\n",
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(info.package_name.is_none());
        assert!(info.dependencies.contains(&"requests".to_string()));
        assert!(info.dependencies.contains(&"pydantic".to_string()));
    }

    #[test]
    fn save_and_load_manifest_cache_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("test.manifests.json");

        let mut cache = HashMap::new();
        cache.insert(
            "r1".to_string(),
            ManifestInfo {
                package_name: Some("my-pkg".to_string()),
                dependencies: vec!["dep-a".to_string()],
                entry_files: vec![],
            },
        );
        save_manifest_cache(&cache, &cache_path).unwrap();

        let loaded = load_manifest_cache(&cache_path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded["r1"].package_name.as_deref(), Some("my-pkg"));
        assert!(loaded["r1"].dependencies.contains(&"dep-a".to_string()));
    }

    #[test]
    fn save_manifest_cache_replaces_the_sidecar_inode() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("brain.lbug.manifests.json");
        let old_link = dir.path().join("old-manifests.json");
        let old = HashMap::from([(
            "repo:old".to_string(),
            ManifestInfo {
                package_name: Some("old-package".to_string()),
                dependencies: Vec::new(),
                entry_files: Vec::new(),
            },
        )]);
        save_manifest_cache(&old, &cache_path).unwrap();
        std::fs::hard_link(&cache_path, &old_link).unwrap();

        let new = HashMap::from([(
            "repo:new".to_string(),
            ManifestInfo {
                package_name: Some("new-package".to_string()),
                dependencies: Vec::new(),
                entry_files: Vec::new(),
            },
        )]);
        save_manifest_cache(&new, &cache_path).unwrap();

        assert_eq!(load_manifest_cache(&cache_path).unwrap().len(), 1);
        assert!(
            load_manifest_cache(&cache_path)
                .unwrap()
                .contains_key("repo:new")
        );
        assert_eq!(load_manifest_cache(&old_link).unwrap().len(), 1);
        assert!(
            load_manifest_cache(&old_link)
                .unwrap()
                .contains_key("repo:old")
        );
    }

    #[test]
    fn manifest_atomic_replace_cleans_partial_temp_after_write_error() {
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("brain.lbug.manifests.json");
        std::fs::write(&cache_path, b"previous-valid-sidecar").unwrap();

        let error = atomic_replace_file(&cache_path, |file| {
            file.write_all(b"partial replacement")?;
            Err(std::io::Error::other("injected write failure"))
        })
        .unwrap_err();

        assert!(error.to_string().contains("injected write failure"));
        assert_eq!(
            std::fs::read(&cache_path).unwrap(),
            b"previous-valid-sidecar"
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn save_manifest_cache_for_db_retires_legacy_only_after_canonical_save() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("brain.lbug");
        let store = nestweaver_store::GraphStore::create(&db_path).unwrap();
        let legacy_path = db_path.with_extension("manifests.json");
        std::fs::write(&legacy_path, r#"{"repo:legacy":{}}"#).unwrap();
        let manifests = HashMap::from([("repo:canonical".to_string(), ManifestInfo::default())]);

        save_manifest_cache_for_db(&manifests, &store, &db_path).unwrap();

        assert!(!legacy_path.exists());
        assert!(
            load_manifest_cache_for_db(&store, &db_path)
                .unwrap()
                .contains_key("repo:canonical")
        );

        std::fs::write(&legacy_path, r#"{"repo:still-safe":{}}"#).unwrap();
        std::fs::remove_file(manifest_cache_path(&db_path)).unwrap();
        std::fs::create_dir(manifest_cache_path(&db_path)).unwrap();
        let error = save_manifest_cache_for_db(&manifests, &store, &db_path).unwrap_err();
        assert!(!error.to_string().is_empty());
        assert!(legacy_path.exists(), "failed canonical save removed legacy");
    }

    #[test]
    fn canonical_manifest_cache_rejects_legacy_foreign_and_stale_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let source_db = dir.path().join("source.lbug");
        let source = nestweaver_store::GraphStore::create(&source_db).unwrap();
        let manifests = HashMap::from([("repo:source".to_string(), ManifestInfo::default())]);
        save_manifest_cache_for_db(&manifests, &source, &source_db).unwrap();
        let source_path = manifest_cache_path(&source_db);
        let valid = std::fs::read(&source_path).unwrap();

        std::fs::write(&source_path, r#"{"repo:legacy":{}}"#).unwrap();
        let legacy = load_manifest_cache_for_db(&source, &source_db).unwrap_err();
        assert!(
            legacy.to_string().contains("run a full reindex"),
            "{legacy}"
        );

        std::fs::write(&source_path, &valid).unwrap();
        source.bump_graph_generation();
        let stale = load_manifest_cache_for_db(&source, &source_db).unwrap_err();
        assert!(
            stale.to_string().contains("stale artifact generation"),
            "{stale}"
        );

        let foreign_db = dir.path().join("foreign.lbug");
        let foreign = nestweaver_store::GraphStore::create(&foreign_db).unwrap();
        let foreign_path = manifest_cache_path(&foreign_db);
        std::fs::write(&foreign_path, valid).unwrap();
        let foreign_error = load_manifest_cache_for_db(&foreign, &foreign_db).unwrap_err();
        assert!(
            foreign_error
                .to_string()
                .contains("foreign artifact identity"),
            "{foreign_error}"
        );
    }

    #[test]
    fn committed_graph_publication_advances_once_and_reconciles_derived_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("brain.lbug");
        let store = nestweaver_store::GraphStore::create(&db_path).unwrap();
        let manifests = HashMap::from([("repo:stable".to_string(), ManifestInfo::default())]);
        save_manifest_cache_for_db(&manifests, &store, &db_path).unwrap();
        store
            .compute_pagerank(
                0.85,
                20,
                &nestweaver_store::ranking::GraphScope::code_only(),
            )
            .unwrap();
        let pagerank_path = crate::sidecar_path(&db_path, ".pagerank.json");
        store.save_pagerank_cache(&pagerank_path).unwrap();

        let unchanged = finalize_committed_graph_mutation(&store, false);
        assert_eq!(
            unchanged.disposition,
            GraphMutationPublicationDisposition::ConfirmedNoChange
        );
        assert_eq!(unchanged.generation_before, unchanged.generation_after);
        assert!(pagerank_path.exists());

        let changed = finalize_committed_graph_mutation(&store, true);
        assert_eq!(
            changed.disposition,
            GraphMutationPublicationDisposition::CommittedComplete
        );
        assert_eq!(changed.generation_after, changed.generation_before + 1);
        assert_eq!(store.graph_generation(), changed.generation_after);
        assert_eq!(
            std::fs::read_to_string(crate::sidecar_path(&db_path, ".generation"))
                .unwrap()
                .parse::<u64>()
                .unwrap(),
            changed.generation_after
        );
        assert!(!pagerank_path.exists());
        assert!(
            load_manifest_cache_for_db(&store, &db_path)
                .unwrap()
                .contains_key("repo:stable")
        );

        let repeated_noop = finalize_committed_graph_mutation(&store, false);
        assert_eq!(repeated_noop.generation_after, changed.generation_after);
    }

    #[test]
    fn committed_graph_publication_reports_reconciliation_degradation() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("brain.lbug");
        let store = nestweaver_store::GraphStore::create(&db_path).unwrap();
        let pagerank_path = crate::sidecar_path(&db_path, ".pagerank.json");
        std::fs::create_dir(&pagerank_path).unwrap();

        let publication =
            begin_graph_mutation_publication(&store, "injected reconciliation failure").unwrap();
        let outcome = publication.finish(true).unwrap();

        assert_eq!(
            outcome.disposition,
            GraphMutationPublicationDisposition::CommittedDegraded
        );
        assert_eq!(outcome.generation_after, outcome.generation_before + 1);
        assert!(
            outcome
                .warnings
                .iter()
                .any(|warning| warning.stage == "invalidate-pagerank-sidecar")
        );
        assert!(
            crate::sidecar_path(&db_path, ".index-dirty").exists(),
            "degraded reconciliation must retain the crash fence"
        );

        let backup_config = crate::backup::BackupConfig {
            db_path: db_path.clone(),
            output_path: dir.path().join("backup.nwsnap.zst"),
            include_clones: false,
            instance_id: "test".to_string(),
            workspace_path: None,
        };
        let backup_error = crate::backup::stage_backup_from_store(&store, &backup_config)
            .err()
            .expect("backup must fail closed on committed-but-degraded publication")
            .to_string();
        assert!(
            backup_error.contains("dirty index publication"),
            "{backup_error}"
        );

        let snapshot_dir = dir.path().join("snapshot");
        let snapshot_stamp = crate::snapshot::Stamp {
            format_version: 0,
            capabilities: Vec::new(),
            instance_id: "test".to_string(),
            brain_uuid: String::new(),
            publication_uuid: String::new(),
            engine_version: env!("CARGO_PKG_VERSION").to_string(),
            min_compatible_engine: crate::snapshot::MIN_SNAPSHOT_READER_VERSION.to_string(),
            schema_hash_core: nestweaver_schema::core_schema_hash(),
            schema_hash_extensions: "none".to_string(),
            schema_hash_effective: "schema".to_string(),
            embedding_model_id: "model".to_string(),
            embedding_dimension: 0,
            embedding_count: 0,
            built_at: "2026-09-02T00:00:00Z".to_string(),
            repos: Vec::new(),
        };
        let snapshot_manifest = crate::snapshot::Manifest { repos: Vec::new() };
        let snapshot_error = crate::snapshot::build_snapshot_from_store(
            &snapshot_dir,
            &snapshot_stamp,
            &snapshot_manifest,
            &store,
        )
        .expect_err("snapshot must fail closed on committed-but-degraded publication")
        .to_string();
        assert!(
            snapshot_error.contains("dirty index publication"),
            "{snapshot_error}"
        );
    }

    #[test]
    fn interrupted_bracketed_mutation_retains_fail_closed_marker() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("brain.lbug");
        let store = nestweaver_store::GraphStore::create(&db_path).unwrap();
        let publication = begin_graph_mutation_publication(&store, "interrupted mutation").unwrap();
        store
            .insert_project(&nestweaver_schema::Project {
                uid: "proj:test:interrupted".to_string(),
                name: "interrupted".to_string(),
                summary: None,
                instance_id: "test".to_string(),
            })
            .unwrap();

        // Model interruption after the graph commit but before `finish`.
        drop(publication);

        assert!(crate::sidecar_path(&db_path, ".index-dirty").exists());
        assert_eq!(store.graph_generation(), 0);
        let error = store
            .ensure_pagerank_loaded()
            .expect_err("ranked reads must fail closed across the interruption window")
            .to_string();
        assert!(error.contains("dirty index publication"), "{error}");
    }

    #[test]
    fn load_manifest_cache_returns_empty_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("nonexistent.json");
        let loaded = load_manifest_cache(&cache_path).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn parse_composer_json_extracts_name_and_deps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("composer.json"),
            r#"{"name":"myorg/api","require":{"laravel/framework":"^10.0"},"require-dev":{"phpunit/phpunit":"^10.0"}}"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert_eq!(info.package_name.as_deref(), Some("myorg/api"));
        assert!(info.dependencies.contains(&"laravel/framework".to_string()));
        assert!(info.dependencies.contains(&"phpunit/phpunit".to_string()));
    }

    #[test]
    fn parse_gemfile_extracts_deps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Gemfile"),
            "source 'https://rubygems.org'\n\ngem 'rails', '~> 7.0'\ngem 'pg'\n",
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(info.dependencies.contains(&"rails".to_string()));
        assert!(info.dependencies.contains(&"pg".to_string()));
    }

    #[test]
    fn parse_pubspec_yaml_extracts_name_and_deps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pubspec.yaml"),
            "name: my_app\ndependencies:\n  http: ^0.13.0\n",
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert_eq!(info.package_name.as_deref(), Some("my_app"));
        assert!(info.dependencies.contains(&"http".to_string()));
    }

    #[test]
    fn parse_package_swift_extracts_deps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Package.swift"),
            "import PackageDescription\nlet package = Package(\n    name: \"MyPkg\",\n    dependencies: [\n        .package(url: \"https://github.com/apple/swift-argument-parser.git\", from: \"1.0.0\"),\n    ]\n)\n",
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert_eq!(info.package_name.as_deref(), Some("MyPkg"));
        assert!(
            info.dependencies
                .contains(&"swift-argument-parser".to_string())
        );
    }

    #[test]
    fn parse_csproj_extracts_deps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("MyApp.csproj"),
            "<Project Sdk=\"Microsoft.NET.Sdk\">\n  <ItemGroup>\n    <PackageReference Include=\"Newtonsoft.Json\" Version=\"13.0.1\" />\n  </ItemGroup>\n</Project>",
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(info.dependencies.contains(&"Newtonsoft.Json".to_string()));
    }

    #[test]
    fn parse_build_gradle_kts_extracts_deps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("build.gradle.kts"),
            "dependencies {\n    implementation(\"org.springframework.boot:spring-boot-starter-web:3.1.0\")\n}\n",
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(
            info.dependencies
                .contains(&"org.springframework.boot:spring-boot-starter-web".to_string())
        );
    }

    #[test]
    fn parse_cmake_extracts_name_and_deps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("CMakeLists.txt"),
            "cmake_minimum_required(VERSION 3.20)\nproject(MyApp)\nfind_package(Boost REQUIRED)\nfind_package(OpenSSL REQUIRED)\n",
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert_eq!(info.package_name.as_deref(), Some("MyApp"));
        assert!(info.dependencies.contains(&"Boost".to_string()));
        assert!(info.dependencies.contains(&"OpenSSL".to_string()));
    }

    #[test]
    fn parse_package_json_extracts_entry_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{
                "name": "my-lib",
                "main": "./dist/index.js",
                "bin": {
                    "cli": "./bin/cli.js"
                },
                "exports": {
                    ".": {
                        "import": "./dist/esm/index.js",
                        "require": "./dist/cjs/index.js"
                    },
                    "./utils": "./dist/utils.js"
                },
                "dependencies": {}
            }"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        // Root-relative entries are rebased/normalized like every other
        // package.json's, so the leading `./` is dropped (parent() of the
        // root "package.json" is "", which is an identity join).
        assert!(info.entry_files.contains(&"dist/index.js".to_string()));
        assert!(info.entry_files.contains(&"bin/cli.js".to_string()));
        assert!(info.entry_files.contains(&"dist/esm/index.js".to_string()));
        assert!(info.entry_files.contains(&"dist/cjs/index.js".to_string()));
        assert!(info.entry_files.contains(&"dist/utils.js".to_string()));
    }

    #[test]
    fn parse_package_json_bin_as_string() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{
                "name": "my-cli",
                "bin": "./bin/main.js",
                "dependencies": {}
            }"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(info.entry_files.contains(&"bin/main.js".to_string()));
    }

    #[test]
    fn parse_package_json_no_entry_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{
                "name": "simple-pkg",
                "dependencies": { "lodash": "^4.0.0" }
            }"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(info.entry_files.is_empty());
    }

    // ── nw-492 (Task 2.7A): nested package.json entry-point discovery ──────

    /// The witness shape: a repo whose root manifest is NOT package.json
    /// (mirrors this very repo's own root Cargo.toml) with a nested
    /// wasm-bindgen-style glue package deeper in the tree. `main` must come
    /// back rebased onto the nested manifest's own directory, not the bare
    /// literal string package.json wrote.
    #[test]
    fn nested_package_json_main_is_rebased_onto_its_own_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"host-crate\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("crates/x/pkg")).unwrap();
        std::fs::write(
            dir.path().join("crates/x/pkg/package.json"),
            r#"{"name": "glue", "main": "./a.js"}"#,
        )
        .unwrap();

        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert_eq!(info.package_name.as_deref(), Some("host-crate"));
        assert!(
            info.entry_files.contains(&"crates/x/pkg/a.js".to_string()),
            "{:?}",
            info.entry_files
        );
        assert!(
            !info.entry_files.contains(&"a.js".to_string()),
            "the unrebased literal must not appear: {:?}",
            info.entry_files
        );
    }

    #[test]
    fn root_package_json_with_name_yields_identical_entries_to_nameless_root() {
        let named_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            named_dir.path().join("package.json"),
            r#"{"name": "has-a-name", "main": "./index.js"}"#,
        )
        .unwrap();
        let named = parse_manifest(&FilesystemReader::new(named_dir.path()));

        let nameless_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            nameless_dir.path().join("package.json"),
            r#"{"main": "./index.js"}"#,
        )
        .unwrap();
        let nameless = parse_manifest(&FilesystemReader::new(nameless_dir.path()));

        assert_eq!(named.package_name.as_deref(), Some("has-a-name"));
        assert_eq!(nameless.package_name, None);
        assert_eq!(
            named.entry_files, nameless.entry_files,
            "entry-file discovery must not depend on the `name` field"
        );
        assert_eq!(named.entry_files, vec!["index.js".to_string()]);
    }

    #[test]
    fn nameless_root_package_json_still_contributes_entry_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"main": "./index.js", "bin": "./cli.js"}"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(info.package_name.is_none());
        assert!(info.entry_files.contains(&"index.js".to_string()));
        assert!(info.entry_files.contains(&"cli.js".to_string()));
    }

    #[test]
    fn exports_string_form_is_collected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name": "x", "exports": "./index.js"}"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(info.entry_files.contains(&"index.js".to_string()));
    }

    #[test]
    fn exports_conditions_map_is_collected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{
                "name": "x",
                "exports": {
                    ".": { "import": "./esm.js", "require": "./cjs.js" }
                }
            }"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(info.entry_files.contains(&"esm.js".to_string()));
        assert!(info.entry_files.contains(&"cjs.js".to_string()));
    }

    #[test]
    fn browser_string_is_included_but_object_form_is_ignored() {
        let string_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            string_dir.path().join("package.json"),
            r#"{"name": "x", "browser": "./web.js"}"#,
        )
        .unwrap();
        let string_info = parse_manifest(&FilesystemReader::new(string_dir.path()));
        assert!(string_info.entry_files.contains(&"web.js".to_string()));

        let object_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            object_dir.path().join("package.json"),
            r#"{"name": "x", "browser": {"./server.js": "./client.js"}}"#,
        )
        .unwrap();
        let object_info = parse_manifest(&FilesystemReader::new(object_dir.path()));
        assert!(
            !object_info.entry_files.contains(&"client.js".to_string()),
            "{:?}",
            object_info.entry_files
        );
        assert!(
            !object_info.entry_files.contains(&"server.js".to_string()),
            "{:?}",
            object_info.entry_files
        );
    }

    #[test]
    fn rebase_entry_normalizes_dot_dot_within_the_repo() {
        // "crates/x/pkg/package.json" declaring "../shared/util.js" should
        // land at "crates/x/shared/util.js" — one `..` pops the manifest's
        // own directory, not the repo root.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"host\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("crates/x/pkg")).unwrap();
        std::fs::write(
            dir.path().join("crates/x/pkg/package.json"),
            r#"{"name": "glue", "main": "../shared/util.js"}"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(
            info.entry_files
                .contains(&"crates/x/shared/util.js".to_string()),
            "{:?}",
            info.entry_files
        );
    }

    #[test]
    fn rebase_entry_dropped_when_dot_dot_escapes_the_repo_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"host\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("crates/x")).unwrap();
        std::fs::write(
            dir.path().join("crates/x/package.json"),
            r#"{"name": "glue", "main": "../../../etc/passwd"}"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(
            info.entry_files.is_empty(),
            "an entry that walks above the repo root must be dropped: {:?}",
            info.entry_files
        );
    }

    #[test]
    fn star_pattern_export_target_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{
                "name": "x",
                "main": "./index.js",
                "exports": { "./*": "./src/*.js" }
            }"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(info.entry_files.contains(&"index.js".to_string()));
        assert!(
            !info.entry_files.iter().any(|e| e.contains('*')),
            "{:?}",
            info.entry_files
        );
    }

    #[test]
    fn node_modules_package_json_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"host\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("node_modules/some-dep")).unwrap();
        std::fs::write(
            dir.path().join("node_modules/some-dep/package.json"),
            r#"{"name": "some-dep", "main": "./index.js"}"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(
            info.entry_files.is_empty(),
            "a node_modules package.json must not contribute entry files: {:?}",
            info.entry_files
        );
    }

    /// Counterweight: `suggest_links` (`suggest.rs`) reads only
    /// `package_name`/`dependencies` off `ManifestInfo`. This pins that a
    /// root `Cargo.toml`'s name/dependencies are unaffected by entry-file
    /// discovery, and that no package.json anywhere means no entry files.
    #[test]
    fn root_cargo_toml_keeps_its_name_and_dependencies_with_no_entry_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"my-crate\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = \"1.0\"\n",
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert_eq!(info.package_name.as_deref(), Some("my-crate"));
        assert!(info.dependencies.contains(&"serde".to_string()));
        assert!(info.entry_files.is_empty());
    }

    /// Counterweight: a root `package.json`'s `package_name`/`dependencies`
    /// are unaffected by folding entry-file discovery into `parse_manifest`.
    #[test]
    fn root_package_json_keeps_its_name_and_dependencies() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{
                "name": "@myorg/api-client",
                "dependencies": { "axios": "^1.0.0" }
            }"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert_eq!(info.package_name.as_deref(), Some("@myorg/api-client"));
        assert!(info.dependencies.contains(&"axios".to_string()));
    }

    // ── 2.7A review follow-up: absolute/array/directory/empty/backslash ────

    /// IMPORTANT (review): an absolute path must never be silently rebased
    /// under a nested manifest's own directory — that would produce a
    /// plausible-looking but wrong repo-relative path instead of being
    /// dropped.
    #[test]
    fn absolute_entry_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"host\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("crates/x")).unwrap();
        std::fs::write(
            dir.path().join("crates/x/package.json"),
            r#"{"name": "glue", "main": "/dist/index.js"}"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(
            info.entry_files.is_empty(),
            "an absolute path must be dropped, not rebased under the \
             manifest's directory: {:?}",
            info.entry_files
        );
    }

    /// `exports` may be Node's documented array fallback list; every string
    /// target in it is a candidate entry point.
    #[test]
    fn exports_array_form_is_collected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name": "x", "exports": ["./modern.js", "./legacy.js"]}"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(info.entry_files.contains(&"modern.js".to_string()));
        assert!(info.entry_files.contains(&"legacy.js".to_string()));
    }

    #[test]
    fn trailing_slash_directory_entry_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name": "x", "main": "lib/"}"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(
            info.entry_files.is_empty(),
            "a directory-main target must not become the inert literal \
             \"lib\" — npm's directory-main resolution is unimplemented \
             (nw-499), so it must be dropped, not mis-rooted: {:?}",
            info.entry_files
        );
    }

    #[test]
    fn empty_string_entry_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"host\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("crates/x")).unwrap();
        std::fs::write(
            dir.path().join("crates/x/package.json"),
            r#"{"name": "glue", "main": "   "}"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(
            info.entry_files.is_empty(),
            "a blank entry must not rebase to the manifest's own directory: {:?}",
            info.entry_files
        );
    }

    #[test]
    fn backslash_and_url_entries_are_dropped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{
                "name": "x",
                "main": "./ok.js",
                "bin": {
                    "win": "src\\win.js",
                    "remote": "https://example.com/cli.js"
                }
            }"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(info.entry_files.contains(&"ok.js".to_string()));
        assert!(
            !info
                .entry_files
                .iter()
                .any(|e| e.contains('\\') || e.contains("example.com")),
            "{:?}",
            info.entry_files
        );
    }
}

#[cfg(test)]
mod hardening_embedding_recovery_tests {
    use super::*;
    #[test]
    fn failed_embedding_reconciliation_keeps_durable_recovery_fence() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        let store = nestweaver_store::GraphStore::create(&db).unwrap();
        store.set_embedding_metadata("fixture", 2).unwrap();
        // A vector whose graph node was deleted: occupancy alone calls it live.
        assert!(store.add_embedding("sym:deleted", vec![1.0, 0.0]));
        store.flush_embedding_index().unwrap();
        assert_eq!(store.embedding_index_occupancy().tombstoned, 0);
        let journal = crate::sidecar_path(&db, ".embeddings.journal");
        std::fs::create_dir(&journal).unwrap();
        let guard = begin_graph_mutation_publication(&store, "post-delete regression").unwrap();
        let outcome = guard.finish(true).unwrap();
        assert!(outcome.is_degraded());
        assert!(
            outcome
                .warnings
                .iter()
                .any(|w| w.stage == "embedding-index")
        );
        assert!(crate::sidecar_path(&db, ".index-dirty").exists());
        drop(store);
        // The durable fence survives reopening before repair; ranked queries
        // cannot treat the old artifact as a clean publication.
        if let Ok(unrepaired) = nestweaver_store::GraphStore::open_read_only(&db) {
            assert!(unrepaired.is_index_publication_dirty());
        }
        std::fs::remove_dir(&journal).unwrap();
        let authority = nestweaver_store::acquire_db_write_lease(&db).unwrap();
        let store = nestweaver_store::GraphStore::open_with_authority(&db, &authority).unwrap();
        crate::index::force_recover_index_publication(&store, &authority).unwrap();
        assert!(!store.has_embedding("sym:deleted"));
        assert!(!crate::sidecar_path(&db, ".index-dirty").exists());
        drop(store);
        drop(authority);
        let reopened = nestweaver_store::GraphStore::open_read_only(&db).unwrap();
        assert!(!reopened.has_embedding("sym:deleted"));
    }
}
