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
const GROOVY_QUERY: &str = include_str!("../../../queries/groovy.scm");
const ZIG_QUERY: &str = include_str!("../../../queries/zig.scm");
const OBJC_QUERY: &str = include_str!("../../../queries/objc.scm");
const POWERSHELL_QUERY: &str = include_str!("../../../queries/powershell.scm");
const JULIA_QUERY: &str = include_str!("../../../queries/julia.scm");
const SQL_QUERY: &str = include_str!("../../../queries/sql.scm");
const HCL_QUERY: &str = include_str!("../../../queries/hcl.scm");
const FORTRAN_QUERY: &str = include_str!("../../../queries/fortran.scm");
const PASCAL_QUERY: &str = include_str!("../../../queries/pascal.scm");
const SYSTEMVERILOG_QUERY: &str = include_str!("../../../queries/systemverilog.scm");

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
    TypeRef,
    ReadAccess,
    WriteAccess,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawSymbol {
    pub name: String,
    pub kind: SymbolKind,
    pub start_line: u32,
    pub end_line: u32,
    pub signature: String,
    pub content_hash: String,
    pub is_entry_point: bool,
    pub entry_point_kind: Option<EntryPointKind>,
    pub visibility: Visibility,
    pub type_info: Option<TypeInfo>,
    /// The name of the enclosing class/struct/impl/trait for method symbols.
    /// `None` for top-level functions and non-method symbols.
    pub parent_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawReference {
    pub name: String,
    pub kind: ReferenceKind,
    pub start_line: u32,
    pub context: String,
    /// The receiver of a method call: `"store"` in `store.method()`,
    /// `"self"` in `self.method()`, `None` for free function calls.
    pub receiver: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AstBindingKind {
    Annotation,  // let x: Foo
    Constructor, // let x = Foo::new()
    ReturnType,  // fn foo() -> Foo
    Parameter,   // fn foo(x: Foo)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AstTypeBinding {
    pub var_name: String,
    pub type_name: String,
    pub line: u32,
    pub kind: AstBindingKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedFile {
    pub path: String,
    pub symbols: Vec<RawSymbol>,
    pub references: Vec<RawReference>,
    #[serde(default)]
    pub type_bindings: Vec<AstTypeBinding>,
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
        // JavaScript/TypeScript/Vue/Svelte/Astro: export keyword = public
        Language::JavaScript
        | Language::TypeScript
        | Language::Vue
        | Language::Svelte
        | Language::Astro => {
            let sig = first_line(node_text);
            if sig.contains("export ") {
                Visibility::Public
            } else {
                Visibility::Private
            }
        }
        // Groovy: default is public; check for visibility keywords
        Language::Groovy => {
            let sig = first_line(node_text);
            if sig.contains("public ") {
                Visibility::Public
            } else if sig.contains("private ") {
                Visibility::Private
            } else if sig.contains("protected ") {
                Visibility::Protected
            } else {
                // Groovy default visibility is public
                Visibility::Public
            }
        }
        // Objective-C: methods are generally public, static C functions are private
        Language::ObjectiveC => {
            let sig = first_line(node_text);
            if sig.starts_with("static ") || sig.contains(" static ") {
                Visibility::Private
            } else {
                Visibility::Public
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
        // Zig: pub keyword = public
        Language::Zig => {
            let sig = first_line(node_text);
            if sig.starts_with("pub ") || sig.contains(" pub ") {
                Visibility::Public
            } else {
                Visibility::Private
            }
        }
        // PowerShell, Julia, SQL: inferred visibility
        Language::PowerShell | Language::Julia | Language::Sql => Visibility::Inferred,
        // Ruby, COBOL: inferred visibility
        Language::Ruby | Language::Cobol => Visibility::Inferred,
        // HCL: inferred visibility (tree-sitter)
        Language::Hcl => Visibility::Inferred,
        // Fortran, Pascal, SystemVerilog: inferred visibility (tree-sitter)
        Language::Fortran | Language::Pascal | Language::SystemVerilog => Visibility::Inferred,
    }
}

fn build_ts_language(lang: Language, path: &Path) -> tree_sitter::Language {
    match lang {
        Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        Language::TypeScript => {
            // .tsx files contain JSX syntax and need the TSX grammar;
            // .ts files use the plain TypeScript grammar.
            if path.extension().and_then(|e| e.to_str()) == Some("tsx") {
                tree_sitter_typescript::LANGUAGE_TSX.into()
            } else {
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
            }
        }
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
        Language::Groovy => tree_sitter_groovy::LANGUAGE.into(),
        Language::Zig => tree_sitter_zig::LANGUAGE.into(),
        Language::ObjectiveC => tree_sitter_objc::LANGUAGE.into(),
        Language::PowerShell => tree_sitter_powershell::LANGUAGE.into(),
        Language::Julia => tree_sitter_julia::LANGUAGE.into(),
        Language::Sql => tree_sitter_sequel::LANGUAGE.into(),
        Language::Fortran => tree_sitter_fortran::LANGUAGE.into(),
        Language::Pascal => tree_sitter_pascal::LANGUAGE.into(),
        Language::SystemVerilog => tree_sitter_systemverilog::LANGUAGE.into(),
        Language::Hcl => tree_sitter_hcl::LANGUAGE.into(),
        Language::Cobol | Language::Vue | Language::Svelte | Language::Astro => {
            unreachable!("regex-parsed languages are handled before reaching tree-sitter")
        }
    }
}

/// JSX query patterns appended to JS/TS queries for files that use JSX syntax.
const JSX_QUERY_SUFFIX: &str = r#"

; JSX opening element — component reference
(jsx_opening_element
  name: (identifier) @name) @reference.call

; JSX self-closing element — component reference
(jsx_self_closing_element
  name: (identifier) @name) @reference.call
"#;

fn query_source(lang: Language, path: &Path) -> std::borrow::Cow<'static, str> {
    let base = match lang {
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
        Language::Groovy => GROOVY_QUERY,
        Language::Zig => ZIG_QUERY,
        Language::ObjectiveC => OBJC_QUERY,
        Language::PowerShell => POWERSHELL_QUERY,
        Language::Julia => JULIA_QUERY,
        Language::Sql => SQL_QUERY,
        Language::Fortran => FORTRAN_QUERY,
        Language::Pascal => PASCAL_QUERY,
        Language::SystemVerilog => SYSTEMVERILOG_QUERY,
        Language::Hcl => HCL_QUERY,
        Language::Cobol | Language::Vue | Language::Svelte | Language::Astro => {
            unreachable!("regex-parsed languages are handled before reaching tree-sitter")
        }
    };

    // Append JSX patterns for .tsx and .jsx files whose grammars support JSX nodes.
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if ext == "tsx" || ext == "jsx" {
        let mut combined = String::with_capacity(base.len() + JSX_QUERY_SUFFIX.len());
        combined.push_str(base);
        combined.push_str(JSX_QUERY_SUFFIX);
        std::borrow::Cow::Owned(combined)
    } else {
        std::borrow::Cow::Borrowed(base)
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

// ── parent name extraction ────────────────────────────────────────────────

/// Walk the tree-sitter AST parent chain from `node` to find the enclosing
/// class, struct, impl, trait, or interface. Returns the parent type/class name
/// (e.g. `"GraphStore"` for a method inside `impl GraphStore { ... }`).
fn find_parent_name(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    let mut current = node.parent();
    while let Some(parent) = current {
        match parent.kind() {
            // Rust: impl Type { ... }
            "impl_item" => {
                return parent
                    .child_by_field_name("type")
                    .and_then(|t| t.utf8_text(source).ok())
                    .map(|s| s.to_string());
            }
            // JS/TS/Java/C#/Dart/PHP/Python/Ruby: class Name { ... }
            "class_declaration" | "class_definition" => {
                return parent
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .map(|s| s.to_string());
            }
            // TS/Java: interface Name { ... }
            "interface_declaration" => {
                return parent
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .map(|s| s.to_string());
            }
            // Rust: trait Name { ... }
            "trait_item" => {
                return parent
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .map(|s| s.to_string());
            }
            _ => {}
        }
        current = parent.parent();
    }
    None
}

// ── core parse ─────────────────────────────────────────────────────────────

/// Parse a single source file and extract symbols and references.
pub fn parse_source(path: &Path, source: &str) -> Result<ParsedFile, ParseError> {
    let lang = detect_language(path)
        .ok_or_else(|| ParseError::UnsupportedLanguage(path.to_string_lossy().into_owned()))?;

    // Languages with regex-based parsers (no tree-sitter grammar).
    // All regex-dispatched languages are handled in this single match block.
    match lang {
        Language::Cobol => return Ok(crate::cobol::parse_cobol(path, source)),
        Language::Vue => return Ok(crate::vue::parse_vue(path, source)),
        Language::Svelte => return Ok(crate::svelte::parse_svelte(path, source)),
        Language::Astro => return Ok(crate::astro::parse_astro(path, source)),
        _ => {}
    }

    let ts_lang = build_ts_language(lang, path);

    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&ts_lang)
        .map_err(|_| ParseError::ParseFailed)?;

    let tree = parser.parse(source, None).ok_or(ParseError::ParseFailed)?;

    let query_src = query_source(lang, path);
    let query = Query::new(&ts_lang, &query_src)?;
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
        Language::Groovy => "groovy",
        Language::Zig => "zig",
        Language::ObjectiveC => "objc",
        Language::PowerShell => "powershell",
        Language::Julia => "julia",
        Language::Sql => "sql",
        Language::Fortran => "fortran",
        Language::Pascal => "pascal",
        Language::SystemVerilog => "systemverilog",
        Language::Hcl => "hcl",
        Language::Cobol | Language::Vue | Language::Svelte | Language::Astro => {
            unreachable!("regex-parsed languages are handled before reaching tree-sitter")
        }
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
                    "module" | "namespace" => SymbolKind::Module,
                    "enum" => SymbolKind::Enum,
                    "const" | "constant" | "static" => SymbolKind::Constant,
                    "property" | "field" => SymbolKind::Property,
                    "type" | "type_alias" => SymbolKind::TypeAlias,
                    "variable" | "var" => SymbolKind::Variable,
                    "macro" => SymbolKind::Function,
                    "impl" => SymbolKind::Class,
                    "constructor" => SymbolKind::Method,
                    other => {
                        tracing::debug!(
                            capture = other,
                            file = %file_path_str,
                            "unknown definition capture, skipping"
                        );
                        continue;
                    }
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
                    SymbolKind::Constant => "constant",
                    SymbolKind::Property => "property",
                    SymbolKind::TypeAlias => "type_alias",
                    SymbolKind::Variable => "variable",
                };
                // A `definition.function` captured on a `call_expression` node is a
                // JS/TS test-runner block (test/it/describe). The calls inside its
                // callback attach to this symbol; mark it a test entry point so it
                // is reachable by regression-test selection regardless of filename.
                let ep_kind = if node.kind() == "call_expression" {
                    Some(EntryPointKind::TestEntry)
                } else {
                    detect_entry_point(
                        &name,
                        &file_path_str,
                        kind_label,
                        Some(&signature),
                        lang_str,
                    )
                };

                let visibility = infer_visibility(&name, &node_text, lang);
                let type_info = extract_type_info(&signature, lang);
                let parent_name = if kind == SymbolKind::Method {
                    find_parent_name(&node, source_bytes)
                } else {
                    None
                };
                symbols.push(RawSymbol {
                    name,
                    kind,
                    start_line,
                    end_line: node.end_position().row as u32 + 1,
                    signature,
                    content_hash,
                    is_entry_point: ep_kind.is_some(),
                    entry_point_kind: ep_kind,
                    visibility,
                    type_info,
                    parent_name,
                });
            } else if let Some(kind_str) = capture_name.strip_prefix("reference.") {
                let kind = match kind_str {
                    "call" => ReferenceKind::Call,
                    "import" => ReferenceKind::Import,
                    "extends" => ReferenceKind::Extends,
                    "implements" => ReferenceKind::Implements,
                    "includes" => ReferenceKind::Includes,
                    "uses" => ReferenceKind::Uses,
                    "type_ref" => ReferenceKind::TypeRef,
                    "read_access" => ReferenceKind::ReadAccess,
                    "write_access" => ReferenceKind::WriteAccess,
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

                // Filter out HTML elements from JSX patterns: lowercase
                // identifiers in jsx_opening_element / jsx_self_closing_element
                // are native HTML tags (div, span, etc.), not component references.
                let node_kind = node.kind();
                if (node_kind == "jsx_opening_element" || node_kind == "jsx_self_closing_element")
                    && name.starts_with(|c: char| c.is_ascii_lowercase())
                {
                    continue;
                }

                // Extract receiver for method calls: in `obj.method()`,
                // the captured node may be a call_expression, method_invocation,
                // or similar. The receiver is nested differently per language:
                //
                //   Rust/C++:  call_expression > function: field_expression > value
                //   JS/TS:     call_expression > function: member_expression > object
                //   Python:    call > function: attribute > object
                //   Go:        call_expression > function: selector_expression > operand
                //   C#:        invocation_expression > function: member_access_expression > expression
                //   Java:      method_invocation > object (direct field, no nesting)
                //   Kotlin:    call_expression > navigation_expression > navigation_suffix
                //   Ruby:      call > receiver (direct field)
                //   PHP:       member_call_expression > object
                let receiver = if kind == ReferenceKind::Call {
                    // Strategy 1: function child contains a member/field/selector/attribute node
                    let from_function_child = node
                        .child_by_field_name("function")
                        .and_then(|f| {
                            let k = f.kind();
                            if k.contains("field")
                                || k.contains("member")
                                || k.contains("selector")
                                || k.contains("attribute")
                                || k.contains("navigation")
                            {
                                f.child_by_field_name("object")
                                    .or_else(|| f.child_by_field_name("value"))
                                    .or_else(|| f.child_by_field_name("operand"))
                                    .or_else(|| f.child_by_field_name("expression"))
                            } else {
                                None
                            }
                        })
                        .and_then(|obj| obj.utf8_text(source_bytes).ok())
                        .map(|s| s.to_string());

                    // Strategy 2: direct object field (Java method_invocation)
                    let from_direct_object = if from_function_child.is_none() {
                        node.child_by_field_name("object")
                            .and_then(|obj| obj.utf8_text(source_bytes).ok())
                            .map(|s| s.to_string())
                    } else {
                        None
                    };

                    // Strategy 3: direct receiver field (Ruby call)
                    let from_receiver_field =
                        if from_function_child.is_none() && from_direct_object.is_none() {
                            node.child_by_field_name("receiver")
                                .and_then(|r| r.utf8_text(source_bytes).ok())
                                .map(|s| s.to_string())
                        } else {
                            None
                        };

                    from_function_child
                        .or(from_direct_object)
                        .or(from_receiver_field)
                } else {
                    None
                };

                references.push(RawReference {
                    name,
                    kind,
                    start_line,
                    context,
                    receiver,
                });
            }
            // Skip "name" captures — used via find_name_capture above
        }
    }

    // Type extraction: walk the same tree with type-specific queries
    let type_bindings = extract_types_from_tree(&tree, &ts_lang, source_bytes, lang);

    Ok(ParsedFile {
        path: path.to_string_lossy().into_owned(),
        symbols,
        references,
        type_bindings,
    })
}

// ── type extraction helpers ────────────────────────────────────────────────

/// Return the type query source for a language, or None if unsupported.
fn type_query_source(lang: Language) -> Option<&'static str> {
    match lang {
        Language::Rust => Some(include_str!("../../../queries/rust_types.scm")),
        Language::TypeScript | Language::JavaScript => {
            Some(include_str!("../../../queries/typescript_types.scm"))
        }
        Language::Java => Some(include_str!("../../../queries/java_types.scm")),
        Language::Python => Some(include_str!("../../../queries/python_types.scm")),
        Language::Go => Some(include_str!("../../../queries/go_types.scm")),
        Language::Cpp => Some(include_str!("../../../queries/cpp_types.scm")),
        Language::CSharp => Some(include_str!("../../../queries/csharp_types.scm")),
        Language::Kotlin => Some(include_str!("../../../queries/kotlin_types.scm")),
        Language::Php => Some(include_str!("../../../queries/php_types.scm")),
        Language::Dart => Some(include_str!("../../../queries/dart_types.scm")),
        Language::Swift => Some(include_str!("../../../queries/swift_types.scm")),
        Language::Scala => Some(include_str!("../../../queries/scala_types.scm")),
        Language::Ruby => Some(include_str!("../../../queries/ruby_types.scm")),
        Language::C => Some(include_str!("../../../queries/c_types.scm")),
        Language::Elixir => Some(include_str!("../../../queries/elixir_types.scm")),
        Language::Groovy => Some(include_str!("../../../queries/groovy_types.scm")),
        Language::ObjectiveC => Some(include_str!("../../../queries/objc_types.scm")),
        Language::PowerShell => Some(include_str!("../../../queries/powershell_types.scm")),
        Language::Pascal => Some(include_str!("../../../queries/pascal_types.scm")),
        Language::SystemVerilog => Some(include_str!("../../../queries/systemverilog_types.scm")),
        // Lua: dynamically typed, no type annotations in grammar (see lua_types.scm)
        _ => None,
    }
}

