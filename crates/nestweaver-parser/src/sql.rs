use crate::parse::{ParsedFile, RawReference, RawSymbol, ReferenceKind, sha256_hex};
use nestweaver_schema::{SymbolKind, Visibility};
use regex::Regex;
use std::path::Path;
use std::sync::LazyLock;

// ── compiled patterns ──────────────────────────────────────────────────────

/// Matches `CREATE TABLE name` (with optional schema prefix).
static RE_TABLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*CREATE\s+TABLE\s+(?:IF\s+NOT\s+EXISTS\s+)?(?:(\w+)\.)?(\w+)").unwrap()
});

/// Matches `CREATE [OR REPLACE] VIEW name`.
static RE_VIEW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*CREATE\s+(?:OR\s+REPLACE\s+)?(?:MATERIALIZED\s+)?VIEW\s+(?:(\w+)\.)?(\w+)")
        .unwrap()
});

/// Matches `CREATE [OR REPLACE] FUNCTION name`.
static RE_FUNCTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*CREATE\s+(?:OR\s+REPLACE\s+)?FUNCTION\s+(?:(\w+)\.)?(\w+)").unwrap()
});

/// Matches `CREATE [OR REPLACE] PROCEDURE name`.
static RE_PROCEDURE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*CREATE\s+(?:OR\s+REPLACE\s+)?PROCEDURE\s+(?:(\w+)\.)?(\w+)").unwrap()
});

/// Matches table references in FROM/JOIN clauses.
static RE_TABLE_REF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(?:FROM|JOIN)\s+(?:(\w+)\.)?(\w+)").unwrap());

/// Matches `REFERENCES table(col)` for foreign keys.
static RE_FK_REF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bREFERENCES\s+(\w+)").unwrap());

// ── public entry point ─────────────────────────────────────────────────────

/// Parse a SQL source file using regex-based line scanning.
///
/// Extracts:
/// - `CREATE TABLE` → [`SymbolKind::Class`]
/// - `CREATE VIEW` → [`SymbolKind::Class`]
/// - `CREATE FUNCTION` → [`SymbolKind::Function`]
/// - `CREATE PROCEDURE` → [`SymbolKind::Function`]
/// - `FROM`/`JOIN` table refs → [`ReferenceKind::Call`]
/// - `REFERENCES` (FK) → [`ReferenceKind::Call`]
pub fn parse_sql(path: &Path, source: &str) -> ParsedFile {
    let path_str = path.to_string_lossy().into_owned();

    let mut symbols: Vec<RawSymbol> = Vec::new();
    let mut references: Vec<RawReference> = Vec::new();

    for (idx, line) in source.lines().enumerate() {
        let line_no = idx as u32 + 1;

        // Skip comment lines
        let trimmed = line.trim();
        if trimmed.starts_with("--") {
            continue;
        }

        // ── symbol detection ───────────────────────────────────────────────

        if let Some(cap) = RE_TABLE.captures(line) {
            let name = cap.get(2).map_or("", |m| m.as_str()).to_string();
            symbols.push(RawSymbol {
                name,
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

        if let Some(cap) = RE_VIEW.captures(line) {
            let name = cap.get(2).map_or("", |m| m.as_str()).to_string();
            symbols.push(RawSymbol {
                name,
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

        if let Some(cap) = RE_FUNCTION.captures(line) {
            let name = cap.get(2).map_or("", |m| m.as_str()).to_string();
            symbols.push(RawSymbol {
                name,
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

        if let Some(cap) = RE_PROCEDURE.captures(line) {
            let name = cap.get(2).map_or("", |m| m.as_str()).to_string();
            symbols.push(RawSymbol {
                name,
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

        for cap in RE_TABLE_REF.captures_iter(line) {
            let name = cap.get(2).map_or("", |m| m.as_str()).to_string();
            // Skip SQL keywords that can follow FROM/JOIN
            if !is_sql_keyword(&name) {
                references.push(RawReference {
                    name,
                    kind: ReferenceKind::Call,
                    start_line: line_no,
                    context: trimmed.to_string(),
                });
            }
        }

        for cap in RE_FK_REF.captures_iter(line) {
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

fn is_sql_keyword(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "SELECT"
            | "WHERE"
            | "SET"
            | "VALUES"
            | "INTO"
            | "AS"
            | "ON"
            | "AND"
            | "OR"
            | "NOT"
            | "NULL"
            | "DEFAULT"
            | "TABLE"
            | "VIEW"
            | "INDEX"
            | "FUNCTION"
            | "PROCEDURE"
    )
}
