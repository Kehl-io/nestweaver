use crate::entry_points::detect_entry_point;
use crate::language::detect_language;
use crate::scope_chain::extract_scope_chain;
use bumpalo::Bump;
use nestweaver_schema::{EntryPointKind, Language, SymbolKind, TypeInfo, Visibility};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
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

// ── query cache ────────────────────────────────────────────────────────────

/// Cache key for a compiled tree-sitter `Query`.
///
/// Two files of the same language produce the same query source *unless* one
/// is a JSX/TSX file (where we append extra JSX patterns).  The
/// `is_type_query` flag distinguishes the symbol-extraction query from the
/// type-binding query for the same language.
#[derive(Hash, Eq, PartialEq, Clone)]
struct QueryCacheKey {
    lang: Language,
    is_jsx: bool,
    is_type_query: bool,
}

/// Global cache of compiled [`Query`] objects, keyed by language + variant.
///
/// `Query` is `Send + Sync` but not `Clone`, so we wrap in `Arc` to allow
/// multiple callers to share the same compiled query without copying it.
static QUERY_CACHE: OnceLock<Mutex<HashMap<QueryCacheKey, Arc<Query>>>> = OnceLock::new();

/// Return a cached (or freshly compiled) `Query` wrapped in an `Arc`.
///
/// The first call for a given `(lang, is_jsx, is_type_query)` triple compiles
/// the S-expression source string and stores the result; subsequent calls
/// return the cached `Arc` without recompiling.
///
/// `query_src` must be the same string for a given `key` on every call —
/// the cache stores only the compiled form, not the source.
fn get_or_compile_query(
    ts_lang: &tree_sitter::Language,
    key: QueryCacheKey,
    query_src: &str,
) -> Result<Arc<Query>, ParseError> {
    let cache = QUERY_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = cache.lock().unwrap_or_else(|e| e.into_inner());

    if let Some(q) = map.get(&key) {
        return Ok(Arc::clone(q));
    }

    let arc = Arc::new(Query::new(ts_lang, query_src)?);
    map.insert(key, Arc::clone(&arc));
    Ok(arc)
}

// ── output types ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReferenceKind {
    Call,
    Import,
    /// The local rename of an aliased import: `use a::b as c;` emits the
    /// usual [`ReferenceKind::Import`] reference for `a::b` plus one
    /// `ImportAlias` reference whose `name` is the alias (`c`) and whose
    /// `context` is the full original import path (`a::b`). The resolver
    /// turns these into named bindings so calls to the alias resolve to the
    /// original item.
    ImportAlias,
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
    /// The `::` separated scope chain for this symbol, built by walking the
    /// tree-sitter AST parent chain (e.g. `"mymod::MyClass"`).
    /// `None` for top-level symbols or languages without scope constructs.
    pub scope_chain: Option<String>,
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

