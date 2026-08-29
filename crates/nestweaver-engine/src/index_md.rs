//! Markdown indexing pipeline — the walking-skeleton sibling of `index.rs`.
//!
//! Walks a vault directory, parses each `.md` file with the markdown parser,
//! and persists flat `Note` nodes alongside a single `Vault` node. No
//! headings, sections, wikilinks, or PPR integration yet — those land in
//! later phases.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::content_reader::ContentReader;
use anyhow::Context;
use globset::GlobSet;
use indicatif::{ProgressBar, ProgressStyle};
use nestweaver_parser::{
    ParsedNote, RawTag, RawWikilink, SkipReasonCode, SkippedFile, TagSource, is_markdown,
    parse_markdown,
};
use nestweaver_schema::{
    EdgeType, Heading, Note, Repo, ResolvedEdge, Section, Tag, Vault, heading_uid, note_uid,
    repo_uid, section_uid, tag_uid, vault_uid,
};
use nestweaver_store::GraphStore;
// walkdir replaced by ContentReader::list_files() — only sidecar/taxonomy paths
// still use direct fs access.

/// Outcome of a markdown index run.
pub struct MarkdownIndexResult {
    pub vault_uid: String,
    pub vault_name: String,
    pub notes_count: usize,
    pub headings_count: usize,
    pub sections_count: usize,
    pub tags_count: usize,
    pub wikilinks_resolved: usize,
    pub wikilinks_unresolved: usize,
    pub skipped: Vec<SkippedFile>,
}

/// Full-refresh outcome with the committed cascade count. Kept separate from
/// [`MarkdownIndexResult`] so existing callers that construct or destructure
/// the stable index result are not broken by an added public field.
pub struct MarkdownRefreshResult {
    pub index: MarkdownIndexResult,
    pub notes_deleted: usize,
}

/// Canonical full-refresh summary shared by direct CLI and daemon progress.
pub fn format_markdown_refresh_summary(result: &MarkdownRefreshResult) -> String {
    let mut summary = format!(
        "Refreshed vault '{}': dropped {} stale note(s), reindexed {} note(s), \
         {} heading(s), {} section(s), {} tag(s), {} wikilink(s) ({} unresolved).",
        result.index.vault_name,
        result.notes_deleted,
        result.index.notes_count,
        result.index.headings_count,
        result.index.sections_count,
        result.index.tags_count,
        result.index.wikilinks_resolved,
        result.index.wikilinks_unresolved,
    );
    if !result.index.skipped.is_empty() {
        summary.push_str(&format!(
            " Coverage DEGRADED: {} note file(s) skipped.",
            result.index.skipped.len()
        ));
    }
    summary
}

/// Directory names skipped when walking a vault. Includes `.obsidian` (config),
/// `.trash` (Obsidian's recycle bin), and common synthetic dirs.
const SKIP_DIRS: &[&str] = &[
    ".obsidian",
    ".trash",
    ".git",
    "node_modules",
    "target",
    ".next",
    ".nuxt",
    "dist",
    "build",
];

/// Returns true if any component of `rel_path` matches one of the vault
/// `SKIP_DIRS`. Used to post-filter results from `ContentReader::list_files()`
/// which may not know about vault-specific skip directories (e.g. `.obsidian`,
/// `.trash`).
fn path_has_vault_skip_dir(rel_path: &Path) -> bool {
    for component in rel_path.components() {
        if let std::path::Component::Normal(name) = component
            && let Some(s) = name.to_str()
            && SKIP_DIRS.contains(&s)
        {
            return true;
        }
    }
    false
}

/// Cap on per-file size to avoid pathological inputs (e.g. multi-MB log dumps
/// pasted into a note). Files above this size are skipped with a warning.
/// Per-file cap on note size. Files larger than this are skipped with a
/// logged warning. Architecture doc §9.7 specifies 1 MB; multi-MB markdown
/// is almost always machine-generated (pasted logs, exported data dumps)
/// and parsing them takes seconds while tanking ranking quality.
pub(crate) const MAX_NOTE_SIZE_BYTES: u64 = 1024 * 1024; // 1 MiB

fn note_reader_limits() -> crate::index_limits::IndexLimits {
    crate::index_limits::IndexLimits::new(MAX_NOTE_SIZE_BYTES)
        .expect("note size policy is within source-reader safety bounds")
}

/// Index a markdown vault into a persistent `GraphStore` at `db_path`.
///
/// After indexing, if a taxonomy file (`_taxonomy.md`, `taxonomy.md`, or
/// `_brain/taxonomy.md`) is found in the vault, its alias mappings are parsed
/// and saved to `<db_path>.aliases.json` for use by seed resolution at query time.
///
/// `extra_ignore_patterns` are additional glob patterns (on top of
/// `.brainignore` or the built-in defaults) that exclude files from indexing.
pub fn index_markdown_directory(
    vault_root: &Path,
    db_path: &Path,
    instance_id: &str,
    vault_name: &str,
) -> Result<MarkdownIndexResult, anyhow::Error> {
    index_markdown_directory_with_ignore(vault_root, db_path, instance_id, vault_name, &[])
}

/// Like [`index_markdown_directory`] but with additional ignore patterns from
/// the `--ignore` CLI flag.
pub fn index_markdown_directory_with_ignore(
    vault_root: &Path,
    db_path: &Path,
    instance_id: &str,
    vault_name: &str,
    extra_ignore_patterns: &[String],
) -> Result<MarkdownIndexResult, anyhow::Error> {
    index_markdown_directory_with_ignore_and_deletion_count(
        vault_root,
        db_path,
        instance_id,
        vault_name,
        extra_ignore_patterns,
    )
    .map(|result| result.index)
}

/// Like [`index_markdown_directory_with_ignore`], but also returns the number
/// of old notes deleted by the successfully committed replacement transaction.
pub fn index_markdown_directory_with_ignore_and_deletion_count(
    vault_root: &Path,
    db_path: &Path,
    instance_id: &str,
    vault_name: &str,
    extra_ignore_patterns: &[String],
) -> Result<MarkdownRefreshResult, anyhow::Error> {
    let store = GraphStore::open_or_create(db_path)
        .with_context(|| format!("failed to open/create GraphStore at {}", db_path.display()))?;
    index_markdown_directory_with_store_and_deletion_count(
        &store,
        vault_root,
        db_path,
        instance_id,
        vault_name,
        extra_ignore_patterns,
    )
}

/// Index a markdown vault using an existing GraphStore (for daemon mode).
pub fn index_markdown_directory_with_store(
    store: &GraphStore,
    vault_root: &Path,
    db_path: &Path,
    instance_id: &str,
    vault_name: &str,
    extra_ignore_patterns: &[String],
) -> Result<MarkdownIndexResult, anyhow::Error> {
    index_markdown_directory_with_store_and_deletion_count(
        store,
        vault_root,
        db_path,
        instance_id,
        vault_name,
        extra_ignore_patterns,
    )
    .map(|result| result.index)
}

/// Like [`index_markdown_directory_with_store`], but also returns the number
/// of old notes deleted by the successfully committed replacement transaction.
pub fn index_markdown_directory_with_store_and_deletion_count(
    store: &GraphStore,
    vault_root: &Path,
    db_path: &Path,
    instance_id: &str,
    vault_name: &str,
    extra_ignore_patterns: &[String],
) -> Result<MarkdownRefreshResult, anyhow::Error> {
    let canonical = std::fs::canonicalize(vault_root).unwrap_or_else(|_| vault_root.to_path_buf());
    let reader =
        crate::content_reader::FilesystemReader::with_limits(&canonical, note_reader_limits());
    let ignore_set = crate::brainignore::load_brain_ignore(&canonical, extra_ignore_patterns);
    let result = index_into_store(&reader, store, instance_id, vault_name, &ignore_set)?;

    let aliases = load_taxonomy_aliases(reader.root());
    if !aliases.is_empty() {
        let sidecar_path = crate::sidecar_path(db_path, ".aliases.json");
        match serde_json::to_string(&aliases) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&sidecar_path, json) {
                    tracing::warn!("failed to write aliases sidecar: {e}");
                } else {
                    tracing::info!(
                        path = %sidecar_path.display(),
                        count = aliases.len(),
                        "wrote taxonomy alias sidecar"
                    );
                }
            }
            Err(e) => tracing::warn!("failed to serialize aliases: {e}"),
        }
    }

    Ok(result)
}

/// Load the taxonomy alias map from the sidecar JSON written by
/// `index_markdown_directory`. Returns an empty map when the file is absent
/// or cannot be parsed — callers should treat this as "no aliases known".
///
/// Map shape: `canonical_name → [alias1, alias2, ...]`.
pub fn load_alias_sidecar(db_path: &Path) -> HashMap<String, Vec<String>> {
    crate::migrate_sidecar(db_path, "aliases.json", ".aliases.json");
    let sidecar_path = crate::sidecar_path(db_path, ".aliases.json");
    let Ok(content) = std::fs::read_to_string(&sidecar_path) else {
        return HashMap::new();
    };
    serde_json::from_str(&content).unwrap_or_default()
}

/// Index a markdown vault into an in-memory `GraphStore` (for tests).
pub fn index_markdown_directory_in_memory(
    vault_root: &Path,
    instance_id: &str,
    vault_name: &str,
) -> Result<(MarkdownIndexResult, GraphStore), anyhow::Error> {
    let store = GraphStore::in_memory().context("create in-memory GraphStore")?;
    let canonical = std::fs::canonicalize(vault_root).unwrap_or_else(|_| vault_root.to_path_buf());
    let reader =
        crate::content_reader::FilesystemReader::with_limits(&canonical, note_reader_limits());
    let ignore_set = crate::brainignore::load_brain_ignore(&canonical, &[]);
    let result = index_into_store(&reader, &store, instance_id, vault_name, &ignore_set)?;
    Ok((result.index, store))
}

/// Index markdown notes from a caller-provided [`ContentReader`] into `store`.
///
/// Unlike [`index_markdown_directory_with_store`], this does not assume a
/// canonical on-disk vault directory — it indexes whatever the `reader`
/// exposes. This is the entry point used by the server-mode worker when a repo
/// is declared as a markdown vault (`type = "vault"`): the reader is a
/// [`crate::content_reader::GitBareReader`] over a bare clone, which has no
/// working tree and therefore no on-disk `.brainignore`. In that case the
/// ignore set falls back to the built-in defaults (see
/// [`crate::brainignore::load_brain_ignore`]).
pub fn index_markdown_with_reader(
    reader: &dyn ContentReader,
    store: &GraphStore,
    instance_id: &str,
    vault_name: &str,
) -> Result<MarkdownIndexResult, anyhow::Error> {
    let ignore_set = crate::brainignore::load_brain_ignore(reader.root(), &[]);
    index_into_store(reader, store, instance_id, vault_name, &ignore_set).map(|result| result.index)
}

/// Server-mode vault entry point: index the markdown exposed by `reader` and
/// record `indexed_sha` on the repo's `Repo` node, while narrowing the caller's
/// write gate to the database-write phase only — the scan and parse passes run
/// off-lock (nw-006).
///
/// `repo_url` doubles as the vault display name and the key from which the
/// `Repo` UID is derived. Recording the SHA (nw-003) lets the worker's
/// up-to-date short-circuit skip an unchanged vault on the next poll; without it
/// the `Repo` row keeps an empty `indexed_sha` and the vault re-indexes every
/// cycle. Mirrors [`crate::index_with_reader_and_write_gate`] for the code path.
pub fn index_markdown_with_reader_and_write_gate<G, F>(
    reader: &dyn ContentReader,
    store: &GraphStore,
    instance_id: &str,
    repo_url: &str,
    indexed_sha: &str,
    acquire_write_guard: F,
) -> Result<MarkdownIndexResult, anyhow::Error>
where
    F: FnOnce() -> Result<G, anyhow::Error>,
{
    let ignore_set = crate::brainignore::load_brain_ignore(reader.root(), &[]);
    let result = index_into_store_with_write_gate(
        reader,
        store,
        instance_id,
        repo_url,
        &ignore_set,
        Some(indexed_sha),
        acquire_write_guard,
    )?;

    // Load taxonomy aliases from the reader (server-mode equivalent of the
    // filesystem-based loading in index_markdown_directory_with_store).
    let aliases = load_taxonomy_aliases_from_reader(reader);
    if !aliases.is_empty()
        && let Some(db_path) = store.db_path()
    {
        let sidecar_path = crate::sidecar_path(db_path, ".aliases.json");
        match serde_json::to_string(&aliases) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&sidecar_path, json) {
                    tracing::warn!("failed to write aliases sidecar: {e}");
                } else {
                    tracing::info!(
                        path = %sidecar_path.display(),
                        count = aliases.len(),
                        "wrote taxonomy alias sidecar (server-mode)"
                    );
                }
            }
            Err(e) => tracing::warn!("failed to serialize aliases: {e}"),
        }
    }

    Ok(result.index)
}

/// Upsert the `Repo` node for `repo_url` so its `indexed_sha` equals `sha`,
/// mirroring what the code path does inside its own gated write region
/// (`index.rs`): insert the row when absent, otherwise update the SHA in place.
fn record_repo_indexed_sha(
    store: &GraphStore,
    instance_id: &str,
    repo_url: &str,
    sha: &str,
) -> Result<(), anyhow::Error> {
    let r_uid = repo_uid(instance_id, repo_url);
    if store.lookup_repo(&r_uid).context("lookup_repo")?.is_none() {
        store
            .insert_repo(&Repo {
                uid: r_uid,
                url: repo_url.trim_end_matches('/').to_string(),
                indexed_sha: sha.to_string(),
                staleness_commits_behind: 0,
                instance_id: instance_id.to_string(),
                name: None,
                root_path: None,
            })
            .context("insert_repo")?;
    } else {
        store
            .update_repo_sha(&r_uid, sha)
            .context("update_repo_sha")?;
    }
    Ok(())
}

/// Outcome of an incremental (`--since`) markdown refresh run.
pub struct MarkdownSinceResult {
    pub vault_name: String,
    pub files_checked: usize,
    pub notes_updated: usize,
    pub notes_deleted: usize,
    pub headings_count: usize,
    pub sections_count: usize,
    pub tags_count: usize,
    pub wikilinks_resolved: usize,
}

/// Incrementally refresh only the files in `vault_root` whose filesystem
/// modification time is >= `since`. For each matching file the old Note and
/// its descendants are atomically replaced by the re-parsed graph.
/// Files that have not changed are untouched.
///
/// If the vault has never been indexed this function creates it first and
/// behaves like a full index (every file counts as "modified since epoch").
pub fn index_markdown_directory_since(
    vault_root: &Path,
    db_path: &Path,
    instance_id: &str,
    vault_name: &str,
    since: std::time::SystemTime,
) -> Result<MarkdownSinceResult, anyhow::Error> {
    index_markdown_directory_since_with_ignore(
        vault_root,
        db_path,
        instance_id,
        vault_name,
        since,
        &[],
    )
}

/// Like [`index_markdown_directory_since`] but with additional ignore patterns.
pub fn index_markdown_directory_since_with_ignore(
    vault_root: &Path,
    db_path: &Path,
    instance_id: &str,
    vault_name: &str,
    since: std::time::SystemTime,
    extra_ignore_patterns: &[String],
) -> Result<MarkdownSinceResult, anyhow::Error> {
    let store = GraphStore::open_or_create(db_path)
        .with_context(|| format!("failed to open/create GraphStore at {}", db_path.display()))?;
    index_markdown_directory_since_with_store_and_ignore(
        &store,
        vault_root,
        instance_id,
        vault_name,
        since,
        extra_ignore_patterns,
    )
}

/// Daemon-owned variant of [`index_markdown_directory_since_with_ignore`].
/// The caller supplies the already-open single writer and is responsible for
/// holding its process-level write gate for the duration of the refresh.
pub fn index_markdown_directory_since_with_store_and_ignore(
    store: &GraphStore,
    vault_root: &Path,
    instance_id: &str,
    vault_name: &str,
    since: std::time::SystemTime,
    extra_ignore_patterns: &[String],
) -> Result<MarkdownSinceResult, anyhow::Error> {
    let canonical = std::fs::canonicalize(vault_root).unwrap_or_else(|_| vault_root.to_path_buf());
    let reader =
        crate::content_reader::FilesystemReader::with_limits(&canonical, note_reader_limits());
    let ignore_set = crate::brainignore::load_brain_ignore(&canonical, extra_ignore_patterns);
    index_markdown_since_with_reader(store, &reader, instance_id, vault_name, since, &ignore_set)
}

