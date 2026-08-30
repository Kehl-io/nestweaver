//! Markdown parser — produces a `ParsedNote` from a Markdown source file.
//!
//! Walking-skeleton scope: title, note_kind, frontmatter JSON, content hash,
//! word count. No headings, sections, wikilinks, tags, or transclusions yet —
//! those land in later phases. The shape mirrors `ParsedFile`/`RawSymbol` so
//! the engine can dispatch on `SourceKind` without conflating Markdown into
//! the code-language enum.

use nestweaver_schema::NoteKind;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MarkdownParseError {
    #[error("failed to read markdown source: {0}")]
    Io(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedNote {
    /// Relative path of the note within its vault.
    pub path: String,
    /// Resolved title: frontmatter `title:`, then first H1, then filename.
    pub title: String,
    /// `Note.note_kind` — derived from frontmatter `type:`, then path/filename
    /// heuristics, then defaults to `General`.
    pub note_kind: NoteKind,
    /// Approximate word count of the body (excluding frontmatter).
    pub word_count: u32,
    /// BLAKE3 hash of the *entire* file (frontmatter + body) — drives change detection.
    pub content_hash: String,
    /// Raw frontmatter as a JSON object (`{}` when absent or unparseable).
    pub frontmatter: serde_json::Value,
    /// The frontmatter's ORIGINAL text, between the `---` fences, exactly as
    /// written. `None` when the note has no frontmatter block.
    ///
    /// Kept alongside `frontmatter` rather than derived from it: that field is
    /// a parsed map re-encoded as JSON, so a YAML-shaped pattern
    /// (`(?m)^\s*id: nw-231`) and a line number are both unrecoverable from it.
    /// Indexing the JSON would make bare-token searches work and leave
    /// YAML-shaped ones silently failing — a new asymmetry replacing the old
    /// one (nw-298).
    pub frontmatter_raw: Option<String>,
    /// Set if frontmatter was present but failed to parse — we keep the note,
    /// just record the error for diagnostics.
    pub frontmatter_error: Option<String>,
    /// All headings in the body, in document order. Line numbers are 1-based
    /// and relative to the body (after frontmatter).
    pub headings: Vec<RawHeading>,
    /// All sections in the body, in document order. Includes a preamble
    /// section (heading_idx=None) when there is text before the first heading.
    pub sections: Vec<RawSection>,
    /// All `[[wikilinks]]` / `![[transclusions]]` from section bodies.
    pub wikilinks: Vec<RawWikilink>,
    /// Aliases declared in frontmatter `aliases:` — used by the wikilink
    /// resolver's priority-2 lookup.
    pub aliases: Vec<String>,
    /// Tags from inline `#tag` syntax and frontmatter `tags:` arrays.
    pub tags: Vec<RawTag>,
    /// Language tags from fenced code blocks (e.g. `python` from ` ```python `).
    /// Empty string entries are omitted; duplicate languages are preserved.
    pub code_languages: Vec<String>,
    /// Obsidian block reference IDs (`^block-id`) found in the note body,
    /// as `(1-based line number, block_id)` pairs.
    pub block_refs: Vec<(u32, String)>,
}

/// A heading discovered in the markdown body.
///
/// `start_line` / `end_line` are 1-based and refer to the line *within the
/// body* (after the closing frontmatter delimiter). End_line is the line of
/// the heading itself — usually equal to start_line for ATX-style headings;
/// for Setext-style (underlined) headings it's the line of the underline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawHeading {
    pub level: u8,
    pub text: String,
    pub slug: String,
    pub start_line: u32,
    pub end_line: u32,
}

/// A section spans the lines from one heading (exclusive) to the next heading
/// at the same or shallower depth (exclusive), or EOF. The preamble is the
/// span from line 1 to the first heading (or EOF if no headings).
///
/// `heading_idx` is `None` for the preamble; otherwise it's the index into
/// `ParsedNote.headings` of the heading that owns this section.
///
/// `callout_type` is set when the section body contains an Obsidian callout
/// (`> [!type]`), e.g. `Some("note")` or `Some("warning")`.
///
/// `checkbox_total` / `checkbox_checked` count `- [ ]` / `- [x]` items in the
/// section body. `is_adr_section` is `true` when the owning heading matches a
/// standard ADR keyword (Context, Decision, Consequences, …).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawSection {
    pub heading_idx: Option<usize>,
    pub start_line: u32,
    pub end_line: u32,
    pub text: String,
    /// Obsidian callout type extracted from `> [!type]` syntax, lowercased.
    pub callout_type: Option<String>,
    /// Total number of checkbox items (`- [ ]` and `- [x]`) in this section.
    pub checkbox_total: u32,
    /// Number of checked checkbox items (`- [x]` / `- [X]`) in this section.
    pub checkbox_checked: u32,
    /// `true` when the section's heading text matches a standard ADR keyword
    /// (e.g. "Context", "Decision", "Consequences", "Status", …).
    pub is_adr_section: bool,
}

/// A wikilink extracted from a section body — `[[Target]]`, `[[Target|display]]`,
/// `[[Target#Heading]]`, or `![[Target]]` (transclusion).
///
/// `section_idx` is the index into `ParsedNote.sections` where this wikilink
/// appears; resolution happens at the engine layer after all notes are parsed.
/// `line` is 1-based relative to the body (post-frontmatter).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawWikilink {
    pub target: String,
    pub heading_anchor: Option<String>,
    pub display: Option<String>,
    pub transclude: bool,
    pub section_idx: usize,
    pub line: u32,
    /// Cross-vault prefix from `[[vault:target]]` syntax.
    /// `None` for same-vault links, `Some("vault-name")` for cross-vault.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault_prefix: Option<String>,
}

/// A tag — either an inline `#tag` in section body, or pulled from frontmatter
/// `tags:` array. Inline tags carry the section_idx they live in; frontmatter
/// tags are note-scoped (section_idx = None).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TagSource {
    Inline,
    Frontmatter,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawTag {
    pub name: String,
    pub source: TagSource,
    pub section_idx: Option<usize>,
    pub line: u32,
}

/// Parse a Markdown source file into a `ParsedNote`.
///
/// Never fails on malformed frontmatter or comrak quirks — the worst case
/// returns a note with title=filename and default fields. The only error
/// path is reading an empty path component.
pub fn parse_markdown(rel_path: &str, source: &str) -> Result<ParsedNote, MarkdownParseError> {
    let content_hash = sha256_hex(source);

    // 1. Strip Obsidian `%%...%%` comments from source before any further
    //    processing so they are excluded from indexing and search.
    let stripped_source = strip_obsidian_comments(source);
    // nw-335: drop raw NUL bytes before anything downstream can carry one into
    // a graph column. `content_hash` above is deliberately taken from the
    // ORIGINAL bytes, so removing the byte does not hide the edit that removes
    // it from change detection. See `parse::strip_nul_bytes` for why this is
    // the right seam rather than the store's canary.
    let stripped_source = crate::parse::strip_nul_bytes(&stripped_source).into_owned();
    let source = stripped_source.as_str();

    // 2. Split frontmatter (if any) from body.
    let (frontmatter_raw, body) = split_frontmatter(source);

    // 3. Parse frontmatter — best-effort, recover from errors.
    let (frontmatter_json, frontmatter_error) = match frontmatter_raw {
        Some(raw) => match serde_yaml::from_str::<serde_yaml::Value>(raw) {
            Ok(value) => match yaml_to_json(&value) {
                Ok(json) => (json, None),
                Err(e) => (serde_json::json!({}), Some(e)),
            },
            Err(e) => (serde_json::json!({}), Some(e.to_string())),
        },
        None => (serde_json::json!({}), None),
    };

    // 4. Title resolution: frontmatter > first H1 > filename stem.
    let title = frontmatter_json
        .get("title")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| extract_first_h1(body))
        .unwrap_or_else(|| title_from_path(rel_path));

    // 5. note_kind: frontmatter `type:` > path heuristic > General.
    let fm_type = frontmatter_json
        .get("type")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let note_kind = match fm_type {
        Some(t) => NoteKind::from_hint(&t),
        None => kind_from_path(rel_path),
    };

    // 6. Word count: split body on whitespace, ignoring HTML / fenced code is good enough for v1.
    let word_count = u32::try_from(body.split_whitespace().count()).unwrap_or(u32::MAX);

    // 7. Extract headings and sections from the body; annotate callout types.
    let mut headings = extract_headings(body);
    let mut sections = extract_sections(body, &headings);
    for sec in sections.iter_mut() {
        sec.callout_type = extract_callout_type(&sec.text);
        let (total, checked) = count_checkboxes(&sec.text);
        sec.checkbox_total = total;
        sec.checkbox_checked = checked;
        if let Some(h_idx) = sec.heading_idx
            && let Some(heading) = headings.get(h_idx)
        {
            sec.is_adr_section = is_adr_heading(&heading.text);
        }
    }

    // 8. Extract wikilinks per section + tags (inline + frontmatter).
    let mut wikilinks = extract_wikilinks(&sections);
    wikilinks.extend(extract_md_links(body, &sections));
    // Frontmatter links attach to the preamble section; see
    // extract_frontmatter_wikilinks for why they cannot be note-scoped today.
    if !sections.is_empty() {
        wikilinks.extend(extract_frontmatter_wikilinks(&frontmatter_json, 0));
    }
    let mut tags = extract_inline_tags(&sections);
    tags.extend(extract_frontmatter_tags(&frontmatter_json));

    // 9. Aliases from frontmatter `aliases:`.
    let aliases = extract_aliases(&frontmatter_json);

    // 10. Code block language tags.
    let code_languages = extract_code_languages(body);

    // 11. Obsidian block references (`^block-id`).
    let block_refs = extract_block_refs(body);

    // nw-185: shift every line number from body-relative to FILE-absolute.
    //
    // Headings and sections were built against `body`, which excludes the
    // frontmatter block, but every consumer renders them as `file:line` --
    // regex.rs computes `file_line = start_line + line_in_text - 1` and prints
    // it as a location. The result was short by exactly the frontmatter length
    // and self-contradictory: a hit reported at line 32 quoted text that lives
    // at line 43. The offset was baked into stored data, so the heading UID of
    // a note whose H1 sits at file line 12 ended in `:2`.
    //
    // Shifting here, once, keeps all the body-relative slicing above correct
    // while making everything that leaves this function file-absolute.
    let frontmatter_lines = source.lines().count() - body.lines().count();
    if frontmatter_lines > 0 {
        let shift = frontmatter_lines as u32;
        for heading in &mut headings {
            heading.start_line += shift;
            heading.end_line += shift;
        }
        for section in &mut sections {
            section.start_line += shift;
            section.end_line += shift;
        }
        for wikilink in &mut wikilinks {
            // Frontmatter links are recorded with line 0 (no body line); leave
            // them alone rather than inventing a position.
            if wikilink.line > 0 {
                wikilink.line += shift;
            }
        }
        for tag in &mut tags {
            if tag.line > 0 {
                tag.line += shift;
            }
        }
    }

    Ok(ParsedNote {
        path: rel_path.to_string(),
        title,
        note_kind,
        word_count,
        content_hash,
        frontmatter: frontmatter_json,
        frontmatter_raw: frontmatter_raw.map(str::to_string),
        frontmatter_error,
        headings,
        sections,
        wikilinks,
        aliases,
        tags,
        code_languages,
        block_refs,
    })
}

