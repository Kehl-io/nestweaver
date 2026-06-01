use crate::parse::{ParsedFile, RawReference, RawSymbol, ReferenceKind, sha256_hex};
use nestweaver_schema::{EntryPointKind, SymbolKind, Visibility};
use regex::Regex;
use std::path::Path;
use std::sync::LazyLock;

// ── compiled patterns ──────────────────────────────────────────────────────

/// Matches `SOMEWORD SECTION.` in column-8+ area.
static RE_SECTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s+([A-Z][A-Z0-9-]*)\s+SECTION\s*\.\s*$").unwrap());

/// Matches a bare paragraph label `SOME-PARA.` on its own line.
/// The label must be in the Area A / Area B position (indented) and end with
/// a period, with no other statement on the same line.
static RE_PARAGRAPH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s+([A-Z][A-Z0-9-]*)\s*\.\s*$").unwrap());

/// Matches `PERFORM some-name` (with optional THRU/THROUGH clause stripped).
static RE_PERFORM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bPERFORM\s+([A-Z][A-Z0-9-]*)").unwrap());

/// Matches `CALL 'name'` or `CALL "name"`.
static RE_CALL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"\bCALL\s+['"]([A-Z][A-Z0-9-]*)['"]"#).unwrap());

/// Matches `COPY name` (copybook name, no extension).
static RE_COPY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bCOPY\s+([A-Z][A-Z0-9-]*)").unwrap());

// ── COBOL division / section keywords to exclude from paragraph detection ──

const COBOL_KEYWORDS: &[&str] = &[
    "IDENTIFICATION",
    "ENVIRONMENT",
    "DATA",
    "PROCEDURE",
    "WORKING-STORAGE",
    "FILE",
    "LINKAGE",
    "LOCAL-STORAGE",
    "SCREEN",
    "REPORT",
    "COMMUNICATION",
    "INPUT-OUTPUT",
    "CONFIGURATION",
    "EXIT",
    "CONTINUE",
    "GOBACK",
    "NEXT",
    "STOP",
    "DISPLAY",
    "MOVE",
    "STRING",
    "ACCEPT",
    "COMPUTE",
    "IF",
    "ELSE",
    "END-IF",
    "EVALUATE",
    "WHEN",
    "PERFORM",
];

fn is_cobol_keyword(name: &str) -> bool {
    COBOL_KEYWORDS.contains(&name)
}

// ── public entry point ─────────────────────────────────────────────────────

/// Parse a COBOL source file using regex-based line scanning.
///
/// Extracts:
/// - Sections (`SECTION` keyword) → [`SymbolKind::Module`]
/// - Paragraphs (bare label ending in `.`) → [`SymbolKind::Function`]
/// - `PERFORM` targets → [`ReferenceKind::Call`]
/// - `CALL 'name'` → [`ReferenceKind::Call`]
/// - `COPY name` → [`ReferenceKind::Includes`]
///
/// Only lines inside PROCEDURE DIVISION are scanned for symbols and
/// references.  Comment lines (column 7 = `*`) are skipped.
pub fn parse_cobol(path: &Path, source: &str) -> ParsedFile {
    let path_str = path.to_string_lossy().into_owned();

    let mut symbols: Vec<RawSymbol> = Vec::new();
    let mut references: Vec<RawReference> = Vec::new();

    let mut in_procedure = false;
    let mut first_symbol = true;

    for (idx, line) in source.lines().enumerate() {
        let line_no = idx as u32 + 1;

        // Column 7 (0-indexed: position 6) is the indicator.
        // A '*' there marks a comment line.
        if line.len() > 6 && line.as_bytes()[6] == b'*' {
            continue;
        }

        let upper = line.to_ascii_uppercase();

        // Track when we enter PROCEDURE DIVISION.
        if upper.trim_start().starts_with("PROCEDURE DIVISION") {
            in_procedure = true;
            continue;
        }

        if !in_procedure {
            continue;
        }

        // ── symbol detection ───────────────────────────────────────────────

        if let Some(cap) = RE_SECTION.captures(&upper) {
            let name = cap[1].to_string();
            if !is_cobol_keyword(&name) {
                let is_first = first_symbol;
                first_symbol = false;
                let ep_kind = if is_first {
                    Some(EntryPointKind::Main)
                } else {
                    None
                };
                symbols.push(RawSymbol {
                    name: name.clone(),
                    kind: SymbolKind::Module,
                    start_line: line_no,
                    end_line: line_no,
                    signature: line.trim().to_string(),
                    content_hash: sha256_hex(line.trim()),
                    is_entry_point: ep_kind.is_some(),
                    entry_point_kind: ep_kind,
                    visibility: Visibility::Inferred,
                    type_info: None,
                });
                continue; // sections can't also be paragraphs
            }
        }

        if let Some(cap) = RE_PARAGRAPH.captures(&upper) {
            let name = cap[1].to_string();
            if !is_cobol_keyword(&name) {
                let is_first = first_symbol;
                first_symbol = false;
                let ep_kind = if is_first {
                    Some(EntryPointKind::Main)
                } else {
                    None
                };
                symbols.push(RawSymbol {
                    name: name.clone(),
                    kind: SymbolKind::Function,
                    start_line: line_no,
                    end_line: line_no,
                    signature: line.trim().to_string(),
                    content_hash: sha256_hex(line.trim()),
                    is_entry_point: ep_kind.is_some(),
                    entry_point_kind: ep_kind,
                    visibility: Visibility::Inferred,
                    type_info: None,
                });
            }
        }

        // ── reference detection ────────────────────────────────────────────

        for cap in RE_PERFORM.captures_iter(&upper) {
            references.push(RawReference {
                name: cap[1].to_string(),
                kind: ReferenceKind::Call,
                start_line: line_no,
                context: line.trim().to_string(),
                receiver: None,
            });
        }

        for cap in RE_CALL.captures_iter(&upper) {
            references.push(RawReference {
                name: cap[1].to_string(),
                kind: ReferenceKind::Call,
                start_line: line_no,
                context: line.trim().to_string(),
                receiver: None,
            });
        }

        for cap in RE_COPY.captures_iter(&upper) {
            references.push(RawReference {
                name: cap[1].to_string(),
                kind: ReferenceKind::Includes,
                start_line: line_no,
                context: line.trim().to_string(),
                receiver: None,
            });
        }
    }

    ParsedFile {
        path: path_str,
        symbols,
        references,
    }
}
