use crate::parse::{ParsedFile, RawReference, RawSymbol, ReferenceKind, sha256_hex};
use nestweaver_schema::{SymbolKind, Visibility};
use regex::Regex;
use std::path::Path;
use std::sync::LazyLock;

// ── compiled patterns ──────────────────────────────────────────────────────

/// Matches `module name` (with optional parameter list or port list on the same line).
static RE_MODULE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bmodule\s+([a-zA-Z_][a-zA-Z0-9_]*)").unwrap());

/// Matches `interface name`.
static RE_INTERFACE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\binterface\s+([a-zA-Z_][a-zA-Z0-9_]*)").unwrap());

/// Matches `class name` (with optional `extends` clause).
static RE_CLASS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bclass\s+([a-zA-Z_][a-zA-Z0-9_]*)").unwrap());

/// Matches `extends base_class`.
static RE_EXTENDS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bextends\s+([a-zA-Z_][a-zA-Z0-9_]*)").unwrap());

/// Matches `endclass`.
static RE_ENDCLASS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bendclass\b").unwrap());

/// Matches `function` declarations: `function [return_type] name`.
static RE_FUNCTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bfunction\s+(?:(?:automatic|static)\s+)?(?:(?:void|int|bit|logic|string|real|integer|byte|shortint|longint|[\w:]+)\s+)?([a-zA-Z_][a-zA-Z0-9_]*)").unwrap()
});

/// Matches `task name`.
static RE_TASK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\btask\s+(?:(?:automatic|static)\s+)?([a-zA-Z_][a-zA-Z0-9_]*)").unwrap()
});

/// Matches `import pkg::*` or `import pkg::name`.
static RE_IMPORT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bimport\s+([a-zA-Z_][a-zA-Z0-9_]*)::\*?([a-zA-Z_][a-zA-Z0-9_]*)?").unwrap()
});

/// Matches `include "filename"` directives.
static RE_INCLUDE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"`include\s+["']([^"']+)["']"#).unwrap());

/// Matches module instantiation: `module_name instance_name (` or `module_name #(`.
static RE_INSTANTIATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b([a-zA-Z_][a-zA-Z0-9_]*)\s+(?:#\s*\(|[a-zA-Z_][a-zA-Z0-9_]*\s*\()").unwrap()
});

/// SystemVerilog keywords that are not module instantiations.
const SV_KEYWORDS: &[&str] = &[
    "module",
    "endmodule",
    "interface",
    "endinterface",
    "class",
    "endclass",
    "function",
    "endfunction",
    "task",
    "endtask",
    "begin",
    "end",
    "if",
    "else",
    "for",
    "while",
    "do",
    "foreach",
    "repeat",
    "forever",
    "case",
    "casex",
    "casez",
    "endcase",
    "default",
    "assign",
    "always",
    "always_ff",
    "always_comb",
    "always_latch",
    "initial",
    "final",
    "generate",
    "endgenerate",
    "input",
    "output",
    "inout",
    "ref",
    "wire",
    "reg",
    "logic",
    "bit",
    "integer",
    "real",
    "string",
    "void",
    "int",
    "byte",
    "shortint",
    "longint",
    "parameter",
    "localparam",
    "typedef",
    "enum",
    "struct",
    "union",
    "package",
    "endpackage",
    "import",
    "export",
    "virtual",
    "pure",
    "extern",
    "static",
    "automatic",
    "protected",
    "local",
    "return",
    "break",
    "continue",
    "fork",
    "join",
    "join_any",
    "join_none",
    "constraint",
    "rand",
    "randc",
    "covergroup",
    "coverpoint",
    "assert",
    "assume",
    "cover",
    "property",
    "sequence",
    "new",
    "extends",
    "implements",
    "super",
    "this",
    "modport",
    "clocking",
    "program",
    "endprogram",
];

// ── public entry point ─────────────────────────────────────────────────────

