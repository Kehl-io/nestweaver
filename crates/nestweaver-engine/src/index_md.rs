//! Markdown indexing pipeline — the walking-skeleton sibling of `index.rs`.
//!
//! Walks a vault directory, parses each `.md` file with the markdown parser,
//! and persists flat `Note` nodes alongside a single `Vault` node. No
//! headings, sections, wikilinks, or PPR integration yet — those land in
//! later phases.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Context;
use nestweaver_parser::{
    ParsedNote, RawTag, RawWikilink, SkippedFile, TagSource, is_markdown, parse_markdown,
};
use nestweaver_schema::{
    Heading, Note, Section, Tag, Vault, heading_uid, note_uid, section_uid, tag_uid, vault_uid,
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
pub fn index_markdown_directory(
    vault_root: &Path,
    db_path: &Path,
    instance_id: &str,
    vault_name: &str,
) -> Result<MarkdownIndexResult, anyhow::Error> {
    let store = GraphStore::open_or_create(db_path)
        .with_context(|| format!("failed to open/create GraphStore at {}", db_path.display()))?;
    index_into_store(vault_root, &store, instance_id, vault_name)
}

/// Index a markdown vault into an in-memory `GraphStore` (for tests).
pub fn index_markdown_directory_in_memory(
    vault_root: &Path,
    instance_id: &str,
    vault_name: &str,
) -> Result<(MarkdownIndexResult, GraphStore), anyhow::Error> {
    let store = GraphStore::in_memory().context("create in-memory GraphStore")?;
    let result = index_into_store(vault_root, &store, instance_id, vault_name)?;
    Ok((result, store))
}

fn index_into_store(
    vault_root: &Path,
    store: &GraphStore,
    instance_id: &str,
    vault_name: &str,
) -> Result<MarkdownIndexResult, anyhow::Error> {
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

    // ── Pass 1: walk, parse, accumulate nodes ───────────────────────────────
    // We accumulate every node's data into batches plus a per-note context
    // so pass 2 (wikilink + tag resolution) can do its work without
    // re-parsing or re-walking.
    let mut all_notes: Vec<Note> = Vec::new();
    let mut all_headings: Vec<Heading> = Vec::new();
    let mut all_sections: Vec<Section> = Vec::new();
    let mut edge_pairs: Vec<(String, String)> = Vec::new();
    let mut note_heading_edges: Vec<(String, String)> = Vec::new();
    let mut note_section_edges: Vec<(String, String)> = Vec::new();
    let mut heading_section_edges: Vec<(String, String)> = Vec::new();
    let mut heading_parent_edges: Vec<(String, String)> = Vec::new();
    let mut skipped: Vec<SkippedFile> = Vec::new();
    // Per-note context for pass-2 cross-reference resolution.
    let mut note_contexts: Vec<NoteContext> = Vec::new();

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

        // Size guard.
        if let Ok(meta) = entry.metadata()
            && meta.len() > MAX_FILE_SIZE_BYTES
        {
            skipped.push(SkippedFile {
                path: path.to_string_lossy().into_owned(),
                reason: format!("file exceeds {} bytes", MAX_FILE_SIZE_BYTES),
            });
            continue;
        }

        // Read.
        let source = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(err) => {
                skipped.push(SkippedFile {
                    path: path.to_string_lossy().into_owned(),
                    reason: format!("read error: {err}"),
                });
                continue;
            }
        };

        // Compute relative path from vault root for stable UIDs.
        let rel_path = path
            .strip_prefix(vault_root)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned();

        let parsed: ParsedNote = match parse_markdown(&rel_path, &source) {
            Ok(p) => p,
            Err(err) => {
                skipped.push(SkippedFile {
                    path: rel_path.clone(),
                    reason: err.to_string(),
                });
                continue;
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
        let (created_at, modified_at) = match entry.metadata() {
            Ok(meta) => {
                let created = meta.created().ok().and_then(format_system_time);
                let modified = meta.modified().ok().and_then(format_system_time);
                (created, modified)
            }
            Err(_) => (None, None),
        };

        all_notes.push(Note {
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
        });
        edge_pairs.push((v_uid.clone(), n_uid.clone()));

        // Derive Heading UIDs and Heading nodes from the parsed outline.
        let heading_uids: Vec<String> = parsed
            .headings
            .iter()
            .map(|h| heading_uid(&n_uid, &h.slug, h.start_line))
            .collect();
        for (idx, h) in parsed.headings.iter().enumerate() {
            let h_uid = heading_uids[idx].clone();
            all_headings.push(Heading {
                uid: h_uid.clone(),
                note_uid: n_uid.clone(),
                level: h.level,
                text: h.text.clone(),
                slug: h.slug.clone(),
                start_line: h.start_line,
                end_line: h.end_line,
                content_hash: sha256_hex_short(&h.text),
            });
            note_heading_edges.push((n_uid.clone(), h_uid));
        }

        // Heading parent edges: for each heading, find its nearest preceding
        // ancestor — the most recent heading whose level is strictly shallower.
        // Standard outline semantics.
        for (idx, h) in parsed.headings.iter().enumerate() {
            for prev_idx in (0..idx).rev() {
                if parsed.headings[prev_idx].level < h.level {
                    heading_parent_edges
                        .push((heading_uids[idx].clone(), heading_uids[prev_idx].clone()));
                    break;
                }
            }
        }

        // Derive Section UIDs and Section nodes.
        let mut section_uids: Vec<String> = Vec::with_capacity(parsed.sections.len());
        for sec in &parsed.sections {
            let text_hash = sha256_hex(&sec.text);
            let short = &text_hash[..12];
            let s_uid = section_uid(&n_uid, sec.start_line, short);
            let word_count = u32::try_from(sec.text.split_whitespace().count()).unwrap_or(u32::MAX);
            let heading_link = sec.heading_idx.map(|i| heading_uids[i].clone());
            all_sections.push(Section {
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
            note_section_edges.push((n_uid.clone(), s_uid.clone()));
            if let Some(h_uid) = heading_link {
                heading_section_edges.push((h_uid, s_uid.clone()));
            }
            section_uids.push(s_uid);
        }

        // Record per-note context for pass-2 cross-reference resolution.
        let folder = Path::new(&rel_path)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        note_contexts.push(NoteContext {
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
        });
    }

    let notes_count = all_notes.len();
    let headings_count = all_headings.len();
    let sections_count = all_sections.len();

    // 3. Batch insert nodes first (edge inserts depend on both endpoints existing).
    store
        .batch_insert_notes(&all_notes)
        .context("batch_insert_notes")?;
    store
        .batch_insert_headings(&all_headings)
        .context("batch_insert_headings")?;
    store
        .batch_insert_sections(&all_sections)
        .context("batch_insert_sections")?;

    // 4. Batch insert all containment edges.
    let edge_refs: Vec<(&str, &str)> = edge_pairs
        .iter()
        .map(|(v, n)| (v.as_str(), n.as_str()))
        .collect();
    store
        .batch_insert_vault_note_edges(&edge_refs)
        .context("batch_insert_vault_note_edges")?;

    let note_heading_refs: Vec<(&str, &str)> = note_heading_edges
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    store
        .batch_insert_note_heading_edges(&note_heading_refs)
        .context("batch_insert_note_heading_edges")?;

    let note_section_refs: Vec<(&str, &str)> = note_section_edges
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    store
        .batch_insert_note_section_edges(&note_section_refs)
        .context("batch_insert_note_section_edges")?;

    let heading_section_refs: Vec<(&str, &str)> = heading_section_edges
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    store
        .batch_insert_heading_section_edges(&heading_section_refs)
        .context("batch_insert_heading_section_edges")?;

    let heading_parent_refs: Vec<(&str, &str)> = heading_parent_edges
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    store
        .batch_insert_heading_parent_edges(&heading_parent_refs)
        .context("batch_insert_heading_parent_edges")?;

    // ── Pass 2: cross-reference resolution (tags + wikilinks) ───────────────
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

    for tag in &all_tags {
        if let Err(e) = store.insert_tag(tag) {
            if e.is_duplicate() {
                tracing::debug!("insert_tag {} skipped (already exists): {e}", tag.name);
            } else {
                return Err(e).context("insert_tag");
            }
        }
    }
    let note_tag_refs: Vec<(&str, &str)> = note_tag_edges
        .iter()
        .map(|(n, t)| (n.as_str(), t.as_str()))
        .collect();
    store
        .batch_insert_note_tag_edges(&note_tag_refs)
        .context("batch_insert_note_tag_edges")?;
    let section_tag_refs: Vec<(&str, &str)> = section_tag_edges
        .iter()
        .map(|(s, t)| (s.as_str(), t.as_str()))
        .collect();
    store
        .batch_insert_section_tag_edges(&section_tag_refs)
        .context("batch_insert_section_tag_edges")?;

    // Wikilink resolution: build lookup indices once, then 5-priority match.
    let lookup = WikilinkLookup::build(&note_contexts);

    let mut wikilink_to_note: Vec<(String, String, f32, String)> = Vec::new();
    let mut wikilink_to_heading: Vec<(String, String, f32, String)> = Vec::new();
    let mut wikilinks_unresolved: usize = 0;

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
                }
            }
        }
    }

    let wikilinks_resolved = wikilink_to_note.len() + wikilink_to_heading.len();

    let wl_note_refs: Vec<(&str, &str, f32, &str)> = wikilink_to_note
        .iter()
        .map(|(s, n, c, d)| (s.as_str(), n.as_str(), *c, d.as_str()))
        .collect();
    store
        .batch_insert_wikilink_to_note_edges(&wl_note_refs)
        .context("batch_insert_wikilink_to_note_edges")?;

    let wl_head_refs: Vec<(&str, &str, f32, &str)> = wikilink_to_heading
        .iter()
        .map(|(s, h, c, d)| (s.as_str(), h.as_str(), *c, d.as_str()))
        .collect();
    store
        .batch_insert_wikilink_to_heading_edges(&wl_head_refs)
        .context("batch_insert_wikilink_to_heading_edges")?;

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
    format!("{:x}", hasher.finalize())
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
}

impl<'a> WikilinkLookup<'a> {
    fn build(notes: &'a [NoteContext]) -> Self {
        let mut by_path: HashMap<String, &'a str> = HashMap::new();
        let mut by_title: HashMap<String, Vec<&'a str>> = HashMap::new();
        let mut by_alias: HashMap<String, Vec<&'a str>> = HashMap::new();
        let mut folder_by_note: HashMap<&'a str, &'a str> = HashMap::new();
        let mut headings_by_note: HashMap<&'a str, HashMap<&'a str, &'a str>> = HashMap::new();
        let mut by_folder_name: HashMap<(String, String), Vec<&'a str>> = HashMap::new();

        for note in notes {
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
}
