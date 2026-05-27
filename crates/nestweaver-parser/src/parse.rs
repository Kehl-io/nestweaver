use crate::entry_points::detect_entry_point;
use crate::language::detect_language;
use nestweaver_schema::{EntryPointKind, Language, SymbolKind, TypeInfo, Visibility};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;
use thiserror::Error;
use tree_sitter::{Query, QueryCursor, StreamingIterator};

// ── query source embedded at compile time ──────────────────────────────────

const JS_QUERY: &str = include_str!("../../../queries/javascript.scm");
const TS_QUERY: &str = include_str!("../../../queries/typescript.scm");
const JAVA_QUERY: &str = include_str!("../../../queries/java.scm");
const GO_QUERY: &str = include_str!("../../../queries/go.scm");
const PY_QUERY: &str = include_str!("../../../queries/python.scm");
const CPP_QUERY: &str = include_str!("../../../queries/cpp.scm");
const RUST_QUERY: &str = include_str!("../../../queries/rust.scm");
const C_QUERY: &str = include_str!("../../../queries/c.scm");
const CSHARP_QUERY: &str = include_str!("../../../queries/csharp.scm");
const KOTLIN_QUERY: &str = include_str!("../../../queries/kotlin.scm");
const PHP_QUERY: &str = include_str!("../../../queries/php.scm");
const RUBY_QUERY: &str = include_str!("../../../queries/ruby.scm");
const DART_QUERY: &str = include_str!("../../../queries/dart.scm");
const SWIFT_QUERY: &str = include_str!("../../../queries/swift.scm");
const LUA_QUERY: &str = include_str!("../../../queries/lua.scm");
const BASH_QUERY: &str = include_str!("../../../queries/bash.scm");
const SCALA_QUERY: &str = include_str!("../../../queries/scala.scm");
const ELIXIR_QUERY: &str = include_str!("../../../queries/elixir.scm");

// ── error ──────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("unsupported language for path: {0}")]
    UnsupportedLanguage(String),
    #[error("tree-sitter query error: {0}")]
    QueryError(#[from] tree_sitter::QueryError),
    #[error("tree-sitter failed to parse")]
    ParseFailed,
}