/// Remove raw NUL bytes from decoded source text.
///
/// nw-335: a NUL is not text, but it is also not necessarily corruption -- a
/// user can paste one into a note. `nestweaver-store`'s corruption canary
/// (`read::string_is_corrupt`) is a LadybugDB #678 partial-scan detector whose
/// stated premise is that no column NestWeaver stores contains a NUL, and it is
/// applied to CONTENT columns as well as navigational ones. One pasted byte
/// therefore made the store refuse a row it had recorded faithfully, which
/// emptied every whole-corpus scan and took Tantivy and the regex trigram
/// corpus to `docs=0` brain-wide.
///
/// Stripping here makes that premise TRUE again, rather than weakening a canary
/// that is doing real work on uids and paths. It is deliberately at the parser,
/// the one seam every ingest path crosses: the filesystem reader's binary sniff
/// is WINDOWED to the leading 8 KiB (the ripgrep/git/grep heuristic), and the
/// watcher's incremental path uses `fs::read_to_string` and has no sniff at all.
///
/// Borrows when there is nothing to strip, so the overwhelmingly common path
/// costs one scan and no allocation -- the same shape as nw-190's lossy decode.
pub fn strip_nul_bytes(source: &str) -> std::borrow::Cow<'_, str> {
    if source.contains('\0') {
        std::borrow::Cow::Owned(source.replace('\0', ""))
    } else {
        std::borrow::Cow::Borrowed(source)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReasonCode {
    Ignored,
    Unsupported,
    Oversized,
    ReadError,
    ParseError,
    Cancelled,
    #[default]
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedFile {
    pub path: String,
    pub reason: String,
    #[serde(default)]
    pub reason_code: SkipReasonCode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_bytes: Option<u64>,
}

impl SkippedFile {
    pub fn new(
        path: impl Into<String>,
        reason_code: SkipReasonCode,
        reason: impl Into<String>,
    ) -> Self {
        let mut reason = reason.into();
        const MAX_SKIP_DETAIL_BYTES: usize = 1024;
        if reason.len() > MAX_SKIP_DETAIL_BYTES {
            let mut end = MAX_SKIP_DETAIL_BYTES;
            while end > 0 && !reason.is_char_boundary(end) {
                end -= 1;
            }
            reason.truncate(end);
            reason.push('…');
        }
        Self {
            path: path.into(),
            reason,
            reason_code,
            observed_bytes: None,
            limit_bytes: None,
        }
    }

    pub fn oversized(path: impl Into<String>, observed_bytes: u64, limit_bytes: u64) -> Self {
        Self {
            path: path.into(),
            reason: format!(
                "file exceeds source size limit ({observed_bytes} > {limit_bytes} bytes)"
            ),
            reason_code: SkipReasonCode::Oversized,
            observed_bytes: Some(observed_bytes),
            limit_bytes: Some(limit_bytes),
        }
    }
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

/// The signature of a symbol: the first line of its node, plus — when that
/// first line is an annotation the grammar folded INTO the node — the
/// declaration line it decorates.
///
/// Most grammars keep annotations outside the declaration node (Rust's
/// `#[test]` is a preceding sibling — see `leading_rust_attributes`), but some
/// fold them in. Dart's `method_declaration` carries `annotation` as a child,
/// so anchoring the capture on the declaration rather than on the bodiless
/// signature made `SimpleGreeter.greet`'s signature the bare string
/// `"@override"` — the span became correct and the signature became useless in
/// the same change. Java already had that defect: BOTH overriding methods in
/// `testdata/java/simple.java` recorded `signature: "@Override"`, which is also
/// why neither carried any `type_info`.
///
/// # Why the annotation is APPENDED TO rather than replaced
///
/// The annotation is load-bearing. `frameworks.rs` recognises Spring by
/// `signature.contains("@RestController")` and Flask/FastAPI by `@app.route` /
/// `@router.`, and contract derivation reads the route out of
/// `@GetMapping("/users/{id}")`. Dropping it to recover the declaration
/// silently disabled all of that — seven engine tests, which is how it was
/// caught. `frameworks.rs`'s own fixture spells a Spring controller as
/// `"@RestController public class UserController"`, so the appended form is the
/// shape the rest of the codebase already expects.
///
/// # Why only the FIRST annotation
///
/// So this change cannot alter which annotations are visible to any consumer.
/// Joining *every* leading annotation makes a class-level
/// `@RequestMapping("/v1/items")` visible for the first time, and contract
/// derivation then mints it as an `ANY /v1/items` ROUTE with the controller
/// class as its implementer — a base path is not an endpoint. That is a real
/// latent defect in contract derivation, but it is not this change's to make
/// observable: keeping the first line exactly as it is today means the set of
/// annotations any consumer can see is unchanged, and all that is added is the
/// declaration being decorated.
///
/// # Objective-C
///
/// Excluded: there `@` is a declaration KEYWORD, not an annotation marker.
/// `@interface`, `@protocol`, `@implementation` and `@property` ARE the
/// declarations, and appending folds a protocol's first method onto its header.
/// `@Override` and `@protocol` are indistinguishable as text, so the exception
/// is named rather than inferred from shape.
fn signature_line(text: &str, lang_str: &str) -> String {
    let first = first_line(text);
    if lang_str == "objc" || !is_folded_annotation(&first) {
        return first;
    }
    match text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .find(|l| !is_folded_annotation(l))
    {
        Some(declaration) => format!("{first} {declaration}"),
        None => first,
    }
}

/// Whether a trimmed line is an annotation the grammar folded into the node,
/// rather than the declaration itself.
fn is_folded_annotation(line: &str) -> bool {
    line.starts_with("#[")
        || (line.starts_with('@')
            && !line.contains('{')
            && !line.contains(';')
            && !line.contains("=>"))
}

/// Whether a captured declaration node sits inside an `export_statement`.
///
/// Tree-sitter's JS/TS grammars wrap exported declarations (`export function`,
/// `export const`, `export default ...`) in a PARENT `export_statement` node,
/// so the `export` keyword is not part of the declaration node's own text. A
/// short ancestor walk (declaration → lexical_declaration → export_statement
/// is the deepest common shape) recovers it. Cheap: at most 3 parent hops.
fn has_export_ancestor(node: &tree_sitter::Node) -> bool {
    let mut current = node.parent();
    for _ in 0..3 {
        match current {
            Some(n) if n.kind() == "export_statement" => return true,
            Some(n) => current = n.parent(),
            None => return false,
        }
    }
    false
}

/// Collect the text of `attribute_item` siblings immediately preceding `node`.
///
/// In tree-sitter-rust an outer attribute like `#[test]` is a *preceding sibling*
/// of the `function_item` it decorates, not part of it — so the function's
/// signature (its first line, starting at `fn`) never contains the attribute.
/// Entry-point detection needs the attribute to recognize `#[test]` /
/// `#[tokio::test]` functions, so gather the leading attributes here and prepend
/// them to the signature passed to detection (only — the stored signature is
/// unchanged). Returns an empty string when there are no leading attributes.
fn leading_rust_attributes(node: &tree_sitter::Node, source: &[u8]) -> String {
    let mut attrs = String::new();
    let mut sib = node.prev_sibling();
    while let Some(s) = sib {
        if s.kind() == "attribute_item" {
            if let Ok(text) = s.utf8_text(source) {
                attrs.push_str(text);
                attrs.push('\n');
            }
            sib = s.prev_sibling();
        } else {
            break;
        }
    }
    attrs
}

/// Expand a Rust `use` declaration into one import [`RawReference`] per path.
///
/// The grammar nests arbitrarily (`use a::{b, c::d, e as f, g::*};`), which
/// tree-sitter query patterns cannot flatten, so the whole `use_declaration`
/// node is captured as `@reference.rust_use` and expanded here:
///
/// - `use a::b;`        → `a::b`
/// - `use a::{b, c};`   → `a::b`, `a::c`
/// - `use a::b as c;`   → `a::b` (the path — resolution needs the origin),
///   plus an `ImportAlias` reference binding the local name `c` to the path
/// - `use a::*;`        → `a` (the module path; wildcard members resolve
///   through the module's import edge)
/// - `use a::{b::{c, d}};` → `a::b::c`, `a::b::d`
fn expand_rust_use_imports(
    node: &tree_sitter::Node,
    source: &[u8],
    references: &mut Vec<RawReference>,
) {
    let context = node
        .utf8_text(source)
        .map(first_line)
        .unwrap_or_default()
        .to_string();
    if let Some(argument) = node.child_by_field_name("argument") {
        expand_rust_use_tree(&argument, "", source, &context, references);
    }
}

/// Recursive worker for [`expand_rust_use_imports`]. `prefix` is the path
/// accumulated so far (empty or ending in `::`).
fn expand_rust_use_tree(
    node: &tree_sitter::Node,
    prefix: &str,
    source: &[u8],
    context: &str,
    references: &mut Vec<RawReference>,
) {
    let text = node.utf8_text(source).unwrap_or("").trim();
    match node.kind() {
        "identifier" | "scoped_identifier" | "crate" => {
            if !text.is_empty() {
                references.push(RawReference {
                    name: format!("{prefix}{text}"),
                    kind: ReferenceKind::Import,
                    start_line: node.start_position().row as u32 + 1,
                    context: context.to_string(),
                    receiver: None,
                });
            }
        }
        // `use a::b::{self, c};` — `self` imports the path `a::b` itself.
        "self" if !prefix.is_empty() => {
            references.push(RawReference {
                name: prefix.trim_end_matches("::").to_string(),
                kind: ReferenceKind::Import,
                start_line: node.start_position().row as u32 + 1,
                context: context.to_string(),
                receiver: None,
            });
        }
        "use_as_clause" => {
            // Import the original path; the alias is a local rename, recorded
            // as an ImportAlias reference so the resolver can bind it.
            if let Some(path) = node.child_by_field_name("path") {
                let before = references.len();
                expand_rust_use_tree(&path, prefix, source, context, references);
                // Only bind the alias when the path actually produced an
                // import reference (e.g. bare `self` with no prefix does not).
                if references.len() > before
                    && let Some(alias) = node.child_by_field_name("alias")
                    && let Ok(alias_text) = alias.utf8_text(source)
                {
                    let alias_text = alias_text.trim();
                    if !alias_text.is_empty() {
                        let specifier = references[before].name.clone();
                        references.push(RawReference {
                            name: alias_text.to_string(),
                            kind: ReferenceKind::ImportAlias,
                            start_line: node.start_position().row as u32 + 1,
                            context: specifier,
                            receiver: None,
                        });
                    }
                }
            }
        }
        "use_wildcard" => {
            // `use a::*;` — record the module path itself so imports of the
            // module resolve and wildcard members resolve through them.
            for i in 0..node.named_child_count() {
                if let Some(child) = node.named_child(i as u32) {
                    expand_rust_use_tree(&child, prefix, source, context, references);
                }
            }
        }
        "use_list" => {
            for i in 0..node.named_child_count() {
                if let Some(child) = node.named_child(i as u32) {
                    expand_rust_use_tree(&child, prefix, source, context, references);
                }
            }
        }
        "scoped_use_list" => {
            let path_prefix = node
                .child_by_field_name("path")
                .and_then(|p| p.utf8_text(source).ok())
                .map(|p| format!("{prefix}{}::", p.trim()))
                .unwrap_or_else(|| prefix.to_string());
            if let Some(list) = node.child_by_field_name("list") {
                expand_rust_use_tree(&list, &path_prefix, source, context, references);
            }
        }
        _ => {}
    }
}

/// Infer symbol visibility from name and surrounding source text based on language conventions.
fn infer_visibility(name: &str, node_text: &str, lang: Language, exported: bool) -> Visibility {
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
            if exported || sig.contains("export ") {
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
        // Ruby: handles inline modifier form (`private def foo`).
        // Section-form (`private` on its own line affecting subsequent methods)
        // requires tracking state across definitions, which tree-sitter queries
        // don't support — those methods stay Public (over-approximate, safe).
        Language::Ruby => {
            let sig = first_line(node_text);
            if sig.starts_with("private") || sig.contains("private ") {
                Visibility::Private
            } else if sig.starts_with("protected") || sig.contains("protected ") {
                Visibility::Protected
            } else {
                Visibility::Public
            }
        }
        // PowerShell: class members can have [public]/[private] attributes
        Language::PowerShell => {
            let sig = first_line(node_text);
            if sig.contains("[hidden]") || sig.contains("hidden ") {
                Visibility::Private
            } else {
                Visibility::Public
            }
        }
        // Fortran: PUBLIC/PRIVATE keywords on module members
        Language::Fortran => {
            let sig = first_line(node_text).to_lowercase();
            if sig.contains("private") {
                Visibility::Private
            } else {
                Visibility::Public
            }
        }
        // Pascal: private/public/protected/published sections in class declarations
        Language::Pascal => {
            let sig = first_line(node_text);
            if sig.contains("private") {
                Visibility::Private
            } else if sig.contains("protected") {
                Visibility::Protected
            } else {
                Visibility::Public
            }
        }
        // SystemVerilog: local/protected keywords on class members
        Language::SystemVerilog => {
            let sig = first_line(node_text);
            if sig.contains("local ") {
                Visibility::Private
            } else if sig.contains("protected ") {
                Visibility::Protected
            } else {
                Visibility::Public
            }
        }
        // Julia: `export` is a module-level statement, not part of function signatures.
        // Visibility is inferred since we can't detect it from the definition node text.
        // SQL, HCL, COBOL: no visibility concept
        Language::Julia | Language::Sql | Language::Hcl | Language::Cobol => Visibility::Inferred,
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
    // Arena for intermediate string allocations during tree-sitter traversal.
    // Amortizes hundreds of small heap allocs per file into a single large block,
    // reducing malloc/free pressure when parsing many files concurrently via rayon.
    let arena = Bump::new();
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
    let is_jsx = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e == "tsx" || e == "jsx");
    let query = get_or_compile_query(
        &ts_lang,
        QueryCacheKey {
            lang,
            is_jsx,
            is_type_query: false,
        },
        &query_src,
    )?;
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

            // Keep node_text as a &str borrowing directly from the source slice —
            // utf8_text() returns &str already; .to_string() would be a wasted heap alloc.
            let node_text: &str = node.utf8_text(source_bytes).unwrap_or("");
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
                    // nw-330 / nw-349 (5). `@definition.impl` is a distinct
                    // capture in queries/rust.scm, and mapping it onto the
                    // same SymbolKind::Class as `struct_item` threw that
                    // distinction away at the last step: testdata/rust/simple.rs
                    // minted THREE `Class SensorManager` rows (lines 4, 18, 28)
                    // for ONE type, indistinguishable by name and kind.
                    //
                    // `Extension` already exists in the schema, already
                    // round-trips through the store, and is semantically the
                    // same thing an `impl` block is — a set of members attached
                    // to a type — so the fact the query already carries now has
                    // somewhere to live.
                    "impl" => SymbolKind::Extension,
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

                // Use the arena for the name fallback so we defer the owned-String
                // allocation until we know this symbol passes the dedup check.
                let name_arena: &str = match &name_text {
                    Some(n) => arena.alloc_str(n),
                    None => node_text,
                };
                let name = name_arena.to_string();

                // nw-291 (M4): `_` is a DISCARD binding, not a name. Rust's
                // `const _: () = assert!(..)`, Go's blank identifier and JS's
                // `const _ = require('lodash')` all produced a graph symbol
                // literally called `_`, which nothing can import, call or
                // reference by name — and which `dead-code` then reported as a
                // high-confidence unreachable symbol.
                //
                // The guard lives here, next to the dedup, rather than in
                // `rust.scm` / `javascript.scm`, because the property is
                // language-independent: it holds for every query pattern in
                // every one of the 49 grammars at once. Only the exact name
                // `_` is filtered — `_helper` is a real, addressable name that
                // merely follows a private-by-convention spelling.
                if name == "_" {
                    continue;
                }

                if !seen_symbols.insert((name.clone(), start_line)) {
                    continue;
                }

                let content_hash = sha256_hex(node_text);
                let signature = signature_line(node_text, lang_str);

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
                    // Rust `#[test]`/`#[tokio::test]` attributes are preceding
                    // siblings, not part of the signature — prepend them (for
                    // detection only) so inline `#[cfg(test)]`-module tests are
                    // flagged TestEntry instead of being invisible to RTS.
                    let detect_sig: std::borrow::Cow<str> = if lang_str == "rust" {
                        let attrs = leading_rust_attributes(&node, source_bytes);
                        if attrs.is_empty() {
                            std::borrow::Cow::Borrowed(signature.as_str())
                        } else {
                            std::borrow::Cow::Owned(format!("{attrs}{signature}"))
                        }
                    } else {
                        std::borrow::Cow::Borrowed(signature.as_str())
                    };
                    detect_entry_point(
                        &name,
                        &file_path_str,
                        kind_label,
                        Some(&detect_sig),
                        lang_str,
                    )
                };

                let visibility =
                    infer_visibility(&name, node_text, lang, has_export_ancestor(&node));
                let type_info = extract_type_info(&signature, lang);
                let parent_name = if matches!(kind, SymbolKind::Method | SymbolKind::Property) {
                    find_parent_name(&node, source_bytes)
                } else {
                    None
                };
                let scope_chain = extract_scope_chain(node, source, lang_str);
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
                    scope_chain,
                });
            } else if let Some(kind_str) = capture_name.strip_prefix("reference.") {
                // Rust `use` declarations are captured whole and expanded here
                // into one import reference per path (list/wildcard/alias forms
                // included); see expand_rust_use_imports.
                if kind_str == "rust_use" {
                    expand_rust_use_imports(&node, source_bytes, &mut references);
                    continue;
                }
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

                let name = name_text.clone().unwrap_or_else(|| strip_quotes(node_text));

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
                        .map(|s| arena.alloc_str(s) as &str);

                    // Strategy 2: direct object field (Java method_invocation)
                    let from_direct_object = if from_function_child.is_none() {
                        node.child_by_field_name("object")
                            .and_then(|obj| obj.utf8_text(source_bytes).ok())
                            .map(|s| arena.alloc_str(s) as &str)
                    } else {
                        None
                    };

                    // Strategy 3: direct receiver field (Ruby call)
                    let from_receiver_field =
                        if from_function_child.is_none() && from_direct_object.is_none() {
                            node.child_by_field_name("receiver")
                                .and_then(|r| r.utf8_text(source_bytes).ok())
                                .map(|s| arena.alloc_str(s) as &str)
                        } else {
                            None
                        };

                    // Strategy 4: qualifying path of a scoped call (nw-152).
                    // `a::b::c()` and `A.b.c()` parse as a scoped/qualified
                    // function node whose `path` (or `scope`) field is the
                    // qualifier. The .scm captures only the trailing identifier,
                    // so without this the qualifier is lost and the resolver has
                    // nothing but a bare name to work with -- which is why a
                    // fully-qualified call with no matching `use` resolved to
                    // nothing at all.
                    let from_scoped_path = if from_function_child.is_none()
                        && from_direct_object.is_none()
                        && from_receiver_field.is_none()
                    {
                        node.child_by_field_name("function")
                            .filter(|f| {
                                let k = f.kind();
                                k.contains("scoped") || k.contains("qualified")
                            })
                            .and_then(|f| {
                                f.child_by_field_name("path")
                                    .or_else(|| f.child_by_field_name("scope"))
                            })
                            .and_then(|p| p.utf8_text(source_bytes).ok())
                            .map(|s| arena.alloc_str(s) as &str)
                    } else {
                        None
                    };

                    // Convert the chosen arena-backed &str to an owned String only once,
                    // at the point where we need it for RawReference.
                    from_function_child
                        .or(from_direct_object)
                        .or(from_receiver_field)
                        .or(from_scoped_path)
                        .map(|s| s.to_string())
                } else if matches!(kind, ReferenceKind::ReadAccess | ReferenceKind::WriteAccess) {
                    // nw-308: a FIELD ACCESS has a receiver too, and until this
                    // existed it was thrown away — extraction was gated to
                    // `Call`, so every ReadAccess/WriteAccess reference carried
                    // `receiver: None` and the resolver's receiver gate waved
                    // all of them through. Measured on this repo: with the gate
                    // covering CALLS only, `impact collect` fell from 162 to 9
                    // while `hubs` in-degree barely moved (771 -> 618), because
                    // `hubs` counts ALL_SYMBOL_EDGE_TYPES — the residual was
                    // ACCESSES and USES, the exact edges the gate could not see.
                    //
                    // Here the CAPTURED node is the member/field/selector node
                    // itself rather than a call wrapping one, so the object is a
                    // direct child rather than one level down under `function`.
                    let k = node.kind();
                    if k.contains("field")
                        || k.contains("member")
                        || k.contains("selector")
                        || k.contains("attribute")
                        || k.contains("navigation")
                    {
                        node.child_by_field_name("object")
                            .or_else(|| node.child_by_field_name("value"))
                            .or_else(|| node.child_by_field_name("operand"))
                            .or_else(|| node.child_by_field_name("expression"))
                            .and_then(|obj| obj.utf8_text(source_bytes).ok())
                            .map(|s| s.to_string())
                    } else {
                        None
                    }
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

    // nw-151: recover calls written inside Rust macro bodies.
    if lang == Language::Rust {
        collect_rust_macro_calls(tree.root_node(), source_bytes, &mut references);
    }

    // nw-291 follow-up: recover constant/static reads written as a bare name.
    if let Some(rules) = constant_read_rules(lang) {
        collect_constant_reads(tree.root_node(), source_bytes, &rules, &mut references);
    }

    // nw-291 follow-up: promote functions a Rust harness macro REGISTERS as
    // roots. See RUST_ENTRY_POINT_REGISTRATION_MACROS.
    if lang == Language::Rust {
        let registered = collect_rust_registered_entry_points(tree.root_node(), source_bytes);
        if !registered.is_empty() {
            for symbol in &mut symbols {
                if !symbol.is_entry_point
                    && symbol.kind == SymbolKind::Function
                    && registered.contains(symbol.name.as_str())
                {
                    symbol.is_entry_point = true;
                    symbol.entry_point_kind = Some(EntryPointKind::Main);
                }
            }
        }
    }

    // nw-155: promote symbols named in an `export { .. }` clause to Public.
    //
    // has_export_ancestor only recognises the INLINE form, where the declaration
    // is nested under an export_statement. The list form is a separate statement
    // elsewhere in the file, so `function _init() {}` + `export { _init as
    // default }` left _init marked Private -- and dead_code::infer_confidence
    // returns High for anything private, so a module's DEFAULT EXPORT was
    // reported as high-confidence dead code. All 154 high-confidence results on
    // the reference graph were of this shape.
    if matches!(
        lang,
        Language::JavaScript
            | Language::TypeScript
            | Language::Vue
            | Language::Svelte
            | Language::Astro
    ) {
        let exported = collect_export_clause_names(tree.root_node(), source_bytes);
        if !exported.is_empty() {
            for symbol in &mut symbols {
                if symbol.visibility == Visibility::Private
                    && exported.contains(symbol.name.as_str())
                {
                    symbol.visibility = Visibility::Public;
                }
            }
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

/// Collect the LOCAL names bound by every `export { .. }` clause in a file.
///
/// For `export { alpha, beta as default }` this yields {"alpha", "beta"} — the
/// local name is what identifies the declaration, not the exported alias.
fn collect_export_clause_names<'a>(
    node: tree_sitter::Node<'a>,
    source_bytes: &'a [u8],
) -> std::collections::HashSet<&'a str> {
    let mut names = std::collections::HashSet::new();
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.kind() == "export_specifier"
            && let Some(local) = current.child_by_field_name("name")
            && let Ok(text) = local.utf8_text(source_bytes)
        {
            names.insert(text);
        }
        let mut cursor = current.walk();
        for child in current.children(&mut cursor) {
            stack.push(child);
        }
    }
    names
}

