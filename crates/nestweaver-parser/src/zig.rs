use crate::entry_points::detect_entry_point;
use crate::parse::{ParsedFile, RawReference, RawSymbol, ReferenceKind, sha256_hex};
use nestweaver_schema::{SymbolKind, Visibility};
use regex::Regex;
use std::path::Path;
use std::sync::LazyLock;

// ── compiled patterns ──────────────────────────────────────────────────────

/// Matches `pub fn name(` or `fn name(`.
static RE_FUNCTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(pub\s+)?fn\s+([a-zA-Z_][a-zA-Z0-9_]*)\s*\(").unwrap());

/// Matches `pub const name = struct {` or `const name = struct {`.
static RE_STRUCT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(pub\s+)?const\s+([a-zA-Z_][a-zA-Z0-9_]*)\s*=\s*(?:extern\s+)?struct\b")
        .unwrap()
});

/// Matches `pub const name = enum {` or `const name = enum {`.
static RE_ENUM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(pub\s+)?const\s+([a-zA-Z_][a-zA-Z0-9_]*)\s*=\s*(?:extern\s+)?enum\b").unwrap()
});

/// Matches `pub const name = union {` or `const name = union {`.
static RE_UNION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(pub\s+)?const\s+([a-zA-Z_][a-zA-Z0-9_]*)\s*=\s*(?:extern\s+)?union\b")
        .unwrap()
});

/// Matches `pub const name = value` — used after struct/enum/union regexes
/// have been checked, so this only catches plain constants.
static RE_CONST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(pub\s+)?const\s+([a-zA-Z_][a-zA-Z0-9_]*)\s*=").unwrap());

/// Matches `@import("name")`.
static RE_IMPORT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"@import\(\s*"([^"]+)"\s*\)"#).unwrap());

/// Matches function calls: `name(`.
static RE_CALL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b([a-zA-Z_][a-zA-Z0-9_]*)\s*\(").unwrap());

// ── public entry point ─────────────────────────────────────────────────────

/// Parse a Zig source file using regex-based line scanning.
pub fn parse_zig(path: &Path, source: &str) -> ParsedFile {
    let path_str = path.to_string_lossy().into_owned();
    let file_path_str = path.to_string_lossy();

    let mut symbols: Vec<RawSymbol> = Vec::new();
    let mut references: Vec<RawReference> = Vec::new();

    for (idx, line) in source.lines().enumerate() {
        let line_no = idx as u32 + 1;
        let trimmed = line.trim();

        // Skip comments
        if trimmed.starts_with("//") {
            continue;
        }

        // ── symbol detection ───────────────────────────────────────────────

        if let Some(cap) = RE_FUNCTION.captures(line) {
            let is_pub = cap.get(1).is_some();
            let name = cap[2].to_string();
            let visibility = if is_pub {
                Visibility::Public
            } else {
                Visibility::Private
            };
            let ep_kind =
                detect_entry_point(&name, &file_path_str, "function", Some(trimmed), "zig");
            symbols.push(RawSymbol {
                name,
                kind: SymbolKind::Function,
                start_line: line_no,
                end_line: line_no,
                signature: trimmed.to_string(),
                content_hash: sha256_hex(trimmed),
                is_entry_point: ep_kind.is_some(),
                entry_point_kind: ep_kind,
                visibility,
                type_info: None,
            });
            continue;
        }

        if let Some(cap) = RE_STRUCT.captures(line) {
            let is_pub = cap.get(1).is_some();
            let name = cap[2].to_string();
            let visibility = if is_pub {
                Visibility::Public
            } else {
                Visibility::Private
            };
            symbols.push(RawSymbol {
                name,
                kind: SymbolKind::Class,
                start_line: line_no,
                end_line: line_no,
                signature: trimmed.to_string(),
                content_hash: sha256_hex(trimmed),
                is_entry_point: false,
                entry_point_kind: None,
                visibility,
                type_info: None,
            });
            continue;
        }

        if let Some(cap) = RE_ENUM.captures(line) {
            let is_pub = cap.get(1).is_some();
            let name = cap[2].to_string();
            let visibility = if is_pub {
                Visibility::Public
            } else {
                Visibility::Private
            };
            symbols.push(RawSymbol {
                name,
                kind: SymbolKind::Class,
                start_line: line_no,
                end_line: line_no,
                signature: trimmed.to_string(),
                content_hash: sha256_hex(trimmed),
                is_entry_point: false,
                entry_point_kind: None,
                visibility,
                type_info: None,
            });
            continue;
        }

        if let Some(cap) = RE_UNION.captures(line) {
            let is_pub = cap.get(1).is_some();
            let name = cap[2].to_string();
            let visibility = if is_pub {
                Visibility::Public
            } else {
                Visibility::Private
            };
            symbols.push(RawSymbol {
                name,
                kind: SymbolKind::Class,
                start_line: line_no,
                end_line: line_no,
                signature: trimmed.to_string(),
                content_hash: sha256_hex(trimmed),
                is_entry_point: false,
                entry_point_kind: None,
                visibility,
                type_info: None,
            });
            continue;
        }

        if let Some(cap) = RE_CONST.captures(line) {
            let is_pub = cap.get(1).is_some();
            let name = cap[2].to_string();
            let visibility = if is_pub {
                Visibility::Public
            } else {
                Visibility::Private
            };
            symbols.push(RawSymbol {
                name,
                kind: SymbolKind::Constant,
                start_line: line_no,
                end_line: line_no,
                signature: trimmed.to_string(),
                content_hash: sha256_hex(trimmed),
                is_entry_point: false,
                entry_point_kind: None,
                visibility,
                type_info: None,
            });
            // Don't continue — fall through to reference detection so
            // `const std = @import("std")` also emits the import ref.
        }

        // ── reference detection ────────────────────────────────────────────

        for cap in RE_IMPORT.captures_iter(line) {
            references.push(RawReference {
                name: cap[1].to_string(),
                kind: ReferenceKind::Import,
                start_line: line_no,
                context: trimmed.to_string(),
                receiver: None,
            });
        }

        // Only detect calls on lines that are not symbol definitions
        for cap in RE_CALL.captures_iter(line) {
            let name = &cap[1];
            // Skip keywords and the @import we already captured
            if matches!(
                name,
                "fn" | "if"
                    | "while"
                    | "for"
                    | "switch"
                    | "return"
                    | "const"
                    | "var"
                    | "pub"
                    | "struct"
                    | "enum"
                    | "union"
                    | "error"
                    | "try"
                    | "catch"
            ) {
                continue;
            }
            references.push(RawReference {
                name: name.to_string(),
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