/// Parse a SystemVerilog source file (`.sv`, `.svh`) using regex-based line scanning.
///
/// Extracts:
/// - Modules → [`SymbolKind::Module`]
/// - Interfaces → [`SymbolKind::Interface`]
/// - Classes → [`SymbolKind::Class`]
/// - Functions → [`SymbolKind::Function`] or [`SymbolKind::Method`] (if inside a class)
/// - Tasks → [`SymbolKind::Function`] or [`SymbolKind::Method`] (if inside a class)
/// - `import` packages → [`ReferenceKind::Import`]
/// - `` `include `` directives → [`ReferenceKind::Includes`]
/// - `extends` clauses → [`ReferenceKind::Extends`]
/// - Module instantiations → [`ReferenceKind::Call`]
pub fn parse_systemverilog(path: &Path, source: &str) -> ParsedFile {
    let path_str = path.to_string_lossy().into_owned();

    let mut symbols: Vec<RawSymbol> = Vec::new();
    let mut references: Vec<RawReference> = Vec::new();

    let mut in_class = false;
    let mut class_depth: i32 = 0;

    for (idx, line) in source.lines().enumerate() {
        let line_no = idx as u32 + 1;
        let trimmed = line.trim();

        // Skip empty lines and single-line comments
        if trimmed.is_empty() || trimmed.starts_with("//") {
            continue;
        }

        // ── `include directives ─────────────────────────────────────
        if let Some(cap) = RE_INCLUDE.captures(trimmed) {
            references.push(RawReference {
                name: cap[1].to_string(),
                kind: ReferenceKind::Includes,
                start_line: line_no,
                context: trimmed.to_string(),
            });
        }

        // ── import statements ──────────────────────────────────────
        if let Some(cap) = RE_IMPORT.captures(trimmed) {
            let pkg = cap[1].to_string();
            references.push(RawReference {
                name: pkg,
                kind: ReferenceKind::Import,
                start_line: line_no,
                context: trimmed.to_string(),
            });
        }

        // ── module declarations ────────────────────────────────────
        if let Some(cap) = RE_MODULE.captures(trimmed) {
            let name = cap[1].to_string();
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

        // ── interface declarations ─────────────────────────────────
        if let Some(cap) = RE_INTERFACE.captures(trimmed) {
            let name = cap[1].to_string();
            symbols.push(RawSymbol {
                name,
                kind: SymbolKind::Interface,
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

        // ── class declarations ─────────────────────────────────────
        if let Some(cap) = RE_CLASS.captures(trimmed) {
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

            // Check for extends clause
            if let Some(ext_cap) = RE_EXTENDS.captures(trimmed) {
                references.push(RawReference {
                    name: ext_cap[1].to_string(),
                    kind: ReferenceKind::Extends,
                    start_line: line_no,
                    context: trimmed.to_string(),
                });
            }

            in_class = true;
            class_depth = 1;
            continue;
        }

        // Track class nesting depth
        if in_class {
            if RE_ENDCLASS.is_match(trimmed) {
                class_depth -= 1;
                if class_depth <= 0 {
                    in_class = false;
                    class_depth = 0;
                }
                continue;
            }
            // Nested class
            if trimmed.contains("class ") && !trimmed.starts_with("//") {
                class_depth += 1;
            }
        }

        // ── function declarations ──────────────────────────────────
        if let Some(cap) = RE_FUNCTION.captures(trimmed) {
            let name = cap[1].to_string();
            let kind = if in_class {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            };
            symbols.push(RawSymbol {
                name,
                kind,
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

        // ── task declarations ──────────────────────────────────────
        if let Some(cap) = RE_TASK.captures(trimmed) {
            let name = cap[1].to_string();
            let kind = if in_class {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            };
            symbols.push(RawSymbol {
                name,
                kind,
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

        // ── module instantiations ──────────────────────────────────
        if let Some(cap) = RE_INSTANTIATION.captures(trimmed) {
            let name = cap[1].to_string();
            if !SV_KEYWORDS.contains(&name.as_str()) {
                references.push(RawReference {
                    name,
                    kind: ReferenceKind::Call,
                    start_line: line_no,
                    context: trimmed.to_string(),
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