/// Rust macros that REGISTER functions as harness entry points, whose argument
/// list the parser otherwise cannot see.
///
/// `criterion_group!(benches, bench_a, bench_b);` plus `criterion_main!(benches);`
/// is how every Criterion benchmark target names its roots. Criterion GENERATES
/// the `main` that calls them, so `bench_a` has no caller anywhere in the source
/// tree and `dead-code` reported every one of them: on a fresh index of this
/// repo the entire top of the unreachable list was `benches/*.rs`, all of it
/// registered and none of it dead.
///
/// WHERE ELSE DOES THIS PROPERTY NEED TO HOLD? Anywhere a function is invoked
/// only from inside a token tree. `collect_rust_macro_calls` (nw-151) already
/// recovers the `foo(..)` shape there and deliberately does NOT treat a BARE
/// identifier as a call, because in `println!("{}", x)` it is a variable. That
/// judgement is correct in general and wrong for exactly this family of macros,
/// so the family is NAMED here rather than the general rule being loosened —
/// adding a harness is a one-line data change, and no other call site moves.
///
/// A name is promoted only when the same file also defines a FUNCTION by that
/// name, so a group label (`benches` names no function), a config key in the
/// braced `criterion_group! { name = ..; targets = .. }` form, or a stray
/// literal cannot invent an entry point out of nothing.
const RUST_ENTRY_POINT_REGISTRATION_MACROS: &[&str] = &["criterion_group", "criterion_main"];

/// Collect every bare identifier appearing inside a
/// [`RUST_ENTRY_POINT_REGISTRATION_MACROS`] invocation.
///
/// Returns names, not symbols: the caller intersects them with the file's own
/// function definitions, which is what keeps this from inventing entry points.
fn collect_rust_registered_entry_points<'a>(
    root: tree_sitter::Node<'a>,
    source_bytes: &'a [u8],
) -> std::collections::HashSet<&'a str> {
    let mut names = std::collections::HashSet::new();
    let mut stack = vec![root];
    while let Some(current) = stack.pop() {
        if current.kind() == "macro_invocation"
            && let Some(macro_name) = current.child_by_field_name("macro")
            && let Ok(text) = macro_name.utf8_text(source_bytes)
            && RUST_ENTRY_POINT_REGISTRATION_MACROS.contains(&text)
        {
            let mut inner = current.walk();
            for part in current.children(&mut inner) {
                if part.kind() == "token_tree" {
                    collect_identifiers_in_token_tree(part, source_bytes, &mut names);
                }
            }
            continue;
        }
        let mut cursor = current.walk();
        for child in current.children(&mut cursor) {
            stack.push(child);
        }
    }
    names
}