fn index_markdown_since_with_reader(
    store: &GraphStore,
    reader: &dyn ContentReader,
    instance_id: &str,
    vault_name: &str,
    since: std::time::SystemTime,
    ignore_set: &GlobSet,
) -> Result<MarkdownSinceResult, anyhow::Error> {
    let vault_root = reader.root();
    let root_str = vault_root.to_string_lossy().into_owned();
    let v_uid = vault_uid(instance_id, &root_str);

    let existing_notes = store
        .list_notes(Some(&v_uid))
        .context("list existing vault notes")?;
    let vault_existed = store.lookup_vault(&v_uid).is_ok();
    let existing_note_uids = existing_notes
        .iter()
        .map(|note| note.uid.clone())
        .collect::<std::collections::HashSet<_>>();

    // NANOSECONDS, to match `ContentReader::file_meta_nanos` (nw-200) — but FLOORED
    // TO THE SECOND first.
    //
    // Both sides must share a unit, and naively converting `since` to exact
    // nanoseconds is not enough: a filesystem whose mtime granularity is
    // coarser than the caller's clock stamps a write performed AFTER `since`
    // with a value slightly BEFORE it. On Linux the VFS stamps mtime from a
    // clock "updated every jiffy" (Documentation/filesystems/multigrain-ts.rst),
    // so a note written microseconds after `since` can carry an mtime up to a
    // jiffy earlier and be silently skipped — the exact miss this filter exists
    // to prevent.
    //
    // Flooring restores the previous behaviour, where BOTH sides were truncated
    // to whole seconds and anything written within `since`'s second compared
    // greater-or-equal. The error is deliberately one-sided: flooring can only
    // ever include a few extra notes, which costs a re-parse, and can never
    // exclude one that was genuinely written after the threshold.
    //
    // The file CACHE keeps full nanosecond precision — that is what closes the
    // same-second edit bug. This coarsening applies only to the `--since`
    // threshold, where inclusiveness beats sharpness.
    let since_nanos = since
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .saturating_mul(1_000_000_000);

    let all_files = reader.list_files()?;

    let mut files_checked = 0usize;
    let mut candidates = Vec::new();
    let mut indexed_paths: HashMap<String, (String, PathBuf)> = HashMap::new();
    let mut eligible_note_uids = HashSet::new();

    for rel_path in all_files {
        if !is_markdown(&rel_path) {
            continue;
        }
        // Skip vault-specific directories.
        if path_has_vault_skip_dir(&rel_path) {
            continue;
        }
        // Apply .brainignore patterns.
        let rel_str = rel_path.to_string_lossy();
        if crate::brainignore::is_ignored(&rel_str, ignore_set) {
            tracing::debug!("brainignore: skipping {}", rel_str);
            continue;
        }

        files_checked += 1;
        let rel_path_str = rel_str.into_owned();
        let n_uid = note_uid(&v_uid, &rel_path_str);
        eligible_note_uids.insert(n_uid.clone());

        // Parse changed files now. Unchanged sources are read later only when
        // the affected-source closure shows their outgoing links may change.
        let changed = match reader.file_meta_nanos(&rel_path) {
            Ok(Some((mtime_nanos, file_size))) => {
                if file_size > MAX_NOTE_SIZE_BYTES {
                    if existing_note_uids.contains(&n_uid) {
                        return Err(anyhow::anyhow!(
                            "cannot safely rebuild wikilinks for oversized indexed note {rel_path_str}"
                        ));
                    }
                    tracing::warn!("skipping oversized file: {}", rel_path_str);
                    continue;
                }
                mtime_nanos >= since_nanos
            }
            Ok(None) => true, // bare repo: no mtime, process unconditionally
            Err(error) => {
                if existing_note_uids.contains(&n_uid) {
                    return Err(anyhow::anyhow!(
                        "cannot safely read metadata for indexed note {rel_path_str}: {error}"
                    ));
                }
                continue;
            }
        };
        if !changed {
            if existing_note_uids.contains(&n_uid) {
                indexed_paths.insert(n_uid, (rel_path_str, vault_root.join(&rel_path)));
            }
            continue;
        }
        let source = match reader.read_file(&rel_path) {
            Ok(s) => s,
            Err(err) => {
                if existing_note_uids.contains(&n_uid) {
                    return Err(anyhow::anyhow!(
                        "cannot safely rebuild wikilinks for indexed note {rel_path_str}: {err}"
                    ));
                }
                tracing::warn!("read error {}: {err}", rel_path_str);
                continue;
            }
        };

        let parsed: ParsedNote = match parse_markdown(&rel_path_str, &source) {
            Ok(p) => p,
            Err(err) => {
                if existing_note_uids.contains(&n_uid) {
                    return Err(anyhow::anyhow!(
                        "cannot safely rebuild wikilinks for indexed note {rel_path_str}: {err}"
                    ));
                }
                tracing::warn!("parse error {rel_path_str}: {err}");
                continue;
            }
        };
        candidates.push(ParsedCandidate {
            rel_path: rel_path_str,
            abs_path: vault_root.join(&rel_path),
            note_uid: n_uid,
            parsed,
            changed,
        });
    }

    // nw-287: the incremental path deletes just as totally as the full one.
    // With an empty scan every indexed note falls into `removed_uids` below, so
    // an unreadable or unmounted vault empties the graph here too. Same guard,
    // same reason: a deletion must be observed, never inferred from silence.
    if eligible_note_uids.is_empty() && vault_existed && !existing_notes.is_empty() {
        anyhow::bail!(
            "refusing to refresh vault '{vault_name}': the scan found no note files, but {} \
             note(s) are indexed. Committing this would delete every one of them. Check that \
             the vault directory is readable and mounted; if it really is empty, drop it with \
             `nestweaver brain remove`.",
            existing_notes.len()
        );
    }

    let removed_uids: std::collections::HashSet<String> = existing_notes
        .iter()
        .filter(|note| !eligible_note_uids.contains(&note.uid))
        .map(|note| note.uid.clone())
        .collect();
    let changed_uids: std::collections::HashSet<String> = candidates
        .iter()
        .filter(|candidate| candidate.changed)
        .map(|candidate| candidate.note_uid.clone())
        .collect();
    let mut delete_note_uids: Vec<String> = removed_uids.iter().cloned().collect();
    delete_note_uids.extend(
        changed_uids
            .iter()
            .filter(|uid| existing_note_uids.contains(*uid))
            .cloned(),
    );
    delete_note_uids.sort();
    delete_note_uids.dedup();

    let notes_updated = candidates
        .iter()
        .filter(|candidate| candidate.changed)
        .count();
    let notes_deleted = delete_note_uids.len();
    if notes_updated == 0 && notes_deleted == 0 && vault_existed {
        return Ok(MarkdownSinceResult {
            vault_name: vault_name.to_string(),
            files_checked,
            notes_updated: 0,
            notes_deleted: 0,
            headings_count: 0,
            sections_count: 0,
            tags_count: 0,
            wikilinks_resolved: 0,
        });
    }

    let mut affected_names = std::collections::HashSet::new();
    for note in &existing_notes {
        if changed_uids.contains(&note.uid) || removed_uids.contains(&note.uid) {
            affected_names.insert(note.uid.to_lowercase());
            let stored = note_context_from_stored(note, Vec::new());
            add_note_identity(
                &mut affected_names,
                &note.title,
                &note.file_path,
                &stored.aliases,
            );
        }
    }
    for candidate in &candidates {
        affected_names.insert(candidate.note_uid.to_lowercase());
        add_note_identity(
            &mut affected_names,
            &candidate.parsed.title,
            &candidate.rel_path,
            &candidate.parsed.aliases,
        );
    }

    let sections = store
        .list_sections_by_vault(&v_uid)
        .context("list existing vault sections")?;
    let section_note: HashMap<String, String> = sections
        .into_iter()
        .map(|section| (section.uid, section.note_uid))
        .collect();
    let note_path_by_uid: HashMap<&str, &str> = existing_notes
        .iter()
        .map(|note| (note.uid.as_str(), note.file_path.as_str()))
        .collect();
    let existing_headings = store
        .list_headings_by_vault(&v_uid)
        .context("list existing vault headings")?;
    let heading_note: HashMap<String, String> = existing_headings
        .iter()
        .map(|heading| (heading.uid.clone(), heading.note_uid.clone()))
        .collect();
    let mut affected_sources = changed_uids.clone();
    for (relation, destination) in [
        ("WIKILINK_TO_NOTE", "dst:Note"),
        ("WIKILINK_TO_HEADING", "dst:Heading"),
    ] {
        for (source_section, target, _, _, link_target) in
            store.wikilink_edges_for_vault(&v_uid, relation, destination)?
        {
            let target_note = if relation == "WIKILINK_TO_NOTE" {
                Some(target.as_str())
            } else {
                heading_note.get(&target).map(String::as_str)
            };
            let normalized_target = link_target.trim().replace('\\', "/").to_lowercase();
            let source_relative_affected = section_note
                .get(&source_section)
                .and_then(|source_uid| note_path_by_uid.get(source_uid.as_str()))
                .and_then(|path| Path::new(path).parent())
                .map(|folder| {
                    let joined = folder
                        .join(&normalized_target)
                        .to_string_lossy()
                        .replace('\\', "/");
                    affected_names.contains(&joined)
                        || affected_names
                            .iter()
                            .any(|identity| identity.ends_with(&format!("/{normalized_target}")))
                })
                .unwrap_or(false);
            if (target_note
                .is_some_and(|uid| changed_uids.contains(uid) || removed_uids.contains(uid))
                || affected_names.contains(&normalized_target)
                || source_relative_affected)
                && let Some(source_note) = section_note.get(&source_section)
            {
                affected_sources.insert(source_note.clone());
            }
        }
    }
    for (_, source_note, source_path, _, link_target) in store.all_unresolved_wikilinks()? {
        // The unresolved table is database-global. Only sources belonging to
        // this vault may participate in its affected-source closure; otherwise
        // a matching target added in vault A can make vault B's source appear
        // "unavailable" and abort A's refresh.
        if !existing_note_uids.contains(&source_note) {
            continue;
        }
        let normalized = link_target.trim().replace('\\', "/").to_lowercase();
        let joined = Path::new(&source_path).parent().map(|folder| {
            folder
                .join(&normalized)
                .to_string_lossy()
                .replace('\\', "/")
        });
        if affected_names.contains(&normalized)
            || joined
                .as_ref()
                .is_some_and(|path| affected_names.contains(path))
            || affected_names
                .iter()
                .any(|identity| identity.ends_with(&format!("/{normalized}")))
        {
            affected_sources.insert(source_note);
        }
    }
    for (source, target, _) in store.typed_note_edges()? {
        if changed_uids.contains(&source)
            || changed_uids.contains(&target)
            || removed_uids.contains(&target)
        {
            affected_sources.insert(source);
        }
    }
    for note in &existing_notes {
        if removed_uids.contains(&note.uid) || changed_uids.contains(&note.uid) {
            continue;
        }
        let context = note_context_from_stored(note, Vec::new());
        let affected = ["supersedes", "depends_on", "caused_by", "relates_to"]
            .into_iter()
            .flat_map(|key| frontmatter_list(&context.frontmatter, key))
            .any(|reference| {
                let normalized = reference.trim().replace('\\', "/").to_lowercase();
                affected_names.contains(&normalized)
                    || affected_names
                        .iter()
                        .any(|identity| identity.ends_with(&format!("/{normalized}")))
            });
        if affected {
            affected_sources.insert(note.uid.clone());
        }
    }
    affected_sources.retain(|uid| !removed_uids.contains(uid));
    for source_uid in affected_sources
        .iter()
        .filter(|uid| !changed_uids.contains(*uid))
    {
        let Some((rel_path, abs_path)) = indexed_paths.get(source_uid) else {
            return Err(anyhow::anyhow!(
                "cannot safely rebuild affected indexed note {source_uid}; its source is ignored or unavailable"
            ));
        };
        let source = reader
            .read_file(Path::new(rel_path))
            .with_context(|| format!("read affected indexed note {rel_path}"))?;
        let parsed = parse_markdown(rel_path, &source)
            .with_context(|| format!("parse affected indexed note {rel_path}"))?;
        candidates.push(ParsedCandidate {
            rel_path: rel_path.clone(),
            abs_path: abs_path.clone(),
            note_uid: source_uid.clone(),
            parsed,
            changed: false,
        });
    }

    let existing_tag_uids: std::collections::HashSet<String> = store
        .list_tags(Some(&v_uid))
        .context("list existing vault tags")?
        .into_iter()
        .map(|tag| tag.uid)
        .collect();
    let mut prepared = Vec::new();
    for candidate in candidates.iter().filter(|candidate| candidate.changed) {
        prepared.push(prepare_single_note(
            &v_uid,
            &candidate.note_uid,
            &candidate.abs_path,
            &candidate.rel_path,
            &candidate.parsed,
            &existing_tag_uids,
        )?);
    }

    // Resolve against the complete prospective graph with the same resolver
    // used by full indexing (path, same-folder stem, title, alias, ambiguity,
    // confidence, and heading-anchor semantics).
    let deleted_set: std::collections::HashSet<&str> =
        delete_note_uids.iter().map(String::as_str).collect();
    let prospective_uids: std::collections::HashSet<String> = existing_notes
        .iter()
        .filter(|note| !deleted_set.contains(note.uid.as_str()))
        .map(|note| note.uid.clone())
        .chain(prepared.iter().map(|graph| graph.note.uid.clone()))
        .collect();
    let mut note_contexts: Vec<NoteContext> = candidates
        .iter()
        .filter(|candidate| prospective_uids.contains(&candidate.note_uid))
        .map(note_context_from_candidate)
        .collect();
    let represented: std::collections::HashSet<String> = note_contexts
        .iter()
        .map(|context| context.note_uid.clone())
        .collect();
    let all_headings = existing_headings;
    let mut headings_by_note: HashMap<String, Vec<Heading>> = HashMap::new();
    for heading in all_headings {
        headings_by_note
            .entry(heading.note_uid.clone())
            .or_default()
            .push(heading);
    }
    for note in &existing_notes {
        if prospective_uids.contains(&note.uid) && !represented.contains(&note.uid) {
            note_contexts.push(note_context_from_stored(
                note,
                headings_by_note.remove(&note.uid).unwrap_or_default(),
            ));
        }
    }
    let lookup = WikilinkLookup::build(&note_contexts);
    let context_by_uid: HashMap<&str, &NoteContext> = note_contexts
        .iter()
        .map(|context| (context.note_uid.as_str(), context))
        .collect();
    let mut rebuild_link_source_uids = Vec::new();
    let mut wikilink_to_note = Vec::new();
    let mut wikilink_to_heading = Vec::new();
    let mut unresolved = Vec::new();
    let mut changed_wikilinks = 0usize;
    for candidate in &candidates {
        if !prospective_uids.contains(&candidate.note_uid) {
            continue;
        }
        rebuild_link_source_uids.push(candidate.note_uid.clone());
        let context = context_by_uid[&candidate.note_uid.as_str()];
        for wikilink in &context.wikilinks {
            let Some(source_section) = context.section_uids.get(wikilink.section_idx) else {
                continue;
            };
            let display = wikilink
                .display
                .clone()
                .unwrap_or_else(|| wikilink.target.clone());
            match lookup.resolve(&wikilink.target, &context.folder) {
                ResolveOutcome::Unresolved => unresolved.push((
                    format!(
                        "unresolved:{}:{}",
                        source_section,
                        crate::hash::blake3_hex_short(&wikilink.target)
                    ),
                    candidate.note_uid.clone(),
                    candidate.rel_path.clone(),
                    candidate.parsed.title.clone(),
                    wikilink.target.clone(),
                )),
                ResolveOutcome::Resolved(targets) => {
                    let confidence = targets[0].confidence / targets.len().max(1) as f32;
                    for target in targets {
                        if let Some(anchor) = &wikilink.heading_anchor
                            && let Some(heading_uid) =
                                lookup.find_heading(&target.note_uid, &slugify_anchor(anchor))
                        {
                            wikilink_to_heading.push((
                                source_section.clone(),
                                heading_uid,
                                confidence,
                                display.clone(),
                                wikilink.target.clone(),
                            ));
                        } else {
                            wikilink_to_note.push((
                                source_section.clone(),
                                target.note_uid,
                                confidence,
                                display.clone(),
                                wikilink.target.clone(),
                            ));
                        }
                        if candidate.changed {
                            changed_wikilinks += 1;
                        }
                    }
                }
            }
        }
    }
    rebuild_link_source_uids.sort();
    rebuild_link_source_uids.dedup();
    let mut unresolved_seen = std::collections::HashSet::new();
    unresolved.retain(|record| unresolved_seen.insert(record.0.clone()));
    let typed_edges = derive_typed_edges(&note_contexts, &lookup)
        .into_iter()
        .filter(|edge| rebuild_link_source_uids.contains(&edge.source_uid))
        .collect::<Vec<_>>();

    let mut notes = Vec::new();
    let mut headings = Vec::new();
    let mut sections = Vec::new();
    let mut vault_note_edges = Vec::new();
    let mut note_heading_edges = Vec::new();
    let mut note_section_edges = Vec::new();
    let mut heading_section_edges = Vec::new();
    let mut heading_parent_edges = Vec::new();
    let mut tags = Vec::new();
    let mut note_tag_edges = Vec::new();
    let mut section_tag_edges = Vec::new();
    let mut tag_seen = std::collections::HashSet::new();
    let total_headings = prepared.iter().map(|graph| graph.headings.len()).sum();
    let total_sections = prepared.iter().map(|graph| graph.sections.len()).sum();
    let total_tags = prepared.iter().map(|graph| graph.tags_count).sum();
    for graph in prepared {
        notes.push(graph.note);
        headings.extend(graph.headings);
        sections.extend(graph.sections);
        vault_note_edges.extend(graph.vault_note_edges);
        note_heading_edges.extend(graph.note_heading_edges);
        note_section_edges.extend(graph.note_section_edges);
        heading_section_edges.extend(graph.heading_section_edges);
        heading_parent_edges.extend(graph.heading_parent_edges);
        tags.extend(
            graph
                .tags
                .into_iter()
                .filter(|tag| tag_seen.insert(tag.uid.clone())),
        );
        note_tag_edges.extend(graph.note_tag_edges);
        section_tag_edges.extend(graph.section_tag_edges);
    }
    let vault_note_refs = string_edge_refs(&vault_note_edges);
    let note_heading_refs = string_edge_refs(&note_heading_edges);
    let note_section_refs = string_edge_refs(&note_section_edges);
    let heading_section_refs = string_edge_refs(&heading_section_edges);
    let heading_parent_refs = string_edge_refs(&heading_parent_edges);
    let note_tag_refs = string_edge_refs(&note_tag_edges);
    let section_tag_refs = string_edge_refs(&section_tag_edges);
    let wikilink_note_refs: Vec<_> = wikilink_to_note
        .iter()
        .map(|(s, t, c, d, l)| (s.as_str(), t.as_str(), *c, d.as_str(), l.as_str()))
        .collect();
    let wikilink_heading_refs: Vec<_> = wikilink_to_heading
        .iter()
        .map(|(s, t, c, d, l)| (s.as_str(), t.as_str(), *c, d.as_str(), l.as_str()))
        .collect();
    // A replacement DETACH-deletes the incumbent Note. Preserve materialized
    // project membership for replacements, but intentionally not for notes
    // that are truly removed from the vault.
    let replacement_uids: std::collections::HashSet<&str> =
        notes.iter().map(|note| note.uid.as_str()).collect();
    let mut project_note_edges = Vec::new();
    for project in store
        .list_projects()
        .context("list materialized projects")?
    {
        for note_uid in store
            .list_project_note_uids(&project.uid)
            .with_context(|| format!("list notes for project {}", project.uid))?
        {
            if replacement_uids.contains(note_uid.as_str()) {
                project_note_edges.push((project.uid.clone(), note_uid));
            }
        }
    }
    let project_note_refs = string_edge_refs(&project_note_edges);
    // nw-204, vault half: collect the Note and Heading UIDs the refresh is
    // about to remove, BEFORE it removes them. Applied after the commit with a
    // liveness filter, because an edited note is deleted and re-inserted under
    // the same uid.
    let embedding_candidates: Vec<String> = delete_note_uids
        .iter()
        .filter_map(|uid| store.note_embedding_candidate_uids(uid).ok())
        .flatten()
        .collect();

    store
        .incremental_vault_refresh_atomically(
            &Vault {
                uid: v_uid.clone(),
                name: vault_name.to_string(),
                root_path: root_str,
                instance_id: instance_id.to_string(),
            },
            &delete_note_uids,
            &rebuild_link_source_uids,
            &notes,
            &headings,
            &sections,
            &vault_note_refs,
            &note_heading_refs,
            &note_section_refs,
            &heading_section_refs,
            &heading_parent_refs,
            &tags,
            &note_tag_refs,
            &section_tag_refs,
            &wikilink_note_refs,
            &wikilink_heading_refs,
            &unresolved,
            &typed_edges,
            &project_note_refs,
        )
        .context("atomic incremental vault refresh")?;

    // Advance + persist the graph generation when any note was mutated.
    // An in-place edit deletes and recreates the note's sections, leaving the
    // candidate-node count unchanged — the generation bump is the only signal
    // that stales the trigram posting table (mirrors `index.rs` and the full
    // vault index above).
    //
    // nw-289: wrapped so the code manifest cache is carried across the
    // boundary. A VAULT index advances the generation that `.manifests.json`
    // is bound to, while being incapable of changing anything the manifest
    // describes.
    crate::manifest::advancing_generation_rebinding_manifests(store, || {
        store.bump_and_persist_generation();
    });

    // Best-effort and AFTER the commit, like the symbol epilogue: a
    // tombstoning failure must not fail a refresh that already succeeded.
    // Without this a CLI `brain refresh` that dropped 200 notes left every
    // note and heading vector live and scored, with only a WRITABLE daemon's
    // periodic reconciler as backstop — so a CLI-only run, or one against a
    // read-only daemon, leaked indefinitely.
    if !embedding_candidates.is_empty() {
        match store.tombstone_deleted_vault_embeddings(&embedding_candidates) {
            Ok(0) => {}
            Ok(removed) => {
                tracing::debug!("vault refresh: tombstoned {removed} dead vault vector(s)")
            }
            Err(error) => tracing::warn!(
                "vault refresh: could not tombstone dead vault vectors: {error}; \
                 the periodic reconciler will reclaim them"
            ),
        }
    }

    Ok(MarkdownSinceResult {
        vault_name: vault_name.to_string(),
        files_checked,
        notes_updated,
        notes_deleted,
        headings_count: total_headings,
        sections_count: total_sections,
        tags_count: total_tags,
        wikilinks_resolved: changed_wikilinks,
    })
}

struct PreparedNoteGraph {
    note: Note,
    headings: Vec<Heading>,
    sections: Vec<Section>,
    vault_note_edges: Vec<(String, String)>,
    note_heading_edges: Vec<(String, String)>,
    note_section_edges: Vec<(String, String)>,
    heading_section_edges: Vec<(String, String)>,
    heading_parent_edges: Vec<(String, String)>,
    tags: Vec<Tag>,
    note_tag_edges: Vec<(String, String)>,
    section_tag_edges: Vec<(String, String)>,
    tags_count: usize,
}

struct ParsedCandidate {
    rel_path: String,
    abs_path: PathBuf,
    note_uid: String,
    parsed: ParsedNote,
    changed: bool,
}

fn add_note_identity(
    identities: &mut HashSet<String>,
    title: &str,
    path: &str,
    aliases: &[String],
) {
    identities.insert(title.to_lowercase());
    let normalized = path.replace('\\', "/").to_lowercase();
    identities.insert(normalized.clone());
    if let Some(without_extension) = normalized
        .strip_suffix(".md")
        .or_else(|| normalized.strip_suffix(".markdown"))
    {
        identities.insert(without_extension.to_string());
    }
    if let Some(stem) = Path::new(path).file_stem().and_then(|stem| stem.to_str()) {
        identities.insert(stem.to_lowercase());
    }
    identities.extend(aliases.iter().map(|alias| alias.to_lowercase()));
}

fn note_context_from_candidate(candidate: &ParsedCandidate) -> NoteContext {
    let heading_uids: Vec<String> = candidate
        .parsed
        .headings
        .iter()
        .map(|heading| heading_uid(&candidate.note_uid, &heading.slug, heading.start_line))
        .collect();
    let section_uids = candidate
        .parsed
        .sections
        .iter()
        .map(|section| {
            let hash = crate::hash::blake3_hex(&section.text);
            section_uid(&candidate.note_uid, section.start_line, &hash[..12])
        })
        .collect();
    NoteContext {
        note_uid: candidate.note_uid.clone(),
        rel_path: candidate.rel_path.clone(),
        title: candidate.parsed.title.clone(),
        folder: Path::new(&candidate.rel_path)
            .parent()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default(),
        aliases: candidate.parsed.aliases.clone(),
        heading_uids,
        heading_slugs: candidate
            .parsed
            .headings
            .iter()
            .map(|heading| heading.slug.clone())
            .collect(),
        section_uids,
        wikilinks: candidate.parsed.wikilinks.clone(),
        tags: candidate.parsed.tags.clone(),
        frontmatter: candidate.parsed.frontmatter.clone(),
        section_heading_text: candidate
            .parsed
            .sections
            .iter()
            .map(|section| {
                section
                    .heading_idx
                    .and_then(|index| candidate.parsed.headings.get(index))
                    .map(|heading| heading.text.to_lowercase())
            })
            .collect(),
    }
}

fn note_context_from_stored(note: &Note, headings: Vec<Heading>) -> NoteContext {
    let frontmatter = note
        .frontmatter
        .as_deref()
        .and_then(|json| serde_json::from_str(json).ok())
        .unwrap_or(serde_json::Value::Null);
    let mut aliases = Vec::new();
    for key in ["alias", "aliases"] {
        if let Some(value) = frontmatter.get(key) {
            match value {
                serde_json::Value::Array(values) => aliases.extend(
                    values
                        .iter()
                        .filter_map(|value| value.as_str().map(str::to_owned)),
                ),
                serde_json::Value::String(value) => aliases.push(value.clone()),
                _ => {}
            }
        }
    }
    aliases = aliases
        .into_iter()
        .map(|alias| alias.trim().to_string())
        .filter(|alias| !alias.is_empty())
        .collect();
    aliases.sort();
    aliases.dedup();
    NoteContext {
        note_uid: note.uid.clone(),
        rel_path: note.file_path.clone(),
        title: note.title.clone(),
        folder: Path::new(&note.file_path)
            .parent()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default(),
        aliases,
        heading_uids: headings.iter().map(|heading| heading.uid.clone()).collect(),
        heading_slugs: headings
            .iter()
            .map(|heading| heading.slug.clone())
            .collect(),
        section_uids: Vec::new(),
        wikilinks: Vec::new(),
        tags: Vec::new(),
        frontmatter,
        section_heading_text: Vec::new(),
    }
}

fn string_edge_refs(edges: &[(String, String)]) -> Vec<(&str, &str)> {
    edges
        .iter()
        .map(|(left, right)| (left.as_str(), right.as_str()))
        .collect()
}