/// Walk the comrak AST and emit every heading in document order. Line numbers
/// are 1-based and relative to the body (the source we hand to comrak).
fn extract_headings(body: &str) -> Vec<RawHeading> {
    use comrak::{Arena, Options, nodes::NodeValue, parse_document};
    let arena = Arena::new();
    let root = parse_document(&arena, body, &Options::default());

    let mut headings = Vec::new();
    let mut slug_counts: std::collections::HashMap<String, u32> = std::collections::HashMap::new();

    for node in root.descendants() {
        let data = node.data.borrow();
        if let NodeValue::Heading(h) = &data.value {
            let mut text = String::new();
            // comrak text concatenation — visit all descendants' Text nodes.
            for child in node.descendants() {
                let cdata = child.data.borrow();
                match &cdata.value {
                    NodeValue::Text(t) => {
                        text.push_str(t);
                    }
                    NodeValue::Code(c) => {
                        text.push_str(&c.literal);
                    }
                    _ => {}
                }
            }
            let text = text.trim().to_string();
            if text.is_empty() {
                continue;
            }
            let base_slug = slugify(&text);
            // Deduplicate: GitHub-style — "heading", "heading-1", "heading-2".
            let slug = match slug_counts.get(&base_slug).copied() {
                None => {
                    slug_counts.insert(base_slug.clone(), 0);
                    base_slug
                }
                Some(n) => {
                    let next = n + 1;
                    slug_counts.insert(base_slug.clone(), next);
                    format!("{base_slug}-{next}")
                }
            };
            let start_line = data.sourcepos.start.line as u32;
            let end_line = data.sourcepos.end.line as u32;
            headings.push(RawHeading {
                level: h.level,
                text,
                slug,
                start_line,
                end_line,
            });
        }
    }

    headings
}

/// Compute sections from heading boundaries.
///
/// A section runs from `heading.end_line + 1` to the line BEFORE the next
/// heading of ANY level, or to EOF. The outline hierarchy (which heading
/// belongs under which) is expressed through `HEADING_PARENT` edges, not
/// through section spans — sections are about owning the immediate body
/// text, not the descendant structure.
///
/// The preamble is from line 1 to the line before the first heading (or
/// EOF if no headings exist).
fn extract_sections(body: &str, headings: &[RawHeading]) -> Vec<RawSection> {
    let lines: Vec<&str> = body.lines().collect();
    let total_lines = lines.len() as u32;
    let mut sections = Vec::new();

    // Preamble: lines before the first heading.
    let preamble_end = if let Some(first) = headings.first() {
        first.start_line.saturating_sub(1)
    } else {
        total_lines
    };
    if preamble_end >= 1 {
        let text = slice_lines(&lines, 1, preamble_end);
        if !text.trim().is_empty() {
            sections.push(RawSection {
                heading_idx: None,
                start_line: 1,
                end_line: preamble_end,
                text,
                callout_type: None,
                checkbox_total: 0,
                checkbox_checked: 0,
                is_adr_section: false,
            });
        }
    }

    // One section per heading — body runs from just after this heading to
    // just before the NEXT heading (any level) or EOF.
    for (idx, h) in headings.iter().enumerate() {
        let body_start = h.end_line + 1;
        let body_end = headings
            .get(idx + 1)
            .map(|next| next.start_line.saturating_sub(1))
            .unwrap_or(total_lines);
        if body_start > total_lines {
            sections.push(RawSection {
                heading_idx: Some(idx),
                start_line: body_start,
                end_line: body_start,
                text: String::new(),
                callout_type: None,
                checkbox_total: 0,
                checkbox_checked: 0,
                is_adr_section: false,
            });
            continue;
        }
        let text = if body_end >= body_start {
            slice_lines(&lines, body_start, body_end)
        } else {
            String::new()
        };
        sections.push(RawSection {
            heading_idx: Some(idx),
            start_line: body_start,
            end_line: body_end.max(body_start),
            text,
            callout_type: None,
            checkbox_total: 0,
            checkbox_checked: 0,
            is_adr_section: false,
        });
    }

    sections
}

/// Concatenate `lines[start-1 ..= end-1]` (1-based, inclusive) back into a string.
fn slice_lines(lines: &[&str], start: u32, end: u32) -> String {
    if start == 0 || start > lines.len() as u32 {
        return String::new();
    }
    let end = end.min(lines.len() as u32);
    let mut out = String::new();
    for line in &lines[(start - 1) as usize..end as usize] {
        out.push_str(line);
        out.push('\n');
    }
    // Trim the trailing newline added on the last line for cosmetic cleanliness.
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

/// GitHub-flavoured slugify (used by Obsidian for `[[Note#Heading]]` resolution).
/// Lowercase, strip everything except alphanumerics + spaces + hyphens, replace
/// spaces with hyphens, collapse repeated hyphens, trim leading/trailing hyphens.
pub fn slugify(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_dash = true;
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            for lc in ch.to_lowercase() {
                out.push(lc);
            }
            prev_dash = false;
        } else if (ch == ' ' || ch == '-' || ch == '_') && !prev_dash {
            out.push('-');
            prev_dash = true;
        }
        // Everything else (punctuation, symbols) is dropped.
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

// ── helpers ────────────────────────────────────────────────────────────────

fn sha256_hex(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hex::encode(hasher.finalize())
}

/// Split YAML frontmatter (delimited by lines containing exactly `---`) from the
/// body. Returns `(Some(frontmatter), body)` if present, else `(None, source)`.
///
/// Obsidian/Jekyll convention: file must START with `---\n`, and the next
/// matching `---\n` ends the block.
fn split_frontmatter(source: &str) -> (Option<&str>, &str) {
    let trimmed = source.strip_prefix('\u{feff}').unwrap_or(source);
    let Some(rest) = trimmed
        .strip_prefix("---\n")
        .or_else(|| trimmed.strip_prefix("---\r\n"))
    else {
        return (None, source);
    };

    // Find the closing `---` line.
    let mut offset = 0;
    for line in rest.lines() {
        if line.trim_end() == "---" {
            let frontmatter = &rest[..offset];
            // Skip past the closing line including its newline.
            let body_start_in_rest = offset + line.len();
            let body = &rest[body_start_in_rest..];
            let body = body
                .strip_prefix('\n')
                .or_else(|| body.strip_prefix("\r\n"))
                .unwrap_or(body);
            return (Some(frontmatter), body);
        }
        offset += line.len() + 1; // +1 for the line terminator
    }

    // No closing delimiter — treat as no frontmatter to avoid swallowing the body.
    (None, source)
}

/// Convert a `serde_yaml::Value` to `serde_json::Value`. Returns an error
/// string if a YAML value cannot be represented in JSON (e.g. non-string map keys).
fn yaml_to_json(value: &serde_yaml::Value) -> Result<serde_json::Value, String> {
    use serde_yaml::Value as Y;
    Ok(match value {
        Y::Null => serde_json::Value::Null,
        Y::Bool(b) => serde_json::Value::Bool(*b),
        Y::Number(n) => {
            if let Some(i) = n.as_i64() {
                serde_json::Value::Number(i.into())
            } else if let Some(u) = n.as_u64() {
                serde_json::Value::Number(u.into())
            } else if let Some(f) = n.as_f64() {
                serde_json::Number::from_f64(f)
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null)
            } else {
                serde_json::Value::Null
            }
        }
        Y::String(s) => serde_json::Value::String(s.clone()),
        Y::Sequence(seq) => {
            let mut out = Vec::with_capacity(seq.len());
            for v in seq {
                out.push(yaml_to_json(v)?);
            }
            serde_json::Value::Array(out)
        }
        Y::Mapping(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                let key = match k {
                    Y::String(s) => s.clone(),
                    Y::Number(n) => n.to_string(),
                    Y::Bool(b) => b.to_string(),
                    _ => return Err("non-scalar YAML map key".to_string()),
                };
                out.insert(key, yaml_to_json(v)?);
            }
            serde_json::Value::Object(out)
        }
        Y::Tagged(t) => yaml_to_json(&t.value)?,
    })
}

