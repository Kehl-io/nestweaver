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
    /// SHA-256 of the *entire* file (frontmatter + body) — drives change detection.
    pub content_hash: String,
    /// Raw frontmatter as a JSON object (`{}` when absent or unparseable).
    pub frontmatter: serde_json::Value,
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
    let headings = extract_headings(body);
    let mut sections = extract_sections(body, &headings);
    for (i, sec) in sections.iter_mut().enumerate() {
        sec.callout_type = extract_callout_type(&sec.text);
        let (total, checked) = count_checkboxes(&sec.text);
        sec.checkbox_total = total;
        sec.checkbox_checked = checked;
        if let Some(h_idx) = sec.heading_idx
            && let Some(heading) = headings.get(h_idx)
        {
            sec.is_adr_section = is_adr_heading(&heading.text);
        }
        let _ = i;
    }

    // 8. Extract wikilinks per section + tags (inline + frontmatter).
    let mut wikilinks = extract_wikilinks(&sections);
    wikilinks.extend(extract_md_links(body, &sections));
    let mut tags = extract_inline_tags(&sections);
    tags.extend(extract_frontmatter_tags(&frontmatter_json));

    // 9. Aliases from frontmatter `aliases:`.
    let aliases = extract_aliases(&frontmatter_json);

    // 10. Code block language tags.
    let code_languages = extract_code_languages(body);

    // 11. Obsidian block references (`^block-id`).
    let block_refs = extract_block_refs(body);

    Ok(ParsedNote {
        path: rel_path.to_string(),
        title,
        note_kind,
        word_count,
        content_hash,
        frontmatter: frontmatter_json,
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
/// dep-light and to handle escapes / nesting predictably. We do NOT match
/// inside fenced code blocks (```...```) or inline code (`...`) — those
/// are not real wikilinks.
fn extract_wikilinks(sections: &[RawSection]) -> Vec<RawWikilink> {
    let mut out = Vec::new();
    for (sec_idx, sec) in sections.iter().enumerate() {
        let stripped = strip_code(&sec.text);
        for (line_offset, line_text) in stripped.lines().enumerate() {
            let mut bytes = line_text.as_bytes();
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
                    Some((t, d)) => (t.trim().to_string(), Some(d.trim().to_string())),
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
                        if colon_pos < slash_pos && colon_pos > 0 {
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
                    section_idx: sec_idx,
                    line: sec.start_line + line_offset as u32,
                    vault_prefix,
                });
                col = end + 2;
                // Advance the slice for the next iteration of the outer loop.
                bytes = line_text.as_bytes();
            }
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
        let bytes = line.as_bytes();
        let mut buf = String::with_capacity(line.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'`' {
                // Find matching backtick on same line.
                let mut j = i + 1;
                while j < bytes.len() && bytes[j] != b'`' {
                    j += 1;
                }
                if j < bytes.len() {
                    for _ in i..=j {
                        buf.push(' ');
                    }
                    i = j + 1;
                    continue;
                }
            }
            buf.push(bytes[i] as char);
            i += 1;
        }
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
            let bytes = line_text.as_bytes();
            let mut i = 0usize;
            while i < bytes.len() {
                let is_boundary =
                    i == 0 || matches!(bytes[i - 1], b' ' | b'\t' | b'(' | b'[' | b',' | b';');
                if bytes[i] == b'#' && is_boundary {
                    let start = i + 1;
                    let mut j = start;
                    while j < bytes.len() {
                        let c = bytes[j];
                        if c.is_ascii_alphanumeric() || c == b'-' || c == b'_' || c == b'/' {
                            j += 1;
                        } else {
                            break;
                        }
                    }
                    if j > start {
                        let name = std::str::from_utf8(&bytes[start..j])
                            .unwrap_or("")
                            .to_lowercase();
                        // Skip bare `#` (no name), pure-numeric tags like #1 (often markdown
                        // issue refs), and trailing-hyphen artefacts.
                        if !name.is_empty() && name.chars().any(|c| c.is_alphabetic()) {
                            out.push(RawTag {
                                name,
                                source: TagSource::Inline,
                                section_idx: Some(sec_idx),
                                line: sec.start_line + line_offset as u32,
                            });
                        }
                        i = j;
                        continue;
                    }
                }
                i += 1;
            }
        }
    }
    out
}

/// Pull tags from frontmatter `tags:` — accepts an array of strings OR a
/// single string (Obsidian + Jekyll variants).
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

    #[test]
    fn note_with_only_preamble_has_one_section() {
        let note = parse_markdown("x.md", "just some text\nno headings here\n").unwrap();
        assert!(note.headings.is_empty());
        assert_eq!(note.sections.len(), 1);
        assert_eq!(note.sections[0].heading_idx, None);
    }

    #[test]
    fn frontmatter_does_not_shift_heading_line_numbers() {
        // Body line 1 = "# After FM", regardless of frontmatter length.
        let src = "---\ntitle: x\nfoo: bar\nbaz: qux\n---\n# After FM\nbody\n";
        let note = parse_markdown("x.md", src).unwrap();
        assert_eq!(note.headings.len(), 1);
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
