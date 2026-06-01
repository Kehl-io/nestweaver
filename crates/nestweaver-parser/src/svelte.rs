use crate::parse::{ParsedFile, RawReference, RawSymbol, ReferenceKind, sha256_hex};
use nestweaver_schema::{SymbolKind, Visibility};
use regex::Regex;
use std::path::Path;
use std::sync::LazyLock;

// ── compiled patterns ──────────────────────────────────────────────────────

/// Matches the `<script ...>` opening tag (with optional attributes like `lang="ts"` or `context="module"`).
static RE_SCRIPT_OPEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<script[^>]*>").unwrap());

/// Matches the `</script>` closing tag.
static RE_SCRIPT_CLOSE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)</script\s*>").unwrap());

/// Matches `export function name(` or `export const name =`.
static RE_EXPORT_NAMED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"export\s+(?:function|const|let|var|class)\s+([a-zA-Z_$][a-zA-Z0-9_$]*)").unwrap()
});

/// Matches `function name(` (named function declarations).
static RE_FUNCTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bfunction\s+([a-zA-Z_$][a-zA-Z0-9_$]*)\s*\(").unwrap());

/// Matches `import ... from '...'`.
static RE_IMPORT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"import\s+.*?from\s+['"]([^'"]+)['"]"#).unwrap());

/// Matches `import '...'` (side-effect imports).
static RE_IMPORT_SIDE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"import\s+['"]([^'"]+)['"]"#).unwrap());

/// Matches function/method calls: `name(`.
static RE_CALL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b([a-zA-Z_$][a-zA-Z0-9_$]*)\s*\(").unwrap());

/// Common JS/TS keywords and builtins to exclude from call detection.
const CALL_EXCLUDE: &[&str] = &[
    "if",
    "for",
    "while",
    "switch",
    "catch",
    "return",
    "import",
    "export",
    "from",
    "const",
    "let",
    "var",
    "function",
    "class",
    "new",
    "typeof",
    "instanceof",
    "void",
    "delete",
    "throw",
    "async",
    "await",
    "yield",
    "true",
    "false",
    "null",
    "undefined",
    "this",
    "super",
    "console",
    "require",
];

// ── public entry point ─────────────────────────────────────────────────────

/// Parse a Svelte Single-File Component (`.svelte`) using regex-based extraction.
///
/// Extracts the `<script>` block content and scans it for:
/// - Exported functions/constants → [`SymbolKind::Function`]
/// - Component name (from filename) → [`SymbolKind::Class`]
/// - `import` statements → [`ReferenceKind::Import`]
/// - Function calls → [`ReferenceKind::Call`]
pub fn parse_svelte(path: &Path, source: &str) -> ParsedFile {
    let path_str = path.to_string_lossy().into_owned();

    let mut symbols: Vec<RawSymbol> = Vec::new();
    let mut references: Vec<RawReference> = Vec::new();

    // Extract the component name from filename (e.g., "Counter.svelte" → "Counter")
    let component_name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Component")
        .to_string();

    // The Svelte file itself is a component — record it
    symbols.push(RawSymbol {
        name: component_name.clone(),
        kind: SymbolKind::Class,
        start_line: 1,
        end_line: 1,
        signature: format!("<svelte:component name=\"{component_name}\">"),
        content_hash: sha256_hex(&component_name),
        is_entry_point: false,
        entry_point_kind: None,
        visibility: Visibility::Public,
        type_info: None,
        parent_name: None,
    });

    // Find <script> block(s)
    let script_blocks = extract_script_blocks(source);

    for (script_content, script_start_line) in &script_blocks {
        let offset = *script_start_line;

        for (idx, line) in script_content.lines().enumerate() {
            let line_no = offset + idx as u32 + 1;
            let trimmed = line.trim();

            // Skip empty lines and comments
            if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with('*') {
                continue;
            }

            // ── symbol detection ───────────────────────────────────────

            // Named exports (export function/const/let)
            if let Some(cap) = RE_EXPORT_NAMED.captures(trimmed) {
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
            }

            // Named function declarations (non-exported)
            if !trimmed.starts_with("export")
                && let Some(cap) = RE_FUNCTION.captures(trimmed)
            {
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
                    visibility: Visibility::Private,
                    type_info: None,
                    parent_name: None,
                });
            }

            // ── reference detection ────────────────────────────────────

            // Import statements
            if let Some(cap) = RE_IMPORT.captures(trimmed) {
                references.push(RawReference {
                    name: cap[1].to_string(),
                    kind: ReferenceKind::Import,
                    start_line: line_no,
                    context: trimmed.to_string(),
                    receiver: None,
                });
            } else if let Some(cap) = RE_IMPORT_SIDE.captures(trimmed) {
                references.push(RawReference {
                    name: cap[1].to_string(),
                    kind: ReferenceKind::Import,
                    start_line: line_no,
                    context: trimmed.to_string(),
                    receiver: None,
                });
            }

            // Function calls (skip imports and keywords)
            if !trimmed.starts_with("import") {
                for cap in RE_CALL.captures_iter(trimmed) {
                    let name = cap[1].to_string();
                    if !CALL_EXCLUDE.contains(&name.as_str()) && name != component_name {
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
        }
    }

    ParsedFile {
        path: path_str,
        symbols,
        references,
    }
}

/// Extract script block content and the starting line number (0-based) of each block.
fn extract_script_blocks(source: &str) -> Vec<(String, u32)> {
    let mut blocks = Vec::new();
    let lines: Vec<&str> = source.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        if RE_SCRIPT_OPEN.is_match(lines[i]) {
            let start = i;
            i += 1;
            let mut content = String::new();
            while i < lines.len() && !RE_SCRIPT_CLOSE.is_match(lines[i]) {
                content.push_str(lines[i]);
                content.push('\n');
                i += 1;
            }
            blocks.push((content, start as u32 + 1));
            i += 1;
        } else {
            i += 1;
        }
    }

    blocks
}
