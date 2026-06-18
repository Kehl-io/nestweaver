//! Markdown indexing pipeline — the walking-skeleton sibling of `index.rs`.
//!
//! Walks a vault directory, parses each `.md` file with the markdown parser,
//! and persists flat `Note` nodes alongside a single `Vault` node. No
//! headings, sections, wikilinks, or PPR integration yet — those land in
//! later phases.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Context;
use globset::GlobSet;
use indicatif::{ProgressBar, ProgressStyle};
use nestweaver_parser::{
    ParsedNote, RawTag, RawWikilink, SkippedFile, TagSource, is_markdown, parse_markdown,
};
use nestweaver_schema::{
    EdgeType, Heading, Note, ResolvedEdge, Section, Tag, Vault, heading_uid, note_uid, section_uid,
    tag_uid, vault_uid,
};
use nestweaver_store::GraphStore;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

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

/// Cap on per-file size to avoid pathological inputs (e.g. multi-MB log dumps
/// pasted into a note). Files above this size are skipped with a warning.
/// Per-file cap on note size. Files larger than this are skipped with a
/// logged warning. Architecture doc §9.7 specifies 1 MB; multi-MB markdown
/// is almost always machine-generated (pasted logs, exported data dumps)
/// and parsing them takes seconds while tanking ranking quality.
const MAX_FILE_SIZE_BYTES: u64 = 1024 * 1024; // 1 MB

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
    let store = GraphStore::open_or_create(db_path)
        .with_context(|| format!("failed to open/create GraphStore at {}", db_path.display()))?;
    index_markdown_directory_with_store(
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
    let ignore_set = crate::brainignore::load_brain_ignore(vault_root, extra_ignore_patterns);
    let result = index_into_store(vault_root, store, instance_id, vault_name, &ignore_set)?;

    let aliases = load_taxonomy_aliases(vault_root);
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
    let ignore_set = crate::brainignore::load_brain_ignore(vault_root, &[]);
    let result = index_into_store(vault_root, &store, instance_id, vault_name, &ignore_set)?;
    Ok((result, store))
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
/// modification time is >= `since`. For each matching file the old Note
/// (and its descendants) is cascade-deleted then re-parsed and re-inserted.
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

    let canonical = std::fs::canonicalize(vault_root).unwrap_or_else(|_| vault_root.to_path_buf());
    let root_str = canonical.to_string_lossy().into_owned();
    let vault_root: &Path = &canonical;
    let v_uid = vault_uid(instance_id, &root_str);
    let ignore_set = crate::brainignore::load_brain_ignore(vault_root, extra_ignore_patterns);

    store
        .upsert_vault(&Vault {
            uid: v_uid.clone(),
            name: vault_name.to_string(),
            root_path: root_str.clone(),
            instance_id: instance_id.to_string(),
        })
        .context("upsert_vault")?;

    // Track visited directory inodes to detect symlink loops.
    #[cfg(unix)]
    let mut seen_inodes: HashSet<(u64, u64)> = HashSet::new();

    let walker = WalkDir::new(vault_root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            if e.file_type().is_dir()
                && e.file_name()
                    .to_str()
                    .is_some_and(|name| SKIP_DIRS.contains(&name))
            {
                return false;
            }
            #[cfg(unix)]
            if e.file_type().is_dir()
                && let Ok(meta) = std::fs::metadata(e.path())
            {
                use std::os::unix::fs::MetadataExt;
                let key = (meta.dev(), meta.ino());
                if !seen_inodes.insert(key) {
                    tracing::debug!("skipping already-visited inode: {}", e.path().display());
                    return false;
                }
            }
            true
        });

    let mut files_checked = 0usize;
    let mut notes_updated = 0usize;
    let mut notes_deleted = 0usize;
    let mut total_headings = 0usize;
    let mut total_sections = 0usize;
    let mut total_tags = 0usize;
    let mut total_wikilinks = 0usize;

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!("walkdir error (since): {err}");
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if !is_markdown(path) {
            continue;
        }
        // Reject symlinks whose target is outside the vault root.
        if entry.path_is_symlink() {
            match std::fs::canonicalize(path) {
                Ok(resolved) if !resolved.starts_with(vault_root) => {
                    tracing::warn!("skipping symlink escaping vault root: {}", path.display());
                    continue;
                }
                Err(_) => {
                    tracing::warn!("cannot resolve symlink: {}", path.display());
                    continue;
                }
                Ok(_) => {}
            }
        }

        // Apply .brainignore patterns.
        let rel_for_ignore = path
            .strip_prefix(vault_root)
            .unwrap_or(path)
            .to_string_lossy();
        if crate::brainignore::is_ignored(&rel_for_ignore, &ignore_set) {
            tracing::debug!("brainignore: skipping {}", rel_for_ignore);
            continue;
        }

        files_checked += 1;

        // Filter by modification time.
        let mtime = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        if mtime < since {
            continue;
        }

        // Size guard.
        if let Ok(meta) = entry.metadata()
            && meta.len() > MAX_FILE_SIZE_BYTES
        {
            tracing::warn!("skipping oversized file: {}", path.display());
            continue;
        }

        let source = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!("read error {}: {err}", path.display());
                continue;
            }
        };

        let rel_path = path
            .strip_prefix(vault_root)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned();

        let parsed: ParsedNote = match parse_markdown(&rel_path, &source) {
            Ok(p) => p,
            Err(err) => {
                tracing::warn!("parse error {rel_path}: {err}");
                continue;
            }
        };

        let n_uid = note_uid(&v_uid, &rel_path);

        // Delete old note data (cascade). Safe when note doesn't exist.
        if let Err(e) = store.delete_note_cascade(&n_uid) {
            tracing::warn!("delete_note_cascade {n_uid} failed: {e}");
        } else {
            notes_deleted += 1;
        }

        let (h_count, s_count, wl_count, t_count) =
            reinsert_single_note(&store, &v_uid, &n_uid, path, &rel_path, &parsed)
                .with_context(|| format!("reinsert_single_note {rel_path}"))?;

        total_headings += h_count;
        total_sections += s_count;
        total_wikilinks += wl_count;
        total_tags += t_count;
        notes_updated += 1;
    }

    Ok(MarkdownSinceResult {
        vault_name: vault_name.to_string(),
        files_checked,
        notes_updated,
        notes_deleted,
        headings_count: total_headings,
        sections_count: total_sections,
        tags_count: total_tags,
        wikilinks_resolved: total_wikilinks,
    })
}