// ── output types ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReferenceKind {
    Call,
    Import,
    Extends,
    Implements,
    Includes,
    Uses,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawSymbol {
    pub name: String,
    pub kind: SymbolKind,
    pub start_line: u32,
    pub signature: String,
    pub content_hash: String,
    pub is_entry_point: bool,
    pub entry_point_kind: Option<EntryPointKind>,
    pub visibility: Visibility,
    pub type_info: Option<TypeInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawReference {
    pub name: String,
    pub kind: ReferenceKind,
    pub start_line: u32,
    pub context: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedFile {
    pub path: String,
    pub symbols: Vec<RawSymbol>,
    pub references: Vec<RawReference>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedFile {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParseResult {
    pub files: Vec<ParsedFile>,
    pub skipped: Vec<SkippedFile>,
}

// ── helpers ────────────────────────────────────────────────────────────────

pub(crate) fn sha256_hex(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hex::encode(hasher.finalize())
}

/// Extract the first line of a node's text as its signature.
fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or("").trim().to_string()
}

/// Infer symbol visibility from name and surrounding source text based on language conventions.
fn infer_visibility(name: &str, node_text: &str, lang: Language) -> Visibility {
    match lang {
        // Go: capitalized = public, lowercase = private
        Language::Go => {
            if name.chars().next().is_some_and(|c| c.is_uppercase()) {
                Visibility::Public
            } else {
                Visibility::Private
            }
        }
        // Python: underscore prefix = private
        Language::Python => {
            if name.starts_with('_') {
                Visibility::Private
            } else {
                Visibility::Inferred
            }
        }
        // Dart: underscore prefix = private
        Language::Dart => {
            if name.starts_with('_') {
                Visibility::Private
            } else {
                Visibility::Public
            }
        }
        // Rust: pub keyword = public
        Language::Rust => {
            let sig = first_line(node_text);
            if sig.starts_with("pub ") || sig.starts_with("pub(") {
                Visibility::Public
            } else {
                Visibility::Private
            }
        }
        // JavaScript/TypeScript: export keyword = public
        Language::JavaScript | Language::TypeScript => {
            let sig = first_line(node_text);
            if sig.contains("export ") {
                Visibility::Public
            } else {
                Visibility::Private
            }
        }
        // Java, Kotlin, C#, PHP, Swift: check for visibility keywords in signature
        Language::Java | Language::Kotlin | Language::CSharp | Language::Php | Language::Swift => {
            let sig = first_line(node_text);
            if sig.contains("public ") || sig.contains("open ") {
                Visibility::Public
            } else if sig.contains("private ") || sig.contains("fileprivate ") {
                Visibility::Private
            } else if sig.contains("protected ") {
                Visibility::Protected
            } else if sig.contains("internal ") {
                Visibility::Internal
            } else {
                Visibility::Inferred
            }
        }
        // C/C++: static = private, else public
        Language::C | Language::Cpp => {
            let sig = first_line(node_text);
            if sig.starts_with("static ") || sig.contains(" static ") {
                Visibility::Private
            } else {
                Visibility::Public
            }
        }
        // Lua: local keyword = private, else global/public
        Language::Lua => {
            let sig = first_line(node_text);
            if sig.starts_with("local ") {
                Visibility::Private
            } else {
                Visibility::Public
            }
        }
        // Bash: all functions have inferred visibility
        Language::Bash => Visibility::Inferred,
        // Scala: check for visibility keywords
        Language::Scala => {
            let sig = first_line(node_text);
            if sig.contains("private ") || sig.contains("private[") {
                Visibility::Private
            } else if sig.contains("protected ") || sig.contains("protected[") {
                Visibility::Protected
            } else {
                Visibility::Public
            }
        }
        // Elixir: defp = private, def = public
        Language::Elixir => {
            let sig = first_line(node_text);
            if sig.contains("defp ") || sig.contains("defmacrop ") {
                Visibility::Private
            } else {
                Visibility::Public
            }
        }
        // Ruby, Cobol: inferred (visibility detection is complex, defer)
        Language::Ruby | Language::Cobol => Visibility::Inferred,
    }
}

fn build_ts_language(lang: Language) -> tree_sitter::Language {
    match lang {
        Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Language::Java => tree_sitter_java::LANGUAGE.into(),
        Language::Go => tree_sitter_go::LANGUAGE.into(),
        Language::Python => tree_sitter_python::LANGUAGE.into(),
        Language::Cpp => tree_sitter_cpp::LANGUAGE.into(),
        Language::Rust => tree_sitter_rust::LANGUAGE.into(),
        Language::C => tree_sitter_c::LANGUAGE.into(),
        Language::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
        Language::Kotlin => tree_sitter_kotlin::LANGUAGE.into(),
        Language::Php => tree_sitter_php::LANGUAGE_PHP_ONLY.into(),
        Language::Ruby => tree_sitter_ruby::LANGUAGE.into(),
        Language::Dart => tree_sitter_dart::LANGUAGE.into(),
        Language::Swift => tree_sitter_swift::LANGUAGE.into(),
        Language::Lua => tree_sitter_lua::LANGUAGE.into(),
        Language::Bash => tree_sitter_bash::LANGUAGE.into(),
        Language::Scala => tree_sitter_scala::LANGUAGE.into(),
        Language::Elixir => tree_sitter_elixir::LANGUAGE.into(),
        Language::Cobol => unreachable!("COBOL is handled before reaching tree-sitter"),
    }
}

fn query_source(lang: Language) -> &'static str {
    match lang {
        Language::JavaScript => JS_QUERY,
        Language::TypeScript => TS_QUERY,
        Language::Java => JAVA_QUERY,
        Language::Go => GO_QUERY,
        Language::Python => PY_QUERY,
        Language::Cpp => CPP_QUERY,
        Language::Rust => RUST_QUERY,
        Language::C => C_QUERY,
        Language::CSharp => CSHARP_QUERY,
        Language::Kotlin => KOTLIN_QUERY,
        Language::Php => PHP_QUERY,
        Language::Ruby => RUBY_QUERY,
        Language::Dart => DART_QUERY,
        Language::Swift => SWIFT_QUERY,
        Language::Lua => LUA_QUERY,
        Language::Bash => BASH_QUERY,
        Language::Scala => SCALA_QUERY,
        Language::Elixir => ELIXIR_QUERY,
        Language::Cobol => unreachable!("COBOL is handled before reaching tree-sitter"),
    }
}

// ── type info extraction ───────────────────────────────────────────────────

fn extract_java_style_type_info(sig: &str) -> Option<TypeInfo> {
    // "public String greet(String name)" or "int main(int argc, char** argv)"
    // Find the word before the opening paren that comes after any modifiers
    let paren_pos = sig.find('(')?;
    let before_paren = sig[..paren_pos].trim();
    let parts: Vec<&str> = before_paren.split_whitespace().collect();
    if parts.len() >= 2 {
        let return_type = parts[parts.len() - 2].to_string();
        // Skip common non-type words
        if [
            "static",
            "void",
            "public",
            "private",
            "protected",
            "abstract",
            "final",
            "override",
            "virtual",
        ]
        .contains(&return_type.as_str())
        {
            if return_type == "void" {
                return Some(TypeInfo {
                    declared_type: None,
                    parameter_types: Vec::new(),
                    return_type: Some("void".to_string()),
                });
            }
            return None;
        }
        return Some(TypeInfo {
            declared_type: None,
            parameter_types: Vec::new(),
            return_type: Some(return_type),
        });
    }
    None
}

fn extract_rust_type_info(sig: &str) -> Option<TypeInfo> {
    if let Some(arrow_pos) = sig.find("->") {
        let return_type = sig[arrow_pos + 2..].trim();
        let return_type = return_type.trim_end_matches('{').trim();
        if !return_type.is_empty() {
            return Some(TypeInfo {
                declared_type: None,
                parameter_types: Vec::new(),
                return_type: Some(return_type.to_string()),
            });
        }
    }
    None
}

fn extract_go_type_info(sig: &str) -> Option<TypeInfo> {
    // "func greet(name string) string {"
    if let Some(close_paren) = sig.rfind(')') {
        let after = sig[close_paren + 1..].trim();
        let return_type = after.trim_end_matches('{').trim();
        if !return_type.is_empty() && !return_type.starts_with('{') {
            return Some(TypeInfo {
                declared_type: None,
                parameter_types: Vec::new(),
                return_type: Some(return_type.to_string()),
            });
        }
    }
    None
}

fn extract_annotated_type_info(sig: &str) -> Option<TypeInfo> {
    // TypeScript: "greet(name: string): string"
    // Kotlin: "fun greet(name: String): String"
    // Swift: "func greet(name: String) -> String"
    // Dart: "String greet(String name)"
    if let Some(arrow_pos) = sig.find("->") {
        let return_type = sig[arrow_pos + 2..].trim();
        let return_type = return_type.trim_end_matches('{').trim();
        if !return_type.is_empty() {
            return Some(TypeInfo {
                declared_type: None,
                parameter_types: Vec::new(),
                return_type: Some(return_type.to_string()),
            });
        }
    }
    // Check for ): Type pattern (TypeScript/Kotlin)
    if let Some(close_paren) = sig.rfind(')') {
        let after = sig[close_paren + 1..].trim();
        if let Some(rest) = after.strip_prefix(':') {
            let return_type = rest.trim().trim_end_matches('{').trim();
            if !return_type.is_empty() {
                return Some(TypeInfo {
                    declared_type: None,
                    parameter_types: Vec::new(),
                    return_type: Some(return_type.to_string()),
                });
            }
        }
    }
    None
}

fn extract_python_type_info(sig: &str) -> Option<TypeInfo> {
    // "def greet(name: str) -> str:"
    if let Some(arrow_pos) = sig.find("->") {
        let return_type = sig[arrow_pos + 2..].trim();
        let return_type = return_type.trim_end_matches(':').trim();
        if !return_type.is_empty() {
            return Some(TypeInfo {
                declared_type: None,
                parameter_types: Vec::new(),
                return_type: Some(return_type.to_string()),
            });
        }
    }
    None
}

fn extract_type_info(signature: &str, lang: Language) -> Option<TypeInfo> {
    match lang {
        // Java/C#: return type is the word before the method name
        // e.g., "public String greet(String name)" → return_type = "String"
        Language::Java | Language::CSharp => extract_java_style_type_info(signature),
        // Go: return type is after the closing paren
        // e.g., "func greet(name string) string" → return_type = "string"
        Language::Go => extract_go_type_info(signature),
        // Rust: return type after ->
        // e.g., "fn greet(name: &str) -> String" → return_type = "String"
        Language::Rust => extract_rust_type_info(signature),
        // TypeScript/Dart/Swift/Kotlin: return type after : or ->
        Language::TypeScript | Language::Dart | Language::Swift | Language::Kotlin => {
            extract_annotated_type_info(signature)
        }
        // Python: return type after ->
        Language::Python => extract_python_type_info(signature),
        _ => None,
    }
}

// ── core parse ─────────────────────────────────────────────────────────────

/// Parse a single source file and extract symbols and references.
pub fn parse_source(path: &Path, source: &str) -> Result<ParsedFile, ParseError> {
    let lang = detect_language(path)
        .ok_or_else(|| ParseError::UnsupportedLanguage(path.to_string_lossy().into_owned()))?;

    if lang == Language::Cobol {
        return Ok(crate::cobol::parse_cobol(path, source));
    }

    let ts_lang = build_ts_language(lang);

    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&ts_lang)
        .map_err(|_| ParseError::ParseFailed)?;

    let tree = parser.parse(source, None).ok_or(ParseError::ParseFailed)?;

    let query = Query::new(&ts_lang, query_source(lang))?;
    let capture_names: Vec<String> = query
        .capture_names()
        .iter()
        .map(|s| s.to_string())
        .collect();

    let lang_str = match lang {
        Language::JavaScript => "javascript",
        Language::TypeScript => "typescript",
        Language::Java => "java",
        Language::Go => "go",
        Language::Python => "python",
        Language::Cpp => "cpp",
        Language::Rust => "rust",
        Language::C => "c",
        Language::CSharp => "csharp",
        Language::Kotlin => "kotlin",
        Language::Php => "php",
        Language::Ruby => "ruby",
        Language::Dart => "dart",
        Language::Swift => "swift",
        Language::Lua => "lua",
        Language::Bash => "bash",
        Language::Scala => "scala",
        Language::Elixir => "elixir",
        Language::Cobol => unreachable!("COBOL is handled before reaching tree-sitter"),
    };
    let file_path_str = path.to_string_lossy();

    let mut symbols: Vec<RawSymbol> = Vec::new();
    let mut references: Vec<RawReference> = Vec::new();
    let mut seen_symbols: std::collections::HashSet<(String, u32)> =
        std::collections::HashSet::new();

    let mut cursor = QueryCursor::new();
    let source_bytes = source.as_bytes();
    let mut matches = cursor.matches(&query, tree.root_node(), source_bytes);

    while let Some(m) = matches.next() {
        let name_text = find_name_capture(m.captures, &capture_names, source_bytes);

        for capture in m.captures {
            let capture_name = &capture_names[capture.index as usize];
            let node = capture.node;

            let node_text = node.utf8_text(source_bytes).unwrap_or("").to_string();
            let start_line = node.start_position().row as u32 + 1;

            if let Some(kind_str) = capture_name.strip_prefix("definition.") {
                let kind = match kind_str {
                    "function" => SymbolKind::Function,
                    "class" => SymbolKind::Class,
                    "method" => SymbolKind::Method,
                    "interface" => SymbolKind::Interface,
                    "trait" => SymbolKind::Trait,
                    "module" => SymbolKind::Module,
                    _ => continue,
                };

                let name = name_text.clone().unwrap_or_else(|| node_text.clone());

                if !seen_symbols.insert((name.clone(), start_line)) {
                    continue;
                }

                let content_hash = sha256_hex(&node_text);
                let signature = first_line(&node_text);

                let kind_label = match kind {
                    SymbolKind::Function => "function",
                    SymbolKind::Class => "class",
                    SymbolKind::Method => "method",
                    SymbolKind::Interface => "interface",
                    SymbolKind::Trait => "trait",
                    SymbolKind::Enum => "enum",
                    SymbolKind::Module => "module",
                    SymbolKind::Extension => "extension",
                };
                let ep_kind = detect_entry_point(
                    &name,
                    &file_path_str,
                    kind_label,
                    Some(&signature),
                    lang_str,
                );

                let visibility = infer_visibility(&name, &node_text, lang);
                let type_info = extract_type_info(&signature, lang);
                symbols.push(RawSymbol {
                    name,
                    kind,
                    start_line,
                    signature,
                    content_hash,
                    is_entry_point: ep_kind.is_some(),
                    entry_point_kind: ep_kind,
                    visibility,
                    type_info,
                });
            } else if let Some(kind_str) = capture_name.strip_prefix("reference.") {
                let kind = match kind_str {
                    "call" => ReferenceKind::Call,
                    "import" => ReferenceKind::Import,
                    "extends" => ReferenceKind::Extends,
                    "implements" => ReferenceKind::Implements,
                    "includes" => ReferenceKind::Includes,
                    "uses" => ReferenceKind::Uses,
                    _ => continue,
                };

                let context = node
                    .parent()
                    .map(|p| {
                        let parent_text = p.utf8_text(source_bytes).unwrap_or("");
                        first_line(parent_text)
                    })
                    .unwrap_or_default();

                let name = name_text
                    .clone()
                    .unwrap_or_else(|| strip_quotes(&node_text));

                references.push(RawReference {
                    name,
                    kind,
                    start_line,
                    context,
                });
            }
            // Skip "name" captures — used via find_name_capture above
        }
    }

    Ok(ParsedFile {
        path: path.to_string_lossy().into_owned(),
        symbols,
        references,
    })
}

/// Find the value of a `@name` capture within the same query match.
fn find_name_capture(
    captures: &[tree_sitter::QueryCapture<'_>],
    capture_names: &[String],
    source_bytes: &[u8],
) -> Option<String> {
    for c in captures {
        if capture_names[c.index as usize] == "name" {
            let text = c.node.utf8_text(source_bytes).unwrap_or("").to_string();
            return Some(strip_quotes(&text));
        }
    }
    None
}

/// Remove surrounding quotes from string literals.
fn strip_quotes(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"'))
            || (s.starts_with('\'') && s.ends_with('\''))
            || (s.starts_with('`') && s.ends_with('`')))
    {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// Parse a batch of (path, source) pairs, logging and skipping failures.
pub fn parse_batch(files: &[(&Path, &str)]) -> ParseResult {
    let mut result = ParseResult {
        files: Vec::new(),
        skipped: Vec::new(),
    };

    for (path, source) in files {
        match parse_source(path, source) {
            Ok(parsed) => result.files.push(parsed),
            Err(e) => {
                let path_str = path.to_string_lossy().into_owned();
                tracing::warn!(path = %path_str, error = %e, "skipping file due to parse error");
                result.skipped.push(SkippedFile {
                    path: path_str,
                    reason: e.to_string(),
                });
            }
        }
    }

    result
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn fixture(rel: &str) -> String {
        let workspace = env!("CARGO_MANIFEST_DIR");
        // CARGO_MANIFEST_DIR is crates/nestweaver-parser
        // testdata is at workspace root
        let root = Path::new(workspace).join("../..").join("testdata");
        let path = root.join(rel);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("failed to read fixture {rel}: {e}"))
    }

    // ── JS tests ────────────────────────────────────────────────────────────

    #[test]
    fn parse_js_extracts_function() {
        let source = fixture("js/simple.js");
        let parsed = parse_source(Path::new("simple.js"), &source).unwrap();
        let functions: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Function && s.name == "greet")
            .collect();
        assert!(!functions.is_empty(), "should find function 'greet'");
        assert_eq!(functions[0].start_line, 5);
    }

    #[test]
    fn parse_js_extracts_class_and_methods() {
        let source = fixture("js/simple.js");
        let parsed = parse_source(Path::new("simple.js"), &source).unwrap();

        let classes: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Class)
            .collect();
        assert!(
            classes.iter().any(|s| s.name == "Animal"),
            "should find class 'Animal'"
        );
        assert!(
            classes.iter().any(|s| s.name == "Dog"),
            "should find class 'Dog'"
        );

        let methods: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Method)
            .collect();
        assert!(
            methods.iter().any(|s| s.name == "speak"),
            "should find method 'speak'"
        );
    }

    #[test]
    fn parse_js_extracts_call_references() {
        let source = fixture("js/simple.js");
        let parsed = parse_source(Path::new("simple.js"), &source).unwrap();

        let calls: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Call)
            .collect();
        assert!(!calls.is_empty(), "should find call references");
        assert!(
            calls.iter().any(|r| r.name == "greet"),
            "should find call to 'greet'; found: {:?}",
            calls.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
    }

    // ── TS tests ────────────────────────────────────────────────────────────

    #[test]
    fn parse_ts_extracts_interface() {
        let source = fixture("ts/simple.ts");
        let parsed = parse_source(Path::new("simple.ts"), &source).unwrap();

        let interfaces: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Interface)
            .collect();
        assert!(
            interfaces.iter().any(|s| s.name == "Greeter"),
            "should find interface 'Greeter'; found: {:?}",
            interfaces.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_ts_extracts_implements_reference() {
        let source = fixture("ts/simple.ts");
        let parsed = parse_source(Path::new("simple.ts"), &source).unwrap();

        let impls: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Implements)
            .collect();
        assert!(
            !impls.is_empty(),
            "should find implements references; all refs: {:?}",
            parsed
                .references
                .iter()
                .map(|r| (&r.name, r.kind))
                .collect::<Vec<_>>()
        );
    }

    // ── Java tests ─────────────────────────────────────────────────────────

    #[test]
    fn parse_java_extracts_class_and_interface() {
        let source = fixture("java/Simple.java");
        let parsed = parse_source(Path::new("Simple.java"), &source).unwrap();

        let interfaces: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Interface)
            .collect();
        assert!(
            interfaces.iter().any(|s| s.name == "Greeter"),
            "should find interface 'Greeter'"
        );

        let classes: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Class)
            .collect();
        assert!(
            classes.iter().any(|s| s.name == "SimpleGreeter"),
            "should find class 'SimpleGreeter'"
        );
    }

    // ── Go tests ───────────────────────────────────────────────────────────

    #[test]
    fn parse_go_extracts_interface_and_struct() {
        let source = fixture("go/simple.go");
        let parsed = parse_source(Path::new("simple.go"), &source).unwrap();

        let interfaces: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Interface)
            .collect();
        assert!(
            interfaces.iter().any(|s| s.name == "Greeter"),
            "should find interface 'Greeter'; found: {:?}",
            interfaces.iter().map(|s| &s.name).collect::<Vec<_>>()
        );

        let classes: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Class)
            .collect();
        assert!(
            classes.iter().any(|s| s.name == "ConsoleGreeter"),
            "should find struct 'ConsoleGreeter' as class; found: {:?}",
            classes.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    // ── Python tests ───────────────────────────────────────────────────────

    #[test]
    fn parse_python_extracts_class_and_function() {
        let source = fixture("python/simple.py");
        let parsed = parse_source(Path::new("simple.py"), &source).unwrap();

        let classes: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Class)
            .collect();
        assert!(
            classes.iter().any(|s| s.name == "Animal"),
            "should find class 'Animal'"
        );
        assert!(
            classes.iter().any(|s| s.name == "Dog"),
            "should find class 'Dog'"
        );

        let functions: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Function)
            .collect();
        assert!(
            functions.iter().any(|s| s.name == "standalone_function"),
            "should find function 'standalone_function'"
        );
    }

    // ── Hash test ──────────────────────────────────────────────────────────

    #[test]
    fn content_hash_is_sha256() {
        let source = r#"function hello() { return 42; }"#;
        let parsed = parse_source(Path::new("test.js"), source).unwrap();
        assert!(
            !parsed.symbols.is_empty(),
            "should parse at least one symbol"
        );
        let hash = &parsed.symbols[0].content_hash;
        assert_eq!(hash.len(), 64, "SHA-256 hex is 64 chars; got: {hash}");
        assert!(
            hash.chars().all(|c| c.is_ascii_hexdigit()),
            "hash should be hex; got: {hash}"
        );
    }

    // ── C++ tests ────────────────────────────────────────────────────────

    #[test]
    fn parse_cpp_extracts_class_and_methods() {
        let source = fixture("cpp/simple.cpp");
        let parsed = parse_source(Path::new("simple.cpp"), &source).unwrap();

        let classes: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Class)
            .collect();
        assert!(
            classes.iter().any(|s| s.name == "SensorManager"),
            "should find class SensorManager; got: {:?}",
            classes.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert!(
            classes.iter().any(|s| s.name == "SensorConfig"),
            "should find struct SensorConfig as class"
        );
        assert!(
            classes.iter().any(|s| s.name == "SensorType"),
            "should find enum SensorType as class"
        );

        let methods: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Method)
            .collect();
        assert!(
            methods.iter().any(|s| s.name == "initialize"),
            "should find method initialize; got: {:?}",
            methods.iter().map(|s| &s.name).collect::<Vec<_>>()
        );

        let functions: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Function)
            .collect();
        assert!(
            functions.iter().any(|s| s.name == "setup"),
            "should find function setup; got: {:?}",
            functions.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_cpp_extracts_references() {
        let source = fixture("cpp/simple.cpp");
        let parsed = parse_source(Path::new("simple.cpp"), &source).unwrap();

        let imports: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Import)
            .collect();
        assert!(
            imports.iter().any(|r| r.name.contains("sensor.h")),
            "should find #include sensor.h; got: {:?}",
            imports.iter().map(|r| &r.name).collect::<Vec<_>>()
        );

        let calls: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Call)
            .collect();
        assert!(
            calls
                .iter()
                .any(|r| r.name == "calibrate" || r.name == "logValue"),
            "should find function calls; got: {:?}",
            calls.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
    }

    // ── Rust tests ──────────────────────────────────────────────────────

    #[test]
    fn parse_rust_extracts_struct_enum_trait() {
        let source = fixture("rust/simple.rs");
        let parsed = parse_source(Path::new("simple.rs"), &source).unwrap();

        let classes: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Class)
            .collect();
        assert!(
            classes.iter().any(|s| s.name == "SensorManager"),
            "should find struct SensorManager as class; got: {:?}",
            classes.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert!(
            classes.iter().any(|s| s.name == "SensorKind"),
            "should find enum SensorKind as class"
        );

        let interfaces: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Interface)
            .collect();
        assert!(
            interfaces.iter().any(|s| s.name == "Readable"),
            "should find trait Readable as interface; got: {:?}",
            interfaces.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_rust_extracts_functions_and_methods() {
        let source = fixture("rust/simple.rs");
        let parsed = parse_source(Path::new("simple.rs"), &source).unwrap();

        let functions: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Function)
            .collect();
        assert!(
            functions.iter().any(|s| s.name == "initialize"),
            "should find free function initialize; got: {:?}",
            functions.iter().map(|s| &s.name).collect::<Vec<_>>()
        );

        let methods: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Method)
            .collect();
        assert!(
            methods.iter().any(|s| s.name == "read"),
            "should find method read; got: {:?}",
            methods.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert!(
            methods.iter().any(|s| s.name == "new"),
            "should find method new; got: {:?}",
            methods.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_rust_extracts_references() {
        let source = fixture("rust/simple.rs");
        let parsed = parse_source(Path::new("simple.rs"), &source).unwrap();

        let imports: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Import)
            .collect();
        assert!(
            imports.iter().any(|r| r.name.contains("HashMap")),
            "should find use std::collections::HashMap; got: {:?}",
            imports.iter().map(|r| &r.name).collect::<Vec<_>>()
        );

        let extends: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Extends)
            .collect();
        assert!(
            extends.iter().any(|r| r.name == "Readable"),
            "should find impl Readable as extends; got: {:?}",
            extends.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_rust_extracts_return_type() {
        let source = fixture("rust/simple.rs");
        let parsed = parse_source(Path::new("simple.rs"), &source).unwrap();

        // Find a function with a return type
        let symbols_with_types: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.type_info.is_some())
            .collect();
        // At least some symbols should have type info extracted
        assert!(
            !symbols_with_types.is_empty(),
            "should extract type info from at least one Rust symbol"
        );
    }

    // ── C tests ─────────────────────────────────────────────────────────────

    #[test]
    fn parse_c_extracts_function_and_struct() {
        let source = fixture("c/simple.c");
        let parsed = parse_source(Path::new("simple.c"), &source).unwrap();

        let functions: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Function)
            .collect();
        assert!(
            functions.iter().any(|s| s.name == "initialize"),
            "should find function 'initialize'; got: {:?}",
            functions.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert!(
            functions.iter().any(|s| s.name == "main"),
            "should find function 'main'"
        );

        let classes: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Class)
            .collect();
        assert!(
            classes.iter().any(|s| s.name == "SensorManager"),
            "should find struct SensorManager; got: {:?}",
            classes.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_c_extracts_references() {
        let source = fixture("c/simple.c");
        let parsed = parse_source(Path::new("simple.c"), &source).unwrap();

        let calls: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Call)
            .collect();
        assert!(
            calls.iter().any(|r| r.name == "initialize"),
            "should find call to initialize; got: {:?}",
            calls.iter().map(|r| &r.name).collect::<Vec<_>>()
        );

        let includes: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Includes)
            .collect();
        assert!(
            includes.iter().any(|r| r.name.contains("sensor.h")),
            "should find #include sensor.h; got: {:?}",
            includes.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
    }

    // ── C# tests ────────────────────────────────────────────────────────

    #[test]
    fn parse_csharp_extracts_class_and_interface() {
        let source = fixture("csharp/Simple.cs");
        let parsed = parse_source(Path::new("Simple.cs"), &source).unwrap();

        let interfaces: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Interface)
            .collect();
        assert!(
            interfaces.iter().any(|s| s.name == "IGreeter"),
            "should find interface 'IGreeter'; got: {:?}",
            interfaces.iter().map(|s| &s.name).collect::<Vec<_>>()
        );

        let classes: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Class)
            .collect();
        assert!(
            classes.iter().any(|s| s.name == "SimpleGreeter"),
            "should find class 'SimpleGreeter'; got: {:?}",
            classes.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_csharp_extracts_references() {
        let source = fixture("csharp/Simple.cs");
        let parsed = parse_source(Path::new("Simple.cs"), &source).unwrap();

        let uses: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Uses)
            .collect();
        assert!(
            !uses.is_empty(),
            "should find using references; all refs: {:?}",
            parsed
                .references
                .iter()
                .map(|r| (&r.name, r.kind))
                .collect::<Vec<_>>()
        );

        let extends: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Extends)
            .collect();
        assert!(
            extends.iter().any(|r| r.name == "IGreeter"),
            "should find extends IGreeter; got: {:?}",
            extends.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
    }

    // ── Kotlin tests ─────────────────────────────────────────────────────

    #[test]
    fn parse_kotlin_extracts_class_and_function() {
        let source = fixture("kotlin/Simple.kt");
        let parsed = parse_source(Path::new("Simple.kt"), &source).unwrap();

        let classes: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Class)
            .collect();
        assert!(
            classes.iter().any(|s| s.name == "SimpleGreeter"),
            "should find class 'SimpleGreeter'; got: {:?}",
            classes.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert!(
            classes.iter().any(|s| s.name == "AppConfig"),
            "should find object 'AppConfig' as class; got: {:?}",
            classes.iter().map(|s| &s.name).collect::<Vec<_>>()
        );

        let functions: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Function)
            .collect();
        assert!(
            functions.iter().any(|s| s.name == "main"),
            "should find top-level function 'main'; got: {:?}",
            functions.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_kotlin_extracts_methods() {
        let source = fixture("kotlin/Simple.kt");
        let parsed = parse_source(Path::new("Simple.kt"), &source).unwrap();

        let methods: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Method)
            .collect();
        assert!(
            methods.iter().any(|s| s.name == "greet"),
            "should find method 'greet'; got: {:?}",
            methods.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert!(
            methods.iter().any(|s| s.name == "logGreeting"),
            "should find method 'logGreeting'; got: {:?}",
            methods.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_kotlin_extracts_references() {
        let source = fixture("kotlin/Simple.kt");
        let parsed = parse_source(Path::new("Simple.kt"), &source).unwrap();

        let imports: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Import)
            .collect();
        assert!(
            !imports.is_empty(),
            "should find import references; all refs: {:?}",
            parsed
                .references
                .iter()
                .map(|r| (&r.name, r.kind))
                .collect::<Vec<_>>()
        );

        let extends: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Extends)
            .collect();
        assert!(
            extends.iter().any(|r| r.name == "Greeter"),
            "should find extends 'Greeter'; got: {:?}",
            extends.iter().map(|r| &r.name).collect::<Vec<_>>()
        );

        let calls: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Call)
            .collect();
        assert!(
            !calls.is_empty(),
            "should find call references; all refs: {:?}",
            parsed
                .references
                .iter()
                .map(|r| (&r.name, r.kind))
                .collect::<Vec<_>>()
        );
    }

    // ── PHP tests ────────────────────────────────────────────────────────

    #[test]
    fn parse_php_extracts_class_and_interface() {
        let source = fixture("php/simple.php");
        let parsed = parse_source(Path::new("simple.php"), &source).unwrap();

        let classes: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Class)
            .collect();
        assert!(
            classes.iter().any(|s| s.name == "SimpleGreeter"),
            "should find class 'SimpleGreeter'; got: {:?}",
            classes.iter().map(|s| &s.name).collect::<Vec<_>>()
        );

        let interfaces: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Interface)
            .collect();
        assert!(
            interfaces.iter().any(|s| s.name == "GreeterInterface"),
            "should find interface 'GreeterInterface'; got: {:?}",
            interfaces.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_php_extracts_trait() {
        let source = fixture("php/simple.php");
        let parsed = parse_source(Path::new("simple.php"), &source).unwrap();

        let traits: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Trait)
            .collect();
        assert!(
            traits.iter().any(|s| s.name == "Loggable"),
            "should find trait 'Loggable' as SymbolKind::Trait; got: {:?}",
            traits.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_php_extracts_methods() {
        let source = fixture("php/simple.php");
        let parsed = parse_source(Path::new("simple.php"), &source).unwrap();

        let methods: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Method)
            .collect();
        assert!(
            methods.iter().any(|s| s.name == "greet"),
            "should find method 'greet'; got: {:?}",
            methods.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_php_extracts_references() {
        let source = fixture("php/simple.php");
        let parsed = parse_source(Path::new("simple.php"), &source).unwrap();

        let uses: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Uses)
            .collect();
        assert!(
            !uses.is_empty(),
            "should find uses references from 'use' statements; all refs: {:?}",
            parsed
                .references
                .iter()
                .map(|r| (&r.name, r.kind))
                .collect::<Vec<_>>()
        );

        let impls: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Implements)
            .collect();
        assert!(
            impls.iter().any(|r| r.name == "GreeterInterface"),
            "should find implements 'GreeterInterface'; got: {:?}",
            impls.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
    }

    // ── Ruby tests ───────────────────────────────────────────────────────

    #[test]
    fn parse_ruby_extracts_classes() {
        let source = fixture("ruby/simple.rb");
        let parsed = parse_source(Path::new("simple.rb"), &source).unwrap();

        let classes: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Class)
            .collect();
        assert!(
            classes.iter().any(|s| s.name == "Greeter"),
            "should find class 'Greeter'; got: {:?}",
            classes.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert!(
            classes.iter().any(|s| s.name == "FormalGreeter"),
            "should find class 'FormalGreeter'; got: {:?}",
            classes.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_ruby_extracts_module() {
        let source = fixture("ruby/simple.rb");
        let parsed = parse_source(Path::new("simple.rb"), &source).unwrap();

        let modules: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Module)
            .collect();
        assert!(
            modules.iter().any(|s| s.name == "Greetings"),
            "should find module 'Greetings' as SymbolKind::Module; got: {:?}",
            modules.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_ruby_extracts_methods() {
        let source = fixture("ruby/simple.rb");
        let parsed = parse_source(Path::new("simple.rb"), &source).unwrap();

        let methods: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Method)
            .collect();
        assert!(
            methods.iter().any(|s| s.name == "greet"),
            "should find method 'greet'; got: {:?}",
            methods.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert!(
            methods.iter().any(|s| s.name == "format_name"),
            "should find method 'format_name'; got: {:?}",
            methods.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert!(
            methods.iter().any(|s| s.name == "standalone_function"),
            "should find top-level method 'standalone_function'; got: {:?}",
            methods.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_ruby_extracts_extends_reference() {
        let source = fixture("ruby/simple.rb");
        let parsed = parse_source(Path::new("simple.rb"), &source).unwrap();

        let extends: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Extends)
            .collect();
        assert!(
            extends.iter().any(|r| r.name == "Greeter"),
            "should find extends 'Greeter'; got: {:?}",
            extends.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_ruby_extracts_call_references() {
        let source = fixture("ruby/simple.rb");
        let parsed = parse_source(Path::new("simple.rb"), &source).unwrap();

        let calls: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Call)
            .collect();
        assert!(
            !calls.is_empty(),
            "should find call references; all refs: {:?}",
            parsed
                .references
                .iter()
                .map(|r| (&r.name, r.kind))
                .collect::<Vec<_>>()
        );
    }

    // ── Dart tests ───────────────────────────────────────────────────────────

    #[test]
    fn parse_dart_extracts_classes() {
        let source = fixture("dart/simple.dart");
        let parsed = parse_source(Path::new("simple.dart"), &source).unwrap();

        let classes: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Class)
            .collect();
        assert!(
            classes.iter().any(|s| s.name == "Greeter"),
            "should find abstract class 'Greeter'; got: {:?}",
            classes.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert!(
            classes.iter().any(|s| s.name == "SimpleGreeter"),
            "should find class 'SimpleGreeter'; got: {:?}",
            classes.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert!(
            classes.iter().any(|s| s.name == "Priority"),
            "should find enum 'Priority' as class; got: {:?}",
            classes.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_dart_extracts_mixin_as_trait() {
        let source = fixture("dart/simple.dart");
        let parsed = parse_source(Path::new("simple.dart"), &source).unwrap();

        let traits: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Trait)
            .collect();
        assert!(
            traits.iter().any(|s| s.name == "Loggable"),
            "should find mixin 'Loggable' as trait; got: {:?}",
            traits.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_dart_extracts_methods() {
        let source = fixture("dart/simple.dart");
        let parsed = parse_source(Path::new("simple.dart"), &source).unwrap();

        let methods: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Method)
            .collect();
        assert!(
            methods.iter().any(|s| s.name == "greet"),
            "should find method 'greet'; got: {:?}",
            methods.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_dart_extracts_main_function() {
        let source = fixture("dart/simple.dart");
        let parsed = parse_source(Path::new("main.dart"), &source).unwrap();

        let functions: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Function)
            .collect();
        assert!(
            functions.iter().any(|s| s.name == "main"),
            "should find top-level function 'main'; got: {:?}",
            functions.iter().map(|s| &s.name).collect::<Vec<_>>()
        );

        // main() should be detected as an entry point
        let main_sym = functions.iter().find(|s| s.name == "main").unwrap();
        assert!(
            main_sym.is_entry_point,
            "main() should be marked as entry point"
        );
    }

    #[test]
    fn parse_dart_extracts_import_references() {
        let source = fixture("dart/simple.dart");
        let parsed = parse_source(Path::new("simple.dart"), &source).unwrap();

        let imports: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Import)
            .collect();
        assert!(
            !imports.is_empty(),
            "should find import references; all refs: {:?}",
            parsed
                .references
                .iter()
                .map(|r| (&r.name, r.kind))
                .collect::<Vec<_>>()
        );
        assert!(
            imports
                .iter()
                .any(|r| r.name.contains("helper") || r.name.contains("flutter")),
            "should find import for helper.dart or flutter; got: {:?}",
            imports.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_dart_extracts_call_references() {
        let source = fixture("dart/simple.dart");
        let parsed = parse_source(Path::new("simple.dart"), &source).unwrap();

        let calls: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Call)
            .collect();
        assert!(
            !calls.is_empty(),
            "should find call references; all refs: {:?}",
            parsed
                .references
                .iter()
                .map(|r| (&r.name, r.kind))
                .collect::<Vec<_>>()
        );
    }

    // ── Swift tests ──────────────────────────────────────────────────────────

    #[test]
    fn parse_swift_extracts_class_and_protocol() {
        let source = fixture("swift/simple.swift");
        let parsed = parse_source(Path::new("simple.swift"), &source).unwrap();

        let classes: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Class)
            .collect();
        assert!(
            classes.iter().any(|s| s.name == "SimpleGreeter"),
            "should find class 'SimpleGreeter'; got: {:?}",
            classes.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert!(
            classes.iter().any(|s| s.name == "AppConfig"),
            "should find struct 'AppConfig' as class; got: {:?}",
            classes.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert!(
            classes.iter().any(|s| s.name == "Priority"),
            "should find enum 'Priority' as class; got: {:?}",
            classes.iter().map(|s| &s.name).collect::<Vec<_>>()
        );

        let interfaces: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Interface)
            .collect();
        assert!(
            interfaces.iter().any(|s| s.name == "Greeter"),
            "should find protocol 'Greeter' as interface; got: {:?}",
            interfaces.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_swift_extracts_functions() {
        let source = fixture("swift/simple.swift");
        let parsed = parse_source(Path::new("simple.swift"), &source).unwrap();

        let functions: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Function)
            .collect();
        assert!(
            functions.iter().any(|s| s.name == "main"),
            "should find top-level function 'main'; got: {:?}",
            functions.iter().map(|s| &s.name).collect::<Vec<_>>()
        );

        // Methods inside class bodies are captured as Method, not Function.
        let methods: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Method)
            .collect();
        assert!(
            methods.iter().any(|s| s.name == "greet"),
            "should find method 'greet' as Method; got: {:?}",
            methods.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert!(
            methods.iter().any(|s| s.name == "formatName"),
            "should find method 'formatName' as Method; got: {:?}",
            methods.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_swift_detects_main_entry_point() {
        let source = fixture("swift/simple.swift");
        let parsed = parse_source(Path::new("main.swift"), &source).unwrap();

        let main_sym = parsed
            .symbols
            .iter()
            .find(|s| s.name == "main" && s.kind == SymbolKind::Function);
        assert!(main_sym.is_some(), "should find function 'main'");
        assert!(
            main_sym.unwrap().is_entry_point,
            "main() should be marked as entry point"
        );
    }

    #[test]
    fn parse_swift_extracts_import_references() {
        let source = fixture("swift/simple.swift");
        let parsed = parse_source(Path::new("simple.swift"), &source).unwrap();

        let imports: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Import)
            .collect();
        assert!(
            imports.iter().any(|r| r.name == "Foundation"),
            "should find import 'Foundation'; got: {:?}",
            imports.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_swift_extracts_call_references() {
        let source = fixture("swift/simple.swift");
        let parsed = parse_source(Path::new("simple.swift"), &source).unwrap();

        let calls: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Call)
            .collect();
        assert!(
            !calls.is_empty(),
            "should find call references; all refs: {:?}",
            parsed
                .references
                .iter()
                .map(|r| (&r.name, r.kind))
                .collect::<Vec<_>>()
        );
    }

    // ── COBOL tests ──────────────────────────────────────────────────────────

    #[test]
    fn parse_cobol_extracts_sections_and_paragraphs() {
        let source = fixture("cobol/simple.cbl");
        let parsed = parse_source(Path::new("simple.cbl"), &source).unwrap();

        let modules: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Module)
            .collect();
        assert!(
            modules.iter().any(|s| s.name == "MAIN-LOGIC"),
            "should find section 'MAIN-LOGIC'; got: {:?}",
            modules.iter().map(|s| &s.name).collect::<Vec<_>>()
        );

        let functions: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Function)
            .collect();
        assert!(
            functions.iter().any(|s| s.name == "INITIALIZE-DATA"),
            "should find paragraph 'INITIALIZE-DATA'; got: {:?}",
            functions.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_cobol_extracts_perform_and_call_references() {
        let source = fixture("cobol/simple.cbl");
        let parsed = parse_source(Path::new("simple.cbl"), &source).unwrap();

        let calls: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Call)
            .collect();
        assert!(
            calls.iter().any(|r| r.name == "INITIALIZE-DATA"),
            "should find PERFORM INITIALIZE-DATA; got: {:?}",
            calls.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
        assert!(
            calls.iter().any(|r| r.name == "UTIL-PROGRAM"),
            "should find CALL 'UTIL-PROGRAM'; got: {:?}",
            calls.iter().map(|r| &r.name).collect::<Vec<_>>()
        );

        let includes: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Includes)
            .collect();
        assert!(
            includes.iter().any(|r| r.name == "COMMON-DEFS"),
            "should find COPY COMMON-DEFS; got: {:?}",
            includes.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
    }

    // ── Visibility detection tests ────────────────────────────────────────

    #[test]
    fn parse_c_detects_static_visibility() {
        let source = fixture("c/simple.c");
        let parsed = parse_source(Path::new("simple.c"), &source).unwrap();

        let static_fn = parsed.symbols.iter().find(|s| s.name == "calibrate");
        assert!(static_fn.is_some(), "should find function 'calibrate'");
        assert_eq!(
            static_fn.unwrap().visibility,
            Visibility::Private,
            "'calibrate' is static so should be Private"
        );

        let public_fn = parsed.symbols.iter().find(|s| s.name == "initialize");
        assert!(public_fn.is_some(), "should find function 'initialize'");
        assert_eq!(
            public_fn.unwrap().visibility,
            Visibility::Public,
            "'initialize' has no static keyword so should be Public"
        );
    }

    #[test]
    fn parse_go_detects_visibility_by_case() {
        let source = fixture("go/simple.go");
        let parsed = parse_source(Path::new("simple.go"), &source).unwrap();

        // Capitalized → public
        let public_fn = parsed.symbols.iter().find(|s| s.name == "NewGreeter");
        assert!(public_fn.is_some(), "should find function 'NewGreeter'");
        assert_eq!(
            public_fn.unwrap().visibility,
            Visibility::Public,
            "'NewGreeter' starts with uppercase so should be Public"
        );

        // Lowercase → private
        let private_fn = parsed.symbols.iter().find(|s| s.name == "main");
        assert!(private_fn.is_some(), "should find function 'main'");
        assert_eq!(
            private_fn.unwrap().visibility,
            Visibility::Private,
            "'main' starts with lowercase so should be Private in Go"
        );
    }

    #[test]
    fn parse_python_public_symbols_are_inferred() {
        let source = fixture("python/simple.py");
        let parsed = parse_source(Path::new("simple.py"), &source).unwrap();

        // Non-underscore symbols → Inferred (not explicitly private)
        let public_fn = parsed
            .symbols
            .iter()
            .find(|s| s.name == "standalone_function");
        assert!(
            public_fn.is_some(),
            "should find function 'standalone_function'"
        );
        assert_eq!(
            public_fn.unwrap().visibility,
            Visibility::Inferred,
            "'standalone_function' has no underscore prefix so should be Inferred"
        );
    }

    // ── Lua tests ────────────────────────────────────────────────────────

    #[test]
    fn parse_lua_extracts_functions() {
        let source = fixture("lua/simple.lua");
        let parsed = parse_source(Path::new("simple.lua"), &source).unwrap();

        let functions: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Function)
            .collect();
        assert!(
            functions.iter().any(|s| s.name == "format_name"),
            "should find global function 'format_name'; got: {:?}",
            functions.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_lua_extracts_methods() {
        let source = fixture("lua/simple.lua");
        let parsed = parse_source(Path::new("simple.lua"), &source).unwrap();

        let methods: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Method)
            .collect();
        assert!(
            !methods.is_empty(),
            "should find methods; got symbols: {:?}",
            parsed
                .symbols
                .iter()
                .map(|s| (&s.name, s.kind))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_lua_extracts_call_references() {
        let source = fixture("lua/simple.lua");
        let parsed = parse_source(Path::new("simple.lua"), &source).unwrap();

        let calls: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Call)
            .collect();
        assert!(
            !calls.is_empty(),
            "should find call references; all refs: {:?}",
            parsed
                .references
                .iter()
                .map(|r| (&r.name, r.kind))
                .collect::<Vec<_>>()
        );
    }

    // ── Bash tests ──────────────────────────────────────────────────────

    #[test]
    fn parse_bash_extracts_functions() {
        let source = fixture("bash/simple.sh");
        let parsed = parse_source(Path::new("simple.sh"), &source).unwrap();

        let functions: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Function)
            .collect();
        assert!(
            functions.iter().any(|s| s.name == "greet"),
            "should find function 'greet'; got: {:?}",
            functions.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert!(
            functions.iter().any(|s| s.name == "format_name"),
            "should find function 'format_name'; got: {:?}",
            functions.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert!(
            functions.iter().any(|s| s.name == "main"),
            "should find function 'main'; got: {:?}",
            functions.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_bash_extracts_call_references() {
        let source = fixture("bash/simple.sh");
        let parsed = parse_source(Path::new("simple.sh"), &source).unwrap();

        let calls: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Call)
            .collect();
        assert!(
            !calls.is_empty(),
            "should find call references; all refs: {:?}",
            parsed
                .references
                .iter()
                .map(|r| (&r.name, r.kind))
                .collect::<Vec<_>>()
        );
    }

    // ── Scala tests ─────────────────────────────────────────────────────

    #[test]
    fn parse_scala_extracts_class_and_trait() {
        let source = fixture("scala/Simple.scala");
        let parsed = parse_source(Path::new("Simple.scala"), &source).unwrap();

        let classes: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Class)
            .collect();
        assert!(
            classes.iter().any(|s| s.name == "SimpleGreeter"),
            "should find class 'SimpleGreeter'; got: {:?}",
            classes.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert!(
            classes.iter().any(|s| s.name == "AppConfig"),
            "should find object 'AppConfig' as class; got: {:?}",
            classes.iter().map(|s| &s.name).collect::<Vec<_>>()
        );

        let traits: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Trait)
            .collect();
        assert!(
            traits.iter().any(|s| s.name == "Greeter"),
            "should find trait 'Greeter'; got: {:?}",
            traits.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_scala_extracts_functions() {
        let source = fixture("scala/Simple.scala");
        let parsed = parse_source(Path::new("Simple.scala"), &source).unwrap();

        let functions: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Function)
            .collect();
        assert!(
            !functions.is_empty(),
            "should find function definitions; got symbols: {:?}",
            parsed
                .symbols
                .iter()
                .map(|s| (&s.name, s.kind))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_scala_extracts_references() {
        let source = fixture("scala/Simple.scala");
        let parsed = parse_source(Path::new("Simple.scala"), &source).unwrap();

        let imports: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Import)
            .collect();
        assert!(
            !imports.is_empty(),
            "should find import references; all refs: {:?}",
            parsed
                .references
                .iter()
                .map(|r| (&r.name, r.kind))
                .collect::<Vec<_>>()
        );
    }

    // ── Elixir tests ────────────────────────────────────────────────────

    #[test]
    fn parse_elixir_extracts_modules() {
        let source = fixture("elixir/simple.ex");
        let parsed = parse_source(Path::new("simple.ex"), &source).unwrap();

        let modules: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Module)
            .collect();
        assert!(
            modules.iter().any(|s| s.name == "Greeter"),
            "should find module 'Greeter'; got: {:?}",
            modules.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_elixir_extracts_functions() {
        let source = fixture("elixir/simple.ex");
        let parsed = parse_source(Path::new("simple.ex"), &source).unwrap();

        let functions: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Function)
            .collect();
        assert!(
            functions.iter().any(|s| s.name == "greet"),
            "should find function 'greet'; got: {:?}",
            functions.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_elixir_extracts_references() {
        let source = fixture("elixir/simple.ex");
        let parsed = parse_source(Path::new("simple.ex"), &source).unwrap();

        let refs: Vec<_> = parsed.references.iter().collect();
        assert!(!refs.is_empty(), "should find references; got none");
    }

    // ── Unsupported language ───────────────────────────────────────────────

    #[test]
    fn unsupported_language_returns_error() {
        let source = "const x = 42;";
        let err = parse_source(Path::new("main.zig"), source).unwrap_err();
        assert!(
            matches!(err, ParseError::UnsupportedLanguage(_)),
            "expected UnsupportedLanguage, got: {err:?}"
        );
    }

    // ── Snapshot tests ────────────────────────────────────────────────────

    mod snapshot_tests {
        use super::*;
        use insta::assert_yaml_snapshot;

        /// Parse a fixture file and return symbols sorted by (start_line, name)
        /// with content_hash zeroed out for determinism.
        fn parsed_symbols(filename: &str, source: &str) -> Vec<RawSymbol> {
            let parsed = parse_source(Path::new(filename), source).unwrap();
            let mut symbols = parsed.symbols;
            symbols.sort_by(|a, b| a.start_line.cmp(&b.start_line).then(a.name.cmp(&b.name)));
            for s in &mut symbols {
                s.content_hash = "0".repeat(64);
            }
            symbols
        }

        /// Parse a fixture file and return references sorted by (start_line, name).
        fn parsed_references(filename: &str, source: &str) -> Vec<RawReference> {
            let parsed = parse_source(Path::new(filename), source).unwrap();
            let mut refs = parsed.references;
            refs.sort_by(|a, b| a.start_line.cmp(&b.start_line).then(a.name.cmp(&b.name)));
            refs
        }

        // ── JS ──────────────────────────────────────────────────────────

        #[test]
        fn snapshot_js_symbols() {
            let source = fixture("js/simple.js");
            assert_yaml_snapshot!(parsed_symbols("simple.js", &source));
        }

        #[test]
        fn snapshot_js_references() {
            let source = fixture("js/simple.js");
            assert_yaml_snapshot!(parsed_references("simple.js", &source));
        }

        // ── TypeScript ──────────────────────────────────────────────────

        #[test]
        fn snapshot_ts_symbols() {
            let source = fixture("ts/simple.ts");
            assert_yaml_snapshot!(parsed_symbols("simple.ts", &source));
        }

        #[test]
        fn snapshot_ts_references() {
            let source = fixture("ts/simple.ts");
            assert_yaml_snapshot!(parsed_references("simple.ts", &source));
        }

        // ── Python ──────────────────────────────────────────────────────

        #[test]
        fn snapshot_python_symbols() {
            let source = fixture("python/simple.py");
            assert_yaml_snapshot!(parsed_symbols("simple.py", &source));
        }

        #[test]
        fn snapshot_python_references() {
            let source = fixture("python/simple.py");
            assert_yaml_snapshot!(parsed_references("simple.py", &source));
        }

        // ── Rust ────────────────────────────────────────────────────────

        #[test]
        fn snapshot_rust_symbols() {
            let source = fixture("rust/simple.rs");
            assert_yaml_snapshot!(parsed_symbols("simple.rs", &source));
        }

        #[test]
        fn snapshot_rust_references() {
            let source = fixture("rust/simple.rs");
            assert_yaml_snapshot!(parsed_references("simple.rs", &source));
        }

        // ── Go ──────────────────────────────────────────────────────────

        #[test]
        fn snapshot_go_symbols() {
            let source = fixture("go/simple.go");
            assert_yaml_snapshot!(parsed_symbols("simple.go", &source));
        }

        #[test]
        fn snapshot_go_references() {
            let source = fixture("go/simple.go");
            assert_yaml_snapshot!(parsed_references("simple.go", &source));
        }

        // ── C ───────────────────────────────────────────────────────────

        #[test]
        fn snapshot_c_symbols() {
            let source = fixture("c/simple.c");
            assert_yaml_snapshot!(parsed_symbols("simple.c", &source));
        }

        #[test]
        fn snapshot_c_references() {
            let source = fixture("c/simple.c");
            assert_yaml_snapshot!(parsed_references("simple.c", &source));
        }

        // ── C++ ─────────────────────────────────────────────────────────

        #[test]
        fn snapshot_cpp_symbols() {
            let source = fixture("cpp/simple.cpp");
            assert_yaml_snapshot!(parsed_symbols("simple.cpp", &source));
        }

        #[test]
        fn snapshot_cpp_references() {
            let source = fixture("cpp/simple.cpp");
            assert_yaml_snapshot!(parsed_references("simple.cpp", &source));
        }

        // ── C# ──────────────────────────────────────────────────────────

        #[test]
        fn snapshot_csharp_symbols() {
            let source = fixture("csharp/Simple.cs");
            assert_yaml_snapshot!(parsed_symbols("Simple.cs", &source));
        }

        #[test]
        fn snapshot_csharp_references() {
            let source = fixture("csharp/Simple.cs");
            assert_yaml_snapshot!(parsed_references("Simple.cs", &source));
        }

        // ── Dart ────────────────────────────────────────────────────────

        #[test]
        fn snapshot_dart_symbols() {
            let source = fixture("dart/simple.dart");
            assert_yaml_snapshot!(parsed_symbols("simple.dart", &source));
        }

        #[test]
        fn snapshot_dart_references() {
            let source = fixture("dart/simple.dart");
            assert_yaml_snapshot!(parsed_references("simple.dart", &source));
        }

        // ── Java ────────────────────────────────────────────────────────

        #[test]
        fn snapshot_java_symbols() {
            let source = fixture("java/Simple.java");
            assert_yaml_snapshot!(parsed_symbols("Simple.java", &source));
        }

        #[test]
        fn snapshot_java_references() {
            let source = fixture("java/Simple.java");
            assert_yaml_snapshot!(parsed_references("Simple.java", &source));
        }

        // ── Kotlin ──────────────────────────────────────────────────────

        #[test]
        fn snapshot_kotlin_symbols() {
            let source = fixture("kotlin/Simple.kt");
            assert_yaml_snapshot!(parsed_symbols("Simple.kt", &source));
        }

        #[test]
        fn snapshot_kotlin_references() {
            let source = fixture("kotlin/Simple.kt");
            assert_yaml_snapshot!(parsed_references("Simple.kt", &source));
        }

        // ── PHP ─────────────────────────────────────────────────────────

        #[test]
        fn snapshot_php_symbols() {
            let source = fixture("php/simple.php");
            assert_yaml_snapshot!(parsed_symbols("simple.php", &source));
        }

        #[test]
        fn snapshot_php_references() {
            let source = fixture("php/simple.php");
            assert_yaml_snapshot!(parsed_references("simple.php", &source));
        }

        // ── Ruby ────────────────────────────────────────────────────────

        #[test]
        fn snapshot_ruby_symbols() {
            let source = fixture("ruby/simple.rb");
            assert_yaml_snapshot!(parsed_symbols("simple.rb", &source));
        }

        #[test]
        fn snapshot_ruby_references() {
            let source = fixture("ruby/simple.rb");
            assert_yaml_snapshot!(parsed_references("simple.rb", &source));
        }

        // ── Swift ───────────────────────────────────────────────────────

        #[test]
        fn snapshot_swift_symbols() {
            let source = fixture("swift/simple.swift");
            assert_yaml_snapshot!(parsed_symbols("simple.swift", &source));
        }

        #[test]
        fn snapshot_swift_references() {
            let source = fixture("swift/simple.swift");
            assert_yaml_snapshot!(parsed_references("simple.swift", &source));
        }

        // ── COBOL ───────────────────────────────────────────────────────

        #[test]
        fn snapshot_cobol_symbols() {
            let source = fixture("cobol/simple.cbl");
            assert_yaml_snapshot!(parsed_symbols("simple.cbl", &source));
        }

        #[test]
        fn snapshot_cobol_references() {
            let source = fixture("cobol/simple.cbl");
            assert_yaml_snapshot!(parsed_references("simple.cbl", &source));
        }

        // ── Lua ─────────────────────────────────────────────────────────

        #[test]
        fn snapshot_lua_symbols() {
            let source = fixture("lua/simple.lua");
            assert_yaml_snapshot!(parsed_symbols("simple.lua", &source));
        }

        #[test]
        fn snapshot_lua_references() {
            let source = fixture("lua/simple.lua");
            assert_yaml_snapshot!(parsed_references("simple.lua", &source));
        }

        // ── Bash ────────────────────────────────────────────────────────

        #[test]
        fn snapshot_bash_symbols() {
            let source = fixture("bash/simple.sh");
            assert_yaml_snapshot!(parsed_symbols("simple.sh", &source));
        }

        #[test]
        fn snapshot_bash_references() {
            let source = fixture("bash/simple.sh");
            assert_yaml_snapshot!(parsed_references("simple.sh", &source));
        }

        // ── Scala ───────────────────────────────────────────────────────

        #[test]
        fn snapshot_scala_symbols() {
            let source = fixture("scala/Simple.scala");
            assert_yaml_snapshot!(parsed_symbols("Simple.scala", &source));
        }

        #[test]
        fn snapshot_scala_references() {
            let source = fixture("scala/Simple.scala");
            assert_yaml_snapshot!(parsed_references("Simple.scala", &source));
        }

        // ── Elixir ──────────────────────────────────────────────────────

        #[test]
        fn snapshot_elixir_symbols() {
            let source = fixture("elixir/simple.ex");
            assert_yaml_snapshot!(parsed_symbols("simple.ex", &source));
        }

        #[test]
        fn snapshot_elixir_references() {
            let source = fixture("elixir/simple.ex");
            assert_yaml_snapshot!(parsed_references("simple.ex", &source));
        }
    }
}
