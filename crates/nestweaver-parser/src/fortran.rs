use crate::parse::{ParsedFile, RawReference, RawSymbol, ReferenceKind, sha256_hex};
use nestweaver_schema::{SymbolKind, Visibility};
use regex::Regex;
use std::path::Path;
use std::sync::LazyLock;

// ── compiled patterns ──────────────────────────────────────────────────────

/// Matches `subroutine name(...)` — case insensitive.
static RE_SUBROUTINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*(?:(?:pure|elemental|recursive)\s+)*subroutine\s+(\w+)").unwrap()
});

/// Matches `function name(...)` with optional return type prefix.
static RE_FUNCTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*(?:(?:pure|elemental|recursive|integer|real|double\s+precision|complex|character|logical|type\s*\([^)]*\))\s+)*function\s+(\w+)")
        .unwrap()
});

/// Matches `module name`.
static RE_MODULE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*module\s+(\w+)\s*$").unwrap());

/// Matches `program name`.
static RE_PROGRAM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*program\s+(\w+)").unwrap());

/// Matches `type :: name` or `type, ... :: name`.
static RE_TYPE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*type(?:\s*,\s*\w+(?:\([^)]*\))?)*\s*(?:::\s*)(\w+)\s*$").unwrap()
});

/// Matches `call name(...)`.
static RE_CALL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bcall\s+(\w+)").unwrap());

/// Matches `use module_name`.
static RE_USE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^\s*use\s+(\w+)").unwrap());

/// Matches `include 'file'` or `include "file"`.
static RE_INCLUDE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)^\s*include\s+['"]([^'"]+)['"]"#).unwrap());

// ── keywords to exclude from module/type detection ─────────────────────────

fn is_fortran_module_keyword(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    matches!(lower.as_str(), "procedure" | "function" | "subroutine")
}

// ── public entry point ─────────────────────────────────────────────────────

/// Parse a Fortran source file using regex-based line scanning.
///
/// Extracts:
/// - Subroutines → [`SymbolKind::Function`]
/// - Functions → [`SymbolKind::Function`]
/// - Modules → [`SymbolKind::Module`]
/// - Programs → [`SymbolKind::Module`]
/// - Derived types → [`SymbolKind::Class`]
/// - `call` statements → [`ReferenceKind::Call`]
/// - `use` statements → [`ReferenceKind::Import`]
/// - `include` statements → [`ReferenceKind::Includes`]
pub fn parse_fortran(path: &Path, source: &str) -> ParsedFile {
    let path_str = path.to_string_lossy().into_owned();

    let mut symbols: Vec<RawSymbol> = Vec::new();
    let mut references: Vec<RawReference> = Vec::new();

    for (idx, line) in source.lines().enumerate() {
        let line_no = idx as u32 + 1;

        // Skip comment lines: '!' is the standard free-form comment character.
        // Fixed-form: column 1 'c', 'C', or '*' marks the line as a comment.
        let trimmed = line.trim();
        if trimmed.starts_with('!') {
            continue;
        }
        if !line.is_empty() && matches!(line.as_bytes()[0], b'c' | b'C' | b'*') {
            continue;
        }

        // ── symbol detection ───────────────────────────────────────────────

        if let Some(cap) = RE_PROGRAM.captures(line) {
            let name = cap[1].to_string();
            symbols.push(RawSymbol {
                name,
                kind: SymbolKind::Module,
                start_line: line_no,
                signature: trimmed.to_string(),
                content_hash: sha256_hex(trimmed),
                is_entry_point: true,
                entry_point_kind: Some(nestweaver_schema::EntryPointKind::Main),
                visibility: Visibility::Public,
                type_info: None,
            });
            continue;
        }

        if let Some(cap) = RE_MODULE.captures(line) {
            let name = cap[1].to_string();
            if !is_fortran_module_keyword(&name) {
                symbols.push(RawSymbol {
                    name,
                    kind: SymbolKind::Module,
                    start_line: line_no,
                    signature: trimmed.to_string(),
                    content_hash: sha256_hex(trimmed),
                    is_entry_point: false,
                    entry_point_kind: None,
                    visibility: Visibility::Public,
                    type_info: None,
                });
                continue;
            }
        }

        if let Some(cap) = RE_TYPE.captures(line) {
            symbols.push(RawSymbol {
                name: cap[1].to_string(),
                kind: SymbolKind::Class,
                start_line: line_no,
                signature: trimmed.to_string(),
                content_hash: sha256_hex(trimmed),
                is_entry_point: false,
                entry_point_kind: None,
                visibility: Visibility::Public,
                type_info: None,
            });
            continue;
        }

        if let Some(cap) = RE_SUBROUTINE.captures(line) {
            symbols.push(RawSymbol {
                name: cap[1].to_string(),
                kind: SymbolKind::Function,
                start_line: line_no,
                signature: trimmed.to_string(),
                content_hash: sha256_hex(trimmed),
                is_entry_point: false,
                entry_point_kind: None,
                visibility: Visibility::Public,
                type_info: None,
            });
            continue;
        }

        if let Some(cap) = RE_FUNCTION.captures(line) {
            symbols.push(RawSymbol {
                name: cap[1].to_string(),
                kind: SymbolKind::Function,
                start_line: line_no,
                signature: trimmed.to_string(),
                content_hash: sha256_hex(trimmed),
                is_entry_point: false,
                entry_point_kind: None,
                visibility: Visibility::Public,
                type_info: None,
            });
            continue;
        }

        // ── reference detection ────────────────────────────────────────────

        if let Some(cap) = RE_USE.captures(line) {
            references.push(RawReference {
                name: cap[1].to_string(),
                kind: ReferenceKind::Import,
                start_line: line_no,
                context: trimmed.to_string(),
            });
        }

        if let Some(cap) = RE_INCLUDE.captures(line) {
            references.push(RawReference {
                name: cap[1].to_string(),
                kind: ReferenceKind::Includes,
                start_line: line_no,
                context: trimmed.to_string(),
            });
        }

        for cap in RE_CALL.captures_iter(line) {
            references.push(RawReference {
                name: cap[1].to_string(),
                kind: ReferenceKind::Call,
                start_line: line_no,
                context: trimmed.to_string(),
            });
        }
    }

    ParsedFile {
        path: path_str,
        symbols,
        references,
    }
}