/// Prepare one replacement note without mutating the graph. All preparations
/// complete before the incremental refresh opens its single transaction.
fn prepare_single_note(
    v_uid: &str,
    n_uid: &str,
    path: &Path,
    rel_path: &str,
    parsed: &ParsedNote,
    existing_tag_uids: &std::collections::HashSet<String>,
) -> Result<PreparedNoteGraph, anyhow::Error> {
    let frontmatter_json = if parsed
        .frontmatter
        .as_object()
        .is_some_and(|m| !m.is_empty())
    {
        serde_json::to_string(&parsed.frontmatter).ok()
    } else {
        None
    };

    let (created_at, modified_at) = match std::fs::metadata(path) {
        Ok(meta) => {
            let c = meta.created().ok().and_then(format_system_time);
            let m = meta.modified().ok().and_then(format_system_time);
            (c, m)
        }
        Err(_) => (None, None),
    };

    let note = Note {
        uid: n_uid.to_string(),
        vault_uid: v_uid.to_string(),
        file_path: rel_path.to_string(),
        title: parsed.title.clone(),
        note_kind: parsed.note_kind,
        word_count: parsed.word_count,
        content_hash: parsed.content_hash.clone(),
        frontmatter_raw: parsed.frontmatter_raw.clone(),
        frontmatter: frontmatter_json,
        created_at,
        modified_at,
        pagerank_score: None,
        embedding: None,
    };

    // Headings.
    let heading_uids: Vec<String> = parsed
        .headings
        .iter()
        .map(|h| heading_uid(n_uid, &h.slug, h.start_line))
        .collect();
    let headings: Vec<Heading> = parsed
        .headings
        .iter()
        .enumerate()
        .map(|(idx, h)| Heading {
            uid: heading_uids[idx].clone(),
            note_uid: n_uid.to_string(),
            level: h.level,
            text: h.text.clone(),
            slug: h.slug.clone(),
            start_line: h.start_line,
            end_line: h.end_line,
            content_hash: crate::hash::blake3_hex_short(&h.text),
            embedding: None,
        })
        .collect();
    let nh_edges: Vec<(String, String)> = heading_uids
        .iter()
        .map(|h| (n_uid.to_string(), h.clone()))
        .collect();

    let mut parent_edges: Vec<(String, String)> = Vec::new();
    for (idx, h) in parsed.headings.iter().enumerate() {
        for prev in (0..idx).rev() {
            if parsed.headings[prev].level < h.level {
                parent_edges.push((heading_uids[idx].clone(), heading_uids[prev].clone()));
                break;
            }
        }
    }

    // Sections.
    let mut section_uids: Vec<String> = Vec::with_capacity(parsed.sections.len());
    let mut sections: Vec<Section> = Vec::with_capacity(parsed.sections.len());
    let mut ns_edges: Vec<(String, String)> = Vec::new();
    let mut hs_edges: Vec<(String, String)> = Vec::new();
    for sec in &parsed.sections {
        let text_hash = crate::hash::blake3_hex(&sec.text);
        let s_uid = section_uid(n_uid, sec.start_line, &text_hash[..12]);
        let word_count = u32::try_from(sec.text.split_whitespace().count()).unwrap_or(u32::MAX);
        let heading_link = sec.heading_idx.map(|i| heading_uids[i].clone());
        sections.push(Section {
            uid: s_uid.clone(),
            note_uid: n_uid.to_string(),
            heading_uid: heading_link.clone(),
            start_line: sec.start_line,
            end_line: sec.end_line,
            text_hash,
            text_content: sec.text.clone(),
            word_count,
            pagerank_score: None,
        });
        ns_edges.push((n_uid.to_string(), s_uid.clone()));
        if let Some(h_uid) = heading_link {
            hs_edges.push((h_uid, s_uid.clone()));
        }
        section_uids.push(s_uid);
    }

    // Tags.
    let mut local_tag_uids: HashMap<String, String> = HashMap::new();
    let mut new_tag_nodes: Vec<Tag> = Vec::new();
    let mut note_tag_edges: Vec<(String, String)> = Vec::new();
    let mut section_tag_edges: Vec<(String, String)> = Vec::new();

    for raw in &parsed.tags {
        let canonical = raw.name.to_lowercase();
        let t_uid = local_tag_uids
            .entry(canonical.clone())
            .or_insert_with(|| {
                let uid = tag_uid(v_uid, &canonical);
                new_tag_nodes.push(Tag {
                    uid: uid.clone(),
                    vault_uid: v_uid.to_string(),
                    name: canonical.clone(),
                });
                uid
            })
            .clone();
        match (raw.source, raw.section_idx) {
            (TagSource::Frontmatter, _) => {
                note_tag_edges.push((n_uid.to_string(), t_uid));
            }
            (TagSource::Inline, Some(idx)) if idx < section_uids.len() => {
                section_tag_edges.push((section_uids[idx].clone(), t_uid));
            }
            _ => {
                note_tag_edges.push((n_uid.to_string(), t_uid));
            }
        }
    }
    new_tag_nodes.retain(|tag| !existing_tag_uids.contains(&tag.uid));
    let tags_count = local_tag_uids.len();
    Ok(PreparedNoteGraph {
        note,
        headings,
        sections,
        vault_note_edges: vec![(v_uid.to_string(), n_uid.to_string())],
        note_heading_edges: nh_edges,
        note_section_edges: ns_edges,
        heading_section_edges: hs_edges,
        heading_parent_edges: parent_edges,
        tags: new_tag_nodes,
        note_tag_edges,
        section_tag_edges,
        tags_count,
    })
}

fn index_into_store(
    reader: &dyn crate::content_reader::ContentReader,
    store: &GraphStore,
    instance_id: &str,
    vault_name: &str,
    ignore_set: &GlobSet,
) -> Result<MarkdownRefreshResult, anyhow::Error> {
    index_into_store_with_write_gate(
        reader,
        store,
        instance_id,
        vault_name,
        ignore_set,
        None,
        || Ok::<_, anyhow::Error>(()),
    )
}

/// Core markdown indexer. The expensive scan and parse passes run *off* the
/// caller's write gate; only the database writes — vault upsert, tag/bulk
/// inserts, and the optional repo-SHA recording — are performed under
/// `acquire_write_guard` (nw-006). This mirrors the code path's
/// `index_into_store_with_write_gate` in `index.rs`, which likewise builds
/// off-lock and acquires the gate just before its write phase.
///
/// `record_repo_sha` is `Some(sha)` only for the server-mode vault path, where
/// `vault_name` is the repo URL: it upserts the repo's `Repo` node with that
/// SHA (nw-003) so an unchanged vault is skipped on the next poll. For
/// local-directory vaults (which have no remote SHA) it is `None`.
fn index_into_store_with_write_gate<G, F>(
    reader: &dyn crate::content_reader::ContentReader,
    store: &GraphStore,
    instance_id: &str,
    vault_name: &str,
    ignore_set: &GlobSet,
    record_repo_sha: Option<&str>,
    acquire_write_guard: F,
) -> Result<MarkdownRefreshResult, anyhow::Error>
where
    F: FnOnce() -> Result<G, anyhow::Error>,
{
    let started = Instant::now();

    // The caller canonicalizes vault_root before constructing the reader, so
    // reader.root() is already canonical — agreeing with the watcher (which
    // sees canonical paths from FSEvents on macOS).
    let vault_root = reader.root();
    let root_str = vault_root.to_string_lossy().into_owned();
    let v_uid = vault_uid(instance_id, &root_str);

    // The Vault node is upserted later, under the write gate, alongside the
    // bulk commit (see "gated write region" below). Computing v_uid here is a
    // pure operation that does not touch the store.

    // ── Phase 1: Scan notes ───────────────────────────────────────────────
    let scan_pb = ProgressBar::new_spinner();
    scan_pb.set_style(
        ProgressStyle::with_template("{spinner:.cyan} {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_spinner()),
    );
    scan_pb.set_message("Scanning notes...");

    /// Per-file data collected during the scan phase.
    struct ScannedNote {
        rel_path: PathBuf,
    }

    let mut scanned_notes: Vec<ScannedNote> = Vec::new();
    let mut skipped: Vec<SkippedFile> = Vec::new();

    // SECURITY: FilesystemReader::list_files() uses follow_links(false)
    // and only returns entries where file_type().is_file() == true,
    // so symlinks (including those pointing outside the vault) are
    // silently excluded — matching the old WalkDir + symlink-rejection
    // behaviour.
    let all_files = reader.list_files()?;
    for rel_path in all_files {
        if !is_markdown(&rel_path) {
            continue;
        }
        // Skip vault-specific directories (e.g. .obsidian, .trash).
        if path_has_vault_skip_dir(&rel_path) {
            continue;
        }

        // Apply .brainignore patterns.
        let rel_str = rel_path.to_string_lossy();
        if crate::brainignore::is_ignored(&rel_str, ignore_set) {
            tracing::debug!("brainignore: skipping {}", rel_str);
            skipped.push(SkippedFile::new(
                rel_str.into_owned(),
                SkipReasonCode::Ignored,
                "matched .brainignore pattern",
            ));
            continue;
        }

        // Size guard.
        if let Ok(Some((_, size))) = reader.file_meta_nanos(&rel_path)
            && size > MAX_NOTE_SIZE_BYTES
        {
            skipped.push(SkippedFile {
                path: rel_str.into_owned(),
                reason: format!("file exceeds {} bytes", MAX_NOTE_SIZE_BYTES),
                reason_code: SkipReasonCode::Oversized,
                observed_bytes: Some(size),
                limit_bytes: Some(MAX_NOTE_SIZE_BYTES),
            });
            continue;
        }

        scanned_notes.push(ScannedNote { rel_path });
        scan_pb.set_message(format!("Scanning notes... {}", scanned_notes.len()));
        scan_pb.tick();
    }

    scan_pb.finish_with_message(format!("Scanned {} notes", scanned_notes.len()));

    // ── Phase 2: Parse markdown ───────────────────────────────────────────
    let total_notes = scanned_notes.len() as u64;
    let parse_pb = ProgressBar::new(total_notes);
    parse_pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.cyan} Parsing [{bar:30.cyan/dim}] {pos}/{len} {wide_msg}",
        )
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("━╸─"),
    );

    // We accumulate every node's data into batches plus a per-note context
    // so pass 2 (wikilink + tag resolution) can do its work without
    // re-parsing or re-walking.

    // Per-note parsing result, collected in parallel then merged sequentially.
    struct NoteParseOutcome {
        note: Note,
        headings: Vec<Heading>,
        sections: Vec<Section>,
        vault_note_edge: (String, String),
        note_heading_edges: Vec<(String, String)>,
        note_section_edges: Vec<(String, String)>,
        heading_section_edges: Vec<(String, String)>,
        heading_parent_edges: Vec<(String, String)>,
        note_context: NoteContext,
    }

    #[allow(clippy::large_enum_variant)]
    enum NoteOutcome {
        Parsed(NoteParseOutcome),
        Skipped(SkippedFile),
    }

    // Run parsing in parallel — each note is independent: read, hash,
    // parse markdown, derive UIDs. CPU/IO-bound work that benefits from
    // multi-core execution.
    use rayon::prelude::*;

    // Same duty-cycle budget as the code-parse phase (see index.rs).
    let cpu_throttle = crate::cpu_throttle::CpuThrottle::from_env();

    let parse_one = |scanned: &ScannedNote| -> NoteOutcome {
        cpu_throttle.check();
        let rel_path = scanned.rel_path.to_string_lossy().into_owned();

        // Read via ContentReader.
        let source = match reader.read_file(&scanned.rel_path) {
            Ok(s) => s,
            Err(err) => {
                parse_pb.inc(1);
                if let Some(oversized) = err.downcast_ref::<crate::content_reader::SourceTooLarge>()
                {
                    return NoteOutcome::Skipped(SkippedFile {
                        path: rel_path,
                        reason: format!("file exceeds {} bytes", MAX_NOTE_SIZE_BYTES),
                        reason_code: SkipReasonCode::Oversized,
                        observed_bytes: Some(oversized.observed_bytes),
                        limit_bytes: Some(MAX_NOTE_SIZE_BYTES),
                    });
                }
                return NoteOutcome::Skipped(SkippedFile::new(
                    rel_path,
                    SkipReasonCode::ReadError,
                    format!("read error: {err}"),
                ));
            }
        };
        // `file_meta_nanos` is unavailable for bare Git readers. Enforce the note
        // policy again on the returned content so any reader implementation
        // remains policy-correct even when it cannot preflight metadata.
        if source.len() as u64 > MAX_NOTE_SIZE_BYTES {
            parse_pb.inc(1);
            return NoteOutcome::Skipped(SkippedFile {
                path: rel_path,
                reason: format!("file exceeds {} bytes", MAX_NOTE_SIZE_BYTES),
                reason_code: SkipReasonCode::Oversized,
                observed_bytes: Some(source.len() as u64),
                limit_bytes: Some(MAX_NOTE_SIZE_BYTES),
            });
        }

        let parsed: ParsedNote = match parse_markdown(&rel_path, &source) {
            Ok(p) => p,
            Err(err) => {
                parse_pb.inc(1);
                return NoteOutcome::Skipped(SkippedFile::new(
                    rel_path.clone(),
                    SkipReasonCode::ParseError,
                    err.to_string(),
                ));
            }
        };

        if let Some(fm_err) = &parsed.frontmatter_error {
            tracing::warn!("frontmatter parse warning for {rel_path}: {fm_err}");
        }

        let n_uid = note_uid(&v_uid, &rel_path);
        let frontmatter_json = if parsed
            .frontmatter
            .as_object()
            .is_some_and(|m| !m.is_empty())
        {
            serde_json::to_string(&parsed.frontmatter).ok()
        } else {
            None
        };

        // File timestamps — best-effort, never fatal. Uses direct
        // fs::metadata for created_at (not in ContentReader trait).
        let (created_at, modified_at) = match std::fs::metadata(vault_root.join(&scanned.rel_path))
        {
            Ok(meta) => {
                let created = meta.created().ok().and_then(format_system_time);
                let modified = meta.modified().ok().and_then(format_system_time);
                (created, modified)
            }
            Err(_) => (None, None),
        };

        let note = Note {
            uid: n_uid.clone(),
            vault_uid: v_uid.clone(),
            file_path: rel_path.clone(),
            title: parsed.title.clone(),
            note_kind: parsed.note_kind,
            word_count: parsed.word_count,
            content_hash: parsed.content_hash.clone(),
            frontmatter: frontmatter_json,
            frontmatter_raw: parsed.frontmatter_raw.clone(),
            created_at,
            modified_at,
            pagerank_score: None,
            embedding: None,
        };
        let vault_note_edge = (v_uid.clone(), n_uid.clone());

        // Derive Heading UIDs and Heading nodes from the parsed outline.
        let heading_uids: Vec<String> = parsed
            .headings
            .iter()
            .map(|h| heading_uid(&n_uid, &h.slug, h.start_line))
            .collect();
        let mut headings = Vec::with_capacity(parsed.headings.len());
        let mut n_h_edges = Vec::with_capacity(parsed.headings.len());
        for (idx, h) in parsed.headings.iter().enumerate() {
            let h_uid = heading_uids[idx].clone();
            headings.push(Heading {
                uid: h_uid.clone(),
                note_uid: n_uid.clone(),
                level: h.level,
                text: h.text.clone(),
                slug: h.slug.clone(),
                start_line: h.start_line,
                end_line: h.end_line,
                content_hash: crate::hash::blake3_hex_short(&h.text),
                embedding: None,
            });
            n_h_edges.push((n_uid.clone(), h_uid));
        }

        // Heading parent edges: for each heading, find its nearest preceding
        // ancestor — the most recent heading whose level is strictly shallower.
        let mut h_parent_edges = Vec::new();
        for (idx, h) in parsed.headings.iter().enumerate() {
            for prev_idx in (0..idx).rev() {
                if parsed.headings[prev_idx].level < h.level {
                    h_parent_edges
                        .push((heading_uids[idx].clone(), heading_uids[prev_idx].clone()));
                    break;
                }
            }
        }

        // Derive Section UIDs and Section nodes.
        let mut sections = Vec::with_capacity(parsed.sections.len());
        let mut n_s_edges = Vec::with_capacity(parsed.sections.len());
        let mut h_s_edges = Vec::new();
        let mut section_uids: Vec<String> = Vec::with_capacity(parsed.sections.len());
        for sec in &parsed.sections {
            let text_hash = crate::hash::blake3_hex(&sec.text);
            let short = &text_hash[..12];
            let s_uid = section_uid(&n_uid, sec.start_line, short);
            let word_count = u32::try_from(sec.text.split_whitespace().count()).unwrap_or(u32::MAX);
            let heading_link = sec.heading_idx.map(|i| heading_uids[i].clone());
            sections.push(Section {
                uid: s_uid.clone(),
                note_uid: n_uid.clone(),
                heading_uid: heading_link.clone(),
                start_line: sec.start_line,
                end_line: sec.end_line,
                text_hash,
                text_content: sec.text.clone(),
                word_count,
                pagerank_score: None,
            });
            n_s_edges.push((n_uid.clone(), s_uid.clone()));
            if let Some(h_uid) = heading_link {
                h_s_edges.push((h_uid, s_uid.clone()));
            }
            section_uids.push(s_uid);
        }

        // Record per-note context for pass-2 cross-reference resolution.
        let folder = Path::new(&rel_path)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let section_heading_text: Vec<Option<String>> = parsed
            .sections
            .iter()
            .map(|sec| {
                sec.heading_idx
                    .and_then(|i| parsed.headings.get(i))
                    .map(|h| h.text.to_lowercase())
            })
            .collect();

        let note_context = NoteContext {
            note_uid: n_uid,
            rel_path,
            title: parsed.title.clone(),
            folder,
            aliases: parsed.aliases.clone(),
            heading_uids,
            heading_slugs: parsed.headings.iter().map(|h| h.slug.clone()).collect(),
            section_uids,
            wikilinks: parsed.wikilinks.clone(),
            tags: parsed.tags.clone(),
            frontmatter: parsed.frontmatter.clone(),
            section_heading_text,
        };

        parse_pb.inc(1);

        NoteOutcome::Parsed(NoteParseOutcome {
            note,
            headings,
            sections,
            vault_note_edge,
            note_heading_edges: n_h_edges,
            note_section_edges: n_s_edges,
            heading_section_edges: h_s_edges,
            heading_parent_edges: h_parent_edges,
            note_context,
        })
    };

    // Same dedicated low-priority pool as the code-parse phase (index.rs).
    let outcomes: Vec<NoteOutcome> =
        crate::parse_pool::install_parse_pool(|| scanned_notes.par_iter().map(parse_one).collect());

    parse_pb.finish_and_clear();

    // ── Sequential merge of parallel results ────────────────────────────
    let mut all_notes: Vec<Note> = Vec::new();
    let mut all_headings: Vec<Heading> = Vec::new();
    let mut all_sections: Vec<Section> = Vec::new();
    let mut edge_pairs: Vec<(String, String)> = Vec::new();
    let mut note_heading_edges: Vec<(String, String)> = Vec::new();
    let mut note_section_edges: Vec<(String, String)> = Vec::new();
    let mut heading_section_edges: Vec<(String, String)> = Vec::new();
    let mut heading_parent_edges: Vec<(String, String)> = Vec::new();
    let mut note_contexts: Vec<NoteContext> = Vec::new();

    for outcome in outcomes {
        match outcome {
            NoteOutcome::Skipped(sf) => {
                skipped.push(sf);
            }
            NoteOutcome::Parsed(p) => {
                all_notes.push(p.note);
                all_headings.extend(p.headings);
                all_sections.extend(p.sections);
                edge_pairs.push(p.vault_note_edge);
                note_heading_edges.extend(p.note_heading_edges);
                note_section_edges.extend(p.note_section_edges);
                heading_section_edges.extend(p.heading_section_edges);
                heading_parent_edges.extend(p.heading_parent_edges);
                note_contexts.push(p.note_context);
            }
        }
    }

    let notes_count = all_notes.len();
    let headings_count = all_headings.len();
    let sections_count = all_sections.len();
    tracing::info!(
        total_notes = notes_count,
        headings = headings_count,
        sections = sections_count,
        "vault indexing complete"
    );

    // ── Pass 2: cross-reference resolution (tags + wikilinks) ───────────────
    // All data is computed from in-memory `note_contexts` (WikilinkLookup does
    // not query the store), so we can pre-compute everything before touching
    // the DB and then flush it all in one transaction via `bulk_vault_write`.
    let resolve_pb = ProgressBar::new_spinner();
    resolve_pb.set_style(
        ProgressStyle::with_template("{spinner:.cyan} {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_spinner()),
    );
    resolve_pb.set_message("Resolving wikilinks and tags...");
    resolve_pb.enable_steady_tick(std::time::Duration::from_millis(100));

    let mut all_tags: Vec<Tag> = Vec::new();
    let mut note_tag_edges: Vec<(String, String)> = Vec::new();
    let mut section_tag_edges: Vec<(String, String)> = Vec::new();
    let mut tag_uid_by_name: HashMap<String, String> = HashMap::new();

    for ctx in &note_contexts {
        for raw in &ctx.tags {
            let canonical = raw.name.to_lowercase();
            let t_uid = tag_uid_by_name
                .entry(canonical.clone())
                .or_insert_with(|| {
                    let uid = tag_uid(&v_uid, &canonical);
                    all_tags.push(Tag {
                        uid: uid.clone(),
                        vault_uid: v_uid.clone(),
                        name: canonical.clone(),
                    });
                    uid
                })
                .clone();
            match (raw.source, raw.section_idx) {
                (TagSource::Frontmatter, _) => {
                    note_tag_edges.push((ctx.note_uid.clone(), t_uid));
                }
                (TagSource::Inline, Some(sec_idx)) if sec_idx < ctx.section_uids.len() => {
                    section_tag_edges.push((ctx.section_uids[sec_idx].clone(), t_uid));
                }
                _ => {
                    // Inline tag with no resolvable section — treat as note-level.
                    note_tag_edges.push((ctx.note_uid.clone(), t_uid));
                }
            }
        }
    }
    let tags_count = all_tags.len();

    // ── Gated write region (nw-006) ───────────────────────────────────────
    // Everything above (scan + parse + in-memory tag/wikilink resolution) is
    // read-only with respect to the store, so it ran off the caller's write
    // gate. Acquire the gate now and hold it through the final commit. The
    // gate closure also performs the job-cancellation check in server mode, so
    // a cancelled job returns here after the off-lock parse — matching the code
    // path's behaviour exactly.
    let _write_guard = acquire_write_guard()?;

    // Whether this vault was already indexed. If so, the atomic reindex write
    // below cascade-deletes the old data and re-inserts the new data in a
    // SINGLE transaction (via `bulk_vault_reindex_write`), so concurrent
    // readers only ever observe the complete old vault or the complete new
    // vault — never the empty intermediate that a separate delete transaction
    // used to expose. Tag nodes are (re-)inserted inside that same transaction;
    // the cascade delete removes this vault's old tags first, so no duplicate
    // handling is needed (tags are vault-scoped by uid). Captured under the
    // gate, before the write.
    let vault_existed = store.lookup_vault(&v_uid).is_ok();

    // nw-287: refuse a whole-vault deletion that was INFERRED rather than
    // observed. `bulk_vault_reindex_write` is a total replacement — it
    // cascade-deletes the old vault and inserts the scan's result — so an empty
    // scan over a vault that HELD notes commits as "every note was deleted".
    //
    // `FilesystemReader::list_files` now refuses an unreadable root, which
    // closes the reported case. This guard is the belt: an unmounted volume
    // presents as an empty-but-READABLE directory, which no enumeration check
    // can distinguish from a genuinely emptied vault. The destructive reading
    // must not be the default one, and the watcher route has no human reading
    // the exit code.
    if all_notes.is_empty() && vault_existed {
        let existing = store
            .list_notes(Some(&v_uid))
            .context("count indexed notes before the stale-drop")?;
        if !existing.is_empty() {
            anyhow::bail!(
                "refusing to reindex vault '{vault_name}' at {root_str}: the scan found no \
                 note files, but {} note(s) are indexed. Committing this would delete every \
                 one of them. Check that the vault directory is readable and mounted; if it \
                 really is empty, drop it with `nestweaver brain remove`.",
                existing.len()
            );
        }
    }

    // Wikilink resolution: build lookup indices once, then 5-priority match.
    let lookup = WikilinkLookup::build(&note_contexts);

    // nw-122: (section, target_node, confidence, display_text, link_target).
    let mut wikilink_to_note: Vec<(String, String, f32, String, String)> = Vec::new();
    let mut wikilink_to_heading: Vec<(String, String, f32, String, String)> = Vec::new();
    let mut wikilinks_unresolved: usize = 0;
    // (uid, source_note_uid, source_path, source_title, wikilink_text)
    let mut unresolved_records: Vec<(String, String, String, String, String)> = Vec::new();

    for ctx in &note_contexts {
        for wl in &ctx.wikilinks {
            // Pass the source section's UID (where the link appears).
            if wl.section_idx >= ctx.section_uids.len() {
                continue;
            }
            let source_section_uid = &ctx.section_uids[wl.section_idx];
            let display = wl.display.clone().unwrap_or_else(|| wl.target.clone());

            match lookup.resolve(&wl.target, &ctx.folder) {
                ResolveOutcome::Resolved(candidates) => {
                    // Confidence: split 1/N for ambiguous resolutions, otherwise use the priority's base.
                    let n = candidates.len() as f32;
                    let conf_per = candidates[0].confidence / n.max(1.0);
                    for cand in &candidates {
                        if let Some(anchor) = &wl.heading_anchor {
                            // Try to find a matching heading slug in the target note.
                            let anchor_lc = slugify_anchor(anchor);
                            if let Some(h_uid) = lookup.find_heading(&cand.note_uid, &anchor_lc) {
                                wikilink_to_heading.push((
                                    source_section_uid.clone(),
                                    h_uid,
                                    conf_per,
                                    display.clone(),
                                    wl.target.clone(),
                                ));
                                continue;
                            }
                            // Anchor missing — fall back to note-level link.
                        }
                        wikilink_to_note.push((
                            source_section_uid.clone(),
                            cand.note_uid.clone(),
                            conf_per,
                            display.clone(),
                            wl.target.clone(),
                        ));
                    }
                }
                ResolveOutcome::Unresolved => {
                    wikilinks_unresolved += 1;
                    tracing::debug!(
                        "unresolved wikilink: '{}' in note '{}'",
                        wl.target,
                        ctx.title,
                    );
                    // Record it so broken-links can surface a genuinely-broken
                    // wikilink (one that resolves to no note at all). UID is
                    // derived from the source section + target text so a
                    // re-index replaces rather than duplicates.
                    let uw_uid = format!(
                        "unresolved:{}:{}",
                        source_section_uid,
                        crate::hash::blake3_hex_short(&wl.target)
                    );
                    unresolved_records.push((
                        uw_uid,
                        ctx.note_uid.clone(),
                        ctx.rel_path.clone(),
                        ctx.title.clone(),
                        wl.target.clone(),
                    ));
                }
            }
        }
    }

    let wikilinks_resolved = wikilink_to_note.len() + wikilink_to_heading.len();

    // 3 & 4. Flush all nodes and edges for this vault in one transaction.
    let notes_deleted = {
        let vault_note_refs: Vec<(&str, &str)> = edge_pairs
            .iter()
            .map(|(v, n)| (v.as_str(), n.as_str()))
            .collect();
        let note_heading_refs: Vec<(&str, &str)> = note_heading_edges
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let note_section_refs: Vec<(&str, &str)> = note_section_edges
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let heading_section_refs: Vec<(&str, &str)> = heading_section_edges
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let heading_parent_refs: Vec<(&str, &str)> = heading_parent_edges
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let note_tag_refs: Vec<(&str, &str)> = note_tag_edges
            .iter()
            .map(|(n, t)| (n.as_str(), t.as_str()))
            .collect();
        let section_tag_refs: Vec<(&str, &str)> = section_tag_edges
            .iter()
            .map(|(s, t)| (s.as_str(), t.as_str()))
            .collect();
        let wl_note_refs: Vec<(&str, &str, f32, &str, &str)> = wikilink_to_note
            .iter()
            .map(|(s, n, c, d, t)| (s.as_str(), n.as_str(), *c, d.as_str(), t.as_str()))
            .collect();
        let wl_head_refs: Vec<(&str, &str, f32, &str, &str)> = wikilink_to_heading
            .iter()
            .map(|(s, h, c, d, t)| (s.as_str(), h.as_str(), *c, d.as_str(), t.as_str()))
            .collect();

        store
            .bulk_vault_reindex_write(
                &Vault {
                    uid: v_uid.clone(),
                    name: vault_name.to_string(),
                    root_path: root_str.clone(),
                    instance_id: instance_id.to_string(),
                },
                vault_existed,
                &all_notes,
                &all_headings,
                &all_sections,
                &vault_note_refs,
                &note_heading_refs,
                &note_section_refs,
                &heading_section_refs,
                &heading_parent_refs,
                &all_tags,
                &note_tag_refs,
                &section_tag_refs,
                &wl_note_refs,
                &wl_head_refs,
            )
            .context("bulk_vault_reindex_write")?
    };

    // Persist genuinely-unresolved wikilinks so broken-links surfaces them.
    // Dedup by uid first (many identical `[[missing]]` links in one section share
    // a uid), then batch-insert on a single connection — a per-row insert here
    // opened a fresh connection per link and made a note with thousands of
    // unresolved links take tens of seconds to a hang.
    {
        let mut seen = std::collections::HashSet::new();
        unresolved_records.retain(|(uid, ..)| seen.insert(uid.clone()));
        if let Err(e) = store.batch_insert_unresolved_wikilinks(&unresolved_records) {
            tracing::warn!("failed to record unresolved wikilinks: {e}");
        }
    }

    resolve_pb.finish_and_clear();

    // ── Pass 3: F11 typed Note→Note relationships ──────────────────────────
    // Derive Supersedes / DependsOn / CausedBy / RelatesTo edges from
    // frontmatter keys and heading-grouped wikilinks. Generic ungrouped
    // wikilinks are untouched (no regression on WIKILINK edges).
    let typed_edges = derive_typed_edges(&note_contexts, &lookup);
    if !typed_edges.is_empty()
        && let Err(e) = store.batch_insert_edges(&typed_edges)
    {
        tracing::warn!("failed to insert typed relationship edges: {e}");
    }

    // nw-003: record the indexed SHA on the repo's Repo node (server-mode vault
    // path only). The markdown indexer above only writes Note/Section/Heading
    // nodes and never touches the Repo row, so without this the row keeps an
    // empty indexed_sha and the worker's up-to-date short-circuit never fires —
    // re-indexing the whole vault every poll. `vault_name` is the repo URL in
    // this path; recorded under the same write gate as the vault commit.
    if let Some(sha) = record_repo_sha {
        record_repo_indexed_sha(store, instance_id, vault_name, sha)?;
    }

    // Advance + persist the graph generation for this vault mutation,
    // mirroring the code path (`index.rs`). Without it the trigram posting
    // table's staleness check is blind to an in-place vault edit: a section
    // delete+recreate keeps the candidate-node count identical, so
    // `regex_search` would trust stale postings and silently drop new/edited
    // note content. `bump_and_persist_generation` persists to the
    // `<db>.generation` sidecar for persistent stores and just bumps for
    // in-memory ones.
    //
    // nw-289: wrapped for the same reason as the incremental path — see
    // `advancing_generation_rebinding_manifests`.
    crate::manifest::advancing_generation_rebinding_manifests(store, || {
        store.bump_and_persist_generation();
    });

    // ── Summary ───────────────────────────────────────────────────────────
    let elapsed = started.elapsed();
    eprintln!(
        "Done: {} notes, {} headings, {} sections, {} tags, {} wikilinks ({:.1}s)",
        notes_count,
        headings_count,
        sections_count,
        tags_count,
        wikilinks_resolved,
        elapsed.as_secs_f64(),
    );

    Ok(MarkdownRefreshResult {
        index: MarkdownIndexResult {
            vault_uid: v_uid,
            vault_name: vault_name.to_string(),
            notes_count,
            headings_count,
            sections_count,
            tags_count,
            wikilinks_resolved,
            wikilinks_unresolved,
            skipped,
        },
        notes_deleted,
    })
}

