use crate::parse::{ParsedFile, RawReference, RawSymbol, ReferenceKind, sha256_hex};
use nestweaver_schema::{SymbolKind, Visibility};
use regex::Regex;
use std::path::Path;
use std::sync::LazyLock;

// ── compiled patterns ──────────────────────────────────────────────────────

/// Matches `function name(...)` — captures the function name.
static RE_FUNCTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*function\s+([a-zA-Z_]\w*)\s*[({]").unwrap());

/// Matches `struct Name` or `mutable struct Name`.
static RE_STRUCT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(?:mutable\s+)?struct\s+([A-Z]\w*)").unwrap());

/// Matches `module Name`.
static RE_MODULE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*module\s+([A-Z]\w*)").unwrap());

/// Matches `macro name(...)`.
static RE_MACRO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*macro\s+([a-zA-Z_]\w*)\s*\(").unwrap());

/// Matches `abstract type Name ... end`.
static RE_ABSTRACT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*abstract\s+type\s+([A-Z]\w*)").unwrap());

/// Matches function calls: `name(...)`.
static RE_CALL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b([a-zA-Z_]\w*)\s*\(").unwrap());

/// Matches `using Module` or `import Module`.
static RE_IMPORT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:using|import)\s+([A-Za-z_]\w*(?:\.[A-Za-z_]\w*)*)").unwrap()
});

/// Matches `include("file.jl")`.
static RE_INCLUDE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"\binclude\s*\(\s*"([^"]+)"\s*\)"#).unwrap());

// ── keywords to exclude from call detection ───────────────────────────────

const JULIA_KEYWORDS: &[&str] = &[
    "if", "else", "elseif", "for", "while", "return", "function", "end", "begin", "let", "try",
    "catch", "finally", "struct", "module", "using", "import", "export", "macro", "quote", "do",
    "in", "isa", "where", "abstract", "type", "mutable", "const", "global", "local",
];

fn is_julia_keyword(name: &str) -> bool {
    JULIA_KEYWORDS.contains(&name)
}

// ── public entry point ─────────────────────────────────────────────────────

/// Parse a Julia source file using regex-based line scanning.
///
/// Extracts:
/// - Functions → [`SymbolKind::Function`]
/// - Structs → [`SymbolKind::Class`]
/// - Modules → [`SymbolKind::Module`]
/// - Macros → [`SymbolKind::Function`]
/// - Abstract types → [`SymbolKind::Interface`]
/// - Function calls → [`ReferenceKind::Call`]
/// - `using`/`import` → [`ReferenceKind::Import`]
/// - `include` → [`ReferenceKind::Includes`]
pub fn parse_julia(path: &Path, source: &str) -> ParsedFile {
    let path_str = path.to_string_lossy().into_owned();

    let mut symbols: Vec<RawSymbol> = Vec::new();
    let mut references: Vec<RawReference> = Vec::new();

    for (idx, line) in source.lines().enumerate() {
        let line_no = idx as u32 + 1;

        // Skip comment lines
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            continue;
        }

        // ── symbol detection ───────────────────────────────────────────────

        if let Some(cap) = RE_MODULE.captures(line) {
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

        if let Some(cap) = RE_ABSTRACT.captures(line) {
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

        if let Some(cap) = RE_STRUCT.captures(line) {
            symbols.push(RawSymbol {
                name: cap[1].to_string(),
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
            continue;
        }

        if let Some(cap) = RE_MACRO.captures(line) {
            symbols.push(RawSymbol {
                name: cap[1].to_string(),
                kind: SymbolKind::Function,
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
            let name = cap[1].to_string();
            symbols.push(RawSymbol {
                name,
                kind: SymbolKind::Function,
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

        if let Some(cap) = RE_IMPORT.captures(line) {
            references.push(RawReference {
                name: cap[1].to_string(),
                kind: ReferenceKind::Import,
                start_line: line_no,
                context: trimmed.to_string(),
                receiver: None,
            });
            continue;
        }

        for cap in RE_INCLUDE.captures_iter(line) {
            references.push(RawReference {
                name: cap[1].to_string(),
                kind: ReferenceKind::Includes,
                start_line: line_no,
                context: trimmed.to_string(),
                receiver: None,
            });
        }

        for cap in RE_CALL.captures_iter(line) {
            let name = cap[1].to_string();
            if !is_julia_keyword(&name) && name != "include" {
                references.push(RawReference {
                    name,
                    kind: ReferenceKind::Call,
                    start_line: line_no,
                    context: trimmed.to_string(),
                    receiver: None,
                });
            }
        }
    }

    ParsedFile {
        path: path_str,
        symbols,
        references,
    }
}