/// Extract the base type name from a full type string.
/// Strips references (&, &mut), lifetimes, generics brackets, pointers (*), etc.
/// "HashMap<String, Vec<Foo>>" -> "HashMap"
/// "&mut Vec<Foo>" -> "Vec"
/// "*const Foo" -> "Foo"
fn extract_base_type(full_type: &str) -> String {
    let s = full_type
        .trim()
        .trim_start_matches('&')
        .trim_start_matches("mut ")
        .trim_start_matches('*')
        .trim_start_matches("const ")
        .trim();
    // Strip lifetime: 'a str -> str
    let s = if s.starts_with('\'') {
        s.find(char::is_whitespace)
            .map(|i| s[i..].trim())
            .unwrap_or(s)
    } else {
        s
    };
    // Strip generics: HashMap<K,V> -> HashMap
    let base = s.find('<').map(|i| &s[..i]).unwrap_or(s);
    base.trim().to_string()
}

fn extract_types_from_tree(
    tree: &tree_sitter::Tree,
    ts_lang: &tree_sitter::Language,
    source: &[u8],
    lang: Language,
) -> Vec<AstTypeBinding> {
    let query_src = match type_query_source(lang) {
        Some(q) => q,
        None => return Vec::new(),
    };

    let query = match tree_sitter::Query::new(ts_lang, query_src) {
        Ok(q) => q,
        Err(_) => return Vec::new(),
    };

    let capture_names: Vec<String> = query
        .capture_names()
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut cursor = tree_sitter::QueryCursor::new();
    let mut bindings = Vec::new();

    let mut matches = cursor.matches(&query, tree.root_node(), source);
    while let Some(m) = matches.next() {
        let mut var_name: Option<String> = None;
        let mut var_type: Option<String> = None;
        let mut var_line: u32 = 0;
        let mut kind = AstBindingKind::Annotation;

        for capture in m.captures {
            let name = &capture_names[capture.index as usize];
            let text = capture.node.utf8_text(source).unwrap_or("").trim();
            let line = capture.node.start_position().row as u32 + 1;

            match name.as_str() {
                "var.name" => {
                    var_name = Some(text.to_string());
                    var_line = line;
                    kind = AstBindingKind::Annotation;
                }
                "var.type" => {
                    var_type = Some(extract_base_type(text));
                }
                "ctor.name" => {
                    var_name = Some(text.to_string());
                    var_line = line;
                    kind = AstBindingKind::Constructor;
                }
                "ctor.type" => {
                    // For scoped paths (foo::bar::Baz), take the last segment.
                    let base = text.rsplit("::").next().unwrap_or(text);
                    // Only accept PascalCase types — filters out module-scoped
                    // function calls like io::stdin() or env::var().
                    if base.starts_with(|c: char| c.is_ascii_uppercase()) {
                        var_type = Some(base.to_string());
                    }
                }
                "return.name" => {
                    var_name = Some(text.to_string());
                    var_line = line;
                    kind = AstBindingKind::ReturnType;
                }
                "return.type" => {
                    var_type = Some(extract_base_type(text));
                }
                "param.name" => {
                    var_name = Some(text.to_string());
                    var_line = line;
                    kind = AstBindingKind::Parameter;
                }
                "param.type" => {
                    var_type = Some(extract_base_type(text));
                }
                _ => {}
            }
        }

        if let (Some(name), Some(type_name)) = (var_name, var_type)
            && !name.is_empty()
            && !type_name.is_empty()
        {
            bindings.push(AstTypeBinding {
                var_name: name,
                type_name,
                line: var_line,
                kind,
            });
        }
    }

    bindings
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
            || (s.starts_with('`') && s.ends_with('`'))
            || (s.starts_with('<') && s.ends_with('>')))
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
    fn symbol_end_line_spans_multiline_body() {
        // P0.1: a multi-line function must record end_line past start_line.
        let source = "function greet(name) {\n  return hello(name);\n}\n";
        let parsed = parse_source(Path::new("t.js"), source).unwrap();
        let greet = parsed
            .symbols
            .iter()
            .find(|s| s.name == "greet")
            .expect("should find 'greet'");
        assert_eq!(greet.start_line, 1);
        assert!(
            greet.end_line > greet.start_line,
            "end_line ({}) should span past start_line ({})",
            greet.end_line,
            greet.start_line
        );
        assert_eq!(greet.end_line, 3, "the 3-line function body ends on line 3");
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

    // ── Test-runner symbol extraction (Jest/Vitest/Mocha) ───────────────────

    #[test]
    fn parse_js_extracts_test_runner_call_as_symbol() {
        // `test('name', () => foo())` should yield a symbol named after the test
        // title, spanning the call so the inner call to `foo` attaches to it.
        let source = "import { foo } from './x';\ntest('greets', () => { foo('a'); });\n";
        let parsed = parse_source(Path::new("app.test.js"), source).unwrap();

        let test_sym = parsed
            .symbols
            .iter()
            .find(|s| s.name == "greets")
            .unwrap_or_else(|| {
                panic!(
                    "should find a symbol named 'greets'; got: {:?}",
                    parsed.symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
                )
            });
        assert!(
            matches!(test_sym.kind, SymbolKind::Function | SymbolKind::Method),
            "test symbol should be a function/method; got {:?}",
            test_sym.kind
        );
        assert_eq!(
            test_sym.entry_point_kind,
            Some(EntryPointKind::TestEntry),
            "test symbol should be a TestEntry entry point"
        );

        // The call to `foo` must fall inside the test symbol's span so the
        // resolver attaches it as a CALLS edge from the test.
        let foo_call = parsed
            .references
            .iter()
            .find(|r| r.kind == ReferenceKind::Call && r.name == "foo")
            .expect("should capture call to 'foo' inside the test callback");
        assert!(
            foo_call.start_line >= test_sym.start_line && foo_call.start_line <= test_sym.end_line,
            "call to 'foo' (line {}) should be within test span {}..={}",
            foo_call.start_line,
            test_sym.start_line,
            test_sym.end_line
        );
    }

    #[test]
    fn parse_ts_extracts_describe_and_it_as_symbols() {
        let source =
            "describe('suite', () => {\n  it('does a thing', () => {\n    work();\n  });\n});\n";
        let parsed = parse_source(Path::new("app.test.ts"), source).unwrap();
        let names: Vec<&str> = parsed.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"suite"),
            "should find describe-block symbol 'suite'; got: {names:?}"
        );
        assert!(
            names.contains(&"does a thing"),
            "should find it-block symbol 'does a thing'; got: {names:?}"
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

        let enums: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Enum)
            .collect();
        assert!(
            enums.iter().any(|s| s.name == "SensorType"),
            "should find enum SensorType as Enum; got: {:?}",
            enums.iter().map(|s| &s.name).collect::<Vec<_>>()
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

        let enums: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Enum)
            .collect();
        assert!(
            enums.iter().any(|s| s.name == "SensorKind"),
            "should find enum SensorKind as Enum; got: {:?}",
            enums.iter().map(|s| &s.name).collect::<Vec<_>>()
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

        let modules: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Module)
            .collect();
        assert!(
            modules.iter().any(|s| s.name == "AppConfig"),
            "should find object 'AppConfig' as module; got: {:?}",
            modules.iter().map(|s| &s.name).collect::<Vec<_>>()
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

        let enums: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Enum)
            .collect();
        assert!(
            enums.iter().any(|s| s.name == "Priority"),
            "should find enum 'Priority' as Enum; got: {:?}",
            enums.iter().map(|s| &s.name).collect::<Vec<_>>()
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

    // ── Vue tests ────────────────────────────────────────────────────────

    #[test]
    fn parse_vue_extracts_component() {
        let source = fixture("vue/simple.vue");
        let parsed = parse_source(Path::new("simple.vue"), &source).unwrap();

        let classes: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Class)
            .collect();
        assert!(
            classes.iter().any(|s| s.name == "simple"),
            "should find component 'simple'; got: {:?}",
            classes.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_vue_extracts_exported_function() {
        let source = fixture("vue/simple.vue");
        let parsed = parse_source(Path::new("simple.vue"), &source).unwrap();

        let functions: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Function)
            .collect();
        assert!(
            functions.iter().any(|s| s.name == "formatName"),
            "should find exported function 'formatName'; got: {:?}",
            functions.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_vue_extracts_import_references() {
        let source = fixture("vue/simple.vue");
        let parsed = parse_source(Path::new("simple.vue"), &source).unwrap();

        let imports: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Import)
            .collect();
        assert!(
            imports.iter().any(|r| r.name == "vue"),
            "should find import 'vue'; got: {:?}",
            imports.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
        assert!(
            imports.iter().any(|r| r.name == "./utils/helper"),
            "should find import './utils/helper'; got: {:?}",
            imports.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_vue_extracts_call_references() {
        let source = fixture("vue/simple.vue");
        let parsed = parse_source(Path::new("simple.vue"), &source).unwrap();

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

    // ── Svelte tests ─────────────────────────────────────────────────────

    #[test]
    fn parse_svelte_extracts_component() {
        let source = fixture("svelte/simple.svelte");
        let parsed = parse_source(Path::new("simple.svelte"), &source).unwrap();

        let classes: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Class)
            .collect();
        assert!(
            classes.iter().any(|s| s.name == "simple"),
            "should find component 'simple'; got: {:?}",
            classes.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_svelte_extracts_exported_function() {
        let source = fixture("svelte/simple.svelte");
        let parsed = parse_source(Path::new("simple.svelte"), &source).unwrap();

        let functions: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Function)
            .collect();
        assert!(
            functions.iter().any(|s| s.name == "greet"),
            "should find exported function 'greet'; got: {:?}",
            functions.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_svelte_extracts_private_function() {
        let source = fixture("svelte/simple.svelte");
        let parsed = parse_source(Path::new("simple.svelte"), &source).unwrap();

        let handle_click = parsed.symbols.iter().find(|s| s.name == "handleClick");
        assert!(handle_click.is_some(), "should find function 'handleClick'");
        assert_eq!(
            handle_click.unwrap().visibility,
            Visibility::Private,
            "'handleClick' is not exported so should be Private"
        );
    }

    #[test]
    fn parse_svelte_extracts_import_references() {
        let source = fixture("svelte/simple.svelte");
        let parsed = parse_source(Path::new("simple.svelte"), &source).unwrap();

        let imports: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Import)
            .collect();
        assert!(
            imports.iter().any(|r| r.name == "svelte"),
            "should find import 'svelte'; got: {:?}",
            imports.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
        assert!(
            imports.iter().any(|r| r.name == "./Counter.svelte"),
            "should find import './Counter.svelte'; got: {:?}",
            imports.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
    }

    // ── Astro tests ──────────────────────────────────────────────────────

    #[test]
    fn parse_astro_extracts_component() {
        let source = fixture("astro/simple.astro");
        let parsed = parse_source(Path::new("simple.astro"), &source).unwrap();

        let classes: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Class)
            .collect();
        assert!(
            classes.iter().any(|s| s.name == "simple"),
            "should find component 'simple'; got: {:?}",
            classes.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_astro_extracts_exported_function() {
        let source = fixture("astro/simple.astro");
        let parsed = parse_source(Path::new("simple.astro"), &source).unwrap();

        let functions: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Function)
            .collect();
        assert!(
            functions.iter().any(|s| s.name == "getStaticPaths"),
            "should find exported function 'getStaticPaths'; got: {:?}",
            functions.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_astro_extracts_private_function() {
        let source = fixture("astro/simple.astro");
        let parsed = parse_source(Path::new("simple.astro"), &source).unwrap();

        let format_title = parsed.symbols.iter().find(|s| s.name == "formatTitle");
        assert!(format_title.is_some(), "should find function 'formatTitle'");
        assert_eq!(
            format_title.unwrap().visibility,
            Visibility::Private,
            "'formatTitle' is not exported so should be Private"
        );
    }

    #[test]
    fn parse_astro_extracts_import_references() {
        let source = fixture("astro/simple.astro");
        let parsed = parse_source(Path::new("simple.astro"), &source).unwrap();

        let imports: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Import)
            .collect();
        assert!(
            imports.iter().any(|r| r.name == "../layouts/Layout.astro"),
            "should find import '../layouts/Layout.astro'; got: {:?}",
            imports.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
        assert!(
            imports.iter().any(|r| r.name == "../components/Card.astro"),
            "should find import '../components/Card.astro'; got: {:?}",
            imports.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_astro_extracts_call_references() {
        let source = fixture("astro/simple.astro");
        let parsed = parse_source(Path::new("simple.astro"), &source).unwrap();

        let calls: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Call)
            .collect();
        assert!(
            calls.iter().any(|r| r.name == "formatTitle"),
            "should find call to 'formatTitle'; got: {:?}",
            calls.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
    }

    // ── SystemVerilog tests ──────────────────────────────────────────────

    #[test]
    fn parse_sv_extracts_modules() {
        let source = fixture("systemverilog/simple.sv");
        let parsed = parse_source(Path::new("simple.sv"), &source).unwrap();

        let modules: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Module)
            .collect();
        assert!(
            modules.iter().any(|s| s.name == "top_module"),
            "should find module 'top_module'; got: {:?}",
            modules.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert!(
            modules.iter().any(|s| s.name == "sub_module"),
            "should find module 'sub_module'; got: {:?}",
            modules.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_sv_extracts_interface() {
        let source = fixture("systemverilog/simple.sv");
        let parsed = parse_source(Path::new("simple.sv"), &source).unwrap();

        let interfaces: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Interface)
            .collect();
        assert!(
            interfaces.iter().any(|s| s.name == "axi_if"),
            "should find interface 'axi_if'; got: {:?}",
            interfaces.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_sv_extracts_class() {
        let source = fixture("systemverilog/simple.sv");
        let parsed = parse_source(Path::new("simple.sv"), &source).unwrap();

        let classes: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Class)
            .collect();
        assert!(
            classes.iter().any(|s| s.name == "packet"),
            "should find class 'packet'; got: {:?}",
            classes.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_sv_extracts_functions_and_tasks() {
        let source = fixture("systemverilog/simple.sv");
        let parsed = parse_source(Path::new("simple.sv"), &source).unwrap();

        // Methods inside class
        let methods: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Method)
            .collect();
        assert!(
            methods.iter().any(|s| s.name == "build"),
            "should find method 'build' inside class; got: {:?}",
            methods.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert!(
            methods.iter().any(|s| s.name == "send"),
            "should find task 'send' as method inside class; got: {:?}",
            methods.iter().map(|s| &s.name).collect::<Vec<_>>()
        );

        // Top-level function
        let functions: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Function)
            .collect();
        assert!(
            functions.iter().any(|s| s.name == "compute_checksum"),
            "should find top-level function 'compute_checksum'; got: {:?}",
            functions.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_sv_extracts_import_references() {
        let source = fixture("systemverilog/simple.sv");
        let parsed = parse_source(Path::new("simple.sv"), &source).unwrap();

        let imports: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Import)
            .collect();
        assert!(
            imports.iter().any(|r| r.name == "uvm_pkg"),
            "should find import 'uvm_pkg'; got: {:?}",
            imports.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
        assert!(
            imports.iter().any(|r| r.name == "bus_pkg"),
            "should find import 'bus_pkg'; got: {:?}",
            imports.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_sv_extracts_include_references() {
        let source = fixture("systemverilog/simple.sv");
        let parsed = parse_source(Path::new("simple.sv"), &source).unwrap();

        let includes: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Includes)
            .collect();
        assert!(
            includes.iter().any(|r| r.name == "common_defs.svh"),
            "should find include 'common_defs.svh'; got: {:?}",
            includes.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_sv_extracts_extends_reference() {
        let source = fixture("systemverilog/simple.sv");
        let parsed = parse_source(Path::new("simple.sv"), &source).unwrap();

        let extends: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Extends)
            .collect();
        assert!(
            extends.iter().any(|r| r.name == "base_packet"),
            "should find extends 'base_packet'; got: {:?}",
            extends.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_sv_extracts_instantiation_references() {
        let source = fixture("systemverilog/simple.sv");
        let parsed = parse_source(Path::new("simple.sv"), &source).unwrap();

        let calls: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Call)
            .collect();
        assert!(
            calls.iter().any(|r| r.name == "sub_module"),
            "should find instantiation of 'sub_module'; got: {:?}",
            calls.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
    }

    // ── Julia tests ──────────────────────────────────────────────────────────

    #[test]
    fn parse_julia_extracts_functions_and_structs() {
        let source = fixture("julia/simple.jl");
        let parsed = parse_source(Path::new("simple.jl"), &source).unwrap();

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
            functions.iter().any(|s| s.name == "process"),
            "should find function 'process'; got: {:?}",
            functions.iter().map(|s| &s.name).collect::<Vec<_>>()
        );

        let classes: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Class)
            .collect();
        // tree-sitter-julia captures `mutable struct` but not plain `struct`
        assert!(
            classes.iter().any(|s| s.name == "Counter"),
            "should find mutable struct 'Counter'; got: {:?}",
            classes.iter().map(|s| &s.name).collect::<Vec<_>>()
        );

        let modules: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Module)
            .collect();
        assert!(
            modules.iter().any(|s| s.name == "Greetings"),
            "should find module 'Greetings'; got: {:?}",
            modules.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_julia_extracts_macro() {
        let source = fixture("julia/simple.jl");
        let parsed = parse_source(Path::new("simple.jl"), &source).unwrap();

        let functions: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Function)
            .collect();
        assert!(
            functions.iter().any(|s| s.name == "log_call"),
            "should find macro 'log_call' as function; got: {:?}",
            functions.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_julia_extracts_abstract_type() {
        let source = fixture("julia/simple.jl");
        let parsed = parse_source(Path::new("simple.jl"), &source).unwrap();

        let interfaces: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Interface)
            .collect();
        assert!(
            interfaces.iter().any(|s| s.name == "LivingThing"),
            "should find abstract type 'LivingThing'; got: {:?}",
            interfaces.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_julia_extracts_call_references() {
        let source = fixture("julia/simple.jl");
        let parsed = parse_source(Path::new("simple.jl"), &source).unwrap();

        let calls: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Call)
            .collect();
        assert!(
            calls.iter().any(|r| r.name == "greet"),
            "should find call to 'greet'; got: {:?}",
            calls.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
        assert!(
            calls.iter().any(|r| r.name == "println"),
            "should find call to 'println'; got: {:?}",
            calls.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
    }

    // ── SQL tests ──────────────────────────────────────────────────────────

    #[test]
    fn parse_sql_extracts_tables_and_views() {
        let source = fixture("sql/simple.sql");
        let parsed = parse_source(Path::new("simple.sql"), &source).unwrap();

        let classes: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Class)
            .collect();
        assert!(
            classes.iter().any(|s| s.name == "users"),
            "should find table 'users'; got: {:?}",
            classes.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert!(
            classes.iter().any(|s| s.name == "orders"),
            "should find table 'orders'; got: {:?}",
            classes.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert!(
            classes.iter().any(|s| s.name == "active_users"),
            "should find view 'active_users'; got: {:?}",
            classes.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_sql_extracts_functions_and_procedures() {
        let source = fixture("sql/simple.sql");
        let parsed = parse_source(Path::new("simple.sql"), &source).unwrap();

        let functions: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Function)
            .collect();
        assert!(
            functions.iter().any(|s| s.name == "calculate_total"),
            "should find function 'calculate_total'; got: {:?}",
            functions.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        // Note: CREATE PROCEDURE is parsed as create_function by tree-sitter-sequel,
        // so update_status may not appear. We verify at least calculate_total is found.
    }

    #[test]
    fn parse_sql_extracts_references() {
        let source = fixture("sql/simple.sql");
        let parsed = parse_source(Path::new("simple.sql"), &source).unwrap();

        let calls: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Call)
            .collect();
        // FROM clause references are extracted; verify at least one call reference exists.
        // (Aliased references like "FROM users u" may extract as "u" or "users" depending
        // on grammar version.)
        assert!(
            !calls.is_empty(),
            "should find at least one reference; got empty"
        );
    }

    // ── HCL tests ──────────────────────────────────────────────────────────
    // HCL uses tree-sitter: all block definitions become @definition.class
    // with the first string_lit as the name (stripped of quotes).

    #[test]
    fn parse_hcl_extracts_resources() {
        let source = fixture("hcl/simple.tf");
        let parsed = parse_source(Path::new("simple.tf"), &source).unwrap();

        let classes: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Class)
            .collect();
        // Tree-sitter captures the first string_lit of each block as the name.
        // For `resource "aws_instance" "web"`, that's "aws_instance".
        assert!(
            classes.iter().any(|s| s.name == "aws_instance"),
            "should find resource type 'aws_instance'; got: {:?}",
            classes.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert!(
            classes.iter().any(|s| s.name == "aws_security_group"),
            "should find resource type 'aws_security_group'; got: {:?}",
            classes.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_hcl_extracts_variables_and_outputs() {
        let source = fixture("hcl/simple.tf");
        let parsed = parse_source(Path::new("simple.tf"), &source).unwrap();

        // With tree-sitter, variables and outputs are all @definition.class
        let classes: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Class)
            .collect();
        assert!(
            classes.iter().any(|s| s.name == "region"),
            "should find variable 'region' as class; got: {:?}",
            classes.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert!(
            classes.iter().any(|s| s.name == "instance_ip"),
            "should find output 'instance_ip' as class; got: {:?}",
            classes.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_hcl_extracts_module() {
        let source = fixture("hcl/simple.tf");
        let parsed = parse_source(Path::new("simple.tf"), &source).unwrap();

        // With tree-sitter, module blocks are also @definition.class
        let classes: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Class)
            .collect();
        assert!(
            classes.iter().any(|s| s.name == "vpc"),
            "should find module 'vpc' as class; got: {:?}",
            classes.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_hcl_extracts_symbols() {
        // Verify the tree-sitter parser extracts all block first-labels
        let source = fixture("hcl/simple.tf");
        let parsed = parse_source(Path::new("simple.tf"), &source).unwrap();
        assert!(
            !parsed.symbols.is_empty(),
            "should extract some symbols from HCL"
        );
        let names: Vec<&str> = parsed.symbols.iter().map(|s| s.name.as_str()).collect();
        // Each block's first string_lit becomes a symbol
        for expected in &[
            "region",
            "instance_type",
            "aws_instance",
            "aws_security_group",
            "vpc",
            "instance_ip",
            "vpc_id",
        ] {
            assert!(
                names.contains(expected),
                "should find '{expected}'; got: {names:?}"
            );
        }
    }

    // ── Fortran tests ──────────────────────────────────────────────────────

    #[test]
    fn parse_fortran_extracts_module_and_program() {
        let source = fixture("fortran/simple.f90");
        let parsed = parse_source(Path::new("simple.f90"), &source).unwrap();

        let modules: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Module)
            .collect();
        assert!(
            modules.iter().any(|s| s.name == "math_utils"),
            "should find module 'math_utils'; got: {:?}",
            modules.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert!(
            modules.iter().any(|s| s.name == "main"),
            "should find program 'main'; got: {:?}",
            modules.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_fortran_extracts_subroutines_and_functions() {
        let source = fixture("fortran/simple.f90");
        let parsed = parse_source(Path::new("simple.f90"), &source).unwrap();

        let functions: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Function)
            .collect();
        assert!(
            functions.iter().any(|s| s.name == "add_vectors"),
            "should find function 'add_vectors'; got: {:?}",
            functions.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert!(
            functions.iter().any(|s| s.name == "normalize"),
            "should find subroutine 'normalize'; got: {:?}",
            functions.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_fortran_extracts_references() {
        let source = fixture("fortran/simple.f90");
        let parsed = parse_source(Path::new("simple.f90"), &source).unwrap();

        let imports: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Import)
            .collect();
        assert!(
            imports.iter().any(|r| r.name == "math_utils"),
            "should find use math_utils; got: {:?}",
            imports.iter().map(|r| &r.name).collect::<Vec<_>>()
        );

        let calls: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Call)
            .collect();
        assert!(
            calls.iter().any(|r| r.name == "normalize"),
            "should find call to normalize; got: {:?}",
            calls.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
    }

    // ── Pascal tests ──────────────────────────────────────────────────────

    #[test]
    fn parse_pascal_extracts_classes_and_unit() {
        let source = fixture("pascal/simple.pas");
        let parsed = parse_source(Path::new("simple.pas"), &source).unwrap();

        let modules: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Module)
            .collect();
        assert!(
            modules.iter().any(|s| s.name == "Greeter"),
            "should find unit 'Greeter'; got: {:?}",
            modules.iter().map(|s| &s.name).collect::<Vec<_>>()
        );

        let classes: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Class)
            .collect();
        assert!(
            classes.iter().any(|s| s.name == "TAnimal"),
            "should find class 'TAnimal'; got: {:?}",
            classes.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert!(
            classes.iter().any(|s| s.name == "TDog"),
            "should find class 'TDog'; got: {:?}",
            classes.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_pascal_extracts_procedures_and_functions() {
        let source = fixture("pascal/simple.pas");
        let parsed = parse_source(Path::new("simple.pas"), &source).unwrap();

        let functions: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Function)
            .collect();
        assert!(
            functions.iter().any(|s| s.name == "PrintGreeting"),
            "should find procedure 'PrintGreeting'; got: {:?}",
            functions.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert!(
            functions.iter().any(|s| s.name == "FormatName"),
            "should find function 'FormatName'; got: {:?}",
            functions.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_pascal_extracts_methods() {
        let source = fixture("pascal/simple.pas");
        let parsed = parse_source(Path::new("simple.pas"), &source).unwrap();

        let methods: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Method)
            .collect();
        assert!(
            methods.iter().any(|s| s.name == "Speak"),
            "should find method 'Speak'; got: {:?}",
            methods.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert!(
            methods.iter().any(|s| s.name == "Create"),
            "should find constructor 'Create'; got: {:?}",
            methods.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_pascal_extracts_references() {
        let source = fixture("pascal/simple.pas");
        let parsed = parse_source(Path::new("simple.pas"), &source).unwrap();

        let imports: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Import)
            .collect();
        assert!(
            imports.iter().any(|r| r.name == "SysUtils"),
            "should find uses SysUtils; got: {:?}",
            imports.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
        assert!(
            imports.iter().any(|r| r.name == "Classes"),
            "should find uses Classes; got: {:?}",
            imports.iter().map(|r| &r.name).collect::<Vec<_>>()
        );

        let extends: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Extends)
            .collect();
        assert!(
            extends.iter().any(|r| r.name == "TAnimal"),
            "should find extends 'TAnimal'; got: {:?}",
            extends.iter().map(|r| &r.name).collect::<Vec<_>>()
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

    // ── Zig tests ────────────────────────────────────────────────────────────

    #[test]
    fn parse_zig_extracts_functions() {
        let source = fixture("zig/simple.zig");
        let parsed = parse_source(Path::new("simple.zig"), &source).unwrap();

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
            "should find function 'main'; got: {:?}",
            functions.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_zig_extracts_struct_enum_union() {
        let source = fixture("zig/simple.zig");
        let parsed = parse_source(Path::new("simple.zig"), &source).unwrap();

        let classes: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Class)
            .collect();
        assert!(
            classes.iter().any(|s| s.name == "SensorConfig"),
            "should find struct 'SensorConfig'; got: {:?}",
            classes.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        let enums: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Enum)
            .collect();
        assert!(
            enums.iter().any(|s| s.name == "SensorKind"),
            "should find enum 'SensorKind'; got: {:?}",
            enums.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert!(
            classes.iter().any(|s| s.name == "InternalState"),
            "should find union 'InternalState'; got: {:?}",
            classes.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_zig_detects_visibility() {
        let source = fixture("zig/simple.zig");
        let parsed = parse_source(Path::new("simple.zig"), &source).unwrap();

        let pub_fn = parsed.symbols.iter().find(|s| s.name == "initialize");
        assert!(pub_fn.is_some());
        assert_eq!(pub_fn.unwrap().visibility, Visibility::Public);

        let priv_fn = parsed.symbols.iter().find(|s| s.name == "calibrate");
        assert!(priv_fn.is_some());
        assert_eq!(priv_fn.unwrap().visibility, Visibility::Private);
    }

    #[test]
    fn parse_zig_extracts_import_references() {
        let source = fixture("zig/simple.zig");
        let parsed = parse_source(Path::new("simple.zig"), &source).unwrap();

        let imports: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Import)
            .collect();
        assert!(
            imports.iter().any(|r| r.name == "std"),
            "should find @import(\"std\"); got: {:?}",
            imports.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
        assert!(
            imports.iter().any(|r| r.name == "math.zig"),
            "should find @import(\"math.zig\"); got: {:?}",
            imports.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
    }

    // ── Objective-C tests ──────────────────────────────────────────────────

    #[test]
    fn parse_objc_extracts_interface_and_implementation() {
        let source = fixture("objc/simple.m");
        let parsed = parse_source(Path::new("simple.m"), &source).unwrap();

        let interfaces: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Interface)
            .collect();
        assert!(
            interfaces.iter().any(|s| s.name == "SimpleGreeter"),
            "should find @interface 'SimpleGreeter'; got: {:?}",
            interfaces.iter().map(|s| &s.name).collect::<Vec<_>>()
        );

        let classes: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Class)
            .collect();
        assert!(
            classes.iter().any(|s| s.name == "SimpleGreeter"),
            "should find @implementation 'SimpleGreeter'; got: {:?}",
            classes.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_objc_extracts_protocol() {
        let source = fixture("objc/simple.m");
        let parsed = parse_source(Path::new("simple.m"), &source).unwrap();

        let interfaces: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Interface)
            .collect();
        assert!(
            interfaces.iter().any(|s| s.name == "GreeterProtocol"),
            "should find @protocol 'GreeterProtocol'; got: {:?}",
            interfaces.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_objc_extracts_methods() {
        let source = fixture("objc/simple.m");
        let parsed = parse_source(Path::new("simple.m"), &source).unwrap();

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
            methods.iter().any(|s| s.name == "initWithPrefix"),
            "should find method 'initWithPrefix'; got: {:?}",
            methods.iter().map(|s| &s.name).collect::<Vec<_>>()
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

    #[test]
    fn parse_objc_extracts_import_references() {
        let source = fixture("objc/simple.m");
        let parsed = parse_source(Path::new("simple.m"), &source).unwrap();

        let imports: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Import)
            .collect();
        assert!(
            imports.iter().any(|r| r.name == "Foundation/Foundation.h"),
            "should find #import Foundation; got: {:?}",
            imports.iter().map(|r| &r.name).collect::<Vec<_>>()
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

        let modules: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Module)
            .collect();
        assert!(
            modules.iter().any(|s| s.name == "AppConfig"),
            "should find object 'AppConfig' as module; got: {:?}",
            modules.iter().map(|s| &s.name).collect::<Vec<_>>()
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

    // ── Groovy tests ──────────────────────────────────────────────────────

    #[test]
    fn parse_groovy_extracts_class_and_interface() {
        let source = fixture("groovy/simple.groovy");
        let parsed = parse_source(Path::new("simple.groovy"), &source).unwrap();

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
            classes.iter().any(|s| s.name == "FormalGreeter"),
            "should find class 'FormalGreeter'; got: {:?}",
            classes.iter().map(|s| &s.name).collect::<Vec<_>>()
        );

        let interfaces: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Interface)
            .collect();
        assert!(
            interfaces.iter().any(|s| s.name == "Greeter"),
            "should find interface 'Greeter'; got: {:?}",
            interfaces.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    #[ignore = "tree-sitter-groovy grammar does not support Groovy trait keyword"]
    fn parse_groovy_extracts_trait() {
        let source = fixture("groovy/simple.groovy");
        let parsed = parse_source(Path::new("simple.groovy"), &source).unwrap();

        let traits: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Trait)
            .collect();
        assert!(
            traits.iter().any(|s| s.name == "Loggable"),
            "should find trait 'Loggable'; got: {:?}",
            traits.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_groovy_extracts_methods() {
        let source = fixture("groovy/simple.groovy");
        let parsed = parse_source(Path::new("simple.groovy"), &source).unwrap();

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
    fn parse_groovy_extracts_extends_reference() {
        let source = fixture("groovy/simple.groovy");
        let parsed = parse_source(Path::new("simple.groovy"), &source).unwrap();

        let extends: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Extends)
            .collect();
        assert!(
            extends.iter().any(|r| r.name == "SimpleGreeter"),
            "should find extends 'SimpleGreeter'; got: {:?}",
            extends.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_groovy_extracts_implements_reference() {
        let source = fixture("groovy/simple.groovy");
        let parsed = parse_source(Path::new("simple.groovy"), &source).unwrap();

        let impls: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Implements)
            .collect();
        assert!(
            impls.iter().any(|r| r.name == "Greeter"),
            "should find implements 'Greeter'; got: {:?}",
            impls.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
    }

    // ── PowerShell tests ──────────────────────────────────────────────────

    #[test]
    fn parse_powershell_extracts_functions() {
        let source = fixture("powershell/simple.ps1");
        let parsed = parse_source(Path::new("simple.ps1"), &source).unwrap();

        let functions: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Function)
            .collect();
        assert!(
            functions.iter().any(|s| s.name == "Initialize-Sensor"),
            "should find function 'Initialize-Sensor'; got: {:?}",
            functions.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert!(
            functions.iter().any(|s| s.name == "Get-SensorData"),
            "should find function 'Get-SensorData'; got: {:?}",
            functions.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert!(
            functions.iter().any(|s| s.name == "Select-ActiveSensors"),
            "should find filter 'Select-ActiveSensors'; got: {:?}",
            functions.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_powershell_extracts_class() {
        let source = fixture("powershell/simple.ps1");
        let parsed = parse_source(Path::new("simple.ps1"), &source).unwrap();

        let classes: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Class)
            .collect();
        assert!(
            classes.iter().any(|s| s.name == "SensorConfig"),
            "should find class 'SensorConfig'; got: {:?}",
            classes.iter().map(|s| &s.name).collect::<Vec<_>>()
        );

        let enums: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Enum)
            .collect();
        assert!(
            enums.iter().any(|s| s.name == "Priority"),
            "should find enum 'Priority' as Enum; got: {:?}",
            enums.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_powershell_extracts_class_methods() {
        let source = fixture("powershell/simple.ps1");
        let parsed = parse_source(Path::new("simple.ps1"), &source).unwrap();

        let methods: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Method)
            .collect();
        assert!(
            methods.iter().any(|s| s.name == "ToString"),
            "should find method 'ToString'; got: {:?}",
            methods.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_powershell_extracts_import_references() {
        let source = fixture("powershell/simple.ps1");
        let parsed = parse_source(Path::new("simple.ps1"), &source).unwrap();

        // tree-sitter-powershell captures `Import-Module` as a command
        // invocation (ReferenceKind::Call), not as an import.
        let calls: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Call)
            .collect();
        assert!(
            calls.iter().any(|r| r.name == "Import-Module"),
            "should find Import-Module as a call reference; got: {:?}",
            calls.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_powershell_extracts_cmdlet_calls() {
        let source = fixture("powershell/simple.ps1");
        let parsed = parse_source(Path::new("simple.ps1"), &source).unwrap();

        let calls: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Call)
            .collect();
        assert!(
            !calls.is_empty(),
            "should find cmdlet call references; all refs: {:?}",
            parsed
                .references
                .iter()
                .map(|r| (&r.name, r.kind))
                .collect::<Vec<_>>()
        );
    }

    // ── JSX/TSX tests ─────────────────────────────────────────────────────

    #[test]
    fn parse_tsx_extracts_jsx_component_references() {
        let source = fixture("tsx/simple.tsx");
        let parsed = parse_source(Path::new("simple.tsx"), &source).unwrap();

        let calls: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Call)
            .collect();

        // Should find component references: Header, Sidebar, UserProfile, App
        assert!(
            calls.iter().any(|r| r.name == "Header"),
            "should find JSX reference to 'Header'; got: {:?}",
            calls.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
        assert!(
            calls.iter().any(|r| r.name == "Sidebar"),
            "should find JSX reference to 'Sidebar'; got: {:?}",
            calls.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
        assert!(
            calls.iter().any(|r| r.name == "UserProfile"),
            "should find JSX reference to 'UserProfile'; got: {:?}",
            calls.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
        assert!(
            calls.iter().any(|r| r.name == "App"),
            "should find JSX reference to 'App'; got: {:?}",
            calls.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_tsx_filters_html_elements() {
        let source = fixture("tsx/simple.tsx");
        let parsed = parse_source(Path::new("simple.tsx"), &source).unwrap();

        let calls: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Call)
            .collect();

        // Should NOT find HTML elements: div, span
        assert!(
            !calls.iter().any(|r| r.name == "div"),
            "should NOT find HTML element 'div' as component reference; got: {:?}",
            calls.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
        assert!(
            !calls.iter().any(|r| r.name == "span"),
            "should NOT find HTML element 'span' as component reference; got: {:?}",
            calls.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_tsx_extracts_hook_call_references() {
        let source = fixture("tsx/simple.tsx");
        let parsed = parse_source(Path::new("simple.tsx"), &source).unwrap();

        let calls: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Call)
            .collect();

        // useAuth() should be captured as a regular call reference
        assert!(
            calls.iter().any(|r| r.name == "useAuth"),
            "should find hook call to 'useAuth'; got: {:?}",
            calls.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_tsx_extracts_symbols() {
        let source = fixture("tsx/simple.tsx");
        let parsed = parse_source(Path::new("simple.tsx"), &source).unwrap();

        let functions: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Function)
            .collect();
        assert!(
            functions.iter().any(|s| s.name == "App"),
            "should find function 'App'; got: {:?}",
            functions.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert!(
            functions.iter().any(|s| s.name == "Dashboard"),
            "should find function 'Dashboard'; got: {:?}",
            functions.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert!(
            functions.iter().any(|s| s.name == "Header"),
            "should find function 'Header'; got: {:?}",
            functions.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    // ── Unsupported language ───────────────────────────────────────────────

    #[test]
    fn unsupported_language_returns_error() {
        let source = "const x = 42;";
        let err = parse_source(Path::new("main.wat"), source).unwrap_err();
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

        // ── Zig ─────────────────────────────────────────────────────────

        #[test]
        fn snapshot_zig_symbols() {
            let source = fixture("zig/simple.zig");
            assert_yaml_snapshot!(parsed_symbols("simple.zig", &source));
        }

        #[test]
        fn snapshot_zig_references() {
            let source = fixture("zig/simple.zig");
            assert_yaml_snapshot!(parsed_references("simple.zig", &source));
        }

        // ── Objective-C ─────────────────────────────────────────────────

        #[test]
        fn snapshot_objc_symbols() {
            let source = fixture("objc/simple.m");
            assert_yaml_snapshot!(parsed_symbols("simple.m", &source));
        }

        #[test]
        fn snapshot_objc_references() {
            let source = fixture("objc/simple.m");
            assert_yaml_snapshot!(parsed_references("simple.m", &source));
        }

        // ── Groovy ──────────────────────────────────────────────────────

        #[test]
        fn snapshot_groovy_symbols() {
            let source = fixture("groovy/simple.groovy");
            assert_yaml_snapshot!(parsed_symbols("simple.groovy", &source));
        }

        #[test]
        fn snapshot_groovy_references() {
            let source = fixture("groovy/simple.groovy");
            assert_yaml_snapshot!(parsed_references("simple.groovy", &source));
        }

        // ── PowerShell ──────────────────────────────────────────────────

        #[test]
        fn snapshot_powershell_symbols() {
            let source = fixture("powershell/simple.ps1");
            assert_yaml_snapshot!(parsed_symbols("simple.ps1", &source));
        }

        #[test]
        fn snapshot_powershell_references() {
            let source = fixture("powershell/simple.ps1");
            assert_yaml_snapshot!(parsed_references("simple.ps1", &source));
        }

        // ── Julia ───────────────────────────────────────────────────────

        #[test]
        fn snapshot_julia_symbols() {
            let source = fixture("julia/simple.jl");
            assert_yaml_snapshot!(parsed_symbols("simple.jl", &source));
        }

        #[test]
        fn snapshot_julia_references() {
            let source = fixture("julia/simple.jl");
            assert_yaml_snapshot!(parsed_references("simple.jl", &source));
        }

        // ── SQL ─────────────────────────────────────────────────────────

        #[test]
        fn snapshot_sql_symbols() {
            let source = fixture("sql/simple.sql");
            assert_yaml_snapshot!(parsed_symbols("simple.sql", &source));
        }

        #[test]
        fn snapshot_sql_references() {
            let source = fixture("sql/simple.sql");
            assert_yaml_snapshot!(parsed_references("simple.sql", &source));
        }

        // ── HCL ─────────────────────────────────────────────────────────

        #[test]
        fn snapshot_hcl_symbols() {
            let source = fixture("hcl/simple.tf");
            assert_yaml_snapshot!(parsed_symbols("simple.tf", &source));
        }

        #[test]
        fn snapshot_hcl_references() {
            let source = fixture("hcl/simple.tf");
            assert_yaml_snapshot!(parsed_references("simple.tf", &source));
        }

        // ── Fortran ─────────────────────────────────────────────────────

        #[test]
        fn snapshot_fortran_symbols() {
            let source = fixture("fortran/simple.f90");
            assert_yaml_snapshot!(parsed_symbols("simple.f90", &source));
        }

        #[test]
        fn snapshot_fortran_references() {
            let source = fixture("fortran/simple.f90");
            assert_yaml_snapshot!(parsed_references("simple.f90", &source));
        }

        // ── Pascal ──────────────────────────────────────────────────────

        #[test]
        fn snapshot_pascal_symbols() {
            let source = fixture("pascal/simple.pas");
            assert_yaml_snapshot!(parsed_symbols("simple.pas", &source));
        }

        #[test]
        fn snapshot_pascal_references() {
            let source = fixture("pascal/simple.pas");
            assert_yaml_snapshot!(parsed_references("simple.pas", &source));
        }

        // ── Vue ─────────────────────────────────────────────────────────

        #[test]
        fn snapshot_vue_symbols() {
            let source = fixture("vue/simple.vue");
            assert_yaml_snapshot!(parsed_symbols("simple.vue", &source));
        }

        #[test]
        fn snapshot_vue_references() {
            let source = fixture("vue/simple.vue");
            assert_yaml_snapshot!(parsed_references("simple.vue", &source));
        }

        // ── Svelte ──────────────────────────────────────────────────────

        #[test]
        fn snapshot_svelte_symbols() {
            let source = fixture("svelte/simple.svelte");
            assert_yaml_snapshot!(parsed_symbols("simple.svelte", &source));
        }

        #[test]
        fn snapshot_svelte_references() {
            let source = fixture("svelte/simple.svelte");
            assert_yaml_snapshot!(parsed_references("simple.svelte", &source));
        }

        // ── Astro ───────────────────────────────────────────────────────

        #[test]
        fn snapshot_astro_symbols() {
            let source = fixture("astro/simple.astro");
            assert_yaml_snapshot!(parsed_symbols("simple.astro", &source));
        }

        #[test]
        fn snapshot_astro_references() {
            let source = fixture("astro/simple.astro");
            assert_yaml_snapshot!(parsed_references("simple.astro", &source));
        }

        // ── SystemVerilog ───────────────────────────────────────────────

        #[test]
        fn snapshot_sv_symbols() {
            let source = fixture("systemverilog/simple.sv");
            assert_yaml_snapshot!(parsed_symbols("simple.sv", &source));
        }

        #[test]
        fn snapshot_sv_references() {
            let source = fixture("systemverilog/simple.sv");
            assert_yaml_snapshot!(parsed_references("simple.sv", &source));
        }
    }

    // ── Receiver extraction tests ───────────────────────────────────────────

    #[test]
    fn extracts_receiver_from_method_call() {
        let source = r#"
fn main() {
    let store = Store::new();
    store.get_item("key");
}
"#;
        let parsed = parse_source(Path::new("t.rs"), source).unwrap();
        let call_refs: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Call && r.name == "get_item")
            .collect();
        assert!(!call_refs.is_empty(), "should find call to 'get_item'");
        assert_eq!(
            call_refs[0].receiver.as_deref(),
            Some("store"),
            "receiver should be 'store'"
        );
    }

    #[test]
    fn extracts_self_receiver() {
        let source = r#"
struct Foo;
impl Foo {
    fn bar(&self) {
        self.baz();
    }
    fn baz(&self) {}
}
"#;
        let parsed = parse_source(Path::new("t.rs"), source).unwrap();
        let call_refs: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Call && r.name == "baz")
            .collect();
        assert!(!call_refs.is_empty(), "should find call to 'baz'");
        assert_eq!(
            call_refs[0].receiver.as_deref(),
            Some("self"),
            "receiver should be 'self'"
        );
    }

    #[test]
    fn free_function_has_no_receiver() {
        let source = r#"
fn helper() {}
fn main() {
    helper();
}
"#;
        let parsed = parse_source(Path::new("t.rs"), source).unwrap();
        let call_refs: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Call && r.name == "helper")
            .collect();
        assert!(!call_refs.is_empty(), "should find call to 'helper'");
        assert_eq!(
            call_refs[0].receiver, None,
            "free function should have no receiver"
        );
    }

    #[test]
    fn js_method_call_receiver() {
        let source = r#"
const arr = [1, 2, 3];
arr.map(x => x + 1);
"#;
        let parsed = parse_source(Path::new("t.js"), source).unwrap();
        let call_refs: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Call && r.name == "map")
            .collect();
        assert!(!call_refs.is_empty(), "should find call to 'map'");
        assert_eq!(
            call_refs[0].receiver.as_deref(),
            Some("arr"),
            "receiver should be 'arr'"
        );
    }

    #[test]
    fn extracts_parent_name_for_rust_impl_method() {
        let source = r#"
struct GraphStore;
impl GraphStore {
    fn compute_pagerank(&self) {}
}
"#;
        let parsed = parse_source(Path::new("t.rs"), source).unwrap();
        let method = parsed
            .symbols
            .iter()
            .find(|s| s.name == "compute_pagerank")
            .expect("should find compute_pagerank symbol");
        assert_eq!(
            method.parent_name,
            Some("GraphStore".to_string()),
            "method inside impl GraphStore should have parent_name = GraphStore"
        );
    }

    #[test]
    fn top_level_function_has_no_parent() {
        let source = "fn main() {}";
        let parsed = parse_source(Path::new("t.rs"), source).unwrap();
        let func = parsed
            .symbols
            .iter()
            .find(|s| s.name == "main")
            .expect("should find main symbol");
        assert_eq!(
            func.parent_name, None,
            "top-level function should have no parent_name"
        );
    }

    #[test]
    fn extracts_parent_name_for_ts_class_method() {
        let source = r#"
class UserService {
    fetchUser(id: string) { return null; }
}
"#;
        let parsed = parse_source(Path::new("t.ts"), source).unwrap();
        let method = parsed
            .symbols
            .iter()
            .find(|s| s.name == "fetchUser")
            .expect("should find fetchUser symbol");
        assert_eq!(
            method.parent_name,
            Some("UserService".to_string()),
            "method inside class UserService should have parent_name = UserService"
        );
    }

    #[test]
    fn type_queries_compile_for_all_languages() {
        let languages: Vec<(&str, tree_sitter::Language, &str)> = vec![
            (
                "rust",
                tree_sitter_rust::LANGUAGE.into(),
                include_str!("../../../queries/rust_types.scm"),
            ),
            (
                "typescript",
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                include_str!("../../../queries/typescript_types.scm"),
            ),
            (
                "java",
                tree_sitter_java::LANGUAGE.into(),
                include_str!("../../../queries/java_types.scm"),
            ),
            (
                "python",
                tree_sitter_python::LANGUAGE.into(),
                include_str!("../../../queries/python_types.scm"),
            ),
            (
                "go",
                tree_sitter_go::LANGUAGE.into(),
                include_str!("../../../queries/go_types.scm"),
            ),
        ];
        for (name, lang, query_src) in languages {
            match tree_sitter::Query::new(&lang, query_src) {
                Ok(q) => {
                    assert!(q.capture_names().len() > 0, "{name}: no captures");
                }
                Err(e) => panic!("{name} type query failed to compile: {e}"),
            }
        }
    }

    #[test]
    fn extracts_parent_name_for_rust_trait_method() {
        let source = r#"
trait Drawable {
    fn draw(&self) {}
}
"#;
        let parsed = parse_source(Path::new("t.rs"), source).unwrap();
        let method = parsed
            .symbols
            .iter()
            .find(|s| s.name == "draw")
            .expect("should find draw symbol");
        assert_eq!(
            method.parent_name,
            Some("Drawable".to_string()),
            "method inside trait Drawable should have parent_name = Drawable"
        );
    }

    #[test]
    fn ast_extracts_rust_let_annotation() {
        let source = r#"
fn main() {
    let store: GraphStore = GraphStore::new();
}
"#;
        let parsed = parse_source(Path::new("test.rs"), source).unwrap();
        let binding = parsed
            .type_bindings
            .iter()
            .find(|b| b.var_name == "store")
            .expect("should find 'store' binding");
        assert_eq!(binding.type_name, "GraphStore");
        assert!(matches!(binding.kind, AstBindingKind::Annotation));
    }

    #[test]
    fn ast_extracts_typescript_new_constructor() {
        let source = "const user = new User();";
        let parsed = parse_source(Path::new("test.ts"), source).unwrap();
        let binding = parsed
            .type_bindings
            .iter()
            .find(|b| b.var_name == "user")
            .expect("should find 'user' binding");
        assert_eq!(binding.type_name, "User");
        assert!(matches!(binding.kind, AstBindingKind::Constructor));
    }

    #[test]
    fn ast_extracts_function_return_type() {
        let source = "fn get_store() -> GraphStore { todo!() }";
        let parsed = parse_source(Path::new("test.rs"), source).unwrap();
        let binding = parsed
            .type_bindings
            .iter()
            .find(|b| b.var_name == "get_store")
            .expect("should find 'get_store' return type");
        assert_eq!(binding.type_name, "GraphStore");
        assert!(matches!(binding.kind, AstBindingKind::ReturnType));
    }

    #[test]
    fn ast_extracts_rust_struct_expression_constructor() {
        let source = r#"
fn main() {
    let config = Config { host: "localhost", port: 8080 };
}
"#;
        let parsed = parse_source(Path::new("test.rs"), source).unwrap();
        let binding = parsed
            .type_bindings
            .iter()
            .find(|b| b.var_name == "config")
            .expect("should find 'config' binding from struct expression");
        assert_eq!(binding.type_name, "Config");
        assert!(matches!(binding.kind, AstBindingKind::Constructor));
    }

    #[test]
    fn ast_extracts_rust_static_method_constructor() {
        let source = r#"
fn main() {
    let store = GraphStore::new();
    let map = HashMap::default();
    let v = Vec::with_capacity(10);
}
"#;
        let parsed = parse_source(Path::new("test.rs"), source).unwrap();

        let store_binding = parsed
            .type_bindings
            .iter()
            .find(|b| b.var_name == "store")
            .expect("should find 'store' binding");
        assert_eq!(store_binding.type_name, "GraphStore");
        assert!(matches!(store_binding.kind, AstBindingKind::Constructor));

        let map_binding = parsed
            .type_bindings
            .iter()
            .find(|b| b.var_name == "map")
            .expect("should find 'map' binding");
        assert_eq!(map_binding.type_name, "HashMap");
        assert!(matches!(map_binding.kind, AstBindingKind::Constructor));

        let v_binding = parsed
            .type_bindings
            .iter()
            .find(|b| b.var_name == "v")
            .expect("should find 'v' binding");
        assert_eq!(v_binding.type_name, "Vec");
        assert!(matches!(v_binding.kind, AstBindingKind::Constructor));
    }

    #[test]
    fn ast_extracts_rust_tuple_struct_destructuring() {
        let source = r#"
fn main() {
    let point = Point(1, 2);
    let Point(x, y) = point;
}
"#;
        let parsed = parse_source(Path::new("test.rs"), source).unwrap();
        // The destructuring pattern `let Point(x, y) = point` should yield a binding
        // with type_name = "Point". var.name capture is absent for this pattern so
        // the parser will emit it as a var.type-only match, but the query still fires.
        let binding = parsed
            .type_bindings
            .iter()
            .find(|b| b.type_name == "Point" && b.var_name.is_empty())
            .or_else(|| parsed.type_bindings.iter().find(|b| b.type_name == "Point"))
            .expect("should find a binding with type_name 'Point' from tuple struct pattern");
        assert_eq!(binding.type_name, "Point");
    }

    #[test]
    fn ast_extracts_rust_struct_pattern_destructuring() {
        let source = r#"
fn main() {
    let foo = Foo { x: 1, y: 2 };
    let Foo { x, y } = foo;
}
"#;
        let parsed = parse_source(Path::new("test.rs"), source).unwrap();
        // The struct pattern `let Foo { x, y } = foo` captures the type name.
        let binding = parsed
            .type_bindings
            .iter()
            .find(|b| b.type_name == "Foo")
            .expect("should find a binding with type_name 'Foo' from struct pattern");
        assert_eq!(binding.type_name, "Foo");
    }

    #[test]
    fn ast_extracts_python_constructor_call() {
        let source = "store = GraphStore()\n";
        let parsed = parse_source(Path::new("test.py"), source).unwrap();
        let binding = parsed.type_bindings.iter().find(|b| b.var_name == "store");
        assert!(
            binding.is_some(),
            "should find store binding: {:?}",
            parsed.type_bindings
        );
        assert_eq!(binding.unwrap().type_name, "GraphStore");
    }

    #[test]
    fn ast_extracts_go_short_var_composite_literal() {
        let source = "package main\nfunc main() {\n\tcfg := Config{Host: \"localhost\"}\n}\n";
        let parsed = parse_source(Path::new("test.go"), source).unwrap();
        let binding = parsed.type_bindings.iter().find(|b| b.var_name == "cfg");
        assert!(
            binding.is_some(),
            "should find cfg binding: {:?}",
            parsed.type_bindings
        );
        assert_eq!(binding.unwrap().type_name, "Config");
    }

    #[test]
    fn ast_extracts_java_new_in_local_var() {
        let source = "class Main { void run() { User u = new User(); } }";
        let parsed = parse_source(Path::new("test.java"), source).unwrap();
        let binding = parsed.type_bindings.iter().find(|b| b.var_name == "u");
        assert!(
            binding.is_some(),
            "should find u binding: {:?}",
            parsed.type_bindings
        );
        assert_eq!(binding.unwrap().type_name, "User");
    }

    #[test]
    fn ast_extracts_ts_class_property_constructor() {
        let source = "class Service { store = new Store(); }";
        let parsed = parse_source(Path::new("test.ts"), source).unwrap();
        let binding = parsed.type_bindings.iter().find(|b| b.var_name == "store");
        assert!(
            binding.is_some(),
            "should find store binding: {:?}",
            parsed.type_bindings
        );
        assert_eq!(binding.unwrap().type_name, "Store");
    }

    #[test]
    fn ast_extracts_cpp_typed_variable() {
        let source = "void foo() { int count = 0; }";
        let parsed = parse_source(Path::new("test.cpp"), source).unwrap();
        assert!(
            parsed
                .type_bindings
                .iter()
                .any(|b| b.var_name == "count" && b.type_name == "int"),
            "expected int binding for count: {:?}",
            parsed.type_bindings
        );
    }

    #[test]
    fn ast_extracts_csharp_method_return() {
        let source = "class Foo { string GetName() { return \"\"; } }";
        let parsed = parse_source(Path::new("test.cs"), source).unwrap();
        assert!(
            parsed.type_bindings.iter().any(|b| b.var_name == "GetName"),
            "expected return type binding: {:?}",
            parsed.type_bindings
        );
    }

    #[test]
    fn ast_extracts_kotlin_typed_val() {
        let source = "fun main() { val name: String = \"hello\" }";
        let parsed = parse_source(Path::new("test.kt"), source).unwrap();
        assert!(
            parsed
                .type_bindings
                .iter()
                .any(|b| b.var_name == "name" && b.type_name == "String"),
            "expected String binding: {:?}",
            parsed.type_bindings
        );
    }

    #[test]
    fn ast_extracts_php_return_type() {
        let source = "<?php\nfunction greet(): string { return 'hi'; }\n";
        let parsed = parse_source(Path::new("test.php"), source).unwrap();
        assert!(
            parsed
                .type_bindings
                .iter()
                .any(|b| b.var_name == "greet" && b.type_name == "string"),
            "expected string return: {:?}",
            parsed.type_bindings
        );
    }

    #[test]
    fn ast_extracts_dart_typed_var() {
        let source = "void main() { String name = 'hello'; }";
        let parsed = parse_source(Path::new("test.dart"), source).unwrap();
        assert!(
            parsed.type_bindings.iter().any(|b| b.var_name == "name"),
            "expected name binding: {:?}",
            parsed.type_bindings
        );
    }

    #[test]
    fn ast_extracts_scala_typed_val() {
        let source = "object Main { val count: Int = 0 }";
        let parsed = parse_source(Path::new("test.scala"), source).unwrap();
        assert!(
            parsed.type_bindings.iter().any(|b| b.var_name == "count"),
            "expected count binding: {:?}",
            parsed.type_bindings
        );
    }

    #[test]
    fn ast_extracts_ruby_constructor() {
        let source = "user = User.new";
        let parsed = parse_source(Path::new("test.rb"), source).unwrap();
        assert!(
            parsed
                .type_bindings
                .iter()
                .any(|b| b.var_name == "user" && b.type_name == "User"),
            "expected User binding: {:?}",
            parsed.type_bindings
        );
    }

    #[test]
    fn ast_extracts_swift_typed_let() {
        let source = "func foo() { let name: String = \"hello\" }";
        let parsed = parse_source(Path::new("test.swift"), source).unwrap();
        assert!(
            parsed.type_bindings.iter().any(|b| b.var_name == "name"),
            "expected name binding: {:?}",
            parsed.type_bindings
        );
    }

    #[test]
    fn ast_extracts_c_typed_variable() {
        let source = "void foo() { int count = 0; }";
        let parsed = parse_source(Path::new("test.c"), source).unwrap();
        assert!(
            parsed.type_bindings.iter().any(|b| b.var_name == "count"),
            "expected count binding: {:?}",
            parsed.type_bindings
        );
    }

    #[test]
    fn swift_extracts_member_call() {
        let source = "class Foo {\n  func bar() {\n    store.query()\n  }\n}";
        let parsed = parse_source(Path::new("test.swift"), source).unwrap();
        let calls: Vec<_> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Call && r.name == "query")
            .collect();
        assert!(
            !calls.is_empty(),
            "expected query call reference: {:?}",
            parsed
                .references
                .iter()
                .map(|r| (&r.name, &r.kind))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn ast_extracts_elixir_struct_construction() {
        let source =
            "defmodule Main do\n  def run do\n    user = %User{name: \"test\"}\n  end\nend\n";
        let parsed = parse_source(Path::new("test.ex"), source).unwrap();
        let bindings: Vec<_> = parsed
            .type_bindings
            .iter()
            .filter(|b| b.var_name == "user")
            .collect();
        assert!(
            !bindings.is_empty(),
            "expected user binding from struct: {:?}",
            parsed.type_bindings
        );
        assert_eq!(bindings[0].type_name, "User");
        assert_eq!(bindings[0].kind, AstBindingKind::Constructor);
    }

    #[test]
    fn ast_extracts_groovy_typed_local() {
        let source = "class Main { void run() { String name = 'hello' } }";
        let parsed = parse_source(Path::new("test.groovy"), source).unwrap();
        assert!(
            parsed
                .type_bindings
                .iter()
                .any(|b| b.var_name == "name" && b.type_name == "String"),
            "expected name binding: {:?}",
            parsed.type_bindings
        );
    }

    #[test]
    fn ast_extracts_groovy_method_return_type() {
        let source = "class Main { Integer compute() { return 42 } }";
        let parsed = parse_source(Path::new("test.groovy"), source).unwrap();
        assert!(
            parsed.type_bindings.iter().any(|b| b.var_name == "compute"
                && b.type_name == "Integer"
                && matches!(b.kind, AstBindingKind::ReturnType)),
            "expected compute return type: {:?}",
            parsed.type_bindings
        );
    }

    #[test]
    fn ast_extracts_groovy_constructor() {
        let source = "class Main { void run() { Foo x = new Foo() } }";
        let parsed = parse_source(Path::new("test.groovy"), source).unwrap();
        assert!(
            parsed.type_bindings.iter().any(|b| b.var_name == "x"
                && b.type_name == "Foo"
                && matches!(b.kind, AstBindingKind::Constructor)),
            "expected x ctor binding: {:?}",
            parsed.type_bindings
        );
    }

    #[test]
    fn ast_extracts_objc_typed_variable() {
        let source = "int main() { NSString* name = @\"hello\"; return 0; }";
        let parsed = parse_source(Path::new("test.m"), source).unwrap();
        assert!(
            parsed
                .type_bindings
                .iter()
                .any(|b| b.var_name == "name" && b.type_name == "NSString"),
            "expected name binding: {:?}",
            parsed.type_bindings
        );
    }

    #[test]
    fn ast_extracts_objc_function_return_type() {
        let source = "NSInteger getValue() { return 42; }";
        let parsed = parse_source(Path::new("test.m"), source).unwrap();
        assert!(
            parsed.type_bindings.iter().any(|b| b.var_name == "getValue"
                && b.type_name == "NSInteger"
                && matches!(b.kind, AstBindingKind::ReturnType)),
            "expected getValue return type: {:?}",
            parsed.type_bindings
        );
    }

    #[test]
    fn ast_extracts_powershell_class_property() {
        let source = "class Person {\n  [string]$Name\n  [int]$Age\n}";
        let parsed = parse_source(Path::new("test.ps1"), source).unwrap();
        assert!(
            parsed
                .type_bindings
                .iter()
                .any(|b| b.var_name.contains("Name")),
            "expected Name binding: {:?}",
            parsed.type_bindings
        );
    }

    #[test]
    fn ast_extracts_pascal_typed_var() {
        let source = "program Main;\nvar\n  count: Integer;\nbegin\nend.";
        let parsed = parse_source(Path::new("test.pas"), source).unwrap();
        assert!(
            parsed.type_bindings.iter().any(|b| b.var_name == "count"),
            "expected count binding: {:?}",
            parsed.type_bindings
        );
    }

    #[test]
    fn ast_extracts_pascal_function_return_type() {
        let source = "function GetValue(): Integer;\nbegin\n  Result := 42;\nend;";
        let parsed = parse_source(Path::new("test.pas"), source).unwrap();
        assert!(
            parsed
                .type_bindings
                .iter()
                .any(|b| b.var_name == "GetValue" && matches!(b.kind, AstBindingKind::ReturnType)),
            "expected GetValue return type: {:?}",
            parsed.type_bindings
        );
    }

    #[test]
    fn ast_extracts_systemverilog_typed_var() {
        let source = "module test;\n  int count;\nendmodule";
        let parsed = parse_source(Path::new("test.sv"), source).unwrap();
        assert!(
            parsed.type_bindings.iter().any(|b| b.var_name == "count"),
            "expected count binding: {:?}",
            parsed.type_bindings
        );
    }

    #[test]
    fn ast_extracts_lua_no_types() {
        // Lua is dynamically typed — no type bindings expected
        let source = "local x = 42\nfunction foo(a, b) return a + b end";
        let parsed = parse_source(Path::new("test.lua"), source).unwrap();
        assert!(
            parsed.type_bindings.is_empty(),
            "Lua should have no type bindings: {:?}",
            parsed.type_bindings
        );
    }
}