// ── Pass-2 support: wikilink resolution ────────────────────────────────────

/// Per-note context accumulated during pass 1 for use in pass 2.
struct NoteContext {
    note_uid: String,
    rel_path: String,
    title: String,
    folder: String,
    aliases: Vec<String>,
    heading_uids: Vec<String>,
    heading_slugs: Vec<String>,
    section_uids: Vec<String>,
    wikilinks: Vec<RawWikilink>,
    tags: Vec<RawTag>,
    /// Parsed frontmatter (F11: source of `supersedes:`/`depends_on:` etc.).
    frontmatter: serde_json::Value,
    /// Lowercased heading text for each section index (F11: detects
    /// "Supersedes" / "Depends on" / "See also" groups). `None` for the
    /// preamble or a section with no owning heading.
    section_heading_text: Vec<Option<String>>,
}

/// Candidate target of a wikilink resolution. Carries the priority-tier
/// confidence so the caller can split it across ambiguous candidates.
#[derive(Debug, Clone)]
struct ResolveCandidate {
    note_uid: String,
    confidence: f32,
}

#[derive(Debug)]
enum ResolveOutcome {
    Resolved(Vec<ResolveCandidate>),
    Unresolved,
}

/// Lookup indices built once over all notes in the vault. Drives the
/// 5-priority wikilink resolver.
/// Resolve `.`/`..` segments in a vault-relative link against the folder the
/// link was written in. Returns `None` if the path escapes the vault root
/// (nw-165).
fn normalize_relative(source_folder: &str, key: &str) -> Option<String> {
    let mut parts: Vec<&str> = if source_folder.is_empty() {
        Vec::new()
    } else {
        source_folder.split('/').filter(|s| !s.is_empty()).collect()
    };
    for segment in key.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

/// Split a vault-relative folder into lowercased path components.
///
/// Empty components are dropped so `""`, `"/"` and `"a//b"` normalise the way
/// callers expect, and the vault root becomes the empty prefix that is an
/// ancestor of every folder.
fn folder_components(folder: &str) -> Vec<String> {
    folder
        .replace('\\', "/")
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| segment.to_lowercase())
        .collect()
}

struct WikilinkLookup<'a> {
    /// Path key → note_uid. Path keys are lowercased, with optional ".md"
    /// stripped, normalised to forward slashes.
    by_path: HashMap<String, &'a str>,
    /// Lowercased title → list of note_uids that have that title.
    by_title: HashMap<String, Vec<&'a str>>,
    /// Lowercased alias → list of note_uids that declare that alias.
    by_alias: HashMap<String, Vec<&'a str>>,
    /// Lowercased filename stem → list of note_uids. Global lookup that
    /// mirrors Obsidian's shortest-path resolution.
    by_stem: HashMap<String, Vec<&'a str>>,
    /// note_uid → folder (relative, forward-slash). For same-folder priority.
    folder_by_note: HashMap<&'a str, &'a str>,
    /// note_uid → heading slug → heading_uid. For anchor resolution.
    headings_by_note: HashMap<&'a str, HashMap<&'a str, &'a str>>,
    /// (folder, lowercased filename stem) → note_uids in that folder.
    ///
    /// Stems ONLY. This map used to hold titles as well, under the same key
    /// space, which broke its own `len() == 1` uniqueness test two ways: a note
    /// whose title equalled its own stem pushed itself twice, and a folder
    /// holding one title-match plus one stem-match looked ambiguous when those
    /// are two different tiers by design (nw-290).
    by_folder_stem: HashMap<(String, String), Vec<&'a str>>,
    /// All known note UIDs (F11: lets frontmatter reference a canonical UID).
    known_uids: HashSet<&'a str>,
}

impl<'a> WikilinkLookup<'a> {
    fn build(notes: &'a [NoteContext]) -> Self {
        let mut by_path: HashMap<String, &'a str> = HashMap::new();
        let mut by_title: HashMap<String, Vec<&'a str>> = HashMap::new();
        let mut by_alias: HashMap<String, Vec<&'a str>> = HashMap::new();
        let mut by_stem: HashMap<String, Vec<&'a str>> = HashMap::new();
        let mut folder_by_note: HashMap<&'a str, &'a str> = HashMap::new();
        let mut headings_by_note: HashMap<&'a str, HashMap<&'a str, &'a str>> = HashMap::new();
        let mut by_folder_stem: HashMap<(String, String), Vec<&'a str>> = HashMap::new();
        let mut known_uids: HashSet<&'a str> = HashSet::new();

        for note in notes {
            known_uids.insert(note.note_uid.as_str());
            // Path forms: "folder/note", "folder/note.md", normalised lowercase.
            let path_lc = note.rel_path.replace('\\', "/").to_lowercase();
            by_path.insert(path_lc.clone(), note.note_uid.as_str());
            if let Some(stripped) = path_lc.strip_suffix(".md") {
                by_path.insert(stripped.to_string(), note.note_uid.as_str());
            } else if let Some(stripped) = path_lc.strip_suffix(".markdown") {
                by_path.insert(stripped.to_string(), note.note_uid.as_str());
            }

            by_title
                .entry(note.title.to_lowercase())
                .or_default()
                .push(note.note_uid.as_str());

            for alias in &note.aliases {
                by_alias
                    .entry(alias.to_lowercase())
                    .or_default()
                    .push(note.note_uid.as_str());
            }

            folder_by_note.insert(note.note_uid.as_str(), note.folder.as_str());

            // Index per-folder lookup keys: lowercased title AND lowercased
            // filename stem. Lets `[[my-note]]` resolve to `folder/my-note.md`
            // even when no other note shares that title globally.
            let folder = note.folder.to_string();
            if let Some(stem) = std::path::Path::new(&note.rel_path)
                .file_stem()
                .and_then(|s| s.to_str())
            {
                let stem_lc = stem.to_lowercase();
                by_folder_stem
                    .entry((folder, stem_lc.clone()))
                    .or_default()
                    .push(note.note_uid.as_str());
                by_stem
                    .entry(stem_lc)
                    .or_default()
                    .push(note.note_uid.as_str());
            }

            let mut heading_map: HashMap<&'a str, &'a str> = HashMap::new();
            for (slug, h_uid) in note.heading_slugs.iter().zip(note.heading_uids.iter()) {
                heading_map.insert(slug.as_str(), h_uid.as_str());
            }
            headings_by_note.insert(note.note_uid.as_str(), heading_map);
        }

        Self {
            by_path,
            by_title,
            by_alias,
            by_stem,
            folder_by_note,
            headings_by_note,
            by_folder_stem,
            known_uids,
        }
    }

    /// Apply the priority resolution scheme to `target`. Confidence decreases
    /// monotonically down the chain — an earlier (more authoritative) tier
    /// must never score below a later one, so downstream consumers can
    /// threshold on confidence without inverting the resolver's own ordering.
    ///
    /// **FILENAME BEFORE TITLE; DIRECTORY PROXIMITY BEFORE GLOBALITY.**
    ///
    /// - Priority 1: path match — target contains `/` and matches a known
    ///   path (lowercased, with/without `.md`) → 1.0.
    /// - Priority 2: same-folder filename stem → 0.95.
    /// - Priority 3: NEAREST-ANCESTOR filename stem → 0.92.
    /// - Priority 4: unique global filename stem (Obsidian shortest-path) → 0.90.
    /// - Priority 5: unique global title → 1.0 when NO file in the vault
    ///   carries that stem, otherwise 0.80.
    /// - Priority 6: alias match → unique 0.7, ambiguous split.
    /// - Priority 7: path-qualified fallback to the last segment → 0.85.
    /// - Priority 8: ambiguous title match → same-folder narrowing 0.5,
    ///   otherwise split across all candidates.
    ///
    /// There is deliberately NO same-folder TITLE tier. Placed above priority 5
    /// it made proximity DECREASE confidence — a title link to a sibling scored
    /// 0.85 while the identical link written from another folder scored 1.0 —
    /// and its only unique job, preferring a co-located note when a title is
    /// ambiguous vault-wide, is already priority 8's narrowing. Adding it would
    /// also push every same-folder title link into `broken_wikilinks`, which is
    /// the surface nw-297 exists to keep readable.
    ///
    /// The title tier used to sit at priority 2, ABOVE every filename tier, and
    /// return 1.0. One note that lost its `# ` heading therefore fell back to
    /// its bare stem as a title, became the unique global title match, and
    /// captured every unqualified `[[Name]]` in the vault — 12 of them across
    /// five unrelated workspaces — at exactly the confidence
    /// `GraphStore::broken_wikilinks` is defined to ignore (`WHERE r.confidence
    /// < 1.0`). The wrong edge was unreportable by construction (nw-290).
    ///
    /// Priority 6's conditional is the precise statement of that: reaching the
    /// title tier at all means NO filename tier matched. If the vault holds no
    /// file with that stem, the title is the only evidence and 1.0 is honest.
    /// If it does, the title match is a guess made AGAINST filename evidence
    /// and must not claim certainty.
    ///
    /// Priority 3 is new (nw-306). Directory scoping used to be exact-folder
    /// equality only, so `**Up:** [[_Overview]]` written one directory below
    /// its hub matched nothing: the same-folder tier saw the wrong folder and
    /// the global-stem tier required vault-wide uniqueness among 21 files named
    /// `_Overview.md`. 38 real hub links were reported broken.
    fn resolve(&self, target: &str, source_folder: &str) -> ResolveOutcome {
        let key = target.trim().replace('\\', "/").to_lowercase();
        // nw-166: markdown links keep their extension (`[x](codebase-recon.md)`),
        // but `by_path` is built from extension-stripped paths and the stem/title
        // tiers never carry one. Without this, a same-folder markdown link
        // resolved to nothing and was reported as a broken link.
        let key = key.strip_suffix(".md").unwrap_or(&key).to_string();
        if key.is_empty() {
            return ResolveOutcome::Unresolved;
        }

        // Priority 1: path match — vault-relative first, then relative to the
        // SOURCE's folder.
        //
        // `by_path` is keyed on full vault-relative paths, so only the first
        // form ever matched. But `[[plans/Rollout Plan]]` written in
        // `Workspaces/ExampleProject/_Overview.md` is Obsidian's RELATIVE-path
        // syntax and means `Workspaces/ExampleProject/plans/Rollout Plan.md`.
        // Every path-qualified link in the vault therefore resolved to nothing —
        // 45 of them, all reported at confidence 0.0 (nw-100).
        //
        // Both forms are exact, unambiguous single-key lookups, so both score
        // 1.0 and priority ordering is preserved.
        if key.contains('/') {
            if let Some(&uid) = self.by_path.get(&key) {
                return ResolveOutcome::Resolved(vec![ResolveCandidate {
                    note_uid: uid.to_string(),
                    confidence: 1.0,
                }]);
            }
            if !source_folder.is_empty() {
                // `by_path` keys are lowercased; `folder` is stored raw.
                let scoped = format!("{}/{key}", source_folder.replace('\\', "/").to_lowercase());
                if let Some(&uid) = self.by_path.get(&scoped) {
                    return ResolveOutcome::Resolved(vec![ResolveCandidate {
                        note_uid: uid.to_string(),
                        confidence: 1.0,
                    }]);
                }
                // nw-165: `.` and `..` segments never matched, because by_path
                // holds only normalized paths. `[[../notes/x]]` and
                // `[[../../../Backlog]]` were reported broken even though the
                // target existed.
                if (key.starts_with("..") || key.contains("/../") || key.contains("./"))
                    && let Some(normalized) =
                        normalize_relative(&source_folder.replace('\\', "/").to_lowercase(), &key)
                    && let Some(&uid) = self.by_path.get(&normalized)
                {
                    return ResolveOutcome::Resolved(vec![ResolveCandidate {
                        note_uid: uid.to_string(),
                        confidence: 1.0,
                    }]);
                }
            }
        }

        // Priority 2: same-folder filename stem.
        // A wikilink `[[target]]` in note F/x.md resolves to F/target.md.
        // This is the tier that lets sibling-relative links work without
        // forcing the user to add aliases or write the full path, and it now
        // runs ABOVE the title tiers — a filename is a stronger claim on a bare
        // `[[Name]]` than a heading is (nw-290).
        //
        // It must stay at 0.95, not 1.0: `broken_wikilinks` selects
        // `confidence < 1.0`, and `a_lower_tier_resolution_is_not_broken`
        // depends on a same-folder match remaining visible there as a
        // resolved-but-lower-tier row.
        if let Some(uids) = self
            .by_folder_stem
            .get(&(source_folder.to_string(), key.clone()))
            && uids.len() == 1
        {
            return ResolveOutcome::Resolved(vec![ResolveCandidate {
                note_uid: uids[0].to_string(),
                confidence: 0.95,
            }]);
        }

        // Priority 3: nearest-ancestor filename stem (nw-306).
        if let Some(uid) = self.nearest_ancestor_stem(&key, source_folder) {
            return ResolveOutcome::Resolved(vec![ResolveCandidate {
                note_uid: uid,
                confidence: 0.92,
            }]);
        }

        // Priority 4: global filename-stem match (Obsidian shortest-path).
        if let Some(uids) = self.by_stem.get(&key)
            && uids.len() == 1
        {
            return ResolveOutcome::Resolved(vec![ResolveCandidate {
                note_uid: uids[0].to_string(),
                confidence: 0.9,
            }]);
        }

        // Priority 5: unique global title. Full confidence ONLY when no file in
        // the vault carries this stem — see the tier table above for why.
        if let Some(uids) = self.by_title.get(&key)
            && uids.len() == 1
        {
            let confidence = if self.by_stem.contains_key(&key) {
                0.80
            } else {
                1.0
            };
            return ResolveOutcome::Resolved(vec![ResolveCandidate {
                note_uid: uids[0].to_string(),
                confidence,
            }]);
        }

        // Priority 6: alias match (unique → 0.7, ambiguous → split).
        if let Some(uids) = self.by_alias.get(&key) {
            if uids.len() == 1 {
                return ResolveOutcome::Resolved(vec![ResolveCandidate {
                    note_uid: uids[0].to_string(),
                    confidence: 0.7,
                }]);
            }
            return ResolveOutcome::Resolved(
                uids.iter()
                    .map(|u| ResolveCandidate {
                        note_uid: u.to_string(),
                        confidence: 0.7,
                    })
                    .collect(),
            );
        }

        // nw-165: path-qualified fallback to the filename stem, which is what
        // Obsidian does. by_stem / by_title / by_folder_name are keyed on bare
        // names and can never contain a slash, so a path-qualified key that
        // missed by_path above could not match ANY later tier and was reported
        // as a genuinely broken link -- 40 such links in the reference vault
        // had an existing target.
        if key.contains('/')
            && let Some(base) = key.rsplit('/').find(|segment| !segment.is_empty())
            && base != key
        {
            // Below the exact-path tiers: only the filename was corroborated,
            // not the path component.
            if let Some(uids) = self.by_stem.get(base)
                && uids.len() == 1
            {
                return ResolveOutcome::Resolved(vec![ResolveCandidate {
                    note_uid: uids[0].to_string(),
                    confidence: 0.85,
                }]);
            }
            if let Some(uids) = self.by_title.get(base)
                && uids.len() == 1
            {
                return ResolveOutcome::Resolved(vec![ResolveCandidate {
                    note_uid: uids[0].to_string(),
                    confidence: 0.85,
                }]);
            }
        }

        // Priority 8: ambiguous title match (was bundled inside the unique
        // title tier; now it's the last-resort tier so we always try alias /
        // same-folder first when the global title is non-unique).
        //
        // Deliberately NOT extended to ambiguous STEMS. Two same-named files in
        // unrelated folders are exactly the case the resolver should decline
        // rather than guess at — the ancestor tier above already narrows the
        // cases where the directory tree carries a real signal.
        if let Some(uids) = self.by_title.get(&key) {
            // Try same-folder narrowing inside the title-multiple case.
            let same_folder: Vec<&&str> = uids
                .iter()
                .filter(|u| self.folder_by_note.get(**u).copied().unwrap_or("") == source_folder)
                .collect();
            if same_folder.len() == 1 {
                return ResolveOutcome::Resolved(vec![ResolveCandidate {
                    note_uid: same_folder[0].to_string(),
                    confidence: 0.5,
                }]);
            }
            // Still ambiguous — split confidence across all candidates.
            return ResolveOutcome::Resolved(
                uids.iter()
                    .map(|u| ResolveCandidate {
                        note_uid: u.to_string(),
                        confidence: 1.0,
                    })
                    .collect(),
            );
        }

        ResolveOutcome::Unresolved
    }