/// Insert a single note (and all its descendants: headings, sections, tags,
/// wikilinks) into the store. Mirrors the watcher's per-file insertion logic.
/// Returns `(headings_count, sections_count, wikilinks_resolved, tags_count)`.
fn reinsert_single_note(
    store: &GraphStore,
    v_uid: &str,
    n_uid: &str,
    path: &Path,
    rel_path: &str,
    parsed: &ParsedNote,
) -> Result<(usize, usize, usize, usize), anyhow::Error> {
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

    store
        .insert_note(&Note {
            uid: n_uid.to_string(),
            vault_uid: v_uid.to_string(),
            file_path: rel_path.to_string(),
            title: parsed.title.clone(),
            note_kind: parsed.note_kind,
            word_count: parsed.word_count,
            content_hash: parsed.content_hash.clone(),
            frontmatter: frontmatter_json,
            created_at,
            modified_at,
            pagerank_score: None,
            embedding: None,
        })
        .context("insert_note")?;
    store
        .insert_vault_note_edge(v_uid, n_uid)
        .context("insert_vault_note_edge")?;

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
            content_hash: sha256_hex_short(&h.text),
            embedding: None,
        })
        .collect();
    store
        .batch_insert_headings(&headings)
        .context("batch_insert_headings")?;
    let nh_edges: Vec<(&str, &str)> = heading_uids.iter().map(|h| (n_uid, h.as_str())).collect();
    store
        .batch_insert_note_heading_edges(&nh_edges)
        .context("batch_insert_note_heading_edges")?;

    let mut parent_edges: Vec<(String, String)> = Vec::new();
    for (idx, h) in parsed.headings.iter().enumerate() {
        for prev in (0..idx).rev() {
            if parsed.headings[prev].level < h.level {
                parent_edges.push((heading_uids[idx].clone(), heading_uids[prev].clone()));
                break;
            }
        }
    }
    let parent_refs: Vec<(&str, &str)> = parent_edges
        .iter()
        .map(|(c, p)| (c.as_str(), p.as_str()))
        .collect();
    store
        .batch_insert_heading_parent_edges(&parent_refs)
        .context("batch_insert_heading_parent_edges")?;

    // Sections.
    let mut section_uids: Vec<String> = Vec::with_capacity(parsed.sections.len());
    let mut sections: Vec<Section> = Vec::with_capacity(parsed.sections.len());
    let mut ns_edges: Vec<(String, String)> = Vec::new();
    let mut hs_edges: Vec<(String, String)> = Vec::new();
    for sec in &parsed.sections {
        let text_hash = sha256_hex(&sec.text);
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
    store
        .batch_insert_sections(&sections)
        .context("batch_insert_sections")?;
    let ns_refs: Vec<(&str, &str)> = ns_edges
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    store
        .batch_insert_note_section_edges(&ns_refs)
        .context("batch_insert_note_section_edges")?;
    let hs_refs: Vec<(&str, &str)> = hs_edges
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    store
        .batch_insert_heading_section_edges(&hs_refs)
        .context("batch_insert_heading_section_edges")?;

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
    for t in &new_tag_nodes {
        if let Err(e) = store.insert_tag(t)
            && !e.is_duplicate()
        {
            tracing::warn!("insert_tag {} failed: {e}", t.name);
        }
    }
    let nt_refs: Vec<(&str, &str)> = note_tag_edges
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    store
        .batch_insert_note_tag_edges(&nt_refs)
        .context("batch_insert_note_tag_edges")?;
    let st_refs: Vec<(&str, &str)> = section_tag_edges
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    store
        .batch_insert_section_tag_edges(&st_refs)
        .context("batch_insert_section_tag_edges")?;
    let tags_count = local_tag_uids.len();

    // Wikilinks: resolve against all notes currently in the DB.
    let all_notes = store.list_notes(None).unwrap_or_default();
    let title_lookup: HashMap<String, Vec<String>> = {
        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        for n in &all_notes {
            map.entry(n.title.to_lowercase())
                .or_default()
                .push(n.uid.clone());
        }
        map
    };
    let mut wl_note_edges: Vec<(String, String, f32, String)> = Vec::new();
    let mut wl_head_edges: Vec<(String, String, f32, String)> = Vec::new();

    for wl in &parsed.wikilinks {
        if wl.section_idx >= section_uids.len() {
            continue;
        }
        let source_section = &section_uids[wl.section_idx];
        let display = wl.display.clone().unwrap_or_else(|| wl.target.clone());
        let key = wl.target.to_lowercase();
        let Some(candidates) = title_lookup.get(&key) else {
            // Genuinely unresolved — record so broken-links can surface it.
            let uw_uid = format!(
                "unresolved:{}:{}",
                source_section,
                sha256_hex_short(&wl.target)
            );
            if let Err(e) = store.insert_unresolved_wikilink(
                &uw_uid,
                n_uid,
                rel_path,
                &parsed.title,
                &wl.target,
            ) {
                tracing::warn!("failed to record unresolved wikilink '{}': {e}", wl.target);
            }
            continue;
        };
        let n = candidates.len() as f32;
        let conf = if n == 1.0 { 1.0 } else { 1.0 / n };
        for target in candidates {
            if let Some(anchor) = &wl.heading_anchor {
                let anchor_slug = nestweaver_parser::markdown::slugify(anchor);
                if let Ok(headings) = store.headings_in_note(target)
                    && let Some(h) = headings.iter().find(|h| h.slug == anchor_slug)
                {
                    wl_head_edges.push((
                        source_section.clone(),
                        h.uid.clone(),
                        conf,
                        display.clone(),
                    ));
                    continue;
                }
            }
            wl_note_edges.push((
                source_section.clone(),
                target.clone(),
                conf,
                display.clone(),
            ));
        }
    }
    let wl_resolved = wl_note_edges.len() + wl_head_edges.len();
    let wl_note_refs: Vec<(&str, &str, f32, &str)> = wl_note_edges
        .iter()
        .map(|(s, n, c, d)| (s.as_str(), n.as_str(), *c, d.as_str()))
        .collect();
    store
        .batch_insert_wikilink_to_note_edges(&wl_note_refs)
        .context("batch_insert_wikilink_to_note_edges")?;
    let wl_head_refs: Vec<(&str, &str, f32, &str)> = wl_head_edges
        .iter()
        .map(|(s, h, c, d)| (s.as_str(), h.as_str(), *c, d.as_str()))
        .collect();
    store
        .batch_insert_wikilink_to_heading_edges(&wl_head_refs)
        .context("batch_insert_wikilink_to_heading_edges")?;

    Ok((headings.len(), sections.len(), wl_resolved, tags_count))
}

