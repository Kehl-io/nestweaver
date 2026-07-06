use crate::parse::{ParsedFile, RawReference, RawSymbol, ReferenceKind, sha256_hex};
use nestweaver_schema::{SymbolKind, Visibility};
use regex::Regex;
use std::path::Path;
use std::sync::LazyLock;

// ── compiled patterns ──────────────────────────────────────────────────────

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

/// Parse an Astro component (`.astro`) using regex-based extraction.
///
/// Astro files have a frontmatter section delimited by `---` markers that
/// contains JavaScript/TypeScript. This parser extracts the frontmatter
/// and scans it for:
/// - Exported functions/constants → [`SymbolKind::Function`]
/// - Component name (from filename) → [`SymbolKind::Class`]
/// - `import` statements → [`ReferenceKind::Import`]
/// - Function calls → [`ReferenceKind::Call`]
pub fn parse_astro(path: &Path, source: &str) -> ParsedFile {
    let path_str = path.to_string_lossy().into_owned();

    let mut symbols: Vec<RawSymbol> = Vec::new();
    let mut references: Vec<RawReference> = Vec::new();

    // Extract the component name from filename (e.g., "Layout.astro" → "Layout")
    let component_name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Component")
        .to_string();

    // The Astro file itself is a component — record it
    symbols.push(RawSymbol {
        name: component_name.clone(),
        kind: SymbolKind::Class,
        start_line: 1,
        end_line: 1,
        signature: format!("<astro:component name=\"{component_name}\">"),
        content_hash: sha256_hex(&component_name),
        is_entry_point: false,
        entry_point_kind: None,
        visibility: Visibility::Public,
        type_info: None,
        parent_name: None,
        scope_chain: None,
    });

    // Extract frontmatter block (between --- markers)
    if let Some((frontmatter, frontmatter_start)) = extract_frontmatter(source) {
        for (idx, line) in frontmatter.lines().enumerate() {
            let line_no = frontmatter_start + idx as u32 + 1;
            let trimmed = line.trim();

            // Skip empty lines and comments
            if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with('*') {
                continue;
            }

            // ── symbol detection ───────────────────────────────────────

            // Named exports
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
                    scope_chain: None,
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
                    scope_chain: None,
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
        type_bindings: Vec::new(),
    }
}

/// Extract the frontmatter content (between `---` markers) and its starting
/// line number (0-based index of the first `---`).
fn extract_frontmatter(source: &str) -> Option<(String, u32)> {
    let lines: Vec<&str> = source.lines().collect();
    let mut first_fence = None;

    for (i, line) in lines.iter().enumerate() {
        if line.trim() == "---" {
            if let Some(start_idx) = first_fence {
                // Found the closing fence
                let start = start_idx + 1;
                let content: String = lines[start..i].join("\n");
                return Some((content, start as u32));
            } else {
                first_fence = Some(i);
            }
        }
    }

    None
}