    /// Of the notes whose filename stem is `key`, return the one living in the
    /// DEEPEST folder that is an ancestor of `source_folder`. `None` when no
    /// candidate is an ancestor, or when two candidates tie at that depth.
    ///
    /// This is the vault's strongest structural signal — "the one nearest in
    /// the directory tree" — and nothing consulted it: the only directory tier
    /// was exact-folder string equality, and the global-stem tier required
    /// vault-wide uniqueness (nw-306).
    ///
    /// Comparison is COMPONENT-WISE, not `str::starts_with`: the latter would
    /// accept `Workspaces/Cortina` as an ancestor of
    /// `Workspaces/Cortina Precision/plans`. Both sides are lowercased and
    /// forward-slashed the way priority 1 already normalises paths.
    fn nearest_ancestor_stem(&self, key: &str, source_folder: &str) -> Option<String> {
        let uids = self.by_stem.get(key)?;
        let source = folder_components(source_folder);

        let mut best_depth: Option<usize> = None;
        let mut best: Option<&str> = None;
        let mut tied = false;
        for uid in uids {
            let folder = self.folder_by_note.get(uid).copied().unwrap_or("");
            let candidate = folder_components(folder);
            // The vault root (no components) is an ancestor of everything.
            if candidate.len() > source.len() || candidate[..] != source[..candidate.len()] {
                continue;
            }
            match best_depth {
                Some(depth) if candidate.len() < depth => {}
                Some(depth) if candidate.len() == depth => tied = true,
                _ => {
                    best_depth = Some(candidate.len());
                    best = Some(uid);
                    tied = false;
                }
            }
        }
        // Two notes with the same stem at the same ancestor depth carry equal
        // evidence. Declining is the honest answer; guessing is nw-290.
        if tied {
            return None;
        }
        best.map(|uid| uid.to_string())
    }

    fn find_heading(&self, note_uid: &str, slug: &str) -> Option<String> {
        let headings = self.headings_by_note.get(note_uid)?;
        headings.get(slug).map(|s| s.to_string())
    }

    /// Resolve a single frontmatter reference (a note UID, a title, or a path)
    /// to exactly one note UID. Used by F11 typed-edge derivation. Tries, in
    /// order: exact known note UID, unique path match, unique title match.
    /// Returns `None` when the reference is unknown or ambiguous.
    fn resolve_one(&self, reference: &str) -> Option<String> {
        let raw = reference.trim();
        if raw.is_empty() {
            return None;
        }
        // 1. Direct note UID (frontmatter may carry the canonical UID).
        if self.known_uids.contains(raw) {
            return Some(raw.to_string());
        }
        let key = raw.replace('\\', "/").to_lowercase();
        // 2. Path match (with/without extension).
        if let Some(&uid) = self.by_path.get(&key) {
            return Some(uid.to_string());
        }
        // 3. Unique title match.
        if let Some(uids) = self.by_title.get(&key)
            && uids.len() == 1
        {
            return Some(uids[0].to_string());
        }
        None
    }
}

/// F11: derive typed Note→Note relationship edges from frontmatter keys and
/// heading-grouped wikilinks.
///
/// Derivation rules:
/// - Frontmatter list keys map directly to edge types (values are note UIDs or
///   titles): `supersedes:` → [`EdgeType::Supersedes`], `depends_on:` →
///   [`EdgeType::DependsOn`], `caused_by:` → [`EdgeType::CausedBy`],
///   `relates_to:` → [`EdgeType::RelatesTo`].
/// - A wikilink appearing under a section whose heading (case-insensitively)
///   is "Supersedes" → [`EdgeType::Supersedes`]; "Depends on"/"Depends" →
///   [`EdgeType::DependsOn`]; "See also"/"Related" → [`EdgeType::RelatesTo`].
/// - Ungrouped wikilinks stay generic WIKILINK edges (untouched here).
///
/// Self-edges and duplicates are dropped. Confidence is 1.0 (explicit author
/// intent). Frontmatter references that don't resolve to a unique note are
/// skipped silently (surfaced later by `dangling_relationships` lint only when
/// the edge was created with a known target).
fn derive_typed_edges(notes: &[NoteContext], lookup: &WikilinkLookup<'_>) -> Vec<ResolvedEdge> {
    use std::collections::HashSet;

    let mut edges: Vec<ResolvedEdge> = Vec::new();
    let mut seen: HashSet<(String, String, &'static str)> = HashSet::new();

    let mut push_edge = |src: &str, tgt: &str, et: EdgeType, edges: &mut Vec<ResolvedEdge>| {
        if src == tgt {
            return;
        }
        let key = (src.to_string(), tgt.to_string(), et.rel_table_name());
        if !seen.insert(key) {
            return;
        }
        edges.push(ResolvedEdge {
            source_uid: src.to_string(),
            target_uid: tgt.to_string(),
            edge_type: et,
            confidence: 1.0,
            link_type: None,
            evidence: Vec::new(),
        });
    };

    for ctx in notes {
        // (a) Frontmatter keys.
        for (fm_key, edge_type) in [
            ("supersedes", EdgeType::Supersedes),
            ("depends_on", EdgeType::DependsOn),
            ("caused_by", EdgeType::CausedBy),
            ("relates_to", EdgeType::RelatesTo),
        ] {
            for reference in frontmatter_list(&ctx.frontmatter, fm_key) {
                if let Some(target_uid) = lookup.resolve_one(&reference) {
                    push_edge(&ctx.note_uid, &target_uid, edge_type, &mut edges);
                }
            }
        }

        // (b) Heading-grouped wikilinks.
        for wl in &ctx.wikilinks {
            let Some(heading) = ctx
                .section_heading_text
                .get(wl.section_idx)
                .and_then(|h| h.as_deref())
            else {
                continue;
            };
            let Some(edge_type) = heading_to_edge_type(heading) else {
                continue;
            };
            // Resolve the wikilink target to a note via the shared resolver.
            if let ResolveOutcome::Resolved(candidates) = lookup.resolve(&wl.target, &ctx.folder)
                && candidates.len() == 1
            {
                push_edge(
                    &ctx.note_uid,
                    &candidates[0].note_uid,
                    edge_type,
                    &mut edges,
                );
            }
        }
    }

    edges
}

/// Map a (lowercased) section heading to a typed edge, or `None` if the heading
/// is not a recognised relationship group.
fn heading_to_edge_type(heading_lc: &str) -> Option<EdgeType> {
    match heading_lc.trim() {
        "supersedes" => Some(EdgeType::Supersedes),
        "depends on" | "depends" | "dependencies" => Some(EdgeType::DependsOn),
        "see also" | "related" => Some(EdgeType::RelatesTo),
        _ => None,
    }
}

/// Extract a frontmatter value as a list of strings. Accepts a YAML/JSON array
/// (`supersedes: [A, B]`) or a single scalar (`supersedes: A`). Non-string
/// elements are skipped. Returns empty when the key is absent.
fn frontmatter_list(frontmatter: &serde_json::Value, key: &str) -> Vec<String> {
    match frontmatter.get(key) {
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
            .filter(|s| !s.is_empty())
            .collect(),
        Some(serde_json::Value::String(s)) => {
            let t = s.trim();
            if t.is_empty() {
                vec![]
            } else {
                vec![t.to_string()]
            }
        }
        _ => vec![],
    }
}

/// Slugify a wikilink anchor using the same algorithm the parser uses for
/// heading slugs. Reuses the parser's logic to avoid drift.
fn slugify_anchor(anchor: &str) -> String {
    nestweaver_parser::markdown::slugify(anchor)
}

/// Render a `SystemTime` as RFC 3339-ish UTC string. Falls back to None on
/// pre-epoch dates.
fn format_system_time(t: std::time::SystemTime) -> Option<String> {
    let duration = t.duration_since(std::time::UNIX_EPOCH).ok()?;
    let secs = duration.as_secs() as i64;
    // Minimal RFC-3339 formatter to avoid pulling in chrono.
    let (year, month, day, hour, minute, second) = secs_to_ymd_hms(secs);
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z"
    ))
}

/// Convert Unix seconds to (year, month, day, hour, minute, second) UTC.
/// Civil-from-days algorithm by Howard Hinnant — accurate for 1900..9999.
fn secs_to_ymd_hms(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let hour = (secs_of_day / 3600) as u32;
    let minute = ((secs_of_day % 3600) / 60) as u32;
    let second = (secs_of_day % 60) as u32;

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y } as i32;
    (year, m, d, hour, minute, second)
}

// ── Taxonomy alias ingestion ───────────────────────────────────────────────

/// Candidate taxonomy file locations within a vault root, in priority order.
const TAXONOMY_FILES: &[&str] = &["_taxonomy.md", "taxonomy.md", "_brain/taxonomy.md"];

/// Parse alias mappings from a vault's taxonomy file, reading via a
/// [`ContentReader`]. This is the reader-agnostic version used by both
/// local-mode and server-mode indexers — server mode has no on-disk working
/// tree, so direct `fs::read_to_string` would fail.
fn load_taxonomy_aliases_from_reader(
    reader: &dyn crate::content_reader::ContentReader,
) -> HashMap<String, Vec<String>> {
    let mut aliases: HashMap<String, Vec<String>> = HashMap::new();

    for name in TAXONOMY_FILES {
        let Ok(content) = reader.read_file(Path::new(name)) else {
            continue;
        };

        parse_taxonomy_content(&content, &mut aliases);

        // Only process the first taxonomy file found.
        break;
    }

    aliases
}

/// Parse alias mappings from a vault's taxonomy file.
///
/// Returns a map of `canonical_name → [alias1, alias2, ...]`. Two formats
/// are supported (both can coexist in the same file):
///
/// **YAML frontmatter** — the frontmatter must have an `aliases` mapping where
/// each key is a canonical name and each value is a list of alias strings:
///
/// ```yaml
/// ---
/// aliases:
///   Authentication: [Auth, AuthSvc]
///   Device Pairing: [Pairing, BT Pairing]
/// ---
/// ```
///
/// **Inline arrow notation** — body lines of the form `Alias -> CanonicalName`:
///
/// ```markdown
/// Auth -> Authentication
/// BT Pairing -> Device Pairing
/// ```
///
/// Both formats add to the same map; the frontmatter format is processed first.
fn load_taxonomy_aliases(vault_root: &Path) -> HashMap<String, Vec<String>> {
    let mut aliases: HashMap<String, Vec<String>> = HashMap::new();

    for name in TAXONOMY_FILES {
        let path = vault_root.join(name);
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };

        parse_taxonomy_content(&content, &mut aliases);

        // Only process the first taxonomy file found.
        break;
    }

    aliases
}

/// Parse taxonomy alias content from a string, appending to `aliases`.
/// Shared by both the filesystem and reader-based taxonomy loaders.
fn parse_taxonomy_content(content: &str, aliases: &mut HashMap<String, Vec<String>>) {
    // Parse YAML frontmatter for `aliases:` mapping.
    if let Some(fm) = extract_frontmatter(content)
        && let Ok(yaml) = serde_yaml::from_str::<serde_yaml::Value>(fm)
        && let Some(mapping) = yaml.get("aliases").and_then(|v| v.as_mapping())
    {
        for (key, value) in mapping {
            if let (Some(canonical), Some(alias_list)) = (key.as_str(), value.as_sequence()) {
                let alts: Vec<String> = alias_list
                    .iter()
                    .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
                    .filter(|s| !s.is_empty())
                    .collect();
                if !alts.is_empty() {
                    aliases
                        .entry(canonical.trim().to_string())
                        .or_default()
                        .extend(alts);
                }
            }
        }
    }

    // Parse inline `Alias -> CanonicalName` lines from the body.
    let body = skip_frontmatter(content);
    for line in body.lines() {
        let trimmed = line.trim();
        if let Some((alias_part, canonical_part)) = trimmed.split_once("->") {
            let alias = alias_part.trim();
            let canonical = canonical_part.trim();
            if !alias.is_empty() && !canonical.is_empty() {
                aliases
                    .entry(canonical.to_string())
                    .or_default()
                    .push(alias.to_string());
            }
        }
    }
}

/// Extract the YAML frontmatter string (between `---` delimiters) from a
/// markdown source. Returns `None` when no frontmatter block is present.
fn extract_frontmatter(source: &str) -> Option<&str> {
    let rest = source
        .strip_prefix("---\n")
        .or_else(|| source.strip_prefix("---\r\n"))?;
    let mut offset = 0;
    for line in rest.lines() {
        if line.trim_end() == "---" {
            return Some(&rest[..offset]);
        }
        offset += line.len() + 1;
    }
    None
}