/// Extract the first H1 (`# Heading`) from the body, falling back to an
/// ATX-style match. Returns `None` if no H1 is found.
fn extract_first_h1(body: &str) -> Option<String> {
    use comrak::{Arena, Options, nodes::NodeValue, parse_document};

    let arena = Arena::new();
    let root = parse_document(&arena, body, &Options::default());

    for node in root.descendants() {
        if let NodeValue::Heading(h) = &node.data.borrow().value
            && h.level == 1
        {
            let mut text = String::new();
            for child in node.descendants() {
                if let NodeValue::Text(t) = &child.data.borrow().value {
                    text.push_str(t);
                }
            }
            let trimmed = text.trim().to_string();
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
        }
    }
    None
}

fn title_from_path(rel_path: &str) -> String {
    Path::new(rel_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(rel_path)
        .to_string()
}

/// Path-based heuristic for `note_kind` when frontmatter is absent.
/// Looks at directory and filename hints.
fn kind_from_path(rel_path: &str) -> NoteKind {
    let lower = rel_path.to_ascii_lowercase();
    let filename = lower.rsplit('/').next().unwrap_or(&lower);

    // Agent config files — detect before general heuristics.
    if matches!(
        filename,
        "claude.md"
            | "agents.md"
            | "gemini.md"
            | ".cursorrules"
            | "copilot-instructions.md"
            | ".windsurfrules"
            | ".clinerules"
    ) || lower.contains(".github/copilot-instructions")
    {
        return NoteKind::AgentConfig;
    }

    if lower.contains("prd") {
        NoteKind::Prd
    } else if lower.contains("design") || lower.contains("rfc") {
        NoteKind::Design
    } else if lower.contains("meeting") || lower.contains("meetings/") {
        NoteKind::Meeting
    } else if lower.contains("journal") || lower.contains("daily") {
        NoteKind::Journal
    } else {
        NoteKind::General
    }
}

/// Scan each section's text for `[[wikilink]]`, `[[wikilink|display]]`,
/// `[[wikilink#heading]]`, and `![[transclude]]` forms.
///
/// Uses a hand-rolled scanner rather than a regex to keep the parser
/// dep-light. The ONE escape it understands is `\|` in an aliased link, which
/// Obsidian requires inside a markdown table (nw-342); it does not handle
/// nesting, and it never claimed to correctly. We do NOT match
/// inside fenced code blocks (```...```) or inline code (`...`) — those
/// are not real wikilinks.
/// Parse every `[[wikilink]]` / `![[transclusion]]` on one line into `out`.
///
/// Shared by body-section extraction and frontmatter extraction so the two
/// cannot drift on alias, anchor or cross-vault-prefix handling.
fn push_wikilinks_from_line(
    line_text: &str,
    section_idx: usize,
    line: u32,
    out: &mut Vec<RawWikilink>,
) {
    let bytes = line_text.as_bytes();
    let mut col = 0usize;
    while col < bytes.len() {
        // Look for `[[` or `![[`.
        let (transclude, start) = if col + 2 < bytes.len()
            && bytes[col] == b'!'
            && bytes[col + 1] == b'['
            && bytes[col + 2] == b'['
        {
            (true, col + 3)
        } else if col + 1 < bytes.len() && bytes[col] == b'[' && bytes[col + 1] == b'[' {
            (false, col + 2)
        } else {
            col += 1;
            continue;
        };
        // Find the matching `]]`.
        let Some(end) = find_close(bytes, start) else {
            col = start;
            continue;
        };
        let inside = &line_text[start..end];
        // Newlines inside an unclosed wikilink? Skip.
        if inside.contains('\n') {
            col = start;
            continue;
        }
        let (target_part, display) = match inside.split_once('|') {
            // nw-342: inside a markdown TABLE, Obsidian REQUIRES the alias pipe
            // to be escaped -- `[[Backlog\|alias]]` is the only correct form
            // there. `split_once('|')` hands the target half back with the
            // backslash still attached, so `Backlog\` became the stored target:
            // a name no file can carry, and the string a user reads back out of
            // `broken-links` and `note_get`.
            //
            // Fixing only the resolver would MASK this. `WikilinkLookup::resolve`
            // normalises `\` to `/` for Windows path forms, so the key became
            // `backlog/` and the path-qualified fallback happened to recover it
            // at 0.85 -- the symptom disappears while the wrong target string
            // stays in the graph. A trailing backslash on a wikilink TARGET has
            // no other meaning, so stripping it is unconditional; the DISPLAY
            // half keeps whatever the user wrote.
            Some((t, d)) => {
                let t = t.trim();
                (
                    t.strip_suffix('\\').unwrap_or(t).trim_end().to_string(),
                    Some(d.trim().to_string()),
                )
            }
            None => (inside.trim().to_string(), None),
        };
        if target_part.is_empty() {
            col = end + 2;
            continue;
        }
        let (target, anchor) = match target_part.split_once('#') {
            Some((t, a)) => (t.trim().to_string(), Some(a.trim().to_string())),
            None => (target_part, None),
        };
        if target.is_empty() {
            col = end + 2;
            continue;
        }
        // Detect cross-vault prefix: [[vault:target]]
        // Only split on `:` if it appears before any `/` (path separator).
        let (vault_prefix, resolved_target) = {
            let slash_pos = target.find('/').unwrap_or(usize::MAX);
            if let Some(colon_pos) = target.find(':') {
                if colon_pos < slash_pos && colon_pos > 1 {
                    (
                        Some(target[..colon_pos].to_string()),
                        target[colon_pos + 1..].to_string(),
                    )
                } else {
                    (None, target)
                }
            } else {
                (None, target)
            }
        };
        out.push(RawWikilink {
            target: resolved_target,
            heading_anchor: anchor,
            display,
            transclude,
            section_idx,
            line,
            vault_prefix,
        });
        col = end + 2;
    }
}

fn extract_wikilinks(sections: &[RawSection]) -> Vec<RawWikilink> {
    let mut out = Vec::new();
    for (sec_idx, sec) in sections.iter().enumerate() {
        let stripped = strip_code(&sec.text);
        for (line_offset, line_text) in stripped.lines().enumerate() {
            push_wikilinks_from_line(
                line_text,
                sec_idx,
                sec.start_line + line_offset as u32,
                &mut out,
            );
        }
    }
    out
}

/// Find the first `]]` at or after `from` in `bytes`. Returns the index of
/// the first `]`. Stops at newlines (wikilinks are single-line).
fn find_close(bytes: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 1 < bytes.len() {
        if bytes[i] == b'\n' {
            return None;
        }
        if bytes[i] == b']' && bytes[i + 1] == b']' {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Remove fenced code blocks (triple-backtick) and inline code spans
/// (single-backtick) from `text` so that wikilink / tag scanners don't
/// match inside them. Preserves line numbers by emitting blank lines for
/// removed content.
fn strip_code(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_fence = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            out.push('\n');
            continue;
        }
        if in_fence {
            out.push('\n');
            continue;
        }
        // Strip inline code spans: replace `...` with spaces of equal length.
        //
        // Operate on `str`, never on raw bytes. This loop used to copy the line
        // through with `buf.push(bytes[i] as char)`, and `u8 as char` maps a
        // byte to the code point of the same value — Latin-1 decoding. Pushing
        // that into a UTF-8 `String` re-encoded it, so an em dash `e2 80 94`
        // came back out as `c3 a2 c2 80 c2 94` and every wikilink or tag
        // containing non-ASCII was silently corrupted (nw-099).
        //
        // Backticks are ASCII, so every index `find` returns is on a character
        // boundary and the slices below are safe.
        let mut buf = String::with_capacity(line.len());
        let mut rest = line;
        while let Some(open) = rest.find('`') {
            buf.push_str(&rest[..open]);
            let after_open = &rest[open + 1..];
            match after_open.find('`') {
                Some(close) => {
                    // Blank the span, both backticks included, one space per
                    // CHARACTER so column positions survive multi-byte text.
                    let span_end = open + 1 + close + 1;
                    for _ in rest[open..span_end].chars() {
                        buf.push(' ');
                    }
                    rest = &rest[span_end..];
                }
                None => {
                    // Unmatched backtick: keep it literally and carry on.
                    buf.push('`');
                    rest = after_open;
                }
            }
        }
        buf.push_str(rest);
        out.push_str(&buf);
        out.push('\n');
    }
    out
}

/// Extract inline `#tag` and `#nested/tag` strings from section bodies.
/// Tags must be preceded by start-of-line or whitespace and consist of
/// alphanumerics, `-`, `_`, `/`. The leading `#` is not stored.
fn extract_inline_tags(sections: &[RawSection]) -> Vec<RawTag> {
    let mut out = Vec::new();
    for (sec_idx, sec) in sections.iter().enumerate() {
        let stripped = strip_code(&sec.text);
        for (line_offset, line_text) in stripped.lines().enumerate() {
            // Scan by CHARACTER, not by byte. Testing each byte with
            // `is_ascii_alphanumeric` stopped at the first non-ASCII byte, so
            // `#café` indexed as `caf` and `#日本語` was dropped outright — its
            // first character is non-ASCII, so the name came out empty. Obsidian
            // supports non-ASCII tags, and a truncated stem can collide with an
            // unrelated real tag (nw-116).
            let mut prev: Option<char> = None;
            let mut prev2: Option<char> = None;
            let mut chars = line_text.char_indices().peekable();
            while let Some((idx, ch)) = chars.next() {
                let is_boundary = match prev {
                    None => true,
                    Some(p) => matches!(p, ' ' | '\t' | '(' | '[' | ',' | ';'),
                };
                // nw-167: `](#` opens a markdown in-page link, not a tag.
                // Accepting `(` as a boundary turned every table-of-contents
                // entry into one: all 46 anchor targets in the reference vault
                // were indexed as tags (#1-document-purpose ... #the-verdict),
                // distorting top_tags, the tag graph and every `tags=` filter.
                let is_markdown_anchor = prev == Some('(') && prev2 == Some(']');
                if ch == '#' && is_boundary && !is_markdown_anchor {
                    let start = idx + ch.len_utf8();
                    let mut end = start;
                    while let Some(&(j, c)) = chars.peek() {
                        if c.is_alphanumeric() || c == '-' || c == '_' || c == '/' {
                            end = j + c.len_utf8();
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    if end > start {
                        let name = line_text[start..end].to_lowercase();
                        // Skip bare `#` (no name), pure-numeric tags like #1 (often markdown
                        // issue refs), and trailing-hyphen artefacts.
                        //
                        // nw-167: also skip hex colours written in prose, e.g.
                        // `(#F5F5F5)` or `(#03a9f4)` -- 91 of the reference
                        // vault's 659 tags were colour literals. Requiring a
                        // digit keeps word-shaped tags that happen to be all
                        // hex letters, such as `#abc` or `#faced`.
                        let is_hex_colour = matches!(name.len(), 3 | 6 | 8)
                            && name.chars().all(|c| c.is_ascii_hexdigit())
                            && name.chars().any(|c| c.is_ascii_digit());
                        if !name.is_empty()
                            && name.chars().any(|c| c.is_alphabetic())
                            && !is_hex_colour
                        {
                            out.push(RawTag {
                                name,
                                source: TagSource::Inline,
                                section_idx: Some(sec_idx),
                                line: sec.start_line + line_offset as u32,
                            });
                        }
                        prev2 = None;
                        prev = line_text[start..end].chars().next_back();
                        continue;
                    }
                }
                prev2 = prev;
                prev = Some(ch);
            }
        }
    }
    out
}

/// Pull tags from frontmatter `tags:` — accepts an array of strings OR a
/// single string (Obsidian + Jekyll variants).
/// Extract `[[wikilinks]]` from every string value in the frontmatter block.
///
/// nw-164: frontmatter links were invisible to the graph because
/// `extract_wikilinks` only walks body sections. That silently dropped 403 of
/// this vault's 2019 links (20%), 237 of them in `Workspaces/*/Backlog.md`
/// where the whole `items:` array — and so every backlog cross-reference —
/// lives inside frontmatter. It also contradicted the vault's own documented
/// convention of linking related notes via a `related:` frontmatter field.
///
/// ATTRIBUTION: the WIKILINK_TO_NOTE / WIKILINK_TO_HEADING rel tables are
/// declared `FROM Section`, so a link must originate from a section. A
/// frontmatter link has no section of its own, so it is attributed to the
/// note's first section (the preamble). Representing these as note-scoped
/// edges would need a new `FROM Note` rel table plus a migration; that is a
/// schema decision, not a parser fix. A note with no sections at all
/// therefore still cannot carry frontmatter links — callers already skip
/// out-of-range section indices.
///
/// Every string value is walked recursively rather than only known link
/// fields, so nested structures (a `related:` inside an `items:` array) are
/// covered without hardcoding field names. `[[...]]` syntax is specific
/// enough that this does not produce false positives.
fn extract_frontmatter_wikilinks(
    frontmatter: &serde_json::Value,
    section_idx: usize,
) -> Vec<RawWikilink> {
    fn walk(value: &serde_json::Value, section_idx: usize, out: &mut Vec<RawWikilink>) {
        match value {
            serde_json::Value::String(text) => {
                for line in text.lines() {
                    // Frontmatter has no meaningful body line number; 0 marks
                    // "from the frontmatter block", matching how frontmatter
                    // tags are recorded.
                    push_wikilinks_from_line(line, section_idx, 0, out);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    walk(item, section_idx, out);
                }
            }
            serde_json::Value::Object(map) => {
                for item in map.values() {
                    walk(item, section_idx, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(frontmatter, section_idx, &mut out);
    out
}

fn extract_frontmatter_tags(frontmatter: &serde_json::Value) -> Vec<RawTag> {
    let mut out = Vec::new();
    let Some(value) = frontmatter.get("tags") else {
        return out;
    };
    let push = |out: &mut Vec<RawTag>, raw: &str| {
        // Trim leading '#' if present.
        let name = raw.trim().trim_start_matches('#').to_lowercase();
        if !name.is_empty() && name.chars().any(|c| c.is_alphabetic()) {
            out.push(RawTag {
                name,
                source: TagSource::Frontmatter,
                section_idx: None,
                line: 0,
            });
        }
    };
    match value {
        serde_json::Value::Array(arr) => {
            for v in arr {
                if let Some(s) = v.as_str() {
                    push(&mut out, s);
                }
            }
        }
        serde_json::Value::String(s) => {
            // Comma-separated or whitespace-separated single string.
            for part in s.split([',', ' ', '\t']).filter(|p| !p.trim().is_empty()) {
                push(&mut out, part);
            }
        }
        _ => {}
    }
    out
}

/// Pull aliases from frontmatter `aliases:` — accepts array of strings or
/// single string.
fn extract_aliases(frontmatter: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(value) = frontmatter
        .get("aliases")
        .or_else(|| frontmatter.get("alias"))
    else {
        return out;
    };
    match value {
        serde_json::Value::Array(arr) => {
            for v in arr {
                if let Some(s) = v.as_str()
                    && !s.trim().is_empty()
                {
                    out.push(s.trim().to_string());
                }
            }
        }
        serde_json::Value::String(s) if !s.trim().is_empty() => {
            out.push(s.trim().to_string());
        }
        _ => {}
    }
    out
}

/// Walk the comrak AST and collect the language tag from every fenced code
/// block that has one. The `info` field on `NodeCodeBlock` holds the info
/// string (e.g. `"python"` for ` ```python `); we take the first whitespace-
/// delimited token (some editors append extra options after a space).
fn extract_code_languages(body: &str) -> Vec<String> {
    use comrak::{Arena, Options, nodes::NodeValue, parse_document};
    let arena = Arena::new();
    let root = parse_document(&arena, body, &Options::default());

    let mut out = Vec::new();
    for node in root.descendants() {
        let data = node.data.borrow();
        if let NodeValue::CodeBlock(cb) = &data.value
            && cb.fenced
        {
            // info string may be e.g. "python title='example.py'"
            let lang = cb.info.split_whitespace().next().unwrap_or("").trim();
            if !lang.is_empty() {
                out.push(lang.to_string());
            }
        }
    }
    out
}

/// Walk the comrak AST and collect `[text](url)` links whose URL ends with
/// `.md` (optionally followed by a `#fragment`). These are emitted as
/// `RawWikilink` entries (with `transclude = false`) so the resolver can
/// treat them the same as `[[wikilinks]]`.
///
/// `section_idx` is set to the section whose line range contains the link;
/// falls back to 0 (preamble / first section) when no match is found.
fn extract_md_links(body: &str, sections: &[RawSection]) -> Vec<RawWikilink> {
    use comrak::{Arena, Options, nodes::NodeValue, parse_document};
    let arena = Arena::new();
    let root = parse_document(&arena, body, &Options::default());

    let mut out = Vec::new();
    for node in root.descendants() {
        let data = node.data.borrow();
        if let NodeValue::Link(link) = &data.value {
            // Separate fragment from path.
            let url = &link.url;
            let (path_part, fragment) = match url.split_once('#') {
                Some((p, f)) => (p, Some(f.to_string())),
                None => (url.as_str(), None),
            };
            // Only care about links pointing to .md files.
            if !path_part.ends_with(".md") {
                continue;
            }
            // ...and only to files IN THE VAULT. An external URL can end in
            // `.md` too — `https://github.com/org/repo/blob/main/notes/x.md`
            // passed this filter, became a wikilink target, and then resolved to
            // nothing forever, so `broken-links` reported it as broken on every
            // run with no possible fix (nw-100). A scheme means it is not a
            // vault path.
            if path_part.contains("://") || path_part.starts_with("//") {
                continue;
            }
            let target = path_part.to_string();
            let line = data.sourcepos.start.line as u32;

            // Collect the link's display text.
            let mut display_text = String::new();
            for child in node.descendants() {
                let cdata = child.data.borrow();
                if let NodeValue::Text(t) = &cdata.value {
                    display_text.push_str(t);
                }
            }
            let display = if display_text.is_empty() {
                None
            } else {
                Some(display_text)
            };

            // Find which section contains this line (body lines are 1-based).
            let section_idx = sections
                .iter()
                .position(|s| line >= s.start_line && line <= s.end_line)
                .unwrap_or(0);

            out.push(RawWikilink {
                target,
                heading_anchor: fragment,
                display,
                transclude: false,
                section_idx,
                line,
                vault_prefix: None,
            });
        }
    }
    out
}

// ── Obsidian-native helpers ────────────────────────────────────────────────

/// Extract the Obsidian callout type from a section's text.
///
/// Looks for the first line matching `> [!type]` (Obsidian callout syntax)
/// and returns the type in lowercase. Returns `None` when no callout is found.
fn extract_callout_type(text: &str) -> Option<String> {
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("> [!")
            && let Some(end) = trimmed.find(']')
        {
            let callout_type = &trimmed[4..end];
            return Some(callout_type.to_lowercase());
        }
    }
    None
}

/// Count checkbox items in section text.
///
/// Matches `- [ ]` (unchecked) and `- [x]`/`- [X]` (checked), also with `*` bullets.
fn count_checkboxes(text: &str) -> (u32, u32) {
    let mut total = 0u32;
    let mut checked = 0u32;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("- [ ] ") || trimmed.starts_with("* [ ] ") {
            total += 1;
        } else if trimmed.starts_with("- [x] ")
            || trimmed.starts_with("- [X] ")
            || trimmed.starts_with("* [x] ")
            || trimmed.starts_with("* [X] ")
        {
            total += 1;
            checked += 1;
        }
    }
    (total, checked)
}

/// Check if a heading text matches a standard ADR (Architecture Decision Record)
/// section keyword.
fn is_adr_heading(heading_text: &str) -> bool {
    let lower = heading_text.to_ascii_lowercase();
    matches!(
        lower.trim(),
        "context"
            | "decision"
            | "consequences"
            | "status"
            | "options"
            | "rationale"
            | "alternatives"
            | "options considered"
            | "decision outcome"
    )
}

/// Scan for Obsidian block references (`^block-id`) at the end of lines.
///
/// Returns `(1-based line number, block_id)` pairs. Block IDs must consist
/// entirely of alphanumerics and hyphens. The ` ^` prefix (space + caret) is
/// required so the ID is not confused with normal caret usage.
fn extract_block_refs(text: &str) -> Vec<(u32, String)> {
    let mut refs = Vec::new();
    for (line_num, line) in text.lines().enumerate() {
        let trimmed = line.trim_end();
        if let Some(pos) = trimmed.rfind(" ^") {
            let block_id = &trimmed[pos + 2..];
            if !block_id.is_empty() && block_id.chars().all(|c| c.is_alphanumeric() || c == '-') {
                refs.push(((line_num + 1) as u32, block_id.to_string()));
            }
        }
    }
    refs
}

/// Strip Obsidian `%%...%%` comment spans from text.
///
/// Comments may span multiple lines. The `%%` delimiters themselves are
/// consumed. Nested `%%` markers are not supported (matches the Obsidian
/// implementation).
fn strip_obsidian_comments(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut in_comment = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if !in_comment && c == '%' && chars.peek() == Some(&'%') {
            in_comment = true;
            chars.next(); // consume second %
        } else if in_comment && c == '%' && chars.peek() == Some(&'%') {
            in_comment = false;
            chars.next(); // consume second %
        } else if !in_comment {
            result.push(c);
        }
    }
    result
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {

    /// nw-164: links in the frontmatter block must reach the graph. 20% of the
    /// reference vault's links live there, including every backlog
    /// cross-reference, because the whole `items:` array is frontmatter.
    #[test]
    fn frontmatter_wikilinks_are_extracted() {
        let source = concat!(
            "---\n",
            "title: Example\n",
            "related:\n",
            "  - \"[[target-one]]\"\n",
            "  - \"[[target-two|alias]]\"\n",
            "items:\n",
            "  - id: x-1\n",
            "    note: \"see [[nested-target]] for detail\"\n",
            "---\n",
            "\n",
            "Body text linking [[body-target]].\n",
        );
        let parsed = parse_markdown("example.md", source).expect("parses");
        let targets: Vec<&str> = parsed.wikilinks.iter().map(|w| w.target.as_str()).collect();
        for expected in ["target-one", "target-two", "nested-target", "body-target"] {
            assert!(
                targets.contains(&expected),
                "missing {expected} in {targets:?}"
            );
        }
        let aliased = parsed
            .wikilinks
            .iter()
            .find(|w| w.target == "target-two")
            .expect("aliased link");
        assert_eq!(aliased.display.as_deref(), Some("alias"));
    }
    use super::*;

    #[test]
    fn parses_plain_note_with_h1_title() {
        let src = "# My First Note\n\nSome body content here.\n";
        let note = parse_markdown("notes/my-first-note.md", src).unwrap();
        assert_eq!(note.title, "My First Note");
        assert_eq!(note.note_kind, NoteKind::General);
        assert!(note.word_count >= 4);
        assert_eq!(note.frontmatter, serde_json::json!({}));
        assert!(note.frontmatter_error.is_none());
    }

    #[test]
    fn falls_back_to_filename_when_no_h1() {
        let src = "Just some body text, no heading.\n";
        let note = parse_markdown("notes/Filename Stem.md", src).unwrap();
        assert_eq!(note.title, "Filename Stem");
    }

    #[test]
    fn parses_frontmatter_title_overrides_h1() {
        let src = "---\ntitle: From Frontmatter\ntype: design\n---\n\n# H1 Title\n\nbody\n";
        let note = parse_markdown("x.md", src).unwrap();
        assert_eq!(note.title, "From Frontmatter");
        assert_eq!(note.note_kind, NoteKind::Design);
        assert_eq!(note.frontmatter["type"], "design");
    }

    #[test]
    fn note_kind_from_path_heuristic() {
        let src = "# X\n";
        let note = parse_markdown("PRDs/auth-prd.md", src).unwrap();
        assert_eq!(note.note_kind, NoteKind::Prd);

        let note = parse_markdown("meetings/2026-05-23.md", src).unwrap();
        assert_eq!(note.note_kind, NoteKind::Meeting);

        let note = parse_markdown("journal/daily/today.md", src).unwrap();
        assert_eq!(note.note_kind, NoteKind::Journal);
    }

    #[test]
    fn malformed_frontmatter_is_recorded_not_fatal() {
        // Unbalanced quote → serde_yaml will error.
        let src = "---\ntitle: \"unclosed\nbroken: [\n---\n\n# Still A Note\n";
        let note = parse_markdown("x.md", src).unwrap();
        assert!(
            note.frontmatter_error.is_some(),
            "frontmatter_error should be set"
        );
        // Body still parses; title falls back to H1.
        assert_eq!(note.title, "Still A Note");
    }

    #[test]
    fn content_hash_changes_with_content() {
        let a = parse_markdown("x.md", "# A\n").unwrap();
        let b = parse_markdown("x.md", "# B\n").unwrap();
        assert_ne!(a.content_hash, b.content_hash);
    }

    #[test]
    fn content_hash_is_deterministic() {
        let a = parse_markdown("x.md", "# A\nbody").unwrap();
        let b = parse_markdown("x.md", "# A\nbody").unwrap();
        assert_eq!(a.content_hash, b.content_hash);
    }

    #[test]
    fn no_frontmatter_when_no_delimiter() {
        let src = "# Title\nbody only\n";
        let note = parse_markdown("x.md", src).unwrap();
        assert_eq!(note.frontmatter, serde_json::json!({}));
        assert!(note.frontmatter_error.is_none());
    }

    #[test]
    fn unterminated_frontmatter_is_not_swallowed() {
        // Opening delimiter without a closing one — body should be preserved.
        let src = "---\ntitle: forever\n\n# Body Heading\nbody text\n";
        let note = parse_markdown("x.md", src).unwrap();
        // No frontmatter recognised → title from H1 of the full source.
        assert_eq!(note.title, "Body Heading");
    }

    // ── Outline extraction ─────────────────────────────────────────────────

    #[test]
    fn extracts_single_h1_and_section() {
        let src = "# Top\n\nThis is the body.\n";
        let note = parse_markdown("x.md", src).unwrap();
        assert_eq!(note.headings.len(), 1);
        assert_eq!(note.headings[0].level, 1);
        assert_eq!(note.headings[0].text, "Top");
        assert_eq!(note.headings[0].slug, "top");
        assert_eq!(note.sections.len(), 1);
        assert_eq!(note.sections[0].heading_idx, Some(0));
        assert!(note.sections[0].text.contains("This is the body."));
    }

    #[test]
    fn extracts_preamble_section() {
        let src = "preamble line one\npreamble line two\n\n# First Heading\nbody\n";
        let note = parse_markdown("x.md", src).unwrap();
        assert_eq!(note.sections.len(), 2);
        assert_eq!(note.sections[0].heading_idx, None);
        assert!(note.sections[0].text.contains("preamble line one"));
        assert_eq!(note.sections[1].heading_idx, Some(0));
    }

    #[test]
    fn nested_headings_get_correct_section_boundaries() {
        let src = "\
# Top
top body

## Sub A
sub a body

## Sub B
sub b body

# Top 2
top 2 body
";
        let note = parse_markdown("x.md", src).unwrap();
        assert_eq!(note.headings.len(), 4);
        // Top section should NOT include Sub A/B — it ends at the next H1.
        let top_section = &note.sections[0];
        assert_eq!(top_section.heading_idx, Some(0));
        assert!(top_section.text.contains("top body"));
        assert!(!top_section.text.contains("sub a body"));
        // Sub A section ends at Sub B start.
        let sub_a = note
            .sections
            .iter()
            .find(|s| {
                s.heading_idx
                    .map(|i| note.headings[i].text == "Sub A")
                    .unwrap_or(false)
            })
            .unwrap();
        assert!(sub_a.text.contains("sub a body"));
        assert!(!sub_a.text.contains("sub b body"));
    }

    #[test]
    fn duplicate_heading_text_gets_unique_slugs() {
        let src = "# Notes\nbody\n\n# Notes\nmore body\n";
        let note = parse_markdown("x.md", src).unwrap();
        assert_eq!(note.headings.len(), 2);
        assert_eq!(note.headings[0].slug, "notes");
        assert_eq!(note.headings[1].slug, "notes-1");
    }

    #[test]
    fn slugify_handles_punctuation_and_unicode() {
        assert_eq!(slugify("Hello, World!"), "hello-world");
        assert_eq!(slugify("  Mixed   Spaces  "), "mixed-spaces");
        assert_eq!(slugify("ALL CAPS"), "all-caps");
        assert_eq!(slugify("with_underscores"), "with-underscores");
        assert_eq!(slugify("Étude in C"), "étude-in-c");
        assert_eq!(slugify("---"), "");
    }

    #[test]
    fn empty_note_has_no_headings_or_sections() {
        let note = parse_markdown("x.md", "").unwrap();
        assert!(note.headings.is_empty());
        assert!(note.sections.is_empty());
    }

    /// nw-342: inside a markdown TABLE, Obsidian REQUIRES the pipe of an
    /// aliased wikilink to be escaped -- `[[Backlog\|alias]]` is the only
    /// correct form there. `split_once('|')` then hands the target half back
    /// with the backslash still attached, so `Backlog\` becomes the stored
    /// target: a name no file can carry, and the string a user reads back out
    /// of `broken-links` and `note_get`.
    ///
    /// The resolver fix alone would mask this. `resolve` normalises `\` to `/`
    /// for Windows path forms, so the key becomes `backlog/` and the
    /// path-qualified fallback happens to recover it at 0.85 -- the symptom
    /// goes away while the wrong target string stays in the graph.
    #[test]
    fn an_escaped_pipe_wikilink_yields_a_bare_stem_target() {
        let note =
            parse_markdown("x.md", "| col |\n| --- |\n| [[Backlog\\|the backlog]] |\n").unwrap();
        assert_eq!(note.wikilinks.len(), 1, "got {:?}", note.wikilinks);
        assert_eq!(
            note.wikilinks[0].target, "Backlog",
            "nw-342: the `\\|` escape must be stripped -- `Backlog\\` is a name no \
             file can carry, and it is what `broken-links` prints back at the user"
        );
        assert_eq!(note.wikilinks[0].display.as_deref(), Some("the backlog"));
    }

    /// nw-342, "where else": the same escape is legal outside a table, and an
    /// unaliased link is unaffected. A trailing backslash that is NOT an escape
    /// of the alias pipe has nowhere else to come from in a wikilink target, so
    /// stripping is unconditional on the target half only -- the DISPLAY half
    /// keeps whatever the user wrote.
    #[test]
    fn stripping_the_pipe_escape_leaves_other_wikilink_forms_alone() {
        let note = parse_markdown(
            "x.md",
            "[[Plain]] and [[Some/Path\\|shown]] and [[Bare|shown two]]\n",
        )
        .unwrap();
        let targets: Vec<&str> = note.wikilinks.iter().map(|w| w.target.as_str()).collect();
        assert_eq!(
            targets,
            vec!["Plain", "Some/Path", "Bare"],
            "{:?}",
            note.wikilinks
        );
    }

    /// nw-335: a pasted NUL is not corruption, it is a byte a user typed into a
    /// note. The store's whole-string canary (`read::string_is_corrupt`) treats
    /// it as LadybugDB #678 partial-scan corruption and refuses the row, which
    /// aborted the whole-corpus section scan behind BOTH global derived indexes.
    /// Sanitising at the single markdown ingest choke point makes the canary's
    /// stated premise -- "note bodies ... none contain NUL" -- true again,
    /// rather than weakening a canary that is doing real work on uids and paths.
    #[test]
    fn a_nul_byte_is_stripped_from_every_field_the_parser_emits() {
        let note = parse_markdown(
            "x.md",
            "---\ntitle: Ti\u{0}tle\n---\n# Head\u{0}ing\n\nbe\u{0}fore [[Tar\u{0}get]] after\n",
        )
        .unwrap();
        assert!(!note.title.contains('\0'), "title: {:?}", note.title);
        assert!(
            note.headings.iter().all(|h| !h.text.contains('\0')),
            "headings: {:?}",
            note.headings
        );
        assert!(
            note.sections.iter().all(|s| !s.text.contains('\0')),
            "sections: {:?}",
            note.sections
        );
        assert!(
            note.wikilinks.iter().all(|w| !w.target.contains('\0')),
            "wikilinks: {:?}",
            note.wikilinks
        );
        assert!(
            note.frontmatter_raw
                .as_deref()
                .is_none_or(|r| !r.contains('\0')),
            "frontmatter_raw: {:?}",
            note.frontmatter_raw
        );
        // Deleting the byte must not delete the text around it.
        assert!(note.sections[0].text.contains("before"));
        assert_eq!(note.title, "Title");
    }

    #[test]
    fn note_with_only_preamble_has_one_section() {
        let note = parse_markdown("x.md", "just some text\nno headings here\n").unwrap();
        assert!(note.headings.is_empty());
        assert_eq!(note.sections.len(), 1);
        assert_eq!(note.sections[0].heading_idx, None);
    }

    #[test]
    fn heading_line_numbers_are_file_absolute() {
        // nw-185: REVERSES the previous contract, which made this body-relative
        // ("body line 1 = the first heading, regardless of frontmatter length").
        //
        // Nothing slices content by these numbers -- read_symbols excludes
        // notes, and note_get returns stored section text -- but every consumer
        // RENDERS them as `file:line`. regex.rs computes
        // `file_line = start_line + line_in_text - 1` and prints it as a
        // location, so results were short by exactly the frontmatter length and
        // self-contradictory: a hit reported at line 32 quoted text living at
        // line 43. Code symbols are already file-absolute, so notes now match.
        let src = "---\ntitle: x\nfoo: bar\nbaz: qux\n---\n# After FM\nbody\n";
        let note = parse_markdown("x.md", src).unwrap();
        assert_eq!(note.headings.len(), 1);
        assert_eq!(
            note.headings[0].start_line, 6,
            "5 frontmatter lines + the heading on file line 6"
        );
    }

    #[test]
    fn a_note_without_frontmatter_is_unshifted() {
        let note = parse_markdown("x.md", "# Top\nbody\n").unwrap();
        assert_eq!(note.headings[0].start_line, 1);
    }

    // ── Wikilinks ──────────────────────────────────────────────────────────

    #[test]
    fn extracts_simple_wikilinks() {
        let src = "# Note\n\nSee [[Other Note]] and [[Third]].\n";
        let note = parse_markdown("x.md", src).unwrap();
        assert_eq!(note.wikilinks.len(), 2);
        let targets: Vec<_> = note.wikilinks.iter().map(|w| w.target.as_str()).collect();
        assert!(targets.contains(&"Other Note"));
        assert!(targets.contains(&"Third"));
        assert!(note.wikilinks.iter().all(|w| !w.transclude));
    }

    #[test]
    fn extracts_aliased_and_anchored_wikilinks() {
        let src =
            "# n\n\n[[Target|display text]] and [[Target#Anchor]] and [[Target#Anchor|alt]]\n";
        let note = parse_markdown("x.md", src).unwrap();
        assert_eq!(note.wikilinks.len(), 3);
        assert_eq!(note.wikilinks[0].display.as_deref(), Some("display text"));
        assert_eq!(note.wikilinks[1].heading_anchor.as_deref(), Some("Anchor"));
        assert_eq!(note.wikilinks[2].heading_anchor.as_deref(), Some("Anchor"));
        assert_eq!(note.wikilinks[2].display.as_deref(), Some("alt"));
    }

    #[test]
    fn extracts_transclusion() {
        let src = "# n\n\n![[Embed Me]]\n";
        let note = parse_markdown("x.md", src).unwrap();
        assert_eq!(note.wikilinks.len(), 1);
        assert!(note.wikilinks[0].transclude);
        assert_eq!(note.wikilinks[0].target, "Embed Me");
    }

    #[test]
    fn wikilinks_inside_fenced_code_are_ignored() {
        let src = "# n\n\n```\n[[Should Not Match]]\n```\n[[Should Match]]\n";
        let note = parse_markdown("x.md", src).unwrap();
        assert_eq!(note.wikilinks.len(), 1);
        assert_eq!(note.wikilinks[0].target, "Should Match");
    }

    #[test]
    fn wikilinks_inside_inline_code_are_ignored() {
        let src = "# n\n\nThis is `[[not a link]]` but [[this is]].\n";
        let note = parse_markdown("x.md", src).unwrap();
        assert_eq!(note.wikilinks.len(), 1);
        assert_eq!(note.wikilinks[0].target, "this is");
    }

    #[test]
    fn empty_wikilink_target_is_dropped() {
        let src = "# n\n\nbroken [[]] and [[ ]] and [[|nothing]]\n";
        let note = parse_markdown("x.md", src).unwrap();
        // All three should be dropped (empty target).
        assert_eq!(note.wikilinks.len(), 0);
    }

    /// nw-099: non-ASCII in a wikilink target must survive `strip_code`.
    ///
    /// `strip_code` used to rebuild each line with `buf.push(bytes[i] as char)`.
    /// In Rust `u8 as char` maps a byte to the code point of the same value —
    /// that is Latin-1 decoding — and pushing it into a UTF-8 `String`
    /// re-encodes it. An em dash `e2 80 94` came back out as
    /// `c3 a2 c2 80 c2 94`, one UTF-8 sequence per original byte.
    ///
    /// The link then resolves to nothing, so this silently severed real links
    /// between real notes. Headings and titles were unaffected because only
    /// wikilink and inline-tag scanning route through `strip_code`.
    #[test]
    fn wikilink_targets_preserve_non_ascii() {
        let src = "# n\n\nSee [[Spike 1 — Byte Path Findings]] for detail.\n";
        let note = parse_markdown("x.md", src).unwrap();
        assert_eq!(note.wikilinks.len(), 1);
        assert_eq!(note.wikilinks[0].target, "Spike 1 — Byte Path Findings");
        assert_eq!(
            note.wikilinks[0].target.as_bytes(),
            "Spike 1 — Byte Path Findings".as_bytes(),
            "em dash must stay as e2 80 94, not double-encode to c3 a2 c2 80 c2 94"
        );
    }

    /// The same corruption hit aliases and anchors, which also come from the
    /// stripped line, and a broader sweep of scripts than the em dash alone.
    #[test]
    fn wikilink_alias_and_anchor_preserve_non_ascii() {
        let src = "# n\n\n[[Café#Menü|Crème brûlée]] and [[日本語ノート]] and [[naïve — test]]\n";
        let note = parse_markdown("x.md", src).unwrap();
        assert_eq!(note.wikilinks.len(), 3);
        assert_eq!(note.wikilinks[0].target, "Café");
        assert_eq!(note.wikilinks[0].heading_anchor.as_deref(), Some("Menü"));
        assert_eq!(note.wikilinks[0].display.as_deref(), Some("Crème brûlée"));
        assert_eq!(note.wikilinks[1].target, "日本語ノート");
        assert_eq!(note.wikilinks[2].target, "naïve — test");
    }

    /// `strip_code` is shared with the inline-tag scanner, so a tag sitting on
    /// a line that contains non-ASCII elsewhere must still be found at the
    /// right line — the byte rewrite used to shift every following offset.
    ///
    /// Non-ASCII tag names are covered separately by
    /// `inline_tags_accept_non_ascii_names`.
    /// nw-116: Obsidian supports non-ASCII tags, but the scanner tested each
    /// byte with `is_ascii_alphanumeric`, so it stopped at the first non-ASCII
    /// byte: `#café` indexed as `caf` and `#niño` as `ni`. That silently splits
    /// a tag namespace, and the truncated stems can collide with unrelated real
    /// tags.
    #[test]
    fn inline_tags_accept_non_ascii_names() {
        let src = "# n\n\nTagged #café and #niño and #日本語 and #arch/décision here.\n";
        let note = parse_markdown("x.md", src).unwrap();
        let names: Vec<&str> = note.tags.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"café"), "tags were: {names:?}");
        assert!(names.contains(&"niño"), "tags were: {names:?}");
        assert!(names.contains(&"日本語"), "tags were: {names:?}");
        assert!(names.contains(&"arch/décision"), "tags were: {names:?}");
        assert!(
            !names.contains(&"caf"),
            "the truncated stem must be gone, not merely joined: {names:?}"
        );
    }

    /// The widened charset must not start swallowing ordinary punctuation that
    /// ends a tag — otherwise `#tag.` or `#tag, next` would absorb the trailing
    /// mark and mint a tag nobody wrote.
    #[test]
    fn inline_tags_still_stop_at_punctuation_and_whitespace() {
        let src = "# n\n\nSee #alpha, #beta. And #gamma; plus #delta! End #ré.\n";
        let note = parse_markdown("x.md", src).unwrap();
        let names: Vec<&str> = note.tags.iter().map(|t| t.name.as_str()).collect();
        for expected in ["alpha", "beta", "gamma", "delta", "ré"] {
            assert!(names.contains(&expected), "missing {expected}: {names:?}");
        }
    }

    /// Widening the charset must not start minting tags from prose. The real
    /// vault contains page and issue ranges written with an EN DASH — `#185–187`,
    /// `#36–37` — and those are not tags. The en dash (U+2013) is punctuation,
    /// not a hyphen-minus, so it must terminate the name, and the resulting
    /// digits-only stem must still be dropped by the numeric filter.
    #[test]
    fn en_dash_ranges_from_prose_are_not_tags() {
        let src = "# n\n\nSee pp. #185–187 and #36–37; also #11–12.\n";
        let note = parse_markdown("x.md", src).unwrap();
        let names: Vec<&str> = note.tags.iter().map(|t| t.name.as_str()).collect();
        assert!(
            names.is_empty(),
            "numeric ranges must not become tags: {names:?}"
        );
    }

    #[test]
    fn inline_tags_survive_non_ascii_on_the_same_line() {
        let src = "# n\n\nRésumé notes — tagged #design and #arch/decision here.\n";
        let note = parse_markdown("x.md", src).unwrap();
        let names: Vec<&str> = note.tags.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"design"), "tags were: {names:?}");
        assert!(names.contains(&"arch/decision"), "tags were: {names:?}");
    }

    /// The fix must not weaken code stripping: a wikilink inside an inline code
    /// span on a line that also contains non-ASCII must still be ignored, and
    /// the real link on that line must still be found.
    #[test]
    fn non_ascii_line_still_strips_inline_code() {
        let src = "# n\n\nRésumé `[[not a link]]` but [[Real — Link]] counts.\n";
        let note = parse_markdown("x.md", src).unwrap();
        assert_eq!(note.wikilinks.len(), 1);
        assert_eq!(note.wikilinks[0].target, "Real — Link");
    }

    // ── Tags ────────────────────────────────────────────────────────────────

    #[test]
    fn extracts_inline_tags() {
        let src = "# n\n\nThis covers #auth and #user/profile.\n";
        let note = parse_markdown("x.md", src).unwrap();
        let names: Vec<_> = note.tags.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"auth"));
        assert!(names.contains(&"user/profile"));
    }

    #[test]
    fn frontmatter_tags_array() {
        let src = "---\ntags: [project, status/active]\n---\n# n\n";
        let note = parse_markdown("x.md", src).unwrap();
        let names: Vec<_> = note.tags.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"project"));
        assert!(names.contains(&"status/active"));
        assert!(note.tags.iter().all(|t| t.source == TagSource::Frontmatter));
    }

    #[test]
    fn frontmatter_tags_string_form() {
        let src = "---\ntags: \"alpha beta, gamma\"\n---\n# n\n";
        let note = parse_markdown("x.md", src).unwrap();
        let names: Vec<_> = note.tags.iter().map(|t| t.name.as_str()).collect();
        for expected in ["alpha", "beta", "gamma"] {
            assert!(
                names.contains(&expected),
                "missing tag '{expected}' in {names:?}"
            );
        }
    }

    #[test]
    fn tags_inside_code_are_ignored() {
        let src = "# n\n\n```\n#code-tag\n```\nText with #real-tag.\n";
        let note = parse_markdown("x.md", src).unwrap();
        let names: Vec<_> = note.tags.iter().map(|t| t.name.as_str()).collect();
        assert!(!names.contains(&"code-tag"));
        assert!(names.contains(&"real-tag"));
    }

    #[test]
    fn pure_numeric_tags_are_dropped() {
        // Markdown convention: `#123` is usually an issue reference, not a tag.
        let src = "# n\n\nSee #123 for context, but #bug-456 is a tag.\n";
        let note = parse_markdown("x.md", src).unwrap();
        let names: Vec<_> = note.tags.iter().map(|t| t.name.as_str()).collect();
        assert!(!names.contains(&"123"));
        assert!(names.contains(&"bug-456"));
    }

    // ── Aliases ─────────────────────────────────────────────────────────────

    #[test]
    fn aliases_from_frontmatter() {
        let src = "---\naliases: [\"Auth\", \"AuthSvc\"]\n---\n# Authentication Service\n";
        let note = parse_markdown("x.md", src).unwrap();
        assert_eq!(note.aliases, vec!["Auth", "AuthSvc"]);
    }

    #[test]
    fn alias_singular_string_form() {
        let src = "---\nalias: shortname\n---\n# Long Name\n";
        let note = parse_markdown("x.md", src).unwrap();
        assert_eq!(note.aliases, vec!["shortname"]);
    }

    // ── Code block language tags ─────────────────────────────────────────────

    #[test]
    fn extracts_fenced_code_block_language() {
        let src = "# n\n\n```python\nprint('hello')\n```\n";
        let note = parse_markdown("x.md", src).unwrap();
        assert_eq!(note.code_languages, vec!["python"]);
    }

    #[test]
    fn extracts_multiple_code_block_languages() {
        let src = "# n\n\n```rust\nfn main() {}\n```\n\n```javascript\nconsole.log(1);\n```\n";
        let note = parse_markdown("x.md", src).unwrap();
        assert_eq!(note.code_languages, vec!["rust", "javascript"]);
    }

    #[test]
    fn fenced_block_without_language_is_not_collected() {
        let src = "# n\n\n```\nno lang tag here\n```\n";
        let note = parse_markdown("x.md", src).unwrap();
        assert!(note.code_languages.is_empty());
    }

    #[test]
    fn code_language_uses_first_token_only() {
        // Some editors write ` ```python title="example.py" `
        let src = "# n\n\n```python title='example.py'\ncode\n```\n";
        let note = parse_markdown("x.md", src).unwrap();
        assert_eq!(note.code_languages, vec!["python"]);
    }

    #[test]
    fn no_code_blocks_yields_empty_languages() {
        let src = "# n\n\nJust prose, no code.\n";
        let note = parse_markdown("x.md", src).unwrap();
        assert!(note.code_languages.is_empty());
    }

    // ── Markdown link (.md) detection ────────────────────────────────────────

    /// nw-100: an EXTERNAL url ending in `.md` is not a vault link.
    ///
    /// These passed the `.md` filter, became wikilink targets, and were then
    /// reported broken on every run — permanently, since nothing in the vault
    /// could ever satisfy them. The shape that triggered it is a plain link to
    /// a file in a git forge, e.g.
    /// `https://github.com/<org>/<repo>/blob/main/docs/releases/v0.2.0.md`.
    #[test]
    fn external_urls_ending_in_md_are_not_wikilinks() {
        let src = "# n\n\nSee [recovery](https://github.com/o/r/blob/main/docs/x.md) and \
                   [local](notes/y.md).\n";
        let note = parse_markdown("x.md", src).unwrap();
        let targets: Vec<&str> = note.wikilinks.iter().map(|w| w.target.as_str()).collect();
        assert!(
            targets.contains(&"notes/y.md"),
            "the in-vault link must still be captured: {targets:?}"
        );
        assert!(
            !targets.iter().any(|t| t.contains("://")),
            "an external url must not become a wikilink: {targets:?}"
        );
    }

    #[test]
    fn detects_md_link_as_wikilink() {
        let src = "# n\n\nSee [Other Note](other-note.md) for details.\n";
        let note = parse_markdown("x.md", src).unwrap();
        let md_links: Vec<_> = note
            .wikilinks
            .iter()
            .filter(|w| w.target.ends_with(".md"))
            .collect();
        assert_eq!(md_links.len(), 1);
        assert_eq!(md_links[0].target, "other-note.md");
        assert_eq!(md_links[0].display.as_deref(), Some("Other Note"));
        assert!(!md_links[0].transclude);
    }

    #[test]
    fn md_link_with_fragment_sets_heading_anchor() {
        let src = "# n\n\nSee [section](guide.md#installation).\n";
        let note = parse_markdown("x.md", src).unwrap();
        let link = note
            .wikilinks
            .iter()
            .find(|w| w.target == "guide.md")
            .expect("link should be present");
        assert_eq!(link.heading_anchor.as_deref(), Some("installation"));
    }

    #[test]
    fn non_md_links_are_not_collected() {
        let src = "# n\n\nVisit [homepage](https://example.com) and [page](page.html).\n";
        let note = parse_markdown("x.md", src).unwrap();
        // None of these point to .md files — wikilink list should be empty.
        assert!(note.wikilinks.is_empty());
    }

    #[test]
    fn md_link_and_wikilink_coexist() {
        let src = "# n\n\n[[Wikilink Target]] and [Markdown](linked.md).\n";
        let note = parse_markdown("x.md", src).unwrap();
        let targets: Vec<_> = note.wikilinks.iter().map(|w| w.target.as_str()).collect();
        assert!(targets.contains(&"Wikilink Target"));
        assert!(targets.contains(&"linked.md"));
    }

    // ── Obsidian callouts ────────────────────────────────────────────────────

    #[test]
    fn callout_type_extracted_from_section() {
        let src = "# n\n\n> [!warning]\n> Something important.\n";
        let note = parse_markdown("x.md", src).unwrap();
        assert_eq!(note.sections.len(), 1);
        assert_eq!(note.sections[0].callout_type.as_deref(), Some("warning"));
    }

    #[test]
    fn callout_type_is_lowercased() {
        let src = "# n\n\n> [!NOTE]\n> A note callout.\n";
        let note = parse_markdown("x.md", src).unwrap();
        assert_eq!(note.sections[0].callout_type.as_deref(), Some("note"));
    }

    #[test]
    fn section_without_callout_has_none_type() {
        let src = "# n\n\nPlain body text.\n";
        let note = parse_markdown("x.md", src).unwrap();
        assert_eq!(note.sections.len(), 1);
        assert!(note.sections[0].callout_type.is_none());
    }

    // ── Obsidian block references ────────────────────────────────────────────

    #[test]
    fn block_refs_extracted_from_body() {
        let src = "# n\n\nThis is a paragraph. ^my-ref\n\nAnother line ^other-ref\n";
        let note = parse_markdown("x.md", src).unwrap();
        assert_eq!(note.block_refs.len(), 2);
        let ids: Vec<&str> = note.block_refs.iter().map(|(_, id)| id.as_str()).collect();
        assert!(ids.contains(&"my-ref"));
        assert!(ids.contains(&"other-ref"));
    }

    #[test]
    fn block_ref_with_invalid_chars_is_ignored() {
        // Block IDs with special characters (other than alphanumeric and `-`) are skipped.
        let src = "# n\n\nLine with ^bad ref\n\nLine with ^good-ref\n";
        let note = parse_markdown("x.md", src).unwrap();
        let ids: Vec<&str> = note.block_refs.iter().map(|(_, id)| id.as_str()).collect();
        assert!(!ids.contains(&"bad ref"));
        assert!(ids.contains(&"good-ref"));
    }

    #[test]
    fn no_block_refs_yields_empty_list() {
        let src = "# n\n\nJust prose, no block refs.\n";
        let note = parse_markdown("x.md", src).unwrap();
        assert!(note.block_refs.is_empty());
    }

    // ── AgentConfig kind detection ───────────────────────────────────────────

    #[test]
    fn detects_claude_md_as_agent_config() {
        let parsed = parse_markdown("CLAUDE.md", "# Instructions\n\nDo this.\n").unwrap();
        assert_eq!(parsed.note_kind, NoteKind::AgentConfig);
    }

    #[test]
    fn detects_agents_md_as_agent_config() {
        let parsed = parse_markdown("AGENTS.md", "# Agents\n").unwrap();
        assert_eq!(parsed.note_kind, NoteKind::AgentConfig);
    }

    #[test]
    fn detects_copilot_instructions() {
        let parsed = parse_markdown(".github/copilot-instructions.md", "# Rules\n").unwrap();
        assert_eq!(parsed.note_kind, NoteKind::AgentConfig);
    }

    #[test]
    fn regular_md_not_agent_config() {
        let parsed = parse_markdown("notes/architecture.md", "# Architecture\n").unwrap();
        assert_ne!(parsed.note_kind, NoteKind::AgentConfig);
    }

    // ── Obsidian comment stripping ───────────────────────────────────────────

    #[test]
    fn obsidian_comments_are_stripped_before_indexing() {
        let src = "# n\n\nVisible text %%hidden comment%% more visible.\n";
        let note = parse_markdown("x.md", src).unwrap();
        // The section text should not contain the comment content.
        let all_text: String = note.sections.iter().map(|s| s.text.as_str()).collect();
        assert!(!all_text.contains("hidden comment"));
        assert!(all_text.contains("Visible text"));
        assert!(all_text.contains("more visible"));
    }

    #[test]
    fn multiline_obsidian_comment_is_stripped() {
        let src = "# n\n\nBefore %%\nhidden\nmultiline\n%% After\n";
        let note = parse_markdown("x.md", src).unwrap();
        let all_text: String = note.sections.iter().map(|s| s.text.as_str()).collect();
        assert!(!all_text.contains("hidden"));
        assert!(!all_text.contains("multiline"));
        assert!(all_text.contains("Before"));
        assert!(all_text.contains("After"));
    }

    #[test]
    fn content_hash_is_of_original_source_not_stripped() {
        // The content hash must reflect the original bytes (for change detection),
        // not the stripped version.
        let with_comment = "# n\n\ntext %%comment%% more\n";
        let without_comment = "# n\n\ntext  more\n";
        let note_with = parse_markdown("x.md", with_comment).unwrap();
        let note_without = parse_markdown("x.md", without_comment).unwrap();
        // Different source → different hash.
        assert_ne!(note_with.content_hash, note_without.content_hash);
    }

    #[test]
    fn parses_vault_prefix_wikilink() {
        let md = "# Note\n\nSee [[work:architecture]] for details.\n";
        let parsed = parse_markdown("test.md", md).unwrap();
        let wl = parsed
            .wikilinks
            .iter()
            .find(|w| w.vault_prefix.is_some())
            .expect("should find cross-vault wikilink");
        assert_eq!(wl.vault_prefix.as_deref(), Some("work"));
        assert_eq!(wl.target, "architecture");
    }

    #[test]
    fn regular_wikilink_has_no_vault_prefix() {
        let md = "# Note\n\n[[architecture]] is here.\n";
        let parsed = parse_markdown("test.md", md).unwrap();
        assert!(
            parsed.wikilinks.iter().all(|w| w.vault_prefix.is_none()),
            "regular wikilinks should have no vault prefix"
        );
    }

    #[test]
    fn path_wikilink_colon_not_treated_as_vault() {
        // [[C:/path/to/file]] — colon after single char is a drive letter, not vault
        let md = "# Note\n\n[[folder/file:with:colons]] ref.\n";
        let parsed = parse_markdown("test.md", md).unwrap();
        // The colon is after `/`, so no vault prefix
        let wl = &parsed.wikilinks[0];
        assert!(
            wl.vault_prefix.is_none(),
            "colon after slash should not be vault prefix"
        );
    }

    #[test]
    fn extracts_checkbox_counts() {
        let md = "# Tasks\n\n- [ ] Do thing\n- [x] Done thing\n- [ ] Another\n";
        let parsed = parse_markdown("test.md", md).unwrap();
        let task_sec = parsed
            .sections
            .iter()
            .find(|s| s.checkbox_total > 0)
            .expect("should find section with checkboxes");
        assert_eq!(task_sec.checkbox_total, 3);
        assert_eq!(task_sec.checkbox_checked, 1);
    }

    #[test]
    fn detects_adr_sections() {
        let md = "# ADR: Use Postgres\n\n## Context\n\nWe need a db.\n\n## Decision\n\nUse Postgres.\n\n## Consequences\n\nMigrations.\n";
        let parsed = parse_markdown("adr-001.md", md).unwrap();
        let adr_count = parsed.sections.iter().filter(|s| s.is_adr_section).count();
        assert!(
            adr_count >= 3,
            "expected >= 3 ADR sections, got {adr_count}"
        );
    }

    #[test]
    fn non_adr_sections_not_flagged() {
        let md = "# Overview\n\nText.\n\n## Features\n\nList.\n";
        let parsed = parse_markdown("readme.md", md).unwrap();
        assert!(
            !parsed.sections.iter().any(|s| s.is_adr_section),
            "regular sections should not be ADR"
        );
    }
}
#[cfg(test)]
mod tag_boundary_tests {
    use super::*;

