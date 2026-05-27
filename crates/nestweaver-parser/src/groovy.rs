use crate::entry_points::detect_entry_point;
use crate::parse::{ParsedFile, RawReference, RawSymbol, ReferenceKind, sha256_hex};
use nestweaver_schema::{SymbolKind, Visibility};
use regex::Regex;
use std::path::Path;
use std::sync::LazyLock;

// ── compiled patterns ──────────────────────────────────────────────────────

/// Matches class declarations: `class ClassName`, with optional modifiers.
static RE_CLASS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^\s*(?:(?:public|private|protected|abstract|final|static)\s+)*class\s+([A-Za-z_][A-Za-z0-9_]*)",
    )
    .unwrap()
});

/// Matches interface declarations.
static RE_INTERFACE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:(?:public|private|protected)\s+)*interface\s+([A-Za-z_][A-Za-z0-9_]*)")
        .unwrap()
});

/// Matches trait declarations (Groovy traits).
static RE_TRAIT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:(?:public|private|protected)\s+)*trait\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap()
});

/// Matches enum declarations.
static RE_ENUM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:(?:public|private|protected)\s+)*enum\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap()
});

/// Matches method/function definitions: `def methodName(` or `ReturnType methodName(`.
static RE_DEF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:(?:public|private|protected|static|final|abstract|synchronized|def)\s+)+([a-zA-Z_][a-zA-Z0-9_]*)\s*\(").unwrap()
});

/// Matches typed method definitions: `ReturnType methodName(`.
static RE_TYPED_METHOD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^\s*(?:(?:public|private|protected|static|final|abstract|synchronized)\s+)*(?:void|int|long|float|double|boolean|String|List|Map|Set|Object|def|[A-Z][a-zA-Z0-9_]*)\s+([a-zA-Z_][a-zA-Z0-9_]*)\s*\(",
    )
    .unwrap()
});

/// Matches `import package.Class`.
static RE_IMPORT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*import\s+([\w.]+)").unwrap());

/// Matches `extends ClassName`.
static RE_EXTENDS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bextends\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap());

/// Matches `implements InterfaceName`.
static RE_IMPLEMENTS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bimplements\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap());

/// Matches function calls: `name(`.
static RE_CALL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b([a-zA-Z_][a-zA-Z0-9_]*)\s*\(").unwrap());

// ── public entry point ─────────────────────────────────────────────────────

/// Parse a Groovy source file using regex-based line scanning.
pub fn parse_groovy(path: &Path, source: &str) -> ParsedFile {
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
        if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with("*") {
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
            let visibility = infer_groovy_visibility(trimmed);
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
                visibility,
                type_info: None,
            });

            // Check for extends/implements on the same line
            if let Some(ext_cap) = RE_EXTENDS.captures(line) {
                references.push(RawReference {
                    name: ext_cap[1].to_string(),
                    kind: ReferenceKind::Extends,
                    start_line: line_no,
                    context: trimmed.to_string(),
                });
            }
            if let Some(impl_cap) = RE_IMPLEMENTS.captures(line) {
                references.push(RawReference {
                    name: impl_cap[1].to_string(),
                    kind: ReferenceKind::Implements,
                    start_line: line_no,
                    context: trimmed.to_string(),
                });
            }
            continue;
        }

        if let Some(cap) = RE_INTERFACE.captures(line) {
            let name = cap[1].to_string();
            let visibility = infer_groovy_visibility(trimmed);
            in_class = true;
            class_brace_depth = brace_depth;
            symbols.push(RawSymbol {
                name,
                kind: SymbolKind::Interface,
                start_line: line_no,
                signature: trimmed.to_string(),
                content_hash: sha256_hex(trimmed),
                is_entry_point: false,
                entry_point_kind: None,
                visibility,
                type_info: None,
            });
            continue;
        }

        if let Some(cap) = RE_TRAIT.captures(line) {
            let name = cap[1].to_string();
            let visibility = infer_groovy_visibility(trimmed);
            in_class = true;
            class_brace_depth = brace_depth;
            symbols.push(RawSymbol {
                name,
                kind: SymbolKind::Trait,
                start_line: line_no,
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
            let name = cap[1].to_string();
            let visibility = infer_groovy_visibility(trimmed);
            symbols.push(RawSymbol {
                name,
                kind: SymbolKind::Class,
                start_line: line_no,
                signature: trimmed.to_string(),
                content_hash: sha256_hex(trimmed),
                is_entry_point: false,
                entry_point_kind: None,
                visibility,
                type_info: None,
            });
            continue;
        }

        // Method/function definitions
        let method_name = RE_DEF
            .captures(line)
            .or_else(|| RE_TYPED_METHOD.captures(line));
        if let Some(cap) = method_name {
            let name = cap[1].to_string();
            // Skip class/interface/trait/enum keywords that might be falsely matched
            if matches!(
                name.as_str(),
                "class" | "interface" | "trait" | "enum" | "if" | "for" | "while" | "switch"
            ) {
                continue;
            }
            let kind = if in_class {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            };
            let visibility = infer_groovy_visibility(trimmed);
            let kind_label = if in_class { "method" } else { "function" };
            let ep_kind =
                detect_entry_point(&name, &file_path_str, kind_label, Some(trimmed), "groovy");
            symbols.push(RawSymbol {
                name,
                kind,
                start_line: line_no,
                signature: trimmed.to_string(),
                content_hash: sha256_hex(trimmed),
                is_entry_point: ep_kind.is_some(),
                entry_point_kind: ep_kind,
                visibility,
                type_info: None,
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
            });
            continue;
        }

        // Function calls (on non-definition lines)
        for cap in RE_CALL.captures_iter(line) {
            let name = &cap[1];
            if matches!(
                name,
                "if" | "for"
                    | "while"
                    | "switch"
                    | "return"
                    | "class"
                    | "def"
                    | "new"
                    | "import"
                    | "package"
                    | "catch"
                    | "try"
                    | "throw"
            ) {
                continue;
            }
            references.push(RawReference {
                name: name.to_string(),
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

fn infer_groovy_visibility(sig: &str) -> Visibility {
    if sig.contains("public ") {
        Visibility::Public
    } else if sig.contains("private ") {
        Visibility::Private
    } else if sig.contains("protected ") {
        Visibility::Protected
    } else {
        // Groovy default is public
        Visibility::Public
    }
}