/// Every `identifier` token anywhere under `node`, including nested token trees.
fn collect_identifiers_in_token_tree<'a>(
    node: tree_sitter::Node<'a>,
    source_bytes: &'a [u8],
    out: &mut std::collections::HashSet<&'a str>,
) {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.kind() == "identifier"
            && let Ok(text) = current.utf8_text(source_bytes)
        {
            out.insert(text);
        }
        let mut cursor = current.walk();
        for child in current.children(&mut cursor) {
            stack.push(child);
        }
    }
}

/// Emit CALL references for calls written inside Rust macro bodies (nw-151).
///
/// tree-sitter-rust parses macro arguments as an opaque `token_tree`, so
/// nothing inside `assert!(..)` is ever a `call_expression` and the .scm
/// captures only the macro's own name. In Rust test suites assertions are the
/// dominant call site, which hollowed out the whole test-to-code call graph
/// that `affected-tests` is built on: of `rollback_current`'s four callers,
/// the three that call it only inside `assert!` produced no edge at all.
///
/// Inside a `token_tree` a call is an `identifier` whose next sibling is a
/// `token_tree` -- `foo(..)` parses as `(identifier) (token_tree ..)`. A bare
/// identifier with no following token_tree (a variable passed to `println!`)
/// is correctly not treated as a call.
///
/// This is deliberately shallow: it recovers the call NAME, leaving the normal
/// resolver priority chain to decide what it points at. A tuple-struct pattern
/// such as `Some(x)` in `matches!` also matches this shape; it resolves like
/// any other bare name, and against std types resolves to nothing.
fn collect_rust_macro_calls(
    node: tree_sitter::Node<'_>,
    source_bytes: &[u8],
    references: &mut Vec<RawReference>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "macro_invocation" {
            let mut inner = child.walk();
            for part in child.children(&mut inner) {
                if part.kind() == "token_tree" {
                    collect_calls_in_token_tree(part, source_bytes, references);
                }
            }
        }
        collect_rust_macro_calls(child, source_bytes, references);
    }
}

fn collect_calls_in_token_tree(
    tree_node: tree_sitter::Node<'_>,
    source_bytes: &[u8],
    references: &mut Vec<RawReference>,
) {
    let mut cursor = tree_node.walk();
    let children: Vec<tree_sitter::Node<'_>> = tree_node.children(&mut cursor).collect();
    for (index, child) in children.iter().enumerate() {
        if child.kind() == "identifier"
            && children
                .get(index + 1)
                .is_some_and(|next| next.kind() == "token_tree")
            && let Ok(name) = child.utf8_text(source_bytes)
        {
            references.push(RawReference {
                name: name.to_string(),
                kind: ReferenceKind::Call,
                start_line: child.start_position().row as u32 + 1,
                context: String::new(),
                receiver: None,
            });
        }
        if child.kind() == "token_tree" {
            collect_calls_in_token_tree(*child, source_bytes, references);
        }
    }
}

/// How to recognise a bare constant read in one language.
struct ConstantReadRules {
    /// Subtrees not to descend into. An import/use path is already expanded
    /// into IMPORT references, and re-reading its segments here would emit a
    /// second, weaker edge for the same syntax.
    skip_subtrees: &'static [&'static str],
    /// `(parent_kind, field_name)` pairs identifying an identifier that is the
    /// DECLARATION's own name rather than a read of it. Without these the
    /// declaration site reads as a reference to itself and every constant
    /// acquires a self-edge.
    definition_sites: &'static [(&'static str, &'static str)],
}

/// Emit ACCESSES references for constants read by BARE NAME (nw-291 follow-up).
///
/// # The gap
///
/// Every language's `.scm` spells `@reference.read_access` as `obj.field` — a
/// field expression. Nothing anywhere captures a plain identifier standing on
/// its own, so a constant read is invisible to the graph unless it happens to
/// be called (`FOO()`) or type-referenced. Measured on a fresh index of this
/// repo: 2,567 `Constant` symbols, 723 with any non-structural inbound edge. Of
/// the top 1,000 `dead-code` candidates, 542 were `Constant`, and every sampled
/// false positive — `SESSION_TTL_SECS`, `TS_QUERY`, `TOOL_VALIDATORS`,
/// `NODE_ROUTE_RECEIVERS`, `CALL_EXCLUDE`, and Python's `DARK` / `REPO_ORDER` /
/// `TOOL_LABELS` — is read by bare name IN ITS OWN FILE, which the resolver's
/// Priority-1 same-file tier resolves the instant a reference exists to resolve.
///
/// # Why the name shape is the filter
///
/// Capturing every identifier would make a variable read indistinguishable from
/// a constant read and hand the resolver a name for every local binding in the
/// tree. `SCREAMING_SNAKE_CASE` is not merely a preference: `rustc`'s
/// `non_upper_case_globals` warns on a const or static spelled otherwise and
/// `non_snake_case` warns on a local spelled this way; PEP 8, the Google JS/TS
/// style guide and the Java conventions say the same for module and `static
/// final` constants. So the shape is an enforced discriminator, not a guess —
/// 5,106 occurrences across this repo against 25,834 existing CALLS edges.
///
/// Like `collect_rust_macro_calls`, this is deliberately shallow: it recovers
/// the NAME and leaves the normal resolver priority chain to decide what it
/// points at. A name matching nothing resolves to nothing and yields no edge.
///
/// # WHERE ELSE DOES THIS PROPERTY NEED TO HOLD?
///
/// The `obj.field`-only spelling of `read_access` is identical in `go.scm`,
/// `java.scm`, `javascript.scm`, `python.scm`, `rust.scm` and `typescript.scm`,
/// so the gap is every one of them. [`constant_read_rules`] is the whole
/// per-language surface: two node-kind lists per entry.
///
/// Deliberately absent:
/// - **Go**, whose exported constants are `CamelCase` and therefore
///   indistinguishable by shape from every other identifier. Go needs a
///   different discriminator and does not get a guessed one here.
/// - **Java** and the rest, which have the gap and would take the JS-shaped
///   rule, but for which this branch has no indexed corpus to measure the
///   before/after false-positive rate on. Every language listed below was
///   measured; claiming the others without measuring is the exact thing this
///   branch exists to stop doing.
fn constant_read_rules(lang: Language) -> Option<ConstantReadRules> {
    match lang {
        Language::Rust => Some(ConstantReadRules {
            skip_subtrees: &["use_declaration"],
            definition_sites: &[("const_item", "name"), ("static_item", "name")],
        }),
        Language::Python => Some(ConstantReadRules {
            skip_subtrees: &[
                "import_statement",
                "import_from_statement",
                "future_import_statement",
            ],
            definition_sites: &[("assignment", "left"), ("augmented_assignment", "left")],
        }),
        Language::TypeScript | Language::JavaScript => Some(ConstantReadRules {
            skip_subtrees: &["import_statement", "export_clause"],
            definition_sites: &[("variable_declarator", "name")],
        }),
        _ => None,
    }
}

fn collect_constant_reads(
    root: tree_sitter::Node<'_>,
    source_bytes: &[u8],
    rules: &ConstantReadRules,
    references: &mut Vec<RawReference>,
) {
    let mut stack = vec![root];
    while let Some(current) = stack.pop() {
        if rules.skip_subtrees.contains(&current.kind()) {
            continue;
        }
        if current.kind() == "identifier"
            && let Ok(name) = current.utf8_text(source_bytes)
            && is_screaming_snake_case(name)
            && !is_own_definition_name(current, rules.definition_sites)
        {
            references.push(RawReference {
                name: name.to_string(),
                kind: ReferenceKind::ReadAccess,
                start_line: current.start_position().row as u32 + 1,
                context: String::new(),
                receiver: None,
            });
        }
        let mut cursor = current.walk();
        for child in current.children(&mut cursor) {
            stack.push(child);
        }
    }
}