/// Return the body portion of a markdown source (after frontmatter, if any).
fn skip_frontmatter(source: &str) -> &str {
    let Some(rest) = source
        .strip_prefix("---\n")
        .or_else(|| source.strip_prefix("---\r\n"))
    else {
        return source;
    };
    let mut offset = 0;
    for line in rest.lines() {
        if line.trim_end() == "---" {
            let body_start = offset + line.len();
            let body = &rest[body_start..];
            return body
                .strip_prefix('\n')
                .or_else(|| body.strip_prefix("\r\n"))
                .unwrap_or(body);
        }
        offset += line.len() + 1;
    }
    // No closing delimiter — treat entire source as body.
    source
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn make_vault(files: &[(&str, &str)]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("vault");
        fs::create_dir_all(&root).unwrap();
        for (rel, content) in files {
            let path = root.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, content).unwrap();
        }
        (dir, root)
    }

    /// True when the current process bypasses filesystem permission bits.
    #[cfg(unix)]
    fn running_as_root() -> bool {
        // SAFETY: `geteuid` takes no arguments, touches no memory and cannot fail.
        unsafe { libc::geteuid() == 0 }
    }

    /// nw-287 (CRITICAL, data loss): a full refresh whose scan could not read
    /// the vault DIRECTORY must fail. It must not commit the stale-drop, and
    /// it must not report success. Confirmed 3x black-box: rc=0,
    /// "dropped 2 stale note(s), reindexed 0", and `brain search` then empty.
    #[cfg(unix)]
    #[test]
    fn refresh_of_an_unreadable_vault_directory_refuses_the_stale_drop() {
        use std::os::unix::fs::PermissionsExt;

        if running_as_root() {
            return;
        }

        let (_dir, root) = make_vault(&[
            ("f1.md", "# F1\n\ncontent one\n"),
            ("f2.md", "# F2\n\ncontent two\n"),
        ]);
        let store = GraphStore::in_memory().unwrap();
        let db_path = root.join("unused.lbug");

        let first = index_markdown_directory_with_store_and_deletion_count(
            &store,
            &root,
            &db_path,
            "default",
            "v",
            &[],
        )
        .unwrap();
        assert_eq!(
            first.index.notes_count, 2,
            "precondition: two notes indexed"
        );

        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o000)).unwrap();
        let observed = index_markdown_directory_with_store_and_deletion_count(
            &store,
            &root,
            &db_path,
            "default",
            "v",
            &[],
        );
        // Restore BEFORE asserting so a failure still lets TempDir clean up.
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();

        match observed {
            Err(_) => {}
            Ok(result) => panic!(
                "refresh reported SUCCESS on an unreadable vault directory: \
                 dropped {} stale note(s), reindexed {}. This is nw-287: the \
                 EACCES is swallowed in FilesystemReader::list_files and the \
                 empty scan is committed as a whole-vault deletion.",
                result.notes_deleted, result.index.notes_count
            ),
        }

        // The graph must be intact whether the refresh errored or not.
        assert_eq!(
            store.list_notes(None).unwrap().len(),
            2,
            "an unreadable vault directory must never delete indexed notes"
        );
    }

    /// nw-287, the case `list_files` alone cannot catch: an UNMOUNTED volume
    /// presents as an empty-but-readable directory. A total-replacement
    /// refresh must not read "I saw no files" as "every file was deleted".
    #[test]
    fn refresh_that_scans_zero_notes_refuses_to_empty_a_populated_vault() {
        let (_dir, root) = make_vault(&[
            ("f1.md", "# F1\n\ncontent one\n"),
            ("f2.md", "# F2\n\ncontent two\n"),
        ]);
        let store = GraphStore::in_memory().unwrap();
        let db_path = root.join("unused.lbug");
        index_markdown_directory_with_store_and_deletion_count(
            &store,
            &root,
            &db_path,
            "default",
            "v",
            &[],
        )
        .unwrap();

        // Simulate the mount going away: the directory is readable and empty.
        std::fs::remove_file(root.join("f1.md")).unwrap();
        std::fs::remove_file(root.join("f2.md")).unwrap();

        let observed = index_markdown_directory_with_store_and_deletion_count(
            &store,
            &root,
            &db_path,
            "default",
            "v",
            &[],
        );
        assert!(
            observed.is_err(),
            "an empty scan over a vault that held notes must fail closed, not \
             commit a whole-vault deletion (nw-287)"
        );
        assert_eq!(
            store.list_notes(None).unwrap().len(),
            2,
            "the previously indexed notes must survive the refusal"
        );
    }

    /// The counterpart that keeps the empty-scan guard from becoming a wall: a
    /// vault that was ALREADY empty must still refresh cleanly, and so must a
    /// vault being indexed for the first time.
    #[test]
    fn refresh_of_a_genuinely_empty_vault_still_succeeds() {
        let (_dir, root) = make_vault(&[]);
        let store = GraphStore::in_memory().unwrap();
        let db_path = root.join("unused.lbug");

        let first = index_markdown_directory_with_store_and_deletion_count(
            &store,
            &root,
            &db_path,
            "default",
            "v",
            &[],
        )
        .expect("first index of an empty vault is not a deletion");
        assert_eq!(first.index.notes_count, 0);

        let second = index_markdown_directory_with_store_and_deletion_count(
            &store,
            &root,
            &db_path,
            "default",
            "v",
            &[],
        )
        .expect("re-refreshing an already-empty vault deletes nothing");
        assert_eq!(second.index.notes_count, 0);
        assert_eq!(second.notes_deleted, 0);
    }

    /// nw-290: a note that falls back to a BARE title must not capture another
    /// workspace's unqualified basename link. `Workspaces/NW/Backlog.md` has no
    /// `# ` heading, so its title is the bare word "Backlog" and it is the
    /// unique global title match — which today short-circuits above every
    /// filename tier, at confidence 1.0.
    #[test]
    fn a_bare_title_does_not_steal_a_sibling_filename_link() {
        let (_dir, root) = make_vault(&[
            (
                "Workspaces/Cortina/_Overview.md",
                "# Cortina — Overview\n\n- [[Backlog]] — execution backlog\n",
            ),
            (
                "Workspaces/Cortina/Backlog.md",
                "# Cortina — Backlog\n\nbody\n",
            ),
            // No `# ` heading: title falls back to the bare stem "Backlog".
            ("Workspaces/NW/Backlog.md", "some body, no heading\n"),
        ]);
        let (result, store) = index_markdown_directory_in_memory(&root, "default", "v").unwrap();
        assert_eq!(result.wikilinks_resolved, 1);

        let notes = store.list_notes(None).unwrap();
        let uid_of = |frag: &str| {
            notes
                .iter()
                .find(|n| n.file_path.contains(frag))
                .map(|n| n.uid.clone())
                .unwrap()
        };
        let edges = store.note_wikilink_edges().unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].0, uid_of("Workspaces/Cortina/_Overview.md"));
        assert_eq!(
            edges[0].1,
            uid_of("Workspaces/Cortina/Backlog.md"),
            "nw-290: the sibling FILENAME must win; a bare title in another \
             workspace must not capture the link"
        );
    }

    /// nw-290, second half: when a title match is the surviving evidence AND
    /// files with that stem exist, it must not claim confidence 1.0 — that is
    /// the band `broken_wikilinks` is defined to ignore (read.rs `WHERE
    /// r.confidence < 1.0`), so a wrong-but-confident edge is unreportable.
    #[test]
    fn a_title_match_competing_with_filenames_never_scores_full_confidence() {
        let (_dir, root) = make_vault(&[
            ("logs/day.md", "# Day\n\nSee [[Backlog]].\n"),
            ("Workspaces/NW/Backlog.md", "no heading here\n"),
            ("Workspaces/Orbit/Backlog.md", "# Orbit — Backlog\n"),
        ]);
        let (_result, store) = index_markdown_directory_in_memory(&root, "default", "v").unwrap();
        let suspect = store.broken_wikilinks().unwrap();
        let row = suspect
            .iter()
            .find(|r| r.wikilink_text.eq_ignore_ascii_case("Backlog"))
            .expect(
                "nw-290: a 1-of-2 filename guess must be visible to broken-links; \
                 today it resolves at 1.0 and never appears here",
            );
        assert!(row.confidence < 1.0, "got {}", row.confidence);
    }

    /// nw-306: `**Up:** [[_Overview]]` from a subfolder must resolve to the
    /// nearest ancestor's `_Overview.md`. Today the only directory tier is
    /// exact-folder equality, and the global-stem tier requires vault-wide
    /// uniqueness, so 38 real hub links are reported broken.
    #[test]
    fn an_unqualified_basename_resolves_to_the_nearest_ancestor() {
        let (_dir, root) = make_vault(&[
            ("Workspaces/Cortina/_Overview.md", "# Cortina — Overview\n"),
            ("Workspaces/Orbit/_Overview.md", "# Orbit — Overview\n"),
            (
                "Workspaces/Cortina/plans/astro-homepage.md",
                "# Astro Homepage\n\n**Up:** [[_Overview]]\n",
            ),
        ]);
        let (result, store) = index_markdown_directory_in_memory(&root, "default", "v").unwrap();
        assert_eq!(
            result.wikilinks_unresolved, 0,
            "nw-306: a hub `Up:` link one directory down must not read as broken"
        );
        assert_eq!(result.wikilinks_resolved, 1);

        let notes = store.list_notes(None).unwrap();
        let uid_of = |frag: &str| {
            notes
                .iter()
                .find(|n| n.file_path.contains(frag))
                .map(|n| n.uid.clone())
                .unwrap()
        };
        let edges = store.note_wikilink_edges().unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(
            edges[0].1,
            uid_of("Workspaces/Cortina/_Overview.md"),
            "the NEAREST ancestor wins; Orbit's identically-named hub must not"
        );
    }

    /// The guard that keeps the nw-306 tier from becoming nw-290: an ancestor
    /// tier must not fire when NO candidate is an ancestor. Companion to the
    /// existing `ambiguous_global_stem_stays_unresolved`.
    #[test]
    fn the_ancestor_tier_does_not_fire_for_unrelated_directories() {
        let (_dir, root) = make_vault(&[
            ("logs/daily.md", "# Daily\n\nSee [[target]].\n"),
            ("f/target.md", "# Alpha\n"),
            ("g/target.md", "# Beta\n"),
        ]);
        let (result, _) = index_markdown_directory_in_memory(&root, "default", "v").unwrap();
        assert_eq!(result.wikilinks_resolved, 0);
        assert_eq!(
            result.wikilinks_unresolved, 1,
            "no candidate is an ancestor of logs/, so the resolver must still decline"
        );
    }

    /// nw-306, the depth tie-break: two ancestors both carry the stem, so the
    /// DEEPEST must win. A prefix comparison that stopped at "is an ancestor"
    /// would be ambiguous here and decline.
    #[test]
    fn the_nearest_ancestor_wins_over_a_shallower_one() {
        let (_dir, root) = make_vault(&[
            ("_Overview.md", "# Vault Root Overview\n"),
            ("Workspaces/Cortina/_Overview.md", "# Cortina — Overview\n"),
            (
                "Workspaces/Cortina/plans/astro.md",
                "# Astro\n\n**Up:** [[_Overview]]\n",
            ),
        ]);
        let (result, store) = index_markdown_directory_in_memory(&root, "default", "v").unwrap();
        assert_eq!(result.wikilinks_resolved, 1);
        assert_eq!(result.wikilinks_unresolved, 0);

        let notes = store.list_notes(None).unwrap();
        let cortina = notes
            .iter()
            .find(|n| n.file_path.contains("Workspaces/Cortina/_Overview.md"))
            .map(|n| n.uid.clone())
            .unwrap();
        let edges = store.note_wikilink_edges().unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(
            edges[0].1, cortina,
            "the nearest ancestor must beat the vault-root one"
        );
    }

    /// nw-298 / F-VAULT-5: a string present ONLY in YAML frontmatter is
    /// invisible to `regex_search` / `count_patterns` while `brain_search`
    /// finds it. Two query surfaces over one database must not disagree about
    /// whether a string exists.
    #[test]
    fn regex_search_sees_frontmatter_only_text() {
        let (_dir, root) = make_vault(&[(
            "Backlog.md",
            "---\nitems:\n  - id: nw-231\n    note: 'saves-and-exits on device'\n---\n             # Backlog\n\nBody text only.\n",
        )]);
        let (_res, store) = index_markdown_directory_in_memory(&root, "default", "v").unwrap();

        let hits = store
            .regex_search("saves-and-exits", None, None, Some(100), Some(5_000))
            .unwrap();
        assert!(
            !hits.results.is_empty(),
            "frontmatter text must be reachable from the exact-match surface; \
             brain_search finds it and regex_search reports posting_hits: 0"
        );

        let counts = store
            .count_patterns(&["nw-231".to_string()], None, None)
            .unwrap();
        assert_eq!(
            counts[0].files_matched, 1,
            "count_patterns must not report a confident zero for text that is present"
        );
    }

    /// nw-298, the half that makes the visibility UNSTABLE rather than merely
    /// asymmetric: a YAML-shaped pattern must match too. Indexing the stored
    /// `Note.frontmatter` JSON column would satisfy the bare-token test above
    /// and still fail this one.
    #[test]
    fn regex_search_matches_yaml_shaped_frontmatter_patterns() {
        let (_dir, root) = make_vault(&[(
            "Backlog.md",
            "---\nid: nw-231\nstatus: ready\n---\n# Backlog\n\nBody.\n",
        )]);
        let (_res, store) = index_markdown_directory_in_memory(&root, "default", "v").unwrap();

        let hits = store
            .regex_search(r"(?m)^status: ready$", None, None, Some(100), Some(5_000))
            .unwrap();
        assert!(
            !hits.results.is_empty(),
            "the RAW frontmatter text must be indexed, not the JSON re-encoding \
             — a YAML-shaped regex is exactly what the vault's own backlog \
             queries look like (nw-298)"
        );
    }

    #[test]
    fn indexes_simple_vault() {
        let (_dir, root) = make_vault(&[
            ("intro.md", "# Intro\n\nHello world.\n"),
            ("notes/two.md", "# Note Two\n\nbody\n"),
        ]);

        let (result, store) =
            index_markdown_directory_in_memory(&root, "default", "test-vault").unwrap();
        assert_eq!(result.notes_count, 2);
        // Two H1s, two body sections.
        assert_eq!(result.headings_count, 2);
        assert_eq!(result.sections_count, 2);
        assert_eq!(result.tags_count, 0);
        assert_eq!(result.wikilinks_resolved, 0);
        assert!(result.skipped.is_empty());

        let notes = store.list_notes(None).unwrap();
        assert_eq!(notes.len(), 2);
        let titles: Vec<_> = notes.iter().map(|n| n.title.as_str()).collect();
        assert!(titles.contains(&"Intro"));
        assert!(titles.contains(&"Note Two"));

        // Verify outline round-trip for one note.
        let intro = notes.iter().find(|n| n.title == "Intro").unwrap();
        let headings = store.headings_in_note(&intro.uid).unwrap();
        assert_eq!(headings.len(), 1);
        assert_eq!(headings[0].text, "Intro");
        let sections = store.sections_in_note(&intro.uid).unwrap();
        assert_eq!(sections.len(), 1);
        assert_eq!(
            sections[0].heading_uid.as_deref(),
            Some(headings[0].uid.as_str())
        );
    }

    #[test]
    fn indexes_nested_headings_and_parent_edges() {
        let src = "\
# Top
top body

## Sub A
sub a body

## Sub B
sub b body
";
        let (_dir, root) = make_vault(&[("nested.md", src)]);
        let (result, store) = index_markdown_directory_in_memory(&root, "default", "v").unwrap();
        assert_eq!(result.headings_count, 3);
        // 3 sections: Top, Sub A, Sub B.
        assert_eq!(result.sections_count, 3);

        let notes = store.list_notes(None).unwrap();
        let h = store.headings_in_note(&notes[0].uid).unwrap();
        assert_eq!(h[0].level, 1);
        assert_eq!(h[1].level, 2);
        assert_eq!(h[2].level, 2);
    }

    #[test]
    fn skips_obsidian_directory() {
        let (_dir, root) = make_vault(&[
            ("real.md", "# Real\n"),
            (".obsidian/workspace.json", "should-not-be-indexed"),
            (".obsidian/notes/leaked.md", "# Leaked\n"),
        ]);

        let (result, _) = index_markdown_directory_in_memory(&root, "default", "x").unwrap();
        assert_eq!(result.notes_count, 1, "only real.md should be indexed");
    }

    #[test]
    fn skips_non_markdown_files() {
        let (_dir, root) = make_vault(&[
            ("note.md", "# Note\n"),
            ("img.png", "fake-binary"),
            ("data.json", "{}"),
        ]);

        let (result, _) = index_markdown_directory_in_memory(&root, "default", "x").unwrap();
        assert_eq!(result.notes_count, 1);
    }

    #[test]
    fn note_size_policy_remains_one_mib_at_exact_boundaries() {
        let note = |title: &str, size: usize| {
            let prefix = format!("# {title}\n\n");
            format!("{prefix}{}", "x".repeat(size - prefix.len()))
        };
        let below = note("Below", MAX_NOTE_SIZE_BYTES as usize - 1);
        let at = note("At", MAX_NOTE_SIZE_BYTES as usize);
        let above = note("Above", MAX_NOTE_SIZE_BYTES as usize + 1);
        let (_dir, root) = make_vault(&[
            ("below.md", below.as_str()),
            ("at.md", at.as_str()),
            ("above.md", above.as_str()),
        ]);

        let (result, _) = index_markdown_directory_in_memory(&root, "default", "limits").unwrap();
        assert_eq!(result.notes_count, 2, "limit - 1 and limit are indexed");
        assert_eq!(result.skipped.len(), 1);
        assert_eq!(result.skipped[0].path, "above.md");
        assert_eq!(result.skipped[0].reason_code, SkipReasonCode::Oversized);
        assert_eq!(
            result.skipped[0].observed_bytes,
            Some(MAX_NOTE_SIZE_BYTES + 1)
        );
        assert_eq!(result.skipped[0].limit_bytes, Some(MAX_NOTE_SIZE_BYTES));
    }

    #[test]
    fn handles_nested_directories() {
        let (_dir, root) = make_vault(&[
            ("a.md", "# A\n"),
            ("sub/b.md", "# B\n"),
            ("sub/deep/c.md", "# C\n"),
        ]);

        let (result, _) = index_markdown_directory_in_memory(&root, "default", "x").unwrap();
        assert_eq!(result.notes_count, 3);
    }

    #[test]
    fn empty_vault_succeeds_with_zero_notes() {
        let (_dir, root) = make_vault(&[]);
        let (result, _) = index_markdown_directory_in_memory(&root, "default", "empty").unwrap();
        assert_eq!(result.notes_count, 0);
    }

    #[test]
    fn formats_unix_epoch_correctly() {
        // 1_716_466_800 = 2024-05-23T12:20:00Z (verified via `date -u -r 1716466800`).
        let s = format_system_time(
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_716_466_800),
        );
        assert_eq!(s.as_deref(), Some("2024-05-23T12:20:00Z"));
    }

    // ── Pass-2 wikilink/tag tests ─────────────────────────────────────────

    /// nw-100: a path-qualified wikilink is Obsidian's RELATIVE-path syntax and
    /// must resolve.
    ///
    /// `by_path` is keyed on full vault-relative paths, so priority 1 only ever
    /// tried the bare target. `[[plans/Target]]` written in
    /// `Workspaces/ExampleProject/_Overview.md` means
    /// `Workspaces/ExampleProject/plans/Target.md`, which never matched — so
    /// every path-qualified link in the vault resolved to nothing (45 of them,
    /// all reported at confidence 0.0).
    #[test]
    fn resolves_a_path_relative_to_the_source_folder() {
        let (_dir, root) = make_vault(&[
            (
                "Workspaces/ExampleProject/_Overview.md",
                "# Overview\n\nSee [[plans/Rollout Plan]].\n",
            ),
            (
                "Workspaces/ExampleProject/plans/Rollout Plan.md",
                "# Rollout Plan\n\nbody\n",
            ),
        ]);
        let (result, _) = index_markdown_directory_in_memory(&root, "default", "v").unwrap();
        assert_eq!(
            result.wikilinks_resolved, 1,
            "a source-folder-relative path must resolve"
        );
        assert_eq!(result.wikilinks_unresolved, 0);
    }

    /// A full vault-relative path must keep working — that was the only form
    /// priority 1 ever handled.
    #[test]
    fn resolves_a_full_vault_relative_path() {
        let (_dir, root) = make_vault(&[
            (
                "notes/index.md",
                "# Index\n\nSee [[Workspaces/ExampleProject/plans/Target]].\n",
            ),
            (
                "Workspaces/ExampleProject/plans/Target.md",
                "# Target\n\nbody\n",
            ),
        ]);
        let (result, _) = index_markdown_directory_in_memory(&root, "default", "v").unwrap();
        assert_eq!(result.wikilinks_resolved, 1);
        assert_eq!(result.wikilinks_unresolved, 0);
    }

    /// A path-qualified link that matches nothing must still be unresolved —
    /// the folder-relative attempt must not start inventing matches.
    #[test]
    fn a_path_qualified_link_to_nowhere_stays_unresolved() {
        let (_dir, root) = make_vault(&[(
            "Workspaces/ExampleProject/_Overview.md",
            "# Overview\n\nSee [[plans/Does Not Exist]].\n",
        )]);
        let (result, _) = index_markdown_directory_in_memory(&root, "default", "v").unwrap();
        assert_eq!(result.wikilinks_resolved, 0);
        assert_eq!(result.wikilinks_unresolved, 1);
    }

    #[test]
    fn resolves_unique_title_wikilink() {
        let (_dir, root) = make_vault(&[
            ("a.md", "# A\n\nSee [[B]].\n"),
            ("b.md", "# B\n\nI am B.\n"),
        ]);
        let (result, _) = index_markdown_directory_in_memory(&root, "default", "v").unwrap();
        assert_eq!(result.wikilinks_resolved, 1);
        assert_eq!(result.wikilinks_unresolved, 0);
    }

    #[test]
    fn unresolved_wikilink_counted() {
        let (_dir, root) = make_vault(&[("a.md", "# A\n\n[[Nonexistent Target]]\n")]);
        let (result, _) = index_markdown_directory_in_memory(&root, "default", "v").unwrap();
        assert_eq!(result.wikilinks_resolved, 0);
        assert_eq!(result.wikilinks_unresolved, 1);
    }

    #[test]
    fn alias_match_resolves_wikilink() {
        let (_dir, root) = make_vault(&[
            (
                "auth.md",
                "---\naliases: [\"AuthSvc\", \"Auth Service\"]\n---\n# Authentication\n",
            ),
            ("caller.md", "# Caller\n\nWe use [[AuthSvc]].\n"),
        ]);
        let (result, _) = index_markdown_directory_in_memory(&root, "default", "v").unwrap();
        assert_eq!(result.wikilinks_resolved, 1);
    }

    #[test]
    fn path_match_resolves_wikilink() {
        let (_dir, root) = make_vault(&[
            ("subdir/target.md", "# T\n\nbody\n"),
            ("root.md", "# R\n\nLink [[subdir/target]].\n"),
        ]);
        let (result, _) = index_markdown_directory_in_memory(&root, "default", "v").unwrap();
        assert_eq!(result.wikilinks_resolved, 1);
    }

    #[test]
    fn global_stem_match_resolves_cross_folder_wikilink() {
        // Obsidian shortest-path: [[Boost Billing]] from another folder must
        // resolve to projects/Boost Billing.md by filename stem, even though
        // the note's title differs and it declares no alias. This was the
        // ~25%-resolution-rate bug: only title/alias/same-folder matched.
        let (_dir, root) = make_vault(&[
            ("projects/Boost Billing.md", "# Billing PRD\n\nbody\n"),
            ("logs/daily.md", "# Daily\n\nShipped [[Boost Billing]].\n"),
        ]);
        let (result, _) = index_markdown_directory_in_memory(&root, "default", "v").unwrap();
        assert_eq!(result.wikilinks_resolved, 1);
        assert_eq!(result.wikilinks_unresolved, 0);
    }

    #[test]
    fn same_folder_match_outranks_global_stem() {
        // Two notes share the stem `target`; the source sits next to one of
        // them. Same-folder (priority 3) must win over the global-stem tier
        // and pick the sibling, not leave the link ambiguous.
        let (_dir, root) = make_vault(&[
            ("f/x.md", "# X\n\nSee [[target]].\n"),
            ("f/target.md", "# Alpha\n\nbody\n"),
            ("g/target.md", "# Beta\n\nbody\n"),
        ]);
        let (result, store) = index_markdown_directory_in_memory(&root, "default", "v").unwrap();
        assert_eq!(result.wikilinks_resolved, 1);

        let notes = store.list_notes(None).unwrap();
        let uid_of = |path_frag: &str| {
            notes
                .iter()
                .find(|n| n.file_path.contains(path_frag))
                .map(|n| n.uid.clone())
                .unwrap()
        };
        let edges = store.note_wikilink_edges().unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].0, uid_of("f/x.md"));
        assert_eq!(
            edges[0].1,
            uid_of("f/target.md"),
            "same-folder sibling must beat the cross-folder stem match"
        );
    }

    #[test]
    fn ambiguous_global_stem_stays_unresolved() {
        // Two notes share the stem and the source is in a third folder: no
        // tier can break the tie, so the link must count as unresolved rather
        // than guess.
        let (_dir, root) = make_vault(&[
            ("logs/daily.md", "# Daily\n\nSee [[target]].\n"),
            ("f/target.md", "# Alpha\n\nbody\n"),
            ("g/target.md", "# Beta\n\nbody\n"),
        ]);
        let (result, _) = index_markdown_directory_in_memory(&root, "default", "v").unwrap();
        assert_eq!(result.wikilinks_resolved, 0);
        assert_eq!(result.wikilinks_unresolved, 1);
    }

    #[test]
    fn heading_anchor_resolves_to_heading_node() {
        let (_dir, root) = make_vault(&[
            (
                "target.md",
                "# T\n\n## Setup\n\nsetup body\n\n## Usage\n\nusage body\n",
            ),
            ("caller.md", "# C\n\nSee [[T#Setup]].\n"),
        ]);
        let (result, store) = index_markdown_directory_in_memory(&root, "default", "v").unwrap();
        assert_eq!(result.wikilinks_resolved, 1);

        // Verify the wikilink went to the Heading variant.
        let count = store.count_wikilink_edges().unwrap();
        assert_eq!(count, 1);
    }

    /// SECURITY regression test: a symlink whose target is outside the
    /// vault root must NOT be indexed. Verifies the architecture doc's
    /// path-traversal hardening on the indexer side.
    #[cfg(unix)]
    #[test]
    fn symlink_escaping_vault_is_skipped() {
        use std::os::unix::fs::symlink;
        // Two real notes in the vault plus a symlink pointing OUTSIDE.
        let (dir, root) = make_vault(&[("real.md", "# Real Note\n"), ("other.md", "# Another\n")]);
        // Create a file outside the vault and symlink to it from inside.
        let outside = dir.path().join("outside-secret.md");
        std::fs::write(&outside, "# SECRET\nshould not be indexed\n").unwrap();
        symlink(&outside, root.join("escape.md")).unwrap();

        let (_result, store) = index_markdown_directory_in_memory(&root, "default", "v").unwrap();

        // Only the two real notes should land. WalkDir with
        // `follow_links(false)` returns symlinks with file_type()
        // is_symlink()==true, not is_file()==true, so they're silently
        // dropped before reaching the file-handling path. The body of
        // `outside-secret.md` therefore never enters the graph — which
        // is the property that matters for the security guarantee.
        let titles: Vec<String> = store
            .list_notes(None)
            .unwrap()
            .into_iter()
            .map(|n| n.title)
            .collect();
        assert_eq!(titles.len(), 2, "got titles {titles:?}");
        assert!(titles.contains(&"Real Note".to_string()));
        assert!(titles.contains(&"Another".to_string()));
        assert!(
            !titles.iter().any(|t| t == "SECRET"),
            "outside-vault symlink content must not be indexed"
        );
    }

    #[test]
    fn tags_create_nodes_and_edges() {
        let (_dir, root) = make_vault(&[
            (
                "a.md",
                "---\ntags: [project, status/active]\n---\n# A\n\nbody #inline-tag\n",
            ),
            ("b.md", "---\ntags: [project]\n---\n# B\n\n#shared\n"),
        ]);
        let (result, store) = index_markdown_directory_in_memory(&root, "default", "v").unwrap();
        // 4 unique tag names: project, status/active, inline-tag, shared.
        assert_eq!(result.tags_count, 4);
        let tags = store.list_tags(None).unwrap();
        let names: Vec<&str> = tags.iter().map(|t| t.name.as_str()).collect();
        for expected in ["project", "status/active", "inline-tag", "shared"] {
            assert!(
                names.contains(&expected),
                "missing tag '{expected}' in {names:?}"
            );
        }
    }

    // ── Taxonomy alias tests ───────────────────────────────────────────────

    #[test]
    fn load_taxonomy_aliases_from_frontmatter() {
        let (_dir, root) = make_vault(&[(
            "_taxonomy.md",
            "---\naliases:\n  Authentication: [Auth, AuthSvc]\n  Device Pairing: [Pairing]\n---\n",
        )]);
        let aliases = load_taxonomy_aliases(&root);
        assert!(
            aliases.contains_key("Authentication"),
            "missing Authentication"
        );
        let auth_aliases = &aliases["Authentication"];
        assert!(auth_aliases.contains(&"Auth".to_string()));
        assert!(auth_aliases.contains(&"AuthSvc".to_string()));
        let dp_aliases = &aliases["Device Pairing"];
        assert!(dp_aliases.contains(&"Pairing".to_string()));
    }

    #[test]
    fn load_taxonomy_aliases_from_inline_arrows() {
        let (_dir, root) = make_vault(&[(
            "taxonomy.md",
            "# Aliases\n\nAuth -> Authentication\nBT Pairing -> Device Pairing\n",
        )]);
        let aliases = load_taxonomy_aliases(&root);
        assert!(
            aliases.contains_key("Authentication"),
            "missing Authentication"
        );
        assert!(aliases["Authentication"].contains(&"Auth".to_string()));
        assert!(aliases["Device Pairing"].contains(&"BT Pairing".to_string()));
    }

    #[test]
    fn taxonomy_aliases_empty_when_no_file() {
        let (_dir, root) = make_vault(&[("note.md", "# Just a note\n")]);
        let aliases = load_taxonomy_aliases(&root);
        assert!(aliases.is_empty());
    }

    #[test]
    fn taxonomy_file_priority_underscore_first() {
        // Both `_taxonomy.md` and `taxonomy.md` present — `_taxonomy.md` wins.
        let (_dir, root) = make_vault(&[
            (
                "_taxonomy.md",
                "# From _taxonomy\n\nPrimary -> CanonicalA\n",
            ),
            (
                "taxonomy.md",
                "# From taxonomy\n\nSecondary -> CanonicalB\n",
            ),
        ]);
        let aliases = load_taxonomy_aliases(&root);
        // Only _taxonomy.md should be processed.
        assert!(aliases.contains_key("CanonicalA"));
        assert!(!aliases.contains_key("CanonicalB"));
    }

    // ── .brainignore tests ────────────────────────────────────────────────

    #[test]
    fn brainignore_excludes_backup_directory() {
        let (_dir, root) = make_vault(&[
            ("notes/real.md", "# Real Note\n\nContent.\n"),
            (
                "notes.backup.20260527/real.md",
                "# Backup Copy\n\nOld content.\n",
            ),
            (
                ".brainignore",
                "# Ignore backup directories\n*.backup.*/**\n",
            ),
        ]);

        let (result, store) =
            index_markdown_directory_in_memory(&root, "default", "test-vault").unwrap();
        assert_eq!(
            result.notes_count, 1,
            "only notes/real.md should be indexed"
        );
        let titles: Vec<String> = store
            .list_notes(None)
            .unwrap()
            .into_iter()
            .map(|n| n.title)
            .collect();
        assert!(
            titles.contains(&"Real Note".to_string()),
            "expected 'Real Note' in {titles:?}"
        );
        assert!(
            !titles.iter().any(|t| t == "Backup Copy"),
            "backup copy must not be indexed"
        );
    }

    #[test]
    fn brainignore_default_patterns_skip_obsidian_and_git() {
        // No .brainignore file — defaults should kick in.
        let (_dir, root) = make_vault(&[
            ("real.md", "# Real\n"),
            (".obsidian/workspace.md", "# Obsidian internal\n"),
            (".git/notes.md", "# Git internal\n"),
            ("node_modules/pkg/readme.md", "# Package readme\n"),
        ]);

        let (result, _) = index_markdown_directory_in_memory(&root, "default", "v").unwrap();
        assert_eq!(result.notes_count, 1, "only real.md should be indexed");
    }

    #[test]
    fn brainignore_custom_file_overrides_defaults() {
        // A .brainignore file with only "drafts/**" means .obsidian is NOT
        // excluded by the brainignore layer (the SKIP_DIRS walker filter
        // still blocks it, but brainignore itself doesn't).
        let (_dir, root) = make_vault(&[
            ("real.md", "# Real\n"),
            ("drafts/wip.md", "# WIP\n"),
            (".brainignore", "drafts/**\n"),
        ]);

        let (result, store) = index_markdown_directory_in_memory(&root, "default", "v").unwrap();
        let titles: Vec<String> = store
            .list_notes(None)
            .unwrap()
            .into_iter()
            .map(|n| n.title)
            .collect();
        assert!(titles.contains(&"Real".to_string()));
        assert!(
            !titles.contains(&"WIP".to_string()),
            "drafts/wip.md should be excluded by .brainignore"
        );
        assert_eq!(result.notes_count, 1);
    }

    #[test]
    fn bare_repo_markdown_not_skipped_by_mtime() {
        use crate::content_reader::ContentReader;
        use std::path::{Path, PathBuf};

        struct MockBareReader;
        impl ContentReader for MockBareReader {
            fn read_file(&self, _rel_path: &Path) -> anyhow::Result<String> {
                Ok("# Test Note\n\nSome content.".to_string())
            }
            fn list_files(&self) -> anyhow::Result<Vec<PathBuf>> {
                Ok(vec![PathBuf::from("notes/test.md")])
            }
            fn file_meta_nanos(&self, _rel_path: &Path) -> anyhow::Result<Option<(u64, u64)>> {
                Ok(None)
            }
            fn root(&self) -> &Path {
                Path::new("/fake/bare")
            }
            fn version_id(&self) -> &str {
                "abc123"
            }
        }

        let reader = MockBareReader;
        let files = reader.list_files().unwrap();
        assert_eq!(files.len(), 1);
        let meta = reader.file_meta_nanos(&files[0]).unwrap();
        assert!(meta.is_none());
        let content = reader.read_file(&files[0]).unwrap();
        assert!(content.contains("# Test Note"));
    }

    /// Create a source repo with `files`, commit, and clone it as a bare repo.
    /// Returns `(tempdir, bare_path, head_sha)`. Mirrors the helper used by the
    /// `GitBareReader` tests in `content_reader.rs`.
    fn setup_bare_repo(files: &[(&str, &str)]) -> (tempfile::TempDir, std::path::PathBuf, String) {
        use std::process::Command;
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src_repo");
        fs::create_dir_all(&src).unwrap();
        for args in [
            vec!["init"],
            vec!["config", "user.email", "test@test.com"],
            vec!["config", "user.name", "Test"],
        ] {
            Command::new("git")
                .args(&args)
                .current_dir(&src)
                .output()
                .unwrap();
        }
        for (path, content) in files {
            let full = src.join(path);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&full, content).unwrap();
        }
        Command::new("git")
            .args(["add", "."])
            .current_dir(&src)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&src)
            .output()
            .unwrap();

        let bare = tmp.path().join("repo.git");
        Command::new("git")
            .args([
                "clone",
                "--bare",
                &src.display().to_string(),
                &bare.display().to_string(),
            ])
            .output()
            .unwrap();
        let sha_out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&bare)
            .output()
            .unwrap();
        let sha = String::from_utf8(sha_out.stdout)
            .unwrap()
            .trim()
            .to_string();
        (tmp, bare, sha)
    }

    /// The worker's vault path: indexing a `type = "vault"` repo runs the
    /// markdown indexer over a bare clone, producing Note/Section nodes rather
    /// than code symbols. The bare clone has no on-disk `.brainignore`, which
    /// `index_markdown_with_reader` must tolerate.
    #[test]
    fn index_markdown_with_reader_over_bare_clone_makes_notes() {
        let (_tmp, bare, sha) = setup_bare_repo(&[
            ("README.md", "# Readme\n\nProject overview.\n"),
            ("docs/guide.md", "# Guide\n\n## Setup\n\ninstall steps\n"),
            // A non-markdown source file that must NOT become a code symbol.
            ("src/lib.rs", "pub fn greet() -> &'static str { \"hi\" }"),
        ]);

        let reader = crate::content_reader::GitBareReader::new(&bare, &sha);
        let store = GraphStore::in_memory().unwrap();
        let result =
            index_markdown_with_reader(&reader, &store, "test-instance", "vault-repo").unwrap();

        // Markdown nodes were produced.
        assert_eq!(result.notes_count, 2, "both .md files should be indexed");
        assert!(result.headings_count >= 2);
        assert!(result.sections_count >= 2);
        assert!(store.count_notes().unwrap() >= 2);
        assert!(store.count_sections().unwrap() >= 2);

        // Crucially, the markdown path indexes no code symbols — the .rs file
        // is ignored by the markdown indexer.
        assert_eq!(
            store.count_symbols().unwrap(),
            0,
            "vault indexing must not produce code symbols"
        );
    }

    /// nw-003: the gated vault entry point must upsert a `Repo` node carrying
    /// the indexed SHA, and re-indexing must update it in place (insert when
    /// absent, update when present — mirroring the code path). Without this the
    /// worker's `remote_sha == indexed_sha` short-circuit never fires for vault
    /// repos.
    #[test]
    fn gated_vault_index_records_repo_indexed_sha() {
        let (_tmp, bare, sha) = setup_bare_repo(&[("README.md", "# Readme\n\nbody\n")]);
        let reader = crate::content_reader::GitBareReader::new(&bare, &sha);
        let store = GraphStore::in_memory().unwrap();

        // The write gate is acquired exactly once and only for the commit phase.
        let gate_calls = std::cell::Cell::new(0u32);
        index_markdown_with_reader_and_write_gate(
            &reader,
            &store,
            "test-instance",
            "vault-repo",
            &sha,
            || {
                gate_calls.set(gate_calls.get() + 1);
                Ok::<_, anyhow::Error>(())
            },
        )
        .unwrap();
        assert_eq!(gate_calls.get(), 1, "write gate acquired exactly once");

        let r_uid = repo_uid("test-instance", "vault-repo");
        let repo = store
            .lookup_repo(&r_uid)
            .unwrap()
            .expect("vault index must upsert a Repo node carrying the SHA");
        assert_eq!(
            repo.indexed_sha, sha,
            "indexed_sha must equal the indexed remote SHA"
        );

        // Re-index at the same SHA: the existing Repo row is updated in place
        // (the helper's update branch), still resolving to the same SHA.
        index_markdown_with_reader_and_write_gate(
            &reader,
            &store,
            "test-instance",
            "vault-repo",
            &sha,
            || Ok::<_, anyhow::Error>(()),
        )
        .unwrap();
        let repo = store.lookup_repo(&r_uid).unwrap().unwrap();
        assert_eq!(repo.indexed_sha, sha);
    }

    /// nw-006: every store write — including the Vault upsert, which previously
    /// ran at the top of `index_into_store` before the parse — now happens
    /// inside the gated region. A failing write gate must therefore leave the
    /// store completely untouched: no Vault node, no notes, no Repo SHA.
    #[test]
    fn gated_vault_index_commits_nothing_when_write_gate_fails() {
        let (_tmp, bare, sha) = setup_bare_repo(&[
            ("README.md", "# Readme\n\nbody\n"),
            ("docs/guide.md", "# Guide\n\n## Setup\n\nsteps\n"),
        ]);
        let reader = crate::content_reader::GitBareReader::new(&bare, &sha);
        let store = GraphStore::in_memory().unwrap();

        let result = index_markdown_with_reader_and_write_gate(
            &reader,
            &store,
            "test-instance",
            "vault-repo",
            &sha,
            || Err::<(), _>(anyhow::anyhow!("simulated job cancellation")),
        );

        assert!(
            result.is_err(),
            "a failing write gate must propagate as an error"
        );
        // The Vault node is upserted under the gate, so it must not exist.
        let v_uid = vault_uid("test-instance", &reader.root().to_string_lossy());
        assert!(
            store.lookup_vault(&v_uid).is_err(),
            "no Vault node may be committed when the gate fails"
        );
        assert_eq!(
            store.count_notes().unwrap(),
            0,
            "no notes committed when the gate fails"
        );
        let r_uid = repo_uid("test-instance", "vault-repo");
        assert!(
            store.lookup_repo(&r_uid).unwrap().is_none(),
            "no Repo SHA recorded when the gate fails"
        );
    }

    /// A full vault index must advance the graph generation, mirroring
    /// the code path (`index.rs`). Without the bump the trigram posting
    /// table's staleness check is blind to vault mutations.
    #[test]
    fn full_vault_index_advances_graph_generation() {
        let (_dir, root) = make_vault(&[("a.md", "# A\n\nalpha body\n")]);
        let (_result, store) = index_markdown_directory_in_memory(&root, "default", "v").unwrap();
        assert!(
            store.graph_generation() > 0,
            "vault indexing must advance the graph generation"
        );
    }

    #[test]
    fn direct_and_daemon_refresh_entry_points_report_committed_delete_counts_equally() {
        let (_dir, root) = make_vault(&[("a.md", "# A\n\nalpha\n"), ("b.md", "# B\n\nbeta\n")]);
        let temp = tempfile::tempdir().unwrap();
        let direct_db = temp.path().join("direct.lbug");
        let daemon_db = temp.path().join("daemon.lbug");

        let direct_first = index_markdown_directory_with_ignore_and_deletion_count(
            &root,
            &direct_db,
            "default",
            "vault",
            &[],
        )
        .unwrap();
        let daemon_store = GraphStore::open_or_create(&daemon_db).unwrap();
        let daemon_first = index_markdown_directory_with_store_and_deletion_count(
            &daemon_store,
            &root,
            &daemon_db,
            "default",
            "vault",
            &[],
        )
        .unwrap();
        assert_eq!(direct_first.notes_deleted, 0);
        assert_eq!(daemon_first.notes_deleted, 0);

        let direct_unchanged = index_markdown_directory_with_ignore_and_deletion_count(
            &root,
            &direct_db,
            "default",
            "vault",
            &[],
        )
        .unwrap();
        let daemon_unchanged = index_markdown_directory_with_store_and_deletion_count(
            &daemon_store,
            &root,
            &daemon_db,
            "default",
            "vault",
            &[],
        )
        .unwrap();
        assert_eq!(direct_unchanged.notes_deleted, 2);
        assert_eq!(daemon_unchanged.notes_deleted, 2);
        assert_eq!(
            format_markdown_refresh_summary(&direct_unchanged),
            format_markdown_refresh_summary(&daemon_unchanged)
        );

        fs::remove_file(root.join("a.md")).unwrap();
        fs::write(root.join("b.md"), "# B changed\n\nnew beta\n").unwrap();
        fs::write(root.join("c.md"), "# C\n\ngamma\n").unwrap();
        let direct_changed = index_markdown_directory_with_ignore_and_deletion_count(
            &root,
            &direct_db,
            "default",
            "vault",
            &[],
        )
        .unwrap();
        let daemon_changed = index_markdown_directory_with_store_and_deletion_count(
            &daemon_store,
            &root,
            &daemon_db,
            "default",
            "vault",
            &[],
        )
        .unwrap();
        assert_eq!(direct_changed.notes_deleted, 2);
        assert_eq!(direct_changed.index.notes_count, 2);
        assert_eq!(daemon_changed.notes_deleted, 2);
        assert_eq!(daemon_changed.index.notes_count, 2);
        assert_eq!(
            format_markdown_refresh_summary(&direct_changed),
            format_markdown_refresh_summary(&daemon_changed)
        );
        assert_eq!(
            format_markdown_refresh_summary(&direct_changed),
            "Refreshed vault 'vault': dropped 2 stale note(s), reindexed 2 note(s), \
             2 heading(s), 2 section(s), 0 tag(s), 0 wikilink(s) (0 unresolved)."
        );
    }

    /// An in-place vault edit (delete + recreate of the same sections)
    /// keeps the candidate-node count identical, so the generation bump is the
    /// ONLY signal that stales the trigram posting table. Re-indexing into the
    /// same store after such an edit must advance the generation while the
    /// node counts stay put.
    #[test]
    fn vault_reindex_after_in_place_edit_advances_generation_with_unchanged_counts() {
        let (_dir, root) = make_vault(&[("a.md", "# A\n\nalpha body\n")]);
        let store = GraphStore::in_memory().unwrap();
        let db_path = root.join("unused.lbug");
        index_markdown_directory_with_store(&store, &root, &db_path, "default", "v", &[]).unwrap();
        let g1 = store.graph_generation();
        let counts1 = (
            store.count_notes().unwrap(),
            store.count_sections().unwrap(),
        );

        // In-place edit: same heading structure, different body text — the
        // section is deleted and recreated, so node counts are unchanged.
        fs::write(root.join("a.md"), "# A\n\nbeta body rewritten\n").unwrap();
        index_markdown_directory_with_store(&store, &root, &db_path, "default", "v", &[]).unwrap();

        assert_eq!(
            (
                store.count_notes().unwrap(),
                store.count_sections().unwrap()
            ),
            counts1,
            "an in-place edit must not change the candidate-node count"
        );
        assert!(
            store.graph_generation() > g1,
            "an in-place vault edit must still advance the graph generation"
        );
    }

    /// The incremental (`--since`) refresh path must advance AND persist
    /// the graph generation, so a later process (or the daemon) observes the
    /// bump and distrusts the stale trigram postings.
    #[test]
    fn since_threshold_still_matches_a_coarse_granularity_mtime() {
        // A filesystem whose mtime granularity is coarser than the caller's
        // clock stamps a write performed AFTER `since` with a value slightly
        // BEFORE it. On Linux the VFS stamps mtime from a clock updated once
        // per jiffy, so this is the normal case there, not an exotic one — and
        // it is invisible on APFS, where mtimes are per-write precise.
        //
        // Comparing exact nanoseconds on both sides silently skipped such a
        // note. Flooring the threshold to the second restores the inclusive
        // window the seconds-based comparison always had.
        let (_dir, root) = make_vault(&[("a.md", "# A\n\nalpha body\n")]);
        let db_path = root.join("brain.lbug");
        index_markdown_directory(&root, &db_path, "default", "v").unwrap();

        let note = root.join("a.md");
        fs::write(&note, "# A\n\nbeta body rewritten\n").unwrap();

        // `since` is captured with full precision AFTER the write...
        let since = std::time::SystemTime::now();
        // ...and the filesystem reports the write floored to its second, i.e.
        // BEFORE `since`. Exactly the coarse-granularity case.
        let coarse = std::time::UNIX_EPOCH
            + std::time::Duration::from_secs(
                since
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
            );
        let handle = fs::OpenOptions::new().write(true).open(&note).unwrap();
        handle
            .set_times(fs::FileTimes::new().set_modified(coarse))
            .unwrap();

        let res = index_markdown_directory_since(&root, &db_path, "default", "v", since).unwrap();
        assert_eq!(
            res.notes_updated, 1,
            "a note written after `since` must not be skipped because the \
             filesystem reported its mtime at coarser granularity"
        );
    }

    #[test]
    fn since_refresh_advances_and_persists_generation_on_in_place_edit() {
        let (_dir, root) = make_vault(&[("a.md", "# A\n\nalpha body\n")]);
        let db_path = root.join("brain.lbug");

        index_markdown_directory(&root, &db_path, "default", "v").unwrap();
        let g1 = {
            let store = GraphStore::open_or_create(&db_path).unwrap();
            store.graph_generation()
        };
        assert!(
            g1 > 0,
            "the full index must persist a non-zero generation to the sidecar"
        );

        // In-place edit; since=UNIX_EPOCH processes every file regardless of
        // mtime granularity.
        fs::write(root.join("a.md"), "# A\n\nbeta body rewritten\n").unwrap();
        let res = index_markdown_directory_since(
            &root,
            &db_path,
            "default",
            "v",
            std::time::SystemTime::UNIX_EPOCH,
        )
        .unwrap();
        assert_eq!(res.notes_updated, 1, "the edited note must be reindexed");

        let g2 = {
            let store = GraphStore::open_or_create(&db_path).unwrap();
            store.graph_generation()
        };
        assert!(
            g2 > g1,
            "the since refresh must advance and persist the graph generation ({g1} -> {g2})"
        );
    }

    #[test]
    fn since_refresh_reuses_daemon_owned_store_without_second_writer() {
        let (_dir, root) = make_vault(&[
            ("a.md", "# A\n\nalpha body\n"),
            ("removed.md", "# Removed\n\nold body\n"),
        ]);
        let db_path = root.join("brain.lbug");
        let store = GraphStore::open_or_create(&db_path).unwrap();
        index_markdown_directory_with_store(&store, &root, &db_path, "owned", "v", &[]).unwrap();

        fs::write(root.join("a.md"), "# A\n\nbeta body\n").unwrap();
        fs::remove_file(root.join("removed.md")).unwrap();
        let result = index_markdown_directory_since_with_store_and_ignore(
            &store,
            &root,
            "owned",
            "v",
            std::time::UNIX_EPOCH,
            &[],
        )
        .expect("daemon-owned refresh must not attempt a second GraphStore writer");

        assert_eq!(result.notes_updated, 1);
        assert_eq!(
            result.notes_deleted, 2,
            "one replacement plus one removed file"
        );
        assert_eq!(store.count_notes().unwrap(), 1);
        let notes = store.list_notes(None).unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(
            notes[0].vault_uid,
            nestweaver_schema::vault_uid(
                "owned",
                // nw-138b: the directory-based indexer canonicalizes the vault root before
                // deriving the uid (index_md.rs:202/251). Tests must do the same, or on
                // macOS - where TMPDIR resolves through /private - they compute a
                // different uid than the code under test. The reader-based variant does
                // NOT canonicalize, so its tests must keep using the raw root.
                &std::fs::canonicalize(&root)
                    .unwrap_or_else(|_| root.clone())
                    .to_string_lossy(),
            )
        );
        assert_eq!(notes[0].title, "A");
    }

    #[test]
    fn since_refresh_atomically_replaces_a_tagged_incumbent() {
        let (_dir, root) = make_vault(&[("a.md", "# Old\n\n#keep old body\n")]);
        let store = GraphStore::in_memory().unwrap();
        let db_path = root.join("unused.lbug");
        index_markdown_directory_with_store(&store, &root, &db_path, "owned", "v", &[]).unwrap();

        fs::write(root.join("a.md"), "# New\n\n#keep new body\n").unwrap();
        let result = index_markdown_directory_since_with_store_and_ignore(
            &store,
            &root,
            "owned",
            "v",
            std::time::SystemTime::UNIX_EPOCH,
            &[],
        )
        .unwrap();

        assert_eq!(result.notes_updated, 1);
        assert_eq!(result.notes_deleted, 1);
        let notes = store.list_notes(None).unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].title, "New");
        assert_eq!(store.list_tags(None).unwrap().len(), 1);
    }

    #[test]
    fn since_refresh_preserves_unchanged_inbound_wikilink_to_changed_note() {
        let (_dir, root) = make_vault(&[
            ("a.md", "# A\n\n[[B#Target]]\n"),
            ("b.md", "# B\n\n## Target\n\nold body\n"),
        ]);
        let store = GraphStore::in_memory().unwrap();
        let db_path = root.join("unused.lbug");
        index_markdown_directory_with_store(&store, &root, &db_path, "owned", "v", &[]).unwrap();
        assert_eq!(store.count_wikilink_edges().unwrap(), 1);

        std::thread::sleep(std::time::Duration::from_millis(1100));
        let since = std::time::SystemTime::now();
        fs::write(root.join("b.md"), "# B\n\n## Target\n\nchanged body\n").unwrap();
        index_markdown_directory_since_with_store_and_ignore(
            &store,
            &root,
            "owned",
            "v",
            since,
            &[],
        )
        .unwrap();
        assert_eq!(store.count_wikilink_edges().unwrap(), 1);
    }

    #[test]
    fn since_refresh_resolves_two_renamed_notes_independent_of_file_order() {
        let (_dir, root) = make_vault(&[
            ("a.md", "# Old A\n\n[[Old B]]\n"),
            ("z.md", "# Old B\n\n[[Old A]]\n"),
        ]);
        let store = GraphStore::in_memory().unwrap();
        let db_path = root.join("unused.lbug");
        index_markdown_directory_with_store(&store, &root, &db_path, "owned", "v", &[]).unwrap();

        fs::write(root.join("a.md"), "# New A\n\n[[New B]]\n").unwrap();
        fs::write(root.join("z.md"), "# New B\n\n[[New A]]\n").unwrap();
        index_markdown_directory_since_with_store_and_ignore(
            &store,
            &root,
            "owned",
            "v",
            std::time::SystemTime::UNIX_EPOCH,
            &[],
        )
        .unwrap();
        assert_eq!(store.count_wikilink_edges().unwrap(), 2);
    }

    #[test]
    fn since_refresh_garbage_collects_replaced_and_deleted_orphan_tags() {
        let (_dir, root) = make_vault(&[
            ("a.md", "# A\n\n#old body\n"),
            ("b.md", "# B\n\n#sole body\n"),
        ]);
        let store = GraphStore::in_memory().unwrap();
        let db_path = root.join("unused.lbug");
        index_markdown_directory_with_store(&store, &root, &db_path, "owned", "v", &[]).unwrap();
        assert_eq!(store.list_tags(None).unwrap().len(), 2);

        fs::write(root.join("a.md"), "# A\n\n#new body\n").unwrap();
        fs::remove_file(root.join("b.md")).unwrap();
        index_markdown_directory_since_with_store_and_ignore(
            &store,
            &root,
            "owned",
            "v",
            std::time::SystemTime::UNIX_EPOCH,
            &[],
        )
        .unwrap();
        let tags = store.list_tags(None).unwrap();
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].name, "new");
    }

    #[test]
    fn since_refresh_uses_full_path_alias_and_confidence_resolution() {
        let (_dir, root) = make_vault(&[(
            "folder/source.md",
            "# Source\n\n[[plans/Rollout]]\n\n[[Short]]\n",
        )]);
        let store = GraphStore::in_memory().unwrap();
        let db_path = root.join("unused.lbug");
        index_markdown_directory_with_store(&store, &root, &db_path, "owned", "v", &[]).unwrap();
        assert_eq!(store.count_wikilink_edges().unwrap(), 0);

        std::thread::sleep(std::time::Duration::from_millis(1100));
        let since = std::time::SystemTime::now();
        fs::create_dir_all(root.join("folder/plans")).unwrap();
        fs::write(
            root.join("folder/plans/Rollout.md"),
            "---\nalias: Short\n---\n# Rollout\n",
        )
        .unwrap();
        let result = index_markdown_directory_since_with_store_and_ignore(
            &store,
            &root,
            "owned",
            "v",
            since,
            &[],
        )
        .unwrap();
        assert_eq!(result.notes_updated, 1);
        assert_eq!(store.count_wikilink_edges().unwrap(), 2);
        let suspect = store.broken_wikilinks().unwrap();
        assert!(
            suspect
                .iter()
                .any(|link| (link.confidence - 0.7).abs() < 0.001)
        );
    }

    #[test]
    fn since_refresh_rederives_unresolved_frontmatter_typed_reference() {
        let (_dir, root) = make_vault(&[("a.md", "---\ndepends_on:\n  - Future\n---\n# A\n")]);
        let store = GraphStore::in_memory().unwrap();
        let db_path = root.join("unused.lbug");
        index_markdown_directory_with_store(&store, &root, &db_path, "owned", "v", &[]).unwrap();
        assert!(store.typed_note_edges().unwrap().is_empty());

        std::thread::sleep(std::time::Duration::from_millis(1100));
        let since = std::time::SystemTime::now();
        fs::write(root.join("future.md"), "# Future\n").unwrap();
        let result = index_markdown_directory_since_with_store_and_ignore(
            &store,
            &root,
            "owned",
            "v",
            since,
            &[],
        )
        .unwrap();
        assert_eq!(result.notes_updated, 1);
        let typed = store.typed_note_edges().unwrap();
        assert_eq!(typed.len(), 1);
        assert_eq!(typed[0].2, "DEPENDS_ON");
    }

    #[test]
    fn since_refresh_ignores_other_vaults_matching_unresolved_targets() {
        let (_a_dir, root_a) = make_vault(&[("a.md", "# A\n")]);
        let (_b_dir, root_b) = make_vault(&[("b.md", "# B\n\n[[Future]]\n")]);
        let store = GraphStore::in_memory().unwrap();
        let db_path = root_a.join("unused.lbug");
        index_markdown_directory_with_store(&store, &root_a, &db_path, "owned", "a", &[]).unwrap();
        index_markdown_directory_with_store(&store, &root_b, &db_path, "owned", "b", &[]).unwrap();
        let unresolved_before = store.all_unresolved_wikilinks().unwrap();
        assert_eq!(unresolved_before.len(), 1);

        std::thread::sleep(std::time::Duration::from_millis(1100));
        let since = std::time::SystemTime::now();
        fs::write(root_a.join("future.md"), "# Future\n").unwrap();
        let result = index_markdown_directory_since_with_store_and_ignore(
            &store,
            &root_a,
            "owned",
            "a",
            since,
            &[],
        )
        .unwrap();

        assert_eq!(result.notes_updated, 1);
        assert_eq!(store.all_unresolved_wikilinks().unwrap(), unresolved_before);
    }

    #[test]
    fn since_refresh_preserves_project_membership_for_replacements_only() {
        // Two notes on purpose. With one, removing it also EMPTIES the vault,
        // which the nw-287 guard refuses — and the subject here is "a removed
        // file drops its project membership", not "a vault may be emptied by a
        // refresh". The second note keeps those two variables apart.
        let (_dir, root) = make_vault(&[("a.md", "# A\n\nold\n"), ("keep.md", "# Keep\n")]);
        let store = GraphStore::in_memory().unwrap();
        let db_path = root.join("unused.lbug");
        index_markdown_directory_with_store(&store, &root, &db_path, "owned", "v", &[]).unwrap();
        let note_uid = store
            .list_notes(None)
            .unwrap()
            .iter()
            .find(|n| n.file_path.ends_with("a.md"))
            .map(|n| n.uid.clone())
            .unwrap();
        let project_uid = "proj:test:incremental-membership";
        store
            .insert_project(&nestweaver_schema::Project {
                uid: project_uid.to_string(),
                name: "Incremental membership".to_string(),
                summary: None,
                instance_id: "owned".to_string(),
            })
            .unwrap();
        store
            .batch_insert_project_note_edges(&[(project_uid, &note_uid)])
            .unwrap();

        fs::write(root.join("a.md"), "# A\n\nnew\n").unwrap();
        index_markdown_directory_since_with_store_and_ignore(
            &store,
            &root,
            "owned",
            "v",
            std::time::SystemTime::UNIX_EPOCH,
            &[],
        )
        .unwrap();
        assert_eq!(
            store.list_project_note_uids(project_uid).unwrap(),
            vec![note_uid.clone()]
        );

        // Full authoritative replacement has the same stable-UID preservation
        // contract; the refresh workflow may materialize projects later, but
        // it must not erase already-valid membership in the meantime.
        index_markdown_directory_with_store(&store, &root, &db_path, "owned", "v", &[]).unwrap();
        assert_eq!(
            store.list_project_note_uids(project_uid).unwrap(),
            vec![note_uid.clone()]
        );

        fs::remove_file(root.join("a.md")).unwrap();
        index_markdown_directory_since_with_store_and_ignore(
            &store,
            &root,
            "owned",
            "v",
            std::time::SystemTime::UNIX_EPOCH,
            &[],
        )
        .unwrap();
        assert!(
            store
                .list_project_note_uids(project_uid)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn since_refresh_reads_only_changed_and_affected_sources() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingReader {
            inner: crate::content_reader::FilesystemReader,
            reads: AtomicUsize,
        }
        impl ContentReader for CountingReader {
            fn read_file(&self, rel_path: &Path) -> anyhow::Result<String> {
                self.reads.fetch_add(1, Ordering::SeqCst);
                self.inner.read_file(rel_path)
            }
            fn list_files(&self) -> anyhow::Result<Vec<PathBuf>> {
                self.inner.list_files()
            }
            fn file_meta_nanos(&self, rel_path: &Path) -> anyhow::Result<Option<(u64, u64)>> {
                self.inner.file_meta_nanos(rel_path)
            }
            fn root(&self) -> &Path {
                self.inner.root()
            }
            fn version_id(&self) -> &str {
                self.inner.version_id()
            }
        }

        let files = (0..101)
            .map(|index| {
                (
                    format!("note-{index}.md"),
                    format!("# Note {index}\n\nbody\n"),
                )
            })
            .collect::<Vec<_>>();
        let borrowed = files
            .iter()
            .map(|(path, body)| (path.as_str(), body.as_str()))
            .collect::<Vec<_>>();
        let (_dir, root) = make_vault(&borrowed);
        let store = GraphStore::in_memory().unwrap();
        let db_path = root.join("unused.lbug");
        index_markdown_directory_with_store(&store, &root, &db_path, "owned", "old", &[]).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(1100));
        let since = std::time::SystemTime::now();
        fs::write(root.join("note-100.md"), "# Changed leaf\n\nnew body\n").unwrap();
        let reader = CountingReader {
            inner: crate::content_reader::FilesystemReader::new(&root),
            reads: AtomicUsize::new(0),
        };
        let ignore_set = crate::brainignore::load_brain_ignore(&root, &[]);
        let result =
            index_markdown_since_with_reader(&store, &reader, "owned", "new", since, &ignore_set)
                .unwrap();
        assert_eq!(result.notes_updated, 1);
        assert_eq!(reader.reads.load(Ordering::SeqCst), 1);

        let generation = store.graph_generation();
        let result = index_markdown_since_with_reader(
            &store,
            &reader,
            "owned",
            "must-not-overwrite-noop",
            std::time::SystemTime::now() + std::time::Duration::from_secs(60),
            &ignore_set,
        )
        .unwrap();
        assert_eq!(result.notes_updated, 0);
        assert_eq!(
            reader.reads.load(Ordering::SeqCst),
            1,
            "no-op reads no content"
        );
        assert_eq!(
            store.graph_generation(),
            generation,
            "no-op opens no mutation"
        );
        assert_eq!(
            store
                .lookup_vault(&vault_uid("owned", &root.to_string_lossy()))
                .unwrap()
                .name,
            "new"
        );
    }

    #[test]
    fn since_refresh_creates_a_never_indexed_empty_vault_once() {
        let (_dir, root) = make_vault(&[]);
        let store = GraphStore::in_memory().unwrap();
        let result = index_markdown_directory_since_with_store_and_ignore(
            &store,
            &root,
            "owned",
            "empty",
            std::time::SystemTime::now(),
            &[],
        )
        .unwrap();
        assert_eq!(result.notes_updated, 0);
        let uid = vault_uid(
            "owned",
            // nw-138b: the directory-based indexer canonicalizes the vault root before
            // deriving the uid (index_md.rs:202/251). Tests must do the same, or on
            // macOS - where TMPDIR resolves through /private - they compute a
            // different uid than the code under test. The reader-based variant does
            // NOT canonicalize, so its tests must keep using the raw root.
            &std::fs::canonicalize(&root)
                .unwrap_or_else(|_| root.clone())
                .to_string_lossy(),
        );
        assert_eq!(store.lookup_vault(&uid).unwrap().name, "empty");
        let generation = store.graph_generation();

        index_markdown_directory_since_with_store_and_ignore(
            &store,
            &root,
            "owned",
            "renamed-noop",
            std::time::SystemTime::now(),
            &[],
        )
        .unwrap();
        assert_eq!(store.lookup_vault(&uid).unwrap().name, "empty");
        assert_eq!(store.graph_generation(), generation);
    }

    /// nw-204, vault half. The vault side had NO synchronous tombstoning: a
    /// refresh that dropped notes left their vectors live and scored, and only
    /// a WRITABLE daemon's periodic reconciler repaired it — so a CLI-only
    /// `brain refresh`, or one against a read-only daemon, leaked forever.
    #[test]
    fn a_vault_refresh_tombstones_vectors_of_notes_it_drops() {
        let (_dir, root) = make_vault(&[("kept.md", "# Kept\n"), ("doomed.md", "# Doomed\n")]);
        let store = GraphStore::in_memory().unwrap();
        let db_path = root.join("unused.lbug");
        index_markdown_directory_with_store(&store, &root, &db_path, "owned", "v", &[]).unwrap();

        let uid_of = |title: &str| {
            store
                .list_notes(None)
                .unwrap()
                .into_iter()
                .find(|note| note.title == title)
                .unwrap_or_else(|| panic!("{title} indexed"))
                .uid
        };
        let doomed_uid = uid_of("Doomed");
        let kept_uid = uid_of("Kept");

        store.set_embedding_metadata("test-model", 2).unwrap();
        // The doomed vector is the probe's nearest neighbour, so a regression
        // displaces the survivor rather than merely lingering unnoticed.
        assert!(store.add_embedding(&doomed_uid, vec![1.0, 0.0]));
        assert!(store.add_embedding(&kept_uid, vec![0.8, 0.6]));
        store.flush_embedding_index().unwrap();
        assert_eq!(
            store.try_vector_search(&[1.0, 0.0], 1).unwrap()[0].0,
            doomed_uid,
            "precondition: the doomed vector must outrank the survivor"
        );

        // Drop the note by ignoring it — the same delete path a removed file
        // takes, without racing the filesystem clock.
        let result = index_markdown_directory_since_with_store_and_ignore(
            &store,
            &root,
            "owned",
            "v",
            std::time::SystemTime::now() + std::time::Duration::from_secs(60),
            &["doomed.md".to_string()],
        )
        .unwrap();
        assert_eq!(
            result.notes_deleted, 1,
            "precondition: the note was dropped"
        );

        let hits = store.try_vector_search(&[1.0, 0.0], 2).unwrap();
        assert!(
            !hits.iter().any(|(uid, _)| uid == &doomed_uid),
            "a dropped note's vector must not still be scored; got {hits:?}"
        );
        assert_eq!(
            hits.first().map(|(uid, _)| uid.as_str()),
            Some(kept_uid.as_str()),
            "the live note must take the top-k slot the dead vector held"
        );
        assert!(
            store.has_embedding(&kept_uid),
            "a surviving note must KEEP its vector"
        );
    }

    #[test]
    fn since_refresh_deletes_an_incumbent_that_becomes_ignored() {
        let (_dir, root) = make_vault(&[("kept.md", "# Kept\n"), ("ignored.md", "# Old\n")]);
        let store = GraphStore::in_memory().unwrap();
        let db_path = root.join("unused.lbug");
        index_markdown_directory_with_store(&store, &root, &db_path, "owned", "v", &[]).unwrap();

        let result = index_markdown_directory_since_with_store_and_ignore(
            &store,
            &root,
            "owned",
            "v",
            std::time::SystemTime::now() + std::time::Duration::from_secs(60),
            &["ignored.md".to_string()],
        )
        .unwrap();
        assert_eq!(result.notes_updated, 0);
        assert_eq!(result.notes_deleted, 1);
        let notes = store.list_notes(None).unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].title, "Kept");
    }

    /// Build `n` distinct notes for a vault, with `make_note(0)` reusable so a
    /// caller can force a primary-key collision by pushing a duplicate.
    #[cfg(test)]
    fn atomic_test_note(v_uid: &str, i: usize) -> Note {
        use nestweaver_schema::NoteKind;
        Note {
            uid: format!("note:{v_uid}:{i}"),
            vault_uid: v_uid.to_string(),
            file_path: format!("n{i}.md"),
            title: format!("Note {i}"),
            note_kind: NoteKind::General,
            word_count: 3,
            content_hash: format!("hash{i}"),
            frontmatter: None,
            frontmatter_raw: None,
            created_at: None,
            modified_at: None,
            pagerank_score: None,
            embedding: None,
        }
    }

    /// T2.2: the vault reindex must fold the cascade delete and the re-insert
    /// into ONE transaction, so a concurrent reader (a fresh connection, which
    /// is exactly what `count_notes` opens) can never observe the empty
    /// intermediate between the delete and the insert.
    ///
    /// This asserts the invariant deterministically via the transaction
    /// boundary: a reindex whose insert phase fails midway must leave the OLD
    /// vault fully intact — never 0 notes. With a separate-transaction delete
    /// (the pre-fix behaviour) the delete commits before the insert runs, so a
    /// failed insert leaves the vault empty (0 notes) → RED. With the delete
    /// and insert sharing one transaction, the failed insert rolls the delete
    /// back and the old vault survives → GREEN. The same single-transaction
    /// boundary is precisely what stops a concurrent reader from ever seeing 0
    /// during a normal (successful) reindex.
    #[test]
    fn vault_reindex_never_exposes_empty() {
        let store = GraphStore::in_memory().unwrap();
        let v_uid = "vlt:atomic";
        let vault = Vault {
            uid: v_uid.to_string(),
            name: "atomic".to_string(),
            root_path: "/tmp/atomic".to_string(),
            instance_id: "test-instance".to_string(),
        };

        // Initial index: N notes, vault did not previously exist.
        let n = 5usize;
        let notes: Vec<Note> = (0..n).map(|i| atomic_test_note(v_uid, i)).collect();
        let edges: Vec<(&str, &str)> = notes.iter().map(|nt| (v_uid, nt.uid.as_str())).collect();
        store
            .bulk_vault_reindex_write(
                &vault,
                false,
                &notes,
                &[],
                &[],
                &edges,
                &[],
                &[],
                &[],
                &[],
                &[],
                &[],
                &[],
                &[],
                &[],
            )
            .unwrap();
        assert_eq!(
            store.count_notes().unwrap(),
            n,
            "initial index should produce N notes"
        );

        // Reindex whose insert phase FAILS: two notes share a uid, so the second
        // CREATE violates the Note primary key — after the cascade delete has
        // already run inside the transaction.
        let mut bad_notes: Vec<Note> = (0..n).map(|i| atomic_test_note(v_uid, i)).collect();
        bad_notes.push(atomic_test_note(v_uid, 0)); // duplicate uid → PK violation
        let bad_edges: Vec<(&str, &str)> = bad_notes
            .iter()
            .map(|nt| (v_uid, nt.uid.as_str()))
            .collect();
        let result = store.bulk_vault_reindex_write(
            &vault,
            true,
            &bad_notes,
            &[],
            &[],
            &bad_edges,
            &[],
            &[],
            &[],
            &[],
            &[],
            &[],
            &[],
            &[],
            &[],
        );
        assert!(
            result.is_err(),
            "a duplicate note uid must fail the reindex write"
        );

        // The invariant: the failed reindex must NEVER have exposed an empty
        // vault. The old vault must survive the aborted transaction intact.
        assert_eq!(
            store.count_notes().unwrap(),
            n,
            "failed reindex exposed an EMPTY vault: the cascade delete committed \
             without the re-insert — delete and insert must share one transaction"
        );
    }

    /// T2.2: a background reader polling the note count throughout a series of
    /// full vault reindexes must never sample 0. This exercises the atomic
    /// delete+insert under real concurrency (a separate reader connection racing
    /// the writer's transaction), complementing the deterministic
    /// transaction-boundary assertion above.
    #[test]
    fn vault_reindex_concurrent_reader_never_sees_empty() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let store = Arc::new(GraphStore::in_memory().unwrap());
        let v_uid = "vlt:concurrent";
        let vault = Vault {
            uid: v_uid.to_string(),
            name: "concurrent".to_string(),
            root_path: "/tmp/concurrent".to_string(),
            instance_id: "test-instance".to_string(),
        };

        let n = 8usize;
        let seed: Vec<Note> = (0..n).map(|i| atomic_test_note(v_uid, i)).collect();
        let seed_edges: Vec<(&str, &str)> =
            seed.iter().map(|nt| (v_uid, nt.uid.as_str())).collect();
        store
            .bulk_vault_reindex_write(
                &vault,
                false,
                &seed,
                &[],
                &[],
                &seed_edges,
                &[],
                &[],
                &[],
                &[],
                &[],
                &[],
                &[],
                &[],
                &[],
            )
            .unwrap();

        // Reader thread: poll the note count until told to stop, recording the
        // minimum it ever observes.
        let stop = Arc::new(AtomicBool::new(false));
        let store_rd = Arc::clone(&store);
        let stop_rd = Arc::clone(&stop);
        let reader = std::thread::spawn(move || {
            let mut min_seen = usize::MAX;
            while !stop_rd.load(Ordering::Relaxed) {
                let c = store_rd.count_notes().unwrap();
                if c < min_seen {
                    min_seen = c;
                }
            }
            min_seen
        });

        // Writer: repeatedly reindex the whole vault (existing → cascade delete
        // + re-insert in one transaction).
        for _ in 0..40 {
            let notes: Vec<Note> = (0..n).map(|i| atomic_test_note(v_uid, i)).collect();
            let edges: Vec<(&str, &str)> =
                notes.iter().map(|nt| (v_uid, nt.uid.as_str())).collect();
            store
                .bulk_vault_reindex_write(
                    &vault,
                    true,
                    &notes,
                    &[],
                    &[],
                    &edges,
                    &[],
                    &[],
                    &[],
                    &[],
                    &[],
                    &[],
                    &[],
                    &[],
                    &[],
                )
                .unwrap();
        }

        stop.store(true, Ordering::Relaxed);
        let min_seen = reader.join().unwrap();
        assert_ne!(
            min_seen, 0,
            "a concurrent reader observed an EMPTY vault mid-reindex; the cascade \
             delete and re-insert must be atomic"
        );
        assert_eq!(
            store.count_notes().unwrap(),
            n,
            "final vault state must have all N notes"
        );
    }
}
#[cfg(test)]
mod link_resolution_tests {
    use super::normalize_relative;

    /// nw-165: `..` and `.` segments must resolve against the source folder.
    #[test]
    fn relative_segments_normalize_against_the_source_folder() {
        assert_eq!(
            normalize_relative("workspaces/orbit/notes/2026-08/prd", "../research/x").as_deref(),
            Some("workspaces/orbit/notes/2026-08/research/x")
        );
        assert_eq!(
            normalize_relative("workspaces/orbit/backlog", "../../../backlog").as_deref(),
            Some("backlog")
        );
        assert_eq!(normalize_relative("a/b", "./c").as_deref(), Some("a/b/c"));
    }

    /// Escaping the vault root yields None rather than a path outside it.
    #[test]
    fn escaping_the_vault_root_is_refused() {
        assert_eq!(normalize_relative("a", "../../x"), None);
        assert_eq!(normalize_relative("", ".."), None);
    }
}
