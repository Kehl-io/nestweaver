use crate::parse::{ParsedFile, RawReference, RawSymbol, ReferenceKind, sha256_hex};
use nestweaver_schema::{SymbolKind, Visibility};
use regex::Regex;
use std::path::Path;
use std::sync::LazyLock;

// ── compiled patterns ──────────────────────────────────────────────────────

/// Matches `procedure Name` (standalone or method body).
static RE_PROCEDURE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*(?:class\s+)?procedure\s+(?:(\w+)\.)?(\w+)").unwrap());

/// Matches `function Name` (standalone or method body).
static RE_FUNCTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*(?:class\s+)?function\s+(?:(\w+)\.)?(\w+)").unwrap());

/// Matches `constructor Name.Create` or `destructor Name.Destroy`.
static RE_CONSTRUCTOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*(?:constructor|destructor)\s+(?:(\w+)\.)?(\w+)").unwrap()
});

/// Matches `unit Name;`.
static RE_UNIT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*unit\s+(\w+)\s*;").unwrap());

/// Matches `program Name;`.
static RE_PROGRAM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*program\s+(\w+)\s*;").unwrap());

/// Matches class declaration: `TName = class` or `TName = class(TParent)`.
static RE_CLASS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*(\w+)\s*=\s*class(?:\s*\((\w+)\))?").unwrap());

/// Matches interface declaration: `IName = interface`.
static RE_INTERFACE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*(\w+)\s*=\s*interface").unwrap());

/// Matches `uses Unit1, Unit2, ...;`.
static RE_USES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^\s*uses\s+(.+)").unwrap());

/// Splits a uses clause by commas.
static RE_COMMA_SPLIT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[,;]").unwrap());

/// Matches `inherited Create(...)` (superclass call).
static RE_INHERITED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\binherited\s+(\w+)").unwrap());

// ── public entry point ─────────────────────────────────────────────────────

/// Parse a Pascal source file using regex-based line scanning.
///
/// Extracts:
/// - Procedures → [`SymbolKind::Function`]
/// - Functions → [`SymbolKind::Function`]
/// - Units → [`SymbolKind::Module`]
/// - Programs → [`SymbolKind::Module`]
/// - Classes → [`SymbolKind::Class`]
/// - Interfaces → [`SymbolKind::Interface`]
/// - `uses` clauses → [`ReferenceKind::Import`]
/// - Class inheritance → [`ReferenceKind::Extends`]
pub fn parse_pascal(path: &Path, source: &str) -> ParsedFile {
    let path_str = path.to_string_lossy().into_owned();

    let mut symbols: Vec<RawSymbol> = Vec::new();
    let mut references: Vec<RawReference> = Vec::new();

    for (idx, line) in source.lines().enumerate() {
        let line_no = idx as u32 + 1;

        // Skip comment lines
        let trimmed = line.trim();
        if trimmed.starts_with("//") || trimmed.starts_with('{') || trimmed.starts_with("(*") {
            continue;
        }

        // ── symbol detection ───────────────────────────────────────────────

        if let Some(cap) = RE_UNIT.captures(line) {
            symbols.push(RawSymbol {
                name: cap[1].to_string(),
                kind: SymbolKind::Module,
                start_line: line_no,
                end_line: line_no,
                signature: trimmed.to_string(),
                content_hash: sha256_hex(trimmed),
                is_entry_point: false,
                entry_point_kind: None,
                visibility: Visibility::Public,
                type_info: None,
                parent_name: None,
            });
            continue;
        }

        if let Some(cap) = RE_PROGRAM.captures(line) {
            symbols.push(RawSymbol {
                name: cap[1].to_string(),
                kind: SymbolKind::Module,
                start_line: line_no,
                end_line: line_no,
                signature: trimmed.to_string(),
                content_hash: sha256_hex(trimmed),
                is_entry_point: true,
                entry_point_kind: Some(nestweaver_schema::EntryPointKind::Main),
                visibility: Visibility::Public,
                type_info: None,
                parent_name: None,
            });
            continue;
        }

        if let Some(cap) = RE_INTERFACE.captures(line) {
            symbols.push(RawSymbol {
                name: cap[1].to_string(),
                kind: SymbolKind::Interface,
                start_line: line_no,
                end_line: line_no,
                signature: trimmed.to_string(),
                content_hash: sha256_hex(trimmed),
                is_entry_point: false,
                entry_point_kind: None,
                visibility: Visibility::Public,
                type_info: None,
                parent_name: None,
            });
            continue;
        }

        if let Some(cap) = RE_CLASS.captures(line) {
            let name = cap[1].to_string();
            symbols.push(RawSymbol {
                name,
                kind: SymbolKind::Class,
                start_line: line_no,
                end_line: line_no,
                signature: trimmed.to_string(),
                content_hash: sha256_hex(trimmed),
                is_entry_point: false,
                entry_point_kind: None,
                visibility: Visibility::Public,
                type_info: None,
                parent_name: None,
            });

            // Extract parent class as extends reference
            if let Some(parent) = cap.get(2) {
                references.push(RawReference {
                    name: parent.as_str().to_string(),
                    kind: ReferenceKind::Extends,
                    start_line: line_no,
                    context: trimmed.to_string(),
                    receiver: None,
                });
            }
            continue;
        }

        if let Some(cap) = RE_CONSTRUCTOR.captures(line) {
            symbols.push(RawSymbol {
                name: cap[2].to_string(),
                kind: SymbolKind::Method,
                start_line: line_no,
                end_line: line_no,
                signature: trimmed.to_string(),
                content_hash: sha256_hex(trimmed),
                is_entry_point: false,
                entry_point_kind: None,
                visibility: Visibility::Public,
                type_info: None,
                parent_name: None,
            });
            continue;
        }

        if let Some(cap) = RE_PROCEDURE.captures(line) {
            let is_method = cap.get(1).is_some();
            symbols.push(RawSymbol {
                name: cap[2].to_string(),
                kind: if is_method {
                    SymbolKind::Method
                } else {
                    SymbolKind::Function
                },
                start_line: line_no,
                end_line: line_no,
                signature: trimmed.to_string(),
                content_hash: sha256_hex(trimmed),
                is_entry_point: false,
                entry_point_kind: None,
                visibility: Visibility::Public,
                type_info: None,
                parent_name: None,
            });
            continue;
        }

        if let Some(cap) = RE_FUNCTION.captures(line) {
            let is_method = cap.get(1).is_some();
            symbols.push(RawSymbol {
                name: cap[2].to_string(),
                kind: if is_method {
                    SymbolKind::Method
                } else {
                    SymbolKind::Function
                },
                start_line: line_no,
                end_line: line_no,
                signature: trimmed.to_string(),
                content_hash: sha256_hex(trimmed),
                is_entry_point: false,
                entry_point_kind: None,
                visibility: Visibility::Public,
                type_info: None,
                parent_name: None,
            });
            continue;
        }

        // ── reference detection ────────────────────────────────────────────

        if let Some(cap) = RE_USES.captures(line) {
            let units_str = &cap[1];
            for part in RE_COMMA_SPLIT.split(units_str) {
                let unit_name = part.trim();
                if !unit_name.is_empty() {
                    references.push(RawReference {
                        name: unit_name.to_string(),
                        kind: ReferenceKind::Import,
                        start_line: line_no,
                        context: trimmed.to_string(),
                        receiver: None,
                    });
                }
            }
            continue;
        }

        if let Some(cap) = RE_INHERITED.captures(line) {
            references.push(RawReference {
                name: cap[1].to_string(),
                kind: ReferenceKind::Call,
                start_line: line_no,
                context: trimmed.to_string(),
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
