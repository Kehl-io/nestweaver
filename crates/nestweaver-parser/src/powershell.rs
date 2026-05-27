use crate::entry_points::detect_entry_point;
use crate::parse::{ParsedFile, RawReference, RawSymbol, ReferenceKind, sha256_hex};
use nestweaver_schema::{SymbolKind, Visibility};
use regex::Regex;
use std::path::Path;
use std::sync::LazyLock;

// ── compiled patterns ──────────────────────────────────────────────────────

/// Matches `function Verb-Noun` or `function Name` (case insensitive).
static RE_FUNCTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*function\s+([A-Za-z_][A-Za-z0-9_-]*)").unwrap());

/// Matches `filter Name`.
static RE_FILTER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*filter\s+([A-Za-z_][A-Za-z0-9_-]*)").unwrap());

/// Matches `class ClassName`.
static RE_CLASS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*class\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap());

/// Matches `enum EnumName`.
static RE_ENUM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*enum\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap());

/// Matches `. .\script.ps1` or `. ./module.psm1` (dot-sourcing).
static RE_DOT_SOURCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^\s*\.\s+['".]?\.?[\\/]?([A-Za-z0-9_.-]+\.ps[m]?1)"#).unwrap());

/// Matches `Import-Module ModuleName` or `using module ModuleName`.
static RE_IMPORT_MODULE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*(?:Import-Module|using\s+module)\s+([A-Za-z0-9_.\\/-]+)").unwrap()
});

/// Matches PowerShell class methods: `[ReturnType] MethodName(` or `MethodName(`.
/// These are inside class bodies and don't use the `function` keyword.
static RE_CLASS_METHOD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:\[[A-Za-z_][A-Za-z0-9_\[\]]*\]\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*\(").unwrap()
});

/// Matches cmdlet-style calls: `Verb-Noun` at start of line or after pipe.
static RE_CMDLET_CALL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b([A-Z][a-zA-Z0-9]+-[A-Z][a-zA-Z0-9]+)\b").unwrap());

// ── public entry point ─────────────────────────────────────────────────────

/// Parse a PowerShell source file using regex-based line scanning.
pub fn parse_powershell(path: &Path, source: &str) -> ParsedFile {
    let path_str = path.to_string_lossy().into_owned();
    let file_path_str = path.to_string_lossy();

    let mut symbols: Vec<RawSymbol> = Vec::new();
    let mut references: Vec<RawReference> = Vec::new();
    let mut in_class = false;
    let mut brace_depth: i32 = 0;
    let mut class_brace_depth: i32 = 0;

    for (idx, line) in source.lines().enumerate() {
        let line_no = idx as u32 + 1;
        let trimmed = line.trim();

        // Skip comments
        if trimmed.starts_with('#') {
            continue;
        }

        // Track brace depth for class membership
        for ch in trimmed.chars() {
            match ch {
                '{' => brace_depth += 1,
                '}' => {
                    brace_depth -= 1;
                    if in_class && brace_depth < class_brace_depth {
                        in_class = false;
                    }
                }
                _ => {}
            }
        }

        // ── symbol detection ───────────────────────────────────────────────

        if let Some(cap) = RE_CLASS.captures(line) {
            let name = cap[1].to_string();
            in_class = true;
            class_brace_depth = brace_depth;
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

        if let Some(cap) = RE_ENUM.captures(line) {
            let name = cap[1].to_string();
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
            let name = cap[1].to_string();
            let kind = if in_class {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            };
            let kind_label = if in_class { "method" } else { "function" };
            let ep_kind = detect_entry_point(
                &name,
                &file_path_str,
                kind_label,
                Some(trimmed),
                "powershell",
            );
            symbols.push(RawSymbol {
                name,
                kind,
                start_line: line_no,
                signature: trimmed.to_string(),
                content_hash: sha256_hex(trimmed),
                is_entry_point: ep_kind.is_some(),
                entry_point_kind: ep_kind,
                visibility: Visibility::Public,
                type_info: None,
            });
            continue;
        }

        // Class methods (inside class body, no `function` keyword)
        if in_class && let Some(cap) = RE_CLASS_METHOD.captures(line) {
            let name = cap[1].to_string();
            // Skip property declarations like `[string]$Name`
            if !name.starts_with('$') && !trimmed.contains('$') {
                let ep_kind = detect_entry_point(
                    &name,
                    &file_path_str,
                    "method",
                    Some(trimmed),
                    "powershell",
                );
                symbols.push(RawSymbol {
                    name,
                    kind: SymbolKind::Method,
                    start_line: line_no,
                    signature: trimmed.to_string(),
                    content_hash: sha256_hex(trimmed),
                    is_entry_point: ep_kind.is_some(),
                    entry_point_kind: ep_kind,
                    visibility: Visibility::Public,
                    type_info: None,
                });
                continue;
            }
        }

        if let Some(cap) = RE_FILTER.captures(line) {
            let name = cap[1].to_string();
            let ep_kind = detect_entry_point(
                &name,
                &file_path_str,
                "function",
                Some(trimmed),
                "powershell",
            );
            symbols.push(RawSymbol {
                name,
                kind: SymbolKind::Function,
                start_line: line_no,
                signature: trimmed.to_string(),
                content_hash: sha256_hex(trimmed),
                is_entry_point: ep_kind.is_some(),
                entry_point_kind: ep_kind,
                visibility: Visibility::Public,
                type_info: None,
            });
            continue;
        }

        // ── reference detection ────────────────────────────────────────────

        if let Some(cap) = RE_IMPORT_MODULE.captures(line) {
            references.push(RawReference {
                name: cap[1].to_string(),
                kind: ReferenceKind::Import,
                start_line: line_no,
                context: trimmed.to_string(),
            });
            continue;
        }

        if let Some(cap) = RE_DOT_SOURCE.captures(line) {
            references.push(RawReference {
                name: cap[1].to_string(),
                kind: ReferenceKind::Includes,
                start_line: line_no,
                context: trimmed.to_string(),
            });
            continue;
        }

        // Cmdlet-style calls (Verb-Noun pattern)
        for cap in RE_CMDLET_CALL.captures_iter(line) {
            let name = cap[1].to_string();
            // Skip if this is a function definition on this line
            if RE_FUNCTION.is_match(line) {
                continue;
            }
            references.push(RawReference {
                name,
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