fn index_into_store(
    vault_root: &Path,
    store: &GraphStore,
    instance_id: &str,
    vault_name: &str,
    ignore_set: &GlobSet,
) -> Result<MarkdownIndexResult, anyhow::Error> {
    let started = Instant::now();

    // Canonicalize the root so vault_uid and note rel_paths agree with the
    // watcher (which sees canonical paths from FSEvents on macOS).
    let canonical = std::fs::canonicalize(vault_root).unwrap_or_else(|_| vault_root.to_path_buf());
    let root_str = canonical.to_string_lossy().into_owned();
    let vault_root: &Path = &canonical;
    let v_uid = vault_uid(instance_id, &root_str);

    // 1. Insert the Vault node. If the vault was already indexed, cascade-
    //    delete the old data first so re-indexing is idempotent.
    if store.lookup_vault(&v_uid).is_ok() {
        store
            .delete_vault_cascade(&v_uid)
            .context("delete_vault_cascade (re-index)")?;
    }
    let vault = Vault {
        uid: v_uid.clone(),
        name: vault_name.to_string(),
        root_path: root_str.clone(),
        instance_id: instance_id.to_string(),
    };
    store.insert_vault(&vault).context("insert_vault")?;

    // ── Phase 1: Scan notes ───────────────────────────────────────────────
    let scan_pb = ProgressBar::new_spinner();
    scan_pb.set_style(
        ProgressStyle::with_template("{spinner:.cyan} {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_spinner()),
    );
    scan_pb.set_message("Scanning notes...");

    /// Per-file data collected during the scan phase.
    struct ScannedNote {
        path: PathBuf,
        metadata: Option<std::fs::Metadata>,
    }

    let mut scanned_notes: Vec<ScannedNote> = Vec::new();
    let mut skipped: Vec<SkippedFile> = Vec::new();

    // Track visited directory inodes to detect symlink loops.
    #[cfg(unix)]
    let mut seen_inodes: HashSet<(u64, u64)> = HashSet::new();

    // SECURITY: do NOT follow symlinks. A vault containing a symlink to
    // /etc/passwd (or any path outside the vault) would otherwise be
    // indexed, and `note_get` would then exfiltrate the target file's
    // contents through Claude. Symlinks are silently skipped.
    let walker = WalkDir::new(vault_root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            if e.file_type().is_dir()
                && e.file_name()
                    .to_str()
                    .is_some_and(|name| SKIP_DIRS.contains(&name))
            {
                return false;
            }
            #[cfg(unix)]
            if e.file_type().is_dir()
                && let Ok(meta) = std::fs::metadata(e.path())
            {
                use std::os::unix::fs::MetadataExt;
                let key = (meta.dev(), meta.ino());
                if !seen_inodes.insert(key) {
                    tracing::debug!("skipping already-visited inode: {}", e.path().display());
                    return false;
                }
            }
            true
        });

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!("walkdir error: {err}");
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if !is_markdown(path) {
            continue;
        }

        // Reject symlinks whose target is outside the vault root.
        if entry.path_is_symlink() {
            match std::fs::canonicalize(path) {
                Ok(resolved) if !resolved.starts_with(vault_root) => {
                    skipped.push(SkippedFile {
                        path: path.to_string_lossy().into_owned(),
                        reason: "symlink target outside vault root".to_string(),
                    });
                    continue;
                }
                Err(e) => {
                    skipped.push(SkippedFile {
                        path: path.to_string_lossy().into_owned(),
                        reason: format!("cannot resolve symlink: {e}"),
                    });
                    continue;
                }
                Ok(_) => {}
            }
        }

        // Apply .brainignore patterns.
        let rel_for_ignore = path
            .strip_prefix(vault_root)
            .unwrap_or(path)
            .to_string_lossy();
        if crate::brainignore::is_ignored(&rel_for_ignore, ignore_set) {
            tracing::debug!("brainignore: skipping {}", rel_for_ignore);
            skipped.push(SkippedFile {
                path: path.to_string_lossy().into_owned(),
                reason: "matched .brainignore pattern".to_string(),
            });
            continue;
        }

        // Size guard.
        let metadata = entry.metadata().ok();
        if let Some(ref meta) = metadata
            && meta.len() > MAX_FILE_SIZE_BYTES
        {
            skipped.push(SkippedFile {
                path: path.to_string_lossy().into_owned(),
                reason: format!("file exceeds {} bytes", MAX_FILE_SIZE_BYTES),
            });
            continue;
        }

        scanned_notes.push(ScannedNote {
            path: path.to_path_buf(),
            metadata,
        });
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

    let outcomes: Vec<NoteOutcome> = scanned_notes
        .par_iter()
        .map(|scanned| {
            let path = &scanned.path;

            let display_name = path
                .strip_prefix(vault_root)
                .unwrap_or(path)
                .to_string_lossy()
                .into_owned();

            // Read.
            let source = match std::fs::read_to_string(path) {
                Ok(s) => s,
                Err(err) => {
                    parse_pb.inc(1);
                    return NoteOutcome::Skipped(SkippedFile {
                        path: path.to_string_lossy().into_owned(),
                        reason: format!("read error: {err}"),
                    });
                }
            };

            // Compute relative path from vault root for stable UIDs.
            let rel_path = display_name;

            let parsed: ParsedNote = match parse_markdown(&rel_path, &source) {
                Ok(p) => p,
                Err(err) => {
                    parse_pb.inc(1);
                    return NoteOutcome::Skipped(SkippedFile {
                        path: rel_path.clone(),
                        reason: err.to_string(),
                    });
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

            // File timestamps — best-effort, never fatal.
            let (created_at, modified_at) = match &scanned.metadata {
                Some(meta) => {
                    let created = meta.created().ok().and_then(format_system_time);
                    let modified = meta.modified().ok().and_then(format_system_time);
                    (created, modified)
                }
                None => (None, None),
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
                    content_hash: sha256_hex_short(&h.text),
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
                let text_hash = sha256_hex(&sec.text);
                let short = &text_hash[..12];
                let s_uid = section_uid(&n_uid, sec.start_line, short);
                let word_count =
                    u32::try_from(sec.text.split_whitespace().count()).unwrap_or(u32::MAX);
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
        })
        .collect();

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

    // Insert tag nodes outside the main transaction so that duplicate tags
    // from earlier index runs are silently skipped rather than aborting the
    // whole batch.
    for tag in &all_tags {
        if let Err(e) = store.insert_tag(tag) {
            if e.is_duplicate() {
                tracing::debug!("insert_tag {} skipped (already exists): {e}", tag.name);
            } else {
                return Err(e).context("insert_tag");
            }
        }
    }

    // Wikilink resolution: build lookup indices once, then 5-priority match.
    let lookup = WikilinkLookup::build(&note_contexts);

    let mut wikilink_to_note: Vec<(String, String, f32, String)> = Vec::new();
    let mut wikilink_to_heading: Vec<(String, String, f32, String)> = Vec::new();
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
                        sha256_hex_short(&wl.target)
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
    {
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
        let wl_note_refs: Vec<(&str, &str, f32, &str)> = wikilink_to_note
            .iter()
            .map(|(s, n, c, d)| (s.as_str(), n.as_str(), *c, d.as_str()))
            .collect();
        let wl_head_refs: Vec<(&str, &str, f32, &str)> = wikilink_to_heading
            .iter()
            .map(|(s, h, c, d)| (s.as_str(), h.as_str(), *c, d.as_str()))
            .collect();

        store
            .bulk_vault_write(
                &all_notes,
                &all_headings,
                &all_sections,
                &vault_note_refs,
                &note_heading_refs,
                &note_section_refs,
                &heading_section_refs,
                &heading_parent_refs,
                &[],
                &note_tag_refs,
                &section_tag_refs,
                &wl_note_refs,
                &wl_head_refs,
            )
            .context("bulk_vault_write")?;
    }

    // Persist genuinely-unresolved wikilinks so broken-links surfaces them.
    for (uid, snu, sp, st, wt) in &unresolved_records {
        if let Err(e) = store.insert_unresolved_wikilink(uid, snu, sp, st, wt) {
            tracing::warn!("failed to record unresolved wikilink '{wt}': {e}");
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

    Ok(MarkdownIndexResult {
        vault_uid: v_uid,
        vault_name: vault_name.to_string(),
        notes_count,
        headings_count,
        sections_count,
        tags_count,
        wikilinks_resolved,
        wikilinks_unresolved,
        skipped,
    })
}

fn sha256_hex(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hex::encode(hasher.finalize())
}

fn sha256_hex_short(text: &str) -> String {
    sha256_hex(text)[..12].to_string()
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
struct WikilinkLookup<'a> {
    /// Path key → note_uid. Path keys are lowercased, with optional ".md"
    /// stripped, normalised to forward slashes.
    by_path: HashMap<String, &'a str>,
    /// Lowercased title → list of note_uids that have that title.
    by_title: HashMap<String, Vec<&'a str>>,
    /// Lowercased alias → list of note_uids that declare that alias.
    by_alias: HashMap<String, Vec<&'a str>>,
    /// note_uid → folder (relative, forward-slash). For same-folder priority.
    folder_by_note: HashMap<&'a str, &'a str>,
    /// note_uid → heading slug → heading_uid. For anchor resolution.
    headings_by_note: HashMap<&'a str, HashMap<&'a str, &'a str>>,
    /// (folder, lowercased title OR filename stem) → list of note_uids in
    /// that folder. Drives priority-3 same-folder resolution: lets a
    /// wikilink target match a sibling note by title OR by `note-name`
    /// even when no global title match exists.
    by_folder_name: HashMap<(String, String), Vec<&'a str>>,
    /// All known note UIDs (F11: lets frontmatter reference a canonical UID).
    known_uids: HashSet<&'a str>,
}

impl<'a> WikilinkLookup<'a> {
    fn build(notes: &'a [NoteContext]) -> Self {
        let mut by_path: HashMap<String, &'a str> = HashMap::new();
        let mut by_title: HashMap<String, Vec<&'a str>> = HashMap::new();
        let mut by_alias: HashMap<String, Vec<&'a str>> = HashMap::new();
        let mut folder_by_note: HashMap<&'a str, &'a str> = HashMap::new();
        let mut headings_by_note: HashMap<&'a str, HashMap<&'a str, &'a str>> = HashMap::new();
        let mut by_folder_name: HashMap<(String, String), Vec<&'a str>> = HashMap::new();
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
            let title_lc = note.title.to_lowercase();
            by_folder_name
                .entry((folder.clone(), title_lc))
                .or_default()
                .push(note.note_uid.as_str());
            if let Some(stem) = std::path::Path::new(&note.rel_path)
                .file_stem()
                .and_then(|s| s.to_str())
            {
                let stem_lc = stem.to_lowercase();
                by_folder_name
                    .entry((folder, stem_lc))
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
            folder_by_note,
            headings_by_note,
            by_folder_name,
            known_uids,
        }
    }

    /// Apply the 5-priority resolution scheme to `target`.
    ///
    /// 1. Path match: target contains `/` and matches a known path (lowercased,
    ///    with/without `.md`) → confidence 1.0.
    /// 2. Unique title match → confidence 1.0.
    /// 3. Alias match → confidence 0.7.
    /// 4. Same-folder match (title scoped to source's folder) → confidence 0.5.
    /// 5. Ambiguous (multiple title/alias matches) → split confidence 1/N.
    fn resolve(&self, target: &str, source_folder: &str) -> ResolveOutcome {
        let key = target.trim().replace('\\', "/").to_lowercase();
        if key.is_empty() {
            return ResolveOutcome::Unresolved;
        }

        // Priority 1: path match.
        if key.contains('/')
            && let Some(&uid) = self.by_path.get(&key)
        {
            return ResolveOutcome::Resolved(vec![ResolveCandidate {
                note_uid: uid.to_string(),
                confidence: 1.0,
            }]);
        }

        // Priority 2: unique title match.
        if let Some(uids) = self.by_title.get(&key)
            && uids.len() == 1
        {
            return ResolveOutcome::Resolved(vec![ResolveCandidate {
                note_uid: uids[0].to_string(),
                confidence: 1.0,
            }]);
        }

        // Priority 3: same-folder match.
        // A wikilink `[[target]]` in note F/x.md resolves to F/y.md when
        // F/y.md has either a title or a filename stem equal to `target`.
        // This is the priority that lets sibling-relative links work
        // without forcing the user to add aliases or write the full path.
        // Also acts as ambiguity-breaker when multiple notes share a
        // title and the source is co-located with one of them.
        if let Some(uids) = self
            .by_folder_name
            .get(&(source_folder.to_string(), key.clone()))
            && uids.len() == 1
        {
            return ResolveOutcome::Resolved(vec![ResolveCandidate {
                note_uid: uids[0].to_string(),
                confidence: 0.5,
            }]);
        }

        // Priority 4: alias match (unique → 0.7, ambiguous → split).
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

        // Priority 5: ambiguous title match (was bundled inside priority
        // 2; now it's the last-resort tier so we always try alias / same-
        // folder first when the global title is non-unique).
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

        // Parse YAML frontmatter for `aliases:` mapping.
        if let Some(fm) = extract_frontmatter(&content)
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
        let body = skip_frontmatter(&content);
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

        // Only process the first taxonomy file found.
        break;
    }

    aliases
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
}