/// `MAX`, `SESSION_TTL_SECS`, `HTTP2_MAX` — upper-case, digits and underscores
/// only, starting with a letter, at least two characters so a single-letter
/// generic parameter cannot qualify.
fn is_screaming_snake_case(name: &str) -> bool {
    name.len() >= 2
        && name.starts_with(|c: char| c.is_ascii_uppercase())
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// True when `node` is the name of the declaration that BINDS it, per the
/// language's [`ConstantReadRules::definition_sites`].
fn is_own_definition_name(node: tree_sitter::Node<'_>, sites: &[(&str, &str)]) -> bool {
    node.parent().is_some_and(|parent| {
        sites.iter().any(|(kind, field)| {
            parent.kind() == *kind && parent.child_by_field_name(field) == Some(node)
        })
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

    let query = match get_or_compile_query(
        ts_lang,
        QueryCacheKey {
            lang,
            is_jsx: false,
            is_type_query: true,
        },
        query_src,
    ) {
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
                result.skipped.push(SkippedFile::new(
                    path_str,
                    SkipReasonCode::ParseError,
                    e.to_string(),
                ));
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

    // ── visibility tests (nw-063) ───────────────────────────────────────────

    /// `export`ed TS/JS declarations are the module's public API. Tree-sitter
    /// wraps them in a parent `export_statement`, so the export keyword is NOT
    /// in the declaration node's own text — visibility must consult ancestors.
    #[test]
    fn ts_exported_symbols_are_public() {
        let src = "export function charge(id: string, amount: number): boolean {\n  return true;\n}\nfunction helper(): void {}\nexport const rate = (x: number): number => x * 2;\nexport default function main(): void {}\n";
        let parsed = parse_source(Path::new("api.ts"), src).unwrap();
        let vis = |name: &str| {
            parsed
                .symbols
                .iter()
                .find(|s| s.name == name)
                .unwrap_or_else(|| {
                    panic!(
                        "{name} not extracted: {:?}",
                        parsed.symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
                    )
                })
                .visibility
        };
        assert_eq!(
            vis("charge"),
            Visibility::Public,
            "export function must be Public"
        );
        assert_eq!(
            vis("helper"),
            Visibility::Private,
            "non-exported stays Private"
        );
        assert_eq!(
            vis("rate"),
            Visibility::Public,
            "export const arrow must be Public"
        );
        assert_eq!(
            vis("main"),
            Visibility::Public,
            "export default must be Public"
        );
    }

    // ── query cache tests ───────────────────────────────────────────────────

    /// Parsing two distinct files with the same language should both succeed,
    /// exercising the cache path on the second call.
    #[test]
    fn query_cache_second_parse_succeeds() {
        let source_a = "fn foo() {}";
        let source_b = "fn bar(x: i32) -> i32 { x }";

        let result_a = parse_source(Path::new("a.rs"), source_a).unwrap();
        let result_b = parse_source(Path::new("b.rs"), source_b).unwrap();

        assert!(
            result_a.symbols.iter().any(|s| s.name == "foo"),
            "expected foo in first parse"
        );
        assert!(
            result_b.symbols.iter().any(|s| s.name == "bar"),
            "expected bar in second parse (cache path)"
        );
    }

    /// TSX and TS files use different query sources; both should parse correctly
    /// even though they share the same `Language::TypeScript`.
    #[test]
    fn query_cache_tsx_vs_ts_both_succeed() {
        let ts_source = "function greet(): string { return 'hi'; }";
        let tsx_source = "function App() { return <div />; }";

        let ts_result = parse_source(Path::new("comp.ts"), ts_source).unwrap();
        let tsx_result = parse_source(Path::new("comp.tsx"), tsx_source).unwrap();

        assert!(
            ts_result.symbols.iter().any(|s| s.name == "greet"),
            "expected greet in .ts parse"
        );
        assert!(
            tsx_result.symbols.iter().any(|s| s.name == "App"),
            "expected App in .tsx parse"
        );
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
    fn parse_rust_flags_inline_cfg_test_functions_as_test_entries() {
        // nw-085: Rust unit tests live in an inline `#[cfg(test)] mod tests`
        // inside a non-test src file, decorated with `#[test]` — a preceding
        // sibling attribute the signature never captured. They must be flagged
        // TestEntry so regression-test selection can reach them.
        let source = "pub fn util() -> u32 { 1 }\n\
                      #[cfg(test)]\n\
                      mod tests {\n\
                          use super::*;\n\
                          #[test]\n\
                          fn test_util_returns_one() { assert_eq!(util(), 1); }\n\
                          #[tokio::test]\n\
                          async fn test_util_async() { let _ = util(); }\n\
                      }\n";
        let parsed = parse_source(Path::new("src/util.rs"), source).unwrap();

        for name in ["test_util_returns_one", "test_util_async"] {
            let sym = parsed
                .symbols
                .iter()
                .find(|s| s.name == name)
                .unwrap_or_else(|| {
                    panic!(
                        "should find '{name}'; got: {:?}",
                        parsed.symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
                    )
                });
            assert_eq!(
                sym.entry_point_kind,
                Some(EntryPointKind::TestEntry),
                "'{name}' should be a TestEntry (has a #[test]/#[tokio::test] attribute)"
            );
        }

        // A plain non-test function must NOT be flagged.
        let util = parsed.symbols.iter().find(|s| s.name == "util").unwrap();
        assert_eq!(util.entry_point_kind, None, "util() is not a test entry");
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

    #[test]
    fn python_module_level_variable_span_is_the_assignment_not_the_file() {
        // nw-326 / F-CODE-4: queries/python.scm attached `@definition.variable`
        // to the `(module)` node, so parse.rs recorded start_line=1 and
        // end_line=EOF+1 for every module-level assignment. `read-symbols` then
        // returned the whole file for a one-line variable — strictly worse than
        // reading the file.
        let source = "\
import os


def helper():
    return 1


LOGGER = os.getcwd()
MAX_RETRIES = 3
";
        let parsed = parse_source(Path::new("mod_vars.py"), source).unwrap();

        let logger = parsed
            .symbols
            .iter()
            .find(|s| s.name == "LOGGER" && s.kind == SymbolKind::Variable)
            .expect("module-level variable LOGGER must be extracted");

        assert_eq!(logger.start_line, 8, "span must start at the assignment");
        assert_eq!(
            logger.end_line, 8,
            "a one-line assignment is a one-line span"
        );

        let max_retries = parsed
            .symbols
            .iter()
            .find(|s| s.name == "MAX_RETRIES" && s.kind == SymbolKind::Variable)
            .expect("module-level variable MAX_RETRIES must be extracted");
        assert_eq!(max_retries.start_line, 9);
        assert_eq!(max_retries.end_line, 9);

        // Two distinct one-line variables must not share a content hash — with
        // the capture on `(module)` both hashed the whole file.
        assert_ne!(
            logger.content_hash, max_retries.content_hash,
            "distinct variables must not share the file's content hash"
        );
        // The signature is the assignment, not the file's first line.
        assert_eq!(logger.signature, "LOGGER = os.getcwd()");

        // Regression guard: functions and classes were already exact and must
        // stay exact.
        let helper = parsed
            .symbols
            .iter()
            .find(|s| s.name == "helper")
            .expect("helper");
        assert_eq!((helper.start_line, helper.end_line), (4, 5));
    }

    #[test]
    fn python_class_and_instance_attribute_spans_are_the_assignment_not_the_class() {
        // nw-326, the "where else" half: queries/python.scm has the SAME
        // container-as-capture shape for class attributes and for
        // `self.x = ...` instance attributes, both anchored on
        // `(class_definition)` — so every attribute got the whole class as its
        // span, its content hash and its signature.
        let source = "\
class Config:
    DEBUG = False
    RETRIES = 3

    def __init__(self):
        self.name = \"config\"
        self.size = 0
";
        let parsed = parse_source(Path::new("config.py"), source).unwrap();
        let prop = |n: &str| {
            parsed
                .symbols
                .iter()
                .find(|s| s.name == n && s.kind == SymbolKind::Property)
                .unwrap_or_else(|| panic!("property {n} must be extracted"))
        };

        assert_eq!((prop("DEBUG").start_line, prop("DEBUG").end_line), (2, 2));
        assert_eq!(
            (prop("RETRIES").start_line, prop("RETRIES").end_line),
            (3, 3)
        );
        assert_eq!((prop("name").start_line, prop("name").end_line), (6, 6));
        assert_eq!((prop("size").start_line, prop("size").end_line), (7, 7));

        assert_ne!(
            prop("DEBUG").content_hash,
            prop("RETRIES").content_hash,
            "distinct attributes must not share the class's content hash"
        );
        // The class-body anchor must be preserved: the attribute still knows
        // which class it belongs to.
        assert_eq!(prop("DEBUG").parent_name.as_deref(), Some("Config"));
        assert_eq!(prop("name").parent_name.as_deref(), Some("Config"));

        // Regression guard: the class itself still spans the whole class.
        let class = parsed
            .symbols
            .iter()
            .find(|s| s.name == "Config" && s.kind == SymbolKind::Class)
            .expect("class Config");
        assert_eq!((class.start_line, class.end_line), (1, 7));
    }

    // ── nw-323 defects C and D: TS/JS reference coverage ───────────────────

    #[test]
    fn ts_export_from_is_captured_as_an_import_reference() {
        // A barrel re-export is an import edge for graph purposes: it is how
        // `NotFoundError` reaches its importers through
        // common/errors.ts -> errors/index.ts -> http-errors.ts.
        let source = "export * from './errors/index.js';\n\
                      export { AppError, isAppError } from './app-error.js';\n\
                      export type { ErrorDetails } from './app-error.js';\n";
        let parsed = parse_source(Path::new("src/common/errors.ts"), source).unwrap();

        let imports: Vec<&str> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Import)
            .map(|r| r.name.as_str())
            .collect();

        assert!(
            imports.contains(&"./errors/index.js"),
            "`export * from` must yield an Import reference; got: {imports:?}"
        );
        assert!(
            imports.contains(&"./app-error.js"),
            "`export {{ … }} from` must yield an Import reference; got: {imports:?}"
        );
    }

    #[test]
    fn js_export_from_is_captured_as_an_import_reference() {
        // Where else does this property hold? javascript.scm has the identical
        // gap; a plain-JS barrel is just as common as a TS one.
        let source = "export * from './a.js';\nexport { b } from './b.js';\n";
        let parsed = parse_source(Path::new("src/index.js"), source).unwrap();
        let imports: Vec<&str> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Import)
            .map(|r| r.name.as_str())
            .collect();
        assert!(imports.contains(&"./a.js"), "got: {imports:?}");
        assert!(imports.contains(&"./b.js"), "got: {imports:?}");
    }

    #[test]
    fn ts_new_expression_is_captured_as_a_call_reference() {
        // `NotFoundError` (56 files) and `NotificationService` (5 constructors)
        // are consumed almost exclusively via `new`. With no capture they have
        // ZERO inbound references before resolution even begins, which is why
        // `impact --min-score 0 --depth 10` still returned 0.
        let source = "import { NotFoundError } from '../../../common/errors.js';\n\
                      export function get(id: string) {\n\
                        throw new NotFoundError('Discrepancy');\n\
                      }\n";
        let parsed = parse_source(Path::new("src/modules/a/service.ts"), source).unwrap();

        assert!(
            parsed
                .references
                .iter()
                .any(|r| r.kind == ReferenceKind::Call && r.name == "NotFoundError"),
            "`new NotFoundError(..)` must yield a Call reference; got: {:?}",
            parsed
                .references
                .iter()
                .map(|r| (&r.kind, &r.name))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn js_new_expression_is_captured_as_a_call_reference() {
        // Where else does this property hold? javascript.scm has the same gap.
        let source = "const s = new NotificationService(deps);\n";
        let parsed = parse_source(Path::new("src/app.js"), source).unwrap();
        assert!(
            parsed
                .references
                .iter()
                .any(|r| r.kind == ReferenceKind::Call && r.name == "NotificationService"),
            "got: {:?}",
            parsed
                .references
                .iter()
                .map(|r| (&r.kind, &r.name))
                .collect::<Vec<_>>()
        );
    }

    // ── nw-291 M4: discard bindings must not become symbols ────────────────

    #[test]
    fn a_discard_binding_is_not_a_symbol() {
        // nw-291 / F-DC-5: `const _: () = assert!(..)` and
        // `const _ = require('lodash')` produced a graph symbol literally named
        // `_`, which then ranked as high-confidence dead code.
        let rust = "const _: () = assert!(true);\npub fn keep() {}\n";
        let parsed = parse_source(Path::new("src/db.rs"), rust).unwrap();
        assert!(
            !parsed.symbols.iter().any(|s| s.name == "_"),
            "a Rust discard const must not be a symbol: {:?}",
            parsed.symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert!(parsed.symbols.iter().any(|s| s.name == "keep"));

        let js = "const _ = require('lodash');\nexport const KEEP = 1;\n";
        let parsed = parse_source(Path::new("src/a.js"), js).unwrap();
        assert!(
            !parsed.symbols.iter().any(|s| s.name == "_"),
            "a JS discard const must not be a symbol: {:?}",
            parsed.symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert!(parsed.symbols.iter().any(|s| s.name == "KEEP"));
    }

    // ── Hash test ──────────────────────────────────────────────────────────

    #[test]
    fn content_hash_is_256bit_hex() {
        let source = r#"function hello() { return 42; }"#;
        let parsed = parse_source(Path::new("test.js"), source).unwrap();
        assert!(
            !parsed.symbols.is_empty(),
            "should parse at least one symbol"
        );
        let hash = &parsed.symbols[0].content_hash;
        assert_eq!(hash.len(), 64, "256-bit hex is 64 chars; got: {hash}");
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
    fn parse_rust_expands_use_lists_wildcards_and_aliases() {
        // Cross-crate/workspace imports come in list, wildcard and alias
        // forms; every path must become an Import reference or the resolver
        // has nothing to resolve against.
        let source = r#"
use nestweaver_engine::rts_eval;
use nestweaver_engine::{alpha, beta};
use nestweaver_engine::store::{self, GraphStore as Store};
use nestweaver_proto::*;
use crate::config::{Settings, load as load_config};
"#;
        let parsed = parse_source(Path::new("server.rs"), source).unwrap();
        let names: Vec<&str> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Import)
            .map(|r| r.name.as_str())
            .collect();

        for expected in [
            "nestweaver_engine::rts_eval",
            "nestweaver_engine::alpha",
            "nestweaver_engine::beta",
            "nestweaver_engine::store",
            "nestweaver_engine::store::GraphStore",
            "nestweaver_proto",
            "crate::config::Settings",
            "crate::config::load",
        ] {
            assert!(
                names.contains(&expected),
                "missing import reference {expected}; got: {names:?}"
            );
        }
        // The alias target must NOT be recorded as an import path.
        assert!(
            !names
                .iter()
                .any(|n| n.ends_with("::Store") || n.ends_with("::load_config")),
            "alias name must not become an import path; got: {names:?}"
        );

        // Aliased imports also emit ImportAlias references binding the local
        // rename to the full original path.
        let aliases: Vec<(&str, &str)> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::ImportAlias)
            .map(|r| (r.name.as_str(), r.context.as_str()))
            .collect();
        assert_eq!(
            aliases,
            [
                ("Store", "nestweaver_engine::store::GraphStore"),
                ("load_config", "crate::config::load"),
            ],
            "aliased imports should produce ImportAlias bindings"
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
    fn sv_standalone_function_span_is_the_declaration_not_the_file() {
        // nw-326, "where else does this property hold?": systemverilog.scm had
        // the identical container-as-capture shape with the FILE ROOT as the
        // container, so `compute_checksum` (simple.sv:62-68) was recorded as
        // lines 1-78 with the file's leading `include as its signature. The
        // checked-in snapshot had the defect baked in as expected output.
        let source = fixture("systemverilog/simple.sv");
        let parsed = parse_source(Path::new("simple.sv"), &source).unwrap();

        let f = parsed
            .symbols
            .iter()
            .find(|s| s.name == "compute_checksum")
            .expect("standalone function must be extracted");
        assert_eq!((f.start_line, f.end_line), (62, 68));
        assert!(
            f.signature
                .starts_with("function automatic int compute_checksum"),
            "signature must be the declaration, not the file's first line: {:?}",
            f.signature
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

    // ── Signature: annotations that the grammar folds in are JOINED ───────

    #[test]
    fn an_annotation_folded_into_a_declaration_is_joined_not_dropped_or_kept_alone() {
        // Anchoring Dart methods on `method_declaration` (so the span covers
        // the body) pulls `@override` into the node, and Java's class/method
        // nodes already carried `@Override`/`@RestController`. Neither the
        // annotation nor the declaration may be lost:
        //
        //  - dropping the annotation silently disabled Spring/Flask framework
        //    detection and contract-route derivation, which read
        //    `signature.contains("@RestController")` / `@app.route`;
        //  - keeping only the annotation is what produced `signature:
        //    "@Override"` with no `type_info` on both Java overrides.
        assert_eq!(
            signature_line("@Override\npublic String greet(String n) {\n}", "java"),
            "@Override public String greet(String n) {"
        );
        assert_eq!(
            signature_line(
                "@RestController\n@RequestMapping(\"/api\")\npublic class C {",
                "java"
            ),
            "@RestController public class C {",
            "ONLY the first line is kept, so the set of annotations any consumer \
             can see is exactly what it was before this function existed — \
             making the second one visible mints a class-level @RequestMapping \
             base path as an `ANY` route"
        );
        assert_eq!(
            signature_line("public class Plain {\n}", "java"),
            "public class Plain {",
            "a declaration with no annotation is untouched"
        );
    }

    #[test]
    fn objective_c_at_signs_are_declarations_and_are_never_joined() {
        // `@` is a declaration KEYWORD in Objective-C, not an annotation
        // marker. Joining folded a protocol's first method onto its header:
        // `@protocol GreeterProtocol` became `- (NSString *)greet:...;`.
        assert_eq!(
            signature_line(
                "@protocol GreeterProtocol\n- (NSString *)greet;\n@end",
                "objc"
            ),
            "@protocol GreeterProtocol"
        );
        assert_eq!(
            signature_line(
                "@interface SimpleGreeter : NSObject\n@property (nonatomic) NSString *p;\n@end",
                "objc"
            ),
            "@interface SimpleGreeter : NSObject"
        );
    }

    // ── nw-330 / nw-349 (5): impl blocks need an identity of their own ────

    #[test]
    fn a_rust_impl_block_is_not_the_same_symbol_as_the_struct_it_implements() {
        // `@definition.impl` is a distinct capture in queries/rust.scm, and
        // parse.rs mapped it onto the SAME SymbolKind::Class as `struct_item`.
        // testdata/rust/simple.rs therefore minted THREE `Class SensorManager`
        // rows (lines 4, 18, 28) for one type, indistinguishable by name and
        // kind — so "which impl block is this method from" was unanswerable and
        // the impl rows were structural dead-code candidates.
        let source = "\
pub struct Foo {
    x: u32,
}

pub trait Greet {
    fn hi(&self);
}

impl Greet for Foo {
    fn hi(&self) {}
}

impl Foo {
    fn new() -> Self { Foo { x: 0 } }
}
";
        let parsed = parse_source(Path::new("i.rs"), source).unwrap();
        let foos: Vec<_> = parsed.symbols.iter().filter(|s| s.name == "Foo").collect();
        assert_eq!(
            foos.len(),
            3,
            "struct + two impl blocks: {:?}",
            foos.iter()
                .map(|s| (s.start_line, s.kind))
                .collect::<Vec<_>>()
        );

        let classes: Vec<_> = foos
            .iter()
            .filter(|s| s.kind == SymbolKind::Class)
            .collect();
        assert_eq!(
            classes.len(),
            1,
            "exactly ONE of the three may be a Class — the struct. The impl \
             blocks must carry their own kind or they are indistinguishable \
             from it: {:?}",
            foos.iter()
                .map(|s| (s.start_line, s.kind))
                .collect::<Vec<_>>()
        );
        assert_eq!(classes[0].start_line, 1, "the Class is the struct");
        assert!(
            foos.iter()
                .filter(|s| s.start_line != 1)
                .all(|s| s.kind == SymbolKind::Extension),
            "both impl blocks are Extensions"
        );

        // The trait relationship must stay recoverable STRUCTURALLY, not only
        // from the signature string. nw-330's "the trait is simply absent from
        // the model" is overstated — this reference already existed, and the
        // kind change must not lose it.
        assert!(
            parsed.references.iter().any(|r| r.name == "Greet"
                && r.kind == ReferenceKind::Extends
                && r.start_line == 9),
            "the trait impl must still emit an Extends reference: {:?}",
            parsed
                .references
                .iter()
                .map(|r| (&r.name, r.kind, r.start_line))
                .collect::<Vec<_>>()
        );
    }

    // ── Span coverage: a symbol's span must cover its own body ────────────
    //
    // Six languages recorded `end_line == start_line` for functions that have a
    // body: cpp, dart (via tree-sitter queries anchored on the signature rather
    // than the definition) and cobol, svelte, vue, astro (via line-scanning
    // regex parsers that had no second line to point at). Fixing one and
    // leaving five is how the identical defect survived a previous round, so
    // all six are pinned here together.

    /// Return the (start, end) span of the named symbol, for span assertions.
    fn span_of(filename: &str, source: &str, name: &str) -> (u32, u32) {
        let parsed = parse_source(Path::new(filename), source).unwrap();
        let sym = parsed
            .symbols
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| {
                panic!(
                    "no symbol named `{name}` in {filename}; got {:?}",
                    parsed
                        .symbols
                        .iter()
                        .map(|s| (&s.name, s.kind, s.start_line, s.end_line))
                        .collect::<Vec<_>>()
                )
            });
        (sym.start_line, sym.end_line)
    }

    #[test]
    fn cpp_function_span_covers_the_body_not_just_the_declarator() {
        // queries/cpp.scm anchored @definition.function/@definition.method on
        // `(function_declarator ..)`, which is the signature WITHOUT the body,
        // so every C++ function recorded end_line == start_line.
        // queries/c.scm anchors the same capture on `(function_definition ..)`
        // and has always been correct.
        let source = "\
void setup() {
    SensorManager mgr;
    mgr.initialize();
}
";
        assert_eq!(
            span_of("span.cpp", source, "setup"),
            (1, 4),
            "a 4-line C++ function must span 1..=4"
        );
    }

    #[test]
    fn cpp_out_of_line_method_span_covers_the_body() {
        let source = "\
void SensorManager::initialize() {
    calibrate();
}
";
        assert_eq!(span_of("m.cpp", source, "initialize"), (1, 3));
    }

    #[test]
    fn cpp_inline_method_span_covers_the_body() {
        // The `field_identifier` rule has to keep matching methods DEFINED
        // inline in a class body, which is the common shape in a header.
        let source = "\
class Sensor {
public:
    void calibrate() {
        reset();
    }
};
";
        assert_eq!(span_of("i.cpp", source, "calibrate"), (3, 5));
    }

    #[test]
    fn cpp_bodiless_declarations_are_still_extracted_and_stay_one_line() {
        // The other half of the same change, and the one that would have made
        // this a regression rather than a fix: a C++ HEADER is nothing but
        // declarations. Anchoring only on `function_definition` — the naive
        // reading of "mirror queries/c.scm" — would extract ZERO symbols from
        // every .h in the corpus. Declarations keep their own rule, and are
        // genuinely one line, so `end_line == start_line` is CORRECT for them.
        let source = "\
class SensorManager {
public:
    void initialize();
    double readTemperature();
};

void freeProto(int x);
";
        assert_eq!(span_of("h.cpp", source, "initialize"), (3, 3));
        assert_eq!(span_of("h.cpp", source, "readTemperature"), (4, 4));
        assert_eq!(span_of("h.cpp", source, "freeProto"), (7, 7));

        // And a definition must NOT also match a declaration rule and mint a
        // second, zero-height symbol for the same function.
        let defined = "void setup() {\n    work();\n}\n";
        let parsed = parse_source(Path::new("d.cpp"), defined).unwrap();
        let setups: Vec<_> = parsed
            .symbols
            .iter()
            .filter(|s| s.name == "setup")
            .collect();
        assert_eq!(
            setups.len(),
            1,
            "one definition, one symbol: {:?}",
            setups
                .iter()
                .map(|s| (s.start_line, s.end_line))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn dart_method_span_covers_the_body() {
        // queries/dart.scm anchored @definition.method on `(method_signature
        // ..)`. The top-level `function_declaration` rule right above it was
        // already anchored correctly, so `main` had a real span while every
        // method in the same file did not.
        let source = "\
class Greeter {
  String greet(String name) {
    return 'Hello, $name!';
  }
}
";
        assert_eq!(span_of("s.dart", source, "greet"), (2, 4));
    }

    #[test]
    fn dart_abstract_method_declaration_is_extracted_and_stays_one_line() {
        // The other half, and the reason "just anchor on the declaration" was
        // not enough: an abstract member is a `declaration` wrapping a bare
        // `function_signature`, never a `method_signature`, so
        // testdata/dart/simple.dart's `Greeter.greet` was extracted as NOTHING
        // at all. It has no body, so one line is the correct span.
        let source = "\
abstract class Greeter {
  String greet(String name);
}
";
        assert_eq!(span_of("a.dart", source, "greet"), (2, 2));
    }

    #[test]
    fn cobol_paragraph_span_covers_its_statements() {
        // A COBOL paragraph has no closing delimiter — it runs to the next
        // label. The scanner recorded the label line as both ends, so every
        // paragraph was zero-height and no PERFORM could be placed inside the
        // paragraph that issued it.
        let source = "\
       PROCEDURE DIVISION.
       MAIN-LOGIC SECTION.
           PERFORM INITIALIZE-DATA
           STOP RUN.

       INITIALIZE-DATA.
           MOVE \"World\" TO WS-NAME.

       DISPLAY-RESULT.
           DISPLAY WS-GREETING.
";
        assert_eq!(
            span_of("p.cbl", source, "MAIN-LOGIC"),
            (2, 4),
            "the section ends at its last statement, not at the blank line after it"
        );
        assert_eq!(span_of("p.cbl", source, "INITIALIZE-DATA"), (6, 7));
        assert_eq!(
            span_of("p.cbl", source, "DISPLAY-RESULT"),
            (9, 10),
            "the last symbol runs to the last code line in the file"
        );
    }

    #[test]
    fn svelte_function_span_covers_the_body_and_a_one_liner_does_not_grow() {
        let source = "\
<script>
  export function greet(name) {
    return `Hello, ${name}!`
  }

  export let count = 0

  function handleClick() {
    count += 1
  }
</script>
";
        assert_eq!(span_of("s.svelte", source, "greet"), (2, 4));
        assert_eq!(span_of("s.svelte", source, "handleClick"), (8, 10));
        assert_eq!(
            span_of("s.svelte", source, "count"),
            (6, 6),
            "`export let count = 0` opens no block; inventing a span for it \
             would be the same defect pointing the other way"
        );
    }

    #[test]
    fn vue_function_span_covers_the_body() {
        let source = "\
<script>
export function formatName(name) {
  return name.trim()
}

function handleClick() {
  console.log(formatName())
}
</script>
";
        assert_eq!(span_of("v.vue", source, "formatName"), (2, 4));
        assert_eq!(span_of("v.vue", source, "handleClick"), (6, 8));
    }

    #[test]
    fn astro_function_span_covers_the_body() {
        let source = "\
---
export function getStaticPaths() {
  return [
    { params: { id: '1' } },
  ]
}

function formatTitle(title) {
  return title.toUpperCase()
}
---
<main />
";
        assert_eq!(
            span_of("a.astro", source, "getStaticPaths"),
            (2, 6),
            "nested object/array braces must not close the function early"
        );
        assert_eq!(span_of("a.astro", source, "formatTitle"), (8, 10));
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
                    assert!(!q.capture_names().is_empty(), "{name}: no captures");
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

    // ── Constructor pattern tests ───────────────────────────────────────────

    #[test]
    fn ast_extracts_kotlin_constructor() {
        let source = "fun main() { val user = User() }";
        let parsed = parse_source(Path::new("test.kt"), source).unwrap();
        let binding = parsed.type_bindings.iter().find(|b| b.var_name == "user");
        assert!(
            binding.is_some(),
            "expected user binding: {:?}",
            parsed.type_bindings
        );
        assert_eq!(binding.unwrap().type_name, "User");
        assert!(matches!(binding.unwrap().kind, AstBindingKind::Constructor));
    }

    #[test]
    fn ast_extracts_dart_constructor() {
        let source = "void main() { var user = User(); }";
        let parsed = parse_source(Path::new("test.dart"), source).unwrap();
        let binding = parsed
            .type_bindings
            .iter()
            .find(|b| b.var_name == "user" && b.type_name == "User");
        assert!(
            binding.is_some(),
            "expected User binding: {:?}",
            parsed.type_bindings
        );
        assert!(matches!(binding.unwrap().kind, AstBindingKind::Constructor));
    }

    #[test]
    fn ast_extracts_swift_constructor() {
        let source = "func foo() { let user = User() }";
        let parsed = parse_source(Path::new("test.swift"), source).unwrap();
        let binding = parsed
            .type_bindings
            .iter()
            .find(|b| b.var_name == "user" && b.type_name == "User");
        assert!(
            binding.is_some(),
            "expected User binding: {:?}",
            parsed.type_bindings
        );
        assert!(matches!(binding.unwrap().kind, AstBindingKind::Constructor));
    }

    #[test]
    fn ast_extracts_scala_new_constructor() {
        let source = "object Main { val user = new User() }";
        let parsed = parse_source(Path::new("test.scala"), source).unwrap();
        let binding = parsed
            .type_bindings
            .iter()
            .find(|b| b.var_name == "user" && b.type_name == "User");
        assert!(
            binding.is_some(),
            "expected User binding from new expression: {:?}",
            parsed.type_bindings
        );
        assert!(matches!(binding.unwrap().kind, AstBindingKind::Constructor));
    }

    #[test]
    fn ast_extracts_scala_apply_constructor() {
        let source = "object Main { val user = User(\"alice\") }";
        let parsed = parse_source(Path::new("test.scala"), source).unwrap();
        let binding = parsed
            .type_bindings
            .iter()
            .find(|b| b.var_name == "user" && b.type_name == "User");
        assert!(
            binding.is_some(),
            "expected User binding from apply-style: {:?}",
            parsed.type_bindings
        );
        assert!(matches!(binding.unwrap().kind, AstBindingKind::Constructor));
    }

    #[test]
    fn ast_extracts_csharp_new_constructor() {
        let source = "class Foo { void Run() { var user = new User(); } }";
        let parsed = parse_source(Path::new("test.cs"), source).unwrap();
        let binding = parsed
            .type_bindings
            .iter()
            .find(|b| b.var_name == "user" && b.type_name == "User");
        assert!(
            binding.is_some(),
            "expected User binding: {:?}",
            parsed.type_bindings
        );
        assert!(matches!(binding.unwrap().kind, AstBindingKind::Constructor));
    }
}
#[cfg(test)]
mod macro_call_tests {
    use super::*;
    use std::path::Path;

    /// nw-151: assertions are the dominant call site in Rust test suites, and
    /// tree-sitter parses macro arguments as an opaque token_tree, so calls
    /// inside `assert!(..)` produced no CALLS edge at all. That hollowed out
    /// the test-to-code graph `affected-tests` depends on.
    #[test]
    fn calls_inside_macro_bodies_are_recovered() {
        let src = "fn t() {\n    assert!(rollback_current(&a, &b).is_ok());\n    assert_eq!(read_current(&r), None);\n    println!(\"{}\", plain_variable);\n}\n";
        let parsed = parse_source(Path::new("src/t.rs"), src).expect("parses");
        let calls: Vec<&str> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::Call)
            .map(|r| r.name.as_str())
            .collect();

        for expected in ["rollback_current", "read_current"] {
            assert!(calls.contains(&expected), "missing {expected} in {calls:?}");
        }
        // A bare identifier passed to a macro is NOT a call.
        assert!(
            !calls.contains(&"plain_variable"),
            "a non-call identifier must not become a call: {calls:?}"
        );
    }

    /// The recovered call must carry the line it appears on, not the macro's.
    #[test]
    fn a_recovered_macro_call_keeps_its_own_line() {
        let src = "fn t() {\n\n\n    assert!(deep_call());\n}\n";
        let parsed = parse_source(Path::new("src/t.rs"), src).expect("parses");
        let call = parsed
            .references
            .iter()
            .find(|r| r.name == "deep_call")
            .expect("recovered call");
        assert_eq!(call.start_line, 4);
    }
}
#[cfg(test)]
mod js_const_scope_tests {
    use super::*;
    use std::path::Path;

    /// nw-150: a block-scoped const is not addressable from outside its block,
    /// so it must not become a graph symbol. Indexing them made
    /// `const where = {..}`, declared inside an else-branch, the single
    /// most-depended-on symbol in a 193k-symbol graph.
    #[test]
    fn only_module_scope_consts_become_symbols() {
        let src = concat!(
            "const moduleLevel = { a: 1 };\n",
            "export const exported = { b: 2 };\n",
            "function handler(x) {\n",
            "  if (x) {\n",
            "    const blockLocal = { class_id: x };\n",
            "    return blockLocal;\n",
            "  }\n",
            "  const functionLocal = 3;\n",
            "  return functionLocal;\n",
            "}\n",
        );
        let parsed = parse_source(Path::new("src/a.js"), src).expect("parses");
        let consts: Vec<&str> = parsed
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Constant)
            .map(|s| s.name.as_str())
            .collect();

        assert!(consts.contains(&"moduleLevel"), "got {consts:?}");
        assert!(consts.contains(&"exported"), "got {consts:?}");
        assert!(
            !consts.contains(&"blockLocal"),
            "a block-local const must not be a symbol: {consts:?}"
        );
        assert!(
            !consts.contains(&"functionLocal"),
            "a function-local const must not be a symbol: {consts:?}"
        );
    }
}
#[cfg(test)]
mod export_clause_tests {
    use super::*;
    use std::path::Path;

    /// nw-155: `export { .. }` is a separate statement from the declaration, so
    /// has_export_ancestor never saw it and the symbol stayed Private. Since
    /// dead_code::infer_confidence returned High for anything private, a
    /// module's DEFAULT EXPORT was reported as high-confidence dead code --
    /// all 154 high-confidence results on the reference graph were of this
    /// shape. `private` no longer promotes to High, but Public is still what
    /// makes this row LOW rather than merely Medium, so the property below is
    /// still the one that matters.
    #[test]
    fn export_clause_names_become_public() {
        let src = concat!(
            "function _helper() { return 1; }\n",
            "function plain() { return 2; }\n",
            "function untouched() { return 3; }\n",
            "export function direct() { return 4; }\n",
            "export { _helper, plain as default };\n",
        );
        let parsed = parse_source(Path::new("src/w.js"), src).expect("parses");
        let vis = |name: &str| {
            parsed
                .symbols
                .iter()
                .find(|s| s.name == name)
                .unwrap_or_else(|| panic!("missing {name}"))
                .visibility
        };
        assert_eq!(vis("_helper"), Visibility::Public, "named in export clause");
        assert_eq!(vis("plain"), Visibility::Public, "aliased as default");
        assert_eq!(
            vis("direct"),
            Visibility::Public,
            "inline export, unchanged"
        );
        assert_eq!(
            vis("untouched"),
            Visibility::Private,
            "a symbol that is genuinely not exported must stay Private"
        );
    }

    /// A file with no export clause must be completely unaffected.
    #[test]
    fn a_file_without_an_export_clause_is_unchanged() {
        let parsed =
            parse_source(Path::new("src/w.js"), "function solo() { return 1; }\n").expect("parses");
        assert_eq!(parsed.symbols[0].visibility, Visibility::Private);
    }
}

/// nw-291 follow-up. Three reachability gaps measured on a fresh Rust index,
/// each of which made a live symbol look unreachable:
/// registration macros, bare constant reads, and module-top-level invocation.
#[cfg(test)]
mod reachability_recovery_tests {
    use super::*;
    use std::path::Path;

    fn parse(name: &str, src: &str) -> ParsedFile {
        parse_source(Path::new(name), src).expect("parses")
    }

    fn entry_points(parsed: &ParsedFile) -> Vec<&str> {
        let mut names: Vec<&str> = parsed
            .symbols
            .iter()
            .filter(|s| s.is_entry_point)
            .map(|s| s.name.as_str())
            .collect();
        names.sort_unstable();
        names
    }

    fn reads(parsed: &ParsedFile) -> Vec<&str> {
        let mut names: Vec<&str> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::ReadAccess)
            .map(|r| r.name.as_str())
            .collect();
        names.sort_unstable();
        names
    }

    /// Criterion GENERATES the `main` that calls these, so `bench_cold_index`
    /// has no caller anywhere in the source tree. On a fresh index of this repo
    /// the top 15 dead-code candidates were `benches/*.rs` and 15 of 15 were
    /// registered, live benchmarks.
    #[test]
    fn criterion_registered_benches_are_entry_points() {
        let src = concat!(
            "fn bench_cold_index(c: &mut Criterion) { synth(); }\n",
            "fn bench_warm_noop(c: &mut Criterion) {}\n",
            "fn synth() {}\n",
            "fn genuinely_unused() {}\n",
            "criterion_group!(benches, bench_cold_index, bench_warm_noop);\n",
            "criterion_main!(benches);\n",
        );
        let parsed = parse("benches/index_benchmarks.rs", src);
        assert_eq!(
            entry_points(&parsed),
            vec!["bench_cold_index", "bench_warm_noop"]
        );
        // `benches` is the group LABEL — it names no function, so it must not
        // conjure one, and `synth` is reached by a normal CALLS edge rather
        // than promoted here.
        assert!(
            !parsed.symbols.iter().any(|s| s.name == "benches"),
            "a group label must not become a symbol"
        );
    }

    /// The braced form carries config keys, none of which name a function.
    #[test]
    fn a_registration_macro_cannot_invent_an_entry_point() {
        let src = concat!(
            "fn bench_one(c: &mut Criterion) {}\n",
            "criterion_group! {\n",
            "    name = benches;\n",
            "    config = Criterion::default();\n",
            "    targets = bench_one\n",
            "}\n",
        );
        let parsed = parse("benches/b.rs", src);
        assert_eq!(entry_points(&parsed), vec!["bench_one"]);
    }

    /// `println!("{FOO}")`-style macros aside, every `.scm` spells
    /// `read_access` as `obj.field`, so a bare constant read produced no
    /// reference at all: 2,567 `Constant` symbols on a fresh index, 723 with
    /// any inbound edge, and 542 of the top 1,000 dead-code candidates.
    #[test]
    fn a_rust_constant_read_by_bare_name_is_a_reference() {
        let src = concat!(
            "const SESSION_TTL_SECS: u64 = 60;\n",
            "static MAX: usize = 4;\n",
            "fn use_them() -> u64 {\n",
            "    let n = MAX;\n",
            "    SESSION_TTL_SECS + n as u64\n",
            "}\n",
        );
        let parsed = parse("src/http.rs", src);
        assert_eq!(reads(&parsed), vec!["MAX", "SESSION_TTL_SECS"]);
    }

    /// Without the definition-site guard every constant acquires a self-edge,
    /// which is noise in PageRank and proves nothing about reachability.
    #[test]
    fn a_constants_own_declaration_is_not_a_read_of_it() {
        let parsed = parse("src/only_decl.rs", "const ALONE: u8 = 1;\n");
        assert!(
            reads(&parsed).is_empty(),
            "the declaration read as a reference to itself: {:?}",
            reads(&parsed)
        );
    }

    /// A `use` path is already expanded into IMPORT references; re-reading its
    /// segments would emit a second, weaker edge for the same syntax.
    #[test]
    fn a_use_path_is_not_also_a_constant_read() {
        let parsed = parse("src/u.rs", "use crate::limits::MAX_DEPTH;\nfn f() {}\n");
        assert!(
            reads(&parsed).is_empty(),
            "the import path was double-counted as a read: {:?}",
            reads(&parsed)
        );
    }

    /// WHERE ELSE: the `obj.field`-only spelling is identical in every query
    /// file. Python's module constants were the second-largest group in the
    /// measured top 15 — `DARK`, `REPO_ORDER`, `TOOL_LABELS` in
    /// `benchmarks/charts.py`, each read a dozen times in its own file.
    #[test]
    fn a_python_module_constant_read_is_a_reference() {
        let src = concat!(
            "REPO_ORDER = [\"a\", \"b\"]\n",
            "def pick(data):\n",
            "    return [r for r in REPO_ORDER if r in data]\n",
        );
        let parsed = parse("benchmarks/charts.py", src);
        assert_eq!(reads(&parsed), vec!["REPO_ORDER"]);
    }
}