    /// nw-167: `](#anchor)` is a markdown in-page link, and `(#F5F5F5)` is a
    /// colour literal. Neither is a tag. Accepting `(` as a tag boundary made
    /// all 46 anchor targets and 91 colour literals in the reference vault into
    /// tags -- roughly 20% of the tag namespace.
    #[test]
    fn markdown_anchors_and_hex_colours_are_not_tags() {
        let source = concat!(
            "# Doc\n",
            "\n",
            "See [Section 3.4](#34-the-datum-trap) and [TOC](#1-document-purpose).\n",
            "The accent is (#F5F5F5) and the link colour is (#03a9f4).\n",
            "A real tag: #project/nestweaver and (#inline-in-parens) counts too.\n",
        );
        let parsed = parse_markdown("doc.md", source).expect("parses");
        let tags: Vec<&str> = parsed.tags.iter().map(|t| t.name.as_str()).collect();

        for absent in [
            "34-the-datum-trap",
            "1-document-purpose",
            "f5f5f5",
            "03a9f4",
        ] {
            assert!(
                !tags.contains(&absent),
                "{absent} must not be a tag: {tags:?}"
            );
        }
        // A genuine tag, including one legitimately inside parentheses.
        assert!(tags.contains(&"project/nestweaver"), "got {tags:?}");
        assert!(tags.contains(&"inline-in-parens"), "got {tags:?}");
    }

    /// The hex-colour guard must require a digit, so word-shaped tags that
    /// happen to be all hex letters survive.
    #[test]
    fn all_letter_hex_shaped_tags_survive() {
        let parsed = parse_markdown("doc.md", "# D\n\nTags: #abc and #faced\n").expect("parses");
        let tags: Vec<&str> = parsed.tags.iter().map(|t| t.name.as_str()).collect();
        assert!(tags.contains(&"abc"), "got {tags:?}");
        assert!(tags.contains(&"faced"), "got {tags:?}");
    }
}
