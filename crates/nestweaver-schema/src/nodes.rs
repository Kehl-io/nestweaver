use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EntryPointKind {
    Main,
    HttpHandler,
    EventListener,
    TestEntry,
    LambdaHandler,
    CronJob,
    CliCommand,
    /// nw-113: a file's top-level code, run when the module is loaded.
    ModuleInit,
}

impl fmt::Display for EntryPointKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Main => write!(f, "main"),
            Self::HttpHandler => write!(f, "http_handler"),
            Self::EventListener => write!(f, "event_listener"),
            Self::TestEntry => write!(f, "test_entry"),
            Self::LambdaHandler => write!(f, "lambda_handler"),
            Self::CronJob => write!(f, "cron_job"),
            Self::CliCommand => write!(f, "cli_command"),
            Self::ModuleInit => write!(f, "module_init"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SymbolKind {
    Function,
    Class,
    Method,
    Interface,
    Trait,
    Enum,
    Module,
    Extension,
    Constant,
    Property,
    TypeAlias,
    Variable,
}

/// nw-113: name of the per-file symbol that owns top-level calls. Not a valid
/// identifier in JS, TS, Python or PHP, so it cannot collide with a real
/// declaration.
pub const MODULE_OWNER_NAME: &str = "<module>";

/// nw-113: whether a symbol is the parser's per-file `<module>` owner of
/// top-level calls. The single definition every consumer uses: kind `Module`,
/// name [`MODULE_OWNER_NAME`] and entry kind `ModuleInit` together, so a real
/// module (a Rust `mod`, a Python package) is never mistaken for it.
pub fn is_module_owner(
    kind: SymbolKind,
    name: &str,
    entry_point_kind: Option<EntryPointKind>,
) -> bool {
    kind == SymbolKind::Module
        && name == MODULE_OWNER_NAME
        && entry_point_kind == Some(EntryPointKind::ModuleInit)
}

/// nw-113: the one display label of a `<module>` owner,
/// `<repo-relative path>::<module>`. Every user-facing surface that names an
/// owner uses this, so an owner is never shown as a bare `<module>` (which
/// cannot be told apart from the owners of other files) or with a blank name.
pub fn module_owner_label(file_path: &str) -> String {
    format!("{file_path}::{MODULE_OWNER_NAME}")
}

/// nw-113: a symbol's display name when only its name and path are at hand
/// (a JSON row, a transport struct). `<module>` is not an identifier in any
/// indexed language, so the name alone identifies an owner here; with a full
/// [`Symbol`] prefer [`Symbol::display_name`].
pub fn symbol_display_name<'a>(name: &'a str, file_path: &str) -> std::borrow::Cow<'a, str> {
    if name == MODULE_OWNER_NAME {
        std::borrow::Cow::Owned(module_owner_label(file_path))
    } else {
        std::borrow::Cow::Borrowed(name)
    }
}

/// nw-113: how many connected `<module>` owners a context answer lists; the
/// rest are counted in `module_owners_omitted`. In schema so the engine and
/// the federation merge apply one cap.
pub const MODULE_OWNER_CONTEXT_CAP: usize = 3;

/// nw-113: whether a display label names a `<module>` owner (see
/// [`module_owner_label`]). `<module>` is not an identifier in any indexed
/// language, so the label alone identifies an owner.
pub fn is_module_owner_label(label: &str) -> bool {
    label == MODULE_OWNER_NAME || label.ends_with(&format!("::{MODULE_OWNER_NAME}"))
}

/// nw-113: the one phrase for owners a capped list leaves out.
pub fn omitted_module_owners_phrase(omitted: usize) -> String {
    format!("+{omitted} more files' top-level code")
}

/// nw-113: a caller list with every real caller and at most
/// [`MODULE_OWNER_CONTEXT_CAP`] owner labels, in their order, followed by
/// [`omitted_module_owners_phrase`] when owners were left out. Keeps a
/// helper called at load time from many files to a short list.
pub fn cap_module_owner_labels(labels: &[String]) -> Vec<String> {
    let (owners, mut capped): (Vec<&String>, Vec<String>) = (
        labels.iter().filter(|l| is_module_owner_label(l)).collect(),
        labels
            .iter()
            .filter(|l| !is_module_owner_label(l))
            .cloned()
            .collect(),
    );
    capped.extend(
        owners
            .iter()
            .take(MODULE_OWNER_CONTEXT_CAP)
            .map(|l| (*l).clone()),
    );
    let omitted = owners.len().saturating_sub(MODULE_OWNER_CONTEXT_CAP);
    if omitted > 0 {
        capped.push(omitted_module_owners_phrase(omitted));
    }
    capped
}

/// nw-113: split a `file::name` lookup spec into its file qualifier and
/// name. `None` when the spec has no `::` or either half is empty.
pub fn split_file_qualifier(spec: &str) -> Option<(&str, &str)> {
    spec.rsplit_once("::")
        .filter(|(qualifier, name)| !qualifier.is_empty() && !name.is_empty())
}

/// nw-113: a qualified lookup spec, `[file::][Container::…]name`.
///
/// The segments before the name are qualifiers. They are read two ways and
/// a candidate matches if either holds:
/// - **container chain:** every qualifier names the candidate's `MEMBER_OF`
///   chain, innermost last (`Outer::Inner::m`, `Svc::run`);
/// - **file, then container chain:** the first qualifier names the
///   candidate's file ([`file_qualifier_rank`]) and the rest name its
///   container chain (`b.ts::Svc::run`, `wsgi.py::app`).
///
/// Neither reading wins over the other: `Svc::run` keeps a method of class
/// `Svc` in `b.ts` as well as the `run` in `a/Svc.ts` whose file stem is
/// `Svc`, so the lookup is ambiguous rather than silently picking one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QualifiedSpec<'a> {
    /// The segments before the name, outermost first.
    pub qualifiers: Vec<&'a str>,
    /// The last segment: the symbol name looked up.
    pub name: &'a str,
}

impl<'a> QualifiedSpec<'a> {
    /// Parse `spec`. `None` for an unqualified spec (no `::`) or one with an
    /// empty segment (`::x`, `a::::b`).
    pub fn parse(spec: &'a str) -> Option<Self> {
        let mut segments: Vec<&str> = spec.split("::").collect();
        if segments.len() < 2 || segments.iter().any(|segment| segment.is_empty()) {
            return None;
        }
        let name = segments.pop()?;
        Some(Self {
            qualifiers: segments,
            name,
        })
    }

    /// How many `MEMBER_OF` levels a match can need: one per qualifier.
    pub fn max_container_depth(&self) -> usize {
        self.qualifiers.len()
    }

    /// Keep the `candidates` (symbols named [`Self::name`]) this spec names.
    ///
    /// `chain_of(uid)` returns a candidate's container names, innermost
    /// first (`["Inner", "Outer"]` for `Outer::Inner::m`); at least
    /// [`Self::max_container_depth`] levels are needed for an exact answer.
    /// Container matches are kept whatever their file. Among file matches
    /// only the best file rank is kept: an exact path beats a path suffix,
    /// which beats a suffix without the extension. Empty when nothing
    /// matches, so a strict lookup can report not found instead of guessing.
    pub fn select<'c>(
        &self,
        candidates: Vec<Symbol>,
        chain_of: impl Fn(&str) -> &'c [String],
    ) -> Vec<Symbol> {
        let chain_matches = |qualifiers: &[&str], chain: &[String]| {
            qualifiers.len() <= chain.len()
                && qualifiers
                    .iter()
                    .rev()
                    .zip(chain)
                    .all(|(qualifier, container)| *qualifier == container)
        };
        let ranked: Vec<(Symbol, bool, Option<u8>)> = candidates
            .into_iter()
            .map(|symbol| {
                let chain = chain_of(&symbol.uid);
                let by_container = chain_matches(&self.qualifiers, chain);
                let by_file = file_qualifier_rank(self.qualifiers[0], &symbol.file_path)
                    .filter(|_| chain_matches(&self.qualifiers[1..], chain));
                (symbol, by_container, by_file)
            })
            .collect();
        let best_file = ranked.iter().filter_map(|(_, _, rank)| *rank).min();
        ranked
            .into_iter()
            .filter(|(_, by_container, by_file)| {
                *by_container || (by_file.is_some() && *by_file == best_file)
            })
            .map(|(symbol, _, _)| symbol)
            .collect()
    }
}

/// The receiver type of a Go method, from its stored signature line:
/// `func (u *UUID) String() string {` names `UUID`. `None` for a non-Go file
/// or a plain function.
///
/// QA #5: Go declares methods outside their type, so the parser records no
/// `MEMBER_OF` container and `UUID::String` named nothing. The receiver is
/// the one place the owning type is written, so qualified lookups and
/// candidate displays read it from there.
pub fn go_receiver_type<'s>(file_path: &str, signature: &'s str) -> Option<&'s str> {
    if !file_path.ends_with(".go") {
        return None;
    }
    let rest = signature.trim_start().strip_prefix("func")?.trim_start();
    let rest = rest.strip_prefix('(')?;
    let receiver = &rest[..rest.find(')')?];
    // `u *UUID`, `*UUID`, `u UUID[T]`, `UUID`: the type is the last token.
    let ty = receiver.split_whitespace().last()?;
    let ty = ty.trim_start_matches('*');
    let ty = ty.split('[').next().unwrap_or(ty);
    // A qualified receiver (`pkg.T`) is not legal Go; keep the last segment
    // defensively.
    let ty = ty.rsplit('.').next().unwrap_or(ty);
    (!ty.is_empty() && ty.chars().all(|c| c.is_alphanumeric() || c == '_')).then_some(ty)
}

/// How closely a `file::name` qualifier names a symbol's file: an exact
/// repo-relative path beats a path suffix, which beats a suffix without the
/// extension (`conftest` for `tests/conftest.py`, the file-stem match).
/// Suffixes only match on a `/` boundary, so `x.js` never matches
/// `index.js`. A leading `./` or `/` is ignored and `\` reads as `/`, so a
/// Windows-style or root-anchored path names the same file.
/// `Some(0)`/`Some(1)`/`Some(2)` in that order, `None` for no match; lower
/// is better.
pub fn file_qualifier_rank(qualifier: &str, file_path: &str) -> Option<u8> {
    let normalised = qualifier.replace('\\', "/");
    let mut qualifier = normalised.as_str();
    while let Some(rest) = qualifier
        .strip_prefix("./")
        .or_else(|| qualifier.strip_prefix('/'))
    {
        qualifier = rest;
    }
    if qualifier.is_empty() {
        return None;
    }
    let suffix_of = |path: &str| {
        path == qualifier
            || path
                .strip_suffix(qualifier)
                .is_some_and(|head| head.ends_with('/'))
    };
    if file_path == qualifier {
        return Some(0);
    }
    if suffix_of(file_path) {
        return Some(1);
    }
    let file_name_start = file_path.rfind('/').map_or(0, |slash| slash + 1);
    let stem = match file_path[file_name_start..].rfind('.') {
        Some(dot) if dot > 0 => &file_path[..file_name_start + dot],
        _ => return None,
    };
    suffix_of(stem).then_some(2)
}

impl SymbolKind {
    /// Kinds that have a body and can therefore be the source of a call
    /// (nw-349/nw-435). Data kinds (`Constant`, `Variable`, `Property`,
    /// `Enum`, `TypeAlias`) have no body and cannot call anything. The
    /// resolver uses this to pick enclosing symbols and the parser uses it to
    /// decide whether a top-level call needs a `<module>` owner (nw-113), so
    /// both apply the same rule.
    pub fn contains_code(self) -> bool {
        matches!(
            self,
            Self::Function
                | Self::Method
                | Self::Class
                | Self::Module
                | Self::Interface
                | Self::Trait
                | Self::Extension
        )
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            SymbolKind::Function => "Function",
            SymbolKind::Class => "Class",
            SymbolKind::Method => "Method",
            SymbolKind::Interface => "Interface",
            SymbolKind::Trait => "Trait",
            SymbolKind::Enum => "Enum",
            SymbolKind::Module => "Module",
            SymbolKind::Extension => "Extension",
            SymbolKind::Constant => "Constant",
            SymbolKind::Property => "Property",
            SymbolKind::TypeAlias => "TypeAlias",
            SymbolKind::Variable => "Variable",
        }
    }
}

impl fmt::Display for SymbolKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum Visibility {
    Public,
    Internal,
    Protected,
    Private,
    #[default]
    Inferred,
}

impl fmt::Display for Visibility {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Visibility::Public => write!(f, "public"),
            Visibility::Internal => write!(f, "internal"),
            Visibility::Protected => write!(f, "protected"),
            Visibility::Private => write!(f, "private"),
            Visibility::Inferred => write!(f, "inferred"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeInfo {
    pub declared_type: Option<String>,
    pub parameter_types: Vec<(String, Option<String>)>,
    pub return_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedType {
    pub type_name: String,
    pub resolution_tier: u8,
    pub confidence: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameworkHint {
    pub framework: String,
    pub role: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Repo {
    pub uid: String,
    pub url: String,
    pub indexed_sha: String,
    pub staleness_commits_behind: u32,
    pub instance_id: String,
    /// Optional display name override. When `Some`, used instead of the
    /// URL-derived basename for display and feature-config matching.
    /// Avoids collisions when multiple repos share a generic basename
    /// (e.g. `client`, `server`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Absolute filesystem path of the local working tree this repo was
    /// indexed from, when one exists. Decoupled from `url`: `url` is the
    /// repo's *identity* (git origin remote when available, else a
    /// `file://` URL), while `root_path` is its on-disk *location*.
    /// `None` for server-side repos indexed from bare clones (no local
    /// working tree) and for rows written before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_path: Option<String>,
}

impl Repo {
    /// Best-effort local working-tree root for this repo.
    ///
    /// Compat shim for the identity/location decoupling: rows written
    /// before `root_path` existed store `None` there but still carry a
    /// `file://<path>` identity `url`, so falling back to stripping the
    /// `file://` prefix keeps those rows behaving as local repos until
    /// they are re-indexed. Remote-identity repos without a working tree
    /// (`https://…` + `root_path: None`) correctly return `None` — the
    /// prefix strip fails — so locality checks skip them.
    ///
    /// Consumers that need a disk path MUST use this helper instead of
    /// stripping `file://` from `url` themselves.
    pub fn local_root(&self) -> Option<&str> {
        match self.root_path.as_deref() {
            Some(p) if !p.is_empty() => Some(p),
            _ => self.url.strip_prefix("file://"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct File {
    pub uid: String,
    pub path: String,
    pub repo_uid: String,
    pub content_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Service {
    pub uid: String,
    pub name: String,
    pub repo_uid: String,
    pub summary: Option<String>,
    pub summary_hash: Option<String>,
    pub embedding: Option<Vec<f32>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    pub uid: String,
    pub name: String,
    pub kind: SymbolKind,
    pub repo_uid: String,
    pub file_path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub signature: String,
    pub summary: Option<String>,
    pub content_hash: String,
    pub embedding: Option<Vec<f32>>,
    pub pagerank_score: Option<f64>,
    pub is_entry_point: bool,
    pub entry_point_kind: Option<EntryPointKind>,
    pub visibility: Visibility,
    pub type_info: Option<TypeInfo>,
    pub framework_hint: Option<FrameworkHint>,
    /// Instance-independent canonical identifier for cross-boundary matching.
    /// Format: `<repo_url_hash>:<file_path>#<name>:<scope_hash>`.
    /// `None` for symbols that haven't been re-indexed with scope-chain extraction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_id: Option<String>,
}

impl Symbol {
    /// nw-113: see [`is_module_owner`].
    pub fn is_module_owner(&self) -> bool {
        is_module_owner(self.kind, &self.name, self.entry_point_kind)
    }

    /// nw-113: the name to show a user: [`module_owner_label`] for an owner,
    /// the plain name for every other symbol.
    pub fn display_name(&self) -> std::borrow::Cow<'_, str> {
        if self.is_module_owner() {
            std::borrow::Cow::Owned(module_owner_label(&self.file_path))
        } else {
            std::borrow::Cow::Borrowed(&self.name)
        }
    }
}

// ── Brain extension: markdown nodes ────────────────────────────────────────
//
// These mirror the Repo/File pair for the markdown domain. A Vault is the
// root of an Obsidian-style markdown collection; a Note is a single .md file
// inside it. Headings, Sections, Tags, and Project nodes will arrive in
// later phases — this is the walking skeleton.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NoteKind {
    General,
    Prd,
    Design,
    Meeting,
    Journal,
    AgentConfig,
}

impl fmt::Display for NoteKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NoteKind::General => write!(f, "General"),
            NoteKind::Prd => write!(f, "PRD"),
            NoteKind::Design => write!(f, "Design"),
            NoteKind::Meeting => write!(f, "Meeting"),
            NoteKind::Journal => write!(f, "Journal"),
            NoteKind::AgentConfig => write!(f, "AgentConfig"),
        }
    }
}

impl NoteKind {
    /// Parse a free-form string from frontmatter `type:` or filename heuristic.
    /// Unknown values fall back to `General`.
    pub fn from_hint(hint: &str) -> Self {
        match hint.trim().to_ascii_lowercase().as_str() {
            "prd" | "product-requirements" => NoteKind::Prd,
            "design" | "design-doc" | "designdoc" => NoteKind::Design,
            "meeting" | "meeting-note" | "meetingnote" => NoteKind::Meeting,
            "journal" | "daily" | "daily-note" => NoteKind::Journal,
            "agent-config" | "agentconfig" => NoteKind::AgentConfig,
            _ => NoteKind::General,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Vault {
    pub uid: String,
    pub name: String,
    pub root_path: String,
    pub instance_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Note {
    pub uid: String,
    pub vault_uid: String,
    pub file_path: String,
    pub title: String,
    pub note_kind: NoteKind,
    pub word_count: u32,
    pub content_hash: String,
    pub frontmatter: Option<String>,
    /// The frontmatter's ORIGINAL YAML text, as written between the `---`
    /// fences. `frontmatter` above is the parsed map serialized as JSON, which
    /// is why both exist: the JSON answers "what fields does this note declare"
    /// and cannot answer "does this file contain this string, and on what
    /// line". Frontmatter is split off before sectioning, so it reaches no
    /// Section either — which is what made 1.36 MB of it unreachable from
    /// `regex_search` / `count_patterns` while `brain_search` found it by
    /// reading the file off disk (nw-298).
    ///
    /// `#[serde(default)]` so notes serialized before this field still
    /// deserialize.
    #[serde(default)]
    pub frontmatter_raw: Option<String>,
    pub created_at: Option<String>,
    pub modified_at: Option<String>,
    pub pagerank_score: Option<f64>,
    pub embedding: Option<Vec<f32>>,
}

/// A heading inside a note. Addressable target of `[[Note#Heading]]` links.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Heading {
    pub uid: String,
    pub note_uid: String,
    pub level: u8,
    pub text: String,
    pub slug: String,
    pub start_line: u32,
    pub end_line: u32,
    pub content_hash: String,
    pub embedding: Option<Vec<f32>>,
}

/// The body text under a heading (or the preamble before the first heading).
/// Unit of retrieval — section-level granularity beats both file-level
/// (too coarse) and paragraph-level (too noisy).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Section {
    pub uid: String,
    pub note_uid: String,
    /// `None` for the preamble (text before any heading).
    pub heading_uid: Option<String>,
    pub start_line: u32,
    pub end_line: u32,
    pub text_hash: String,
    /// Full section text. Populated by the markdown indexer; consumed by
    /// the Tantivy reindex path (`TantivyIndex::reindex_from_store`) so
    /// BM25 search after a cold start matches section bodies, not just
    /// titles. Per architecture doc §8.4: inline is fine up to ~50K
    /// sections; future v2 may migrate to lazy load-from-disk if RAM
    /// pressure shows up at larger scale.
    pub text_content: String,
    pub word_count: u32,
    pub pagerank_score: Option<f64>,
}

/// A canonical tag (`#tag` or `#nested/tag`). One node per (vault, name);
/// the same tag string in two different vaults is two distinct nodes (vault
/// scoping is the v1 default — cross-vault tag aggregation is a later
/// concern). Name is stored lowercased, slash-separated for `#a/b/c`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tag {
    pub uid: String,
    pub vault_uid: String,
    pub name: String,
}

/// A logical grouping that can span vaults and repos. Generalises the
/// existing code-side `Feature` config — projects bundle notes, repos,
/// services, and vaults that belong to the same piece of work.
///
/// `instance_id` is included so projects scope to the instance the user
/// is working in (single-user setups use the default instance).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub uid: String,
    pub name: String,
    pub summary: Option<String>,
    pub instance_id: String,
}

/// An API contract surface — one HTTP route, one gRPC method, or one GraphQL
/// operation. Contracts are derived two ways:
///
/// 1. **Declared** — parsed from a spec file (OpenAPI/Swagger, `.proto`,
///    `.graphql`). `source_path` points at the spec.
/// 2. **Code-derived** — minted from a framework handler (Spring/NestJS) when
///    no spec declares it. `source_path` points at the handler's source file.
///
/// Contracts are treated as **hypotheses**, not ground truth — the
/// `confidence` on the incident `IMPLEMENTS_CONTRACT` edge records match
/// quality, and drift diagnostics surface the declared/implemented set diff.
///
/// `kind` is one of `http` | `grpc` | `graphql`. For HTTP, `verb` + `path`
/// are populated and `operation_id` is the spec's `operationId` (if any).
/// For gRPC/GraphQL, `operation_id` carries the fully-qualified identifier
/// and `verb`/`path` are `None`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Contract {
    pub uid: String,
    /// `http` | `grpc` | `graphql`.
    pub kind: String,
    /// HTTP verb (GET/POST/...), upper-cased. `None` for gRPC/GraphQL.
    pub verb: Option<String>,
    /// Normalized HTTP route path. `None` for gRPC/GraphQL.
    pub path: Option<String>,
    /// Spec `operationId` (HTTP) or fully-qualified method/operation
    /// (gRPC/GraphQL).
    pub operation_id: Option<String>,
    /// Owning repo UID.
    pub repo_uid: String,
    /// Path to the spec file (declared) or handler source file (code-derived).
    pub source_path: String,
    /// Confidence the contract is real / correctly extracted. Declared
    /// contracts are 1.0; code-derived contracts inherit the handler
    /// match confidence.
    pub confidence: f32,
}

#[cfg(test)]
mod tests {
    use super::{Repo, SymbolKind};

    #[test]
    fn go_receiver_type_reads_the_receiver_from_the_signature() {
        use super::go_receiver_type;
        assert_eq!(
            go_receiver_type("uuid.go", "func (u UUID) String() string {"),
            Some("UUID")
        );
        assert_eq!(
            go_receiver_type("a/b.go", "func (g *Greeter) Greet() {"),
            Some("Greeter")
        );
        assert_eq!(
            go_receiver_type("x.go", "func (l *List[T]) Len() int {"),
            Some("List")
        );
        assert_eq!(
            go_receiver_type("x.go", "func (UUID) Version() Version {"),
            Some("UUID")
        );
        assert_eq!(
            go_receiver_type("x.go", "func Parse(s string) (UUID, error) {"),
            None
        );
        assert_eq!(
            go_receiver_type("x.rs", "func (u UUID) String() string {"),
            None
        );
    }

    fn symbol_at(name: &str, file_path: &str) -> super::Symbol {
        super::Symbol {
            uid: format!("sym:{file_path}:{name}"),
            name: name.into(),
            kind: SymbolKind::Function,
            repo_uid: "repo:r".into(),
            file_path: file_path.into(),
            start_line: 1,
            end_line: 1,
            signature: String::new(),
            summary: None,
            content_hash: String::new(),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: super::Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        }
    }

    fn owner_at(file_path: &str) -> super::Symbol {
        let mut owner = symbol_at(super::MODULE_OWNER_NAME, file_path);
        owner.kind = SymbolKind::Module;
        owner.is_entry_point = true;
        owner.entry_point_kind = Some(super::EntryPointKind::ModuleInit);
        owner
    }

    fn paths(symbols: &[super::Symbol]) -> Vec<&str> {
        symbols.iter().map(|s| s.file_path.as_str()).collect()
    }

    /// nw-113: an owner is always shown as `<path>::<module>`.
    #[test]
    fn owner_display_name_carries_its_repo_relative_path() {
        assert_eq!(
            owner_at("src/controllers/index.js").display_name(),
            "src/controllers/index.js::<module>"
        );
        assert_eq!(symbol_at("listen", "index.js").display_name(), "listen");
        assert_eq!(
            super::symbol_display_name(super::MODULE_OWNER_NAME, "wsgi.py"),
            "wsgi.py::<module>"
        );
        assert_eq!(super::symbol_display_name("app", "wsgi.py"), "app");
    }

    /// nw-113: `file::name` narrows by exact path, then a unique path suffix,
    /// then a suffix without the extension; a qualifier that names no file
    /// and no container matches nothing.
    #[test]
    fn file_qualifier_narrows_by_path_suffix() {
        let narrow = |candidates: Vec<super::Symbol>, spec: &str| {
            let spec = super::QualifiedSpec::parse(spec).expect("qualified");
            spec.select(candidates, |_| &[])
        };
        let owners = || {
            vec![
                owner_at("index.js"),
                owner_at("src/controllers/index.js"),
                owner_at("app/wsgi.py"),
                owner_at("tests/conftest.py"),
            ]
        };
        let owner = |path: &str| format!("{path}::<module>");
        assert_eq!(paths(&narrow(owners(), &owner("index.js"))), ["index.js"]);
        assert_eq!(
            paths(&narrow(owners(), &owner("controllers/index.js"))),
            ["src/controllers/index.js"]
        );
        assert_eq!(paths(&narrow(owners(), &owner("wsgi.py"))), ["app/wsgi.py"]);
        assert_eq!(paths(&narrow(owners(), &owner("wsgi"))), ["app/wsgi.py"]);
        assert_eq!(
            paths(&narrow(owners(), &owner("./tests/conftest.py"))),
            ["tests/conftest.py"]
        );
        assert_eq!(
            paths(&narrow(owners(), &owner("/tests/conftest.py"))),
            ["tests/conftest.py"],
            "a leading slash is ignored"
        );
        assert_eq!(
            paths(&narrow(owners(), &owner("src\\controllers\\index.js"))),
            ["src/controllers/index.js"],
            "a backslash reads as a slash"
        );
        assert!(
            narrow(owners(), &owner("x.js")).is_empty(),
            "no boundary match"
        );
        assert!(narrow(owners(), &owner("Class")).is_empty(), "not a path");
        let utils = vec![symbol_at("f", "a/util.py"), symbol_at("f", "b/util.py")];
        assert_eq!(
            narrow(utils, "util.py::f").len(),
            2,
            "an ambiguous suffix stays ambiguous"
        );
        assert_eq!(
            super::split_file_qualifier("src/a.js::<module>"),
            Some(("src/a.js", "<module>"))
        );
        assert_eq!(super::split_file_qualifier("<module>"), None);
        assert_eq!(super::split_file_qualifier("::x"), None);
        assert_eq!(super::QualifiedSpec::parse("<module>"), None);
        assert_eq!(super::QualifiedSpec::parse("a::::b"), None);
        assert_eq!(
            super::QualifiedSpec::parse("b.ts::Svc::run"),
            Some(super::QualifiedSpec {
                qualifiers: vec!["b.ts", "Svc"],
                name: "run"
            })
        );
    }

    /// nw-113: one predicate identifies the per-file `<module>` owner; a real
    /// symbol of the same kind or name is never mistaken for it.
    #[test]
    fn module_owner_needs_its_kind_name_and_entry_kind() {
        use super::{EntryPointKind, MODULE_OWNER_NAME, is_module_owner};
        let init = Some(EntryPointKind::ModuleInit);
        assert!(is_module_owner(SymbolKind::Module, MODULE_OWNER_NAME, init));
        assert!(!is_module_owner(
            SymbolKind::Function,
            MODULE_OWNER_NAME,
            init
        ));
        assert!(!is_module_owner(SymbolKind::Module, "utils", init));
        assert!(!is_module_owner(
            SymbolKind::Module,
            MODULE_OWNER_NAME,
            None
        ));
        assert!(!is_module_owner(
            SymbolKind::Module,
            MODULE_OWNER_NAME,
            Some(EntryPointKind::Main)
        ));
    }

    /// The resolver and the parser share one code-bearing rule (nw-113).
    #[test]
    fn code_bearing_kinds_are_the_ones_with_bodies() {
        use SymbolKind as K;
        for kind in [
            K::Function,
            K::Method,
            K::Class,
            K::Module,
            K::Interface,
            K::Trait,
            K::Extension,
        ] {
            assert!(kind.contains_code(), "{kind:?}");
        }
        for kind in [K::Enum, K::Constant, K::Variable, K::Property, K::TypeAlias] {
            assert!(!kind.contains_code(), "{kind:?}");
        }
    }

    fn repo(url: &str, root_path: Option<&str>) -> Repo {
        Repo {
            uid: "repo:test".to_string(),
            url: url.to_string(),
            indexed_sha: "abc".to_string(),
            staleness_commits_behind: 0,
            instance_id: "test".to_string(),
            name: None,
            root_path: root_path.map(String::from),
        }
    }

    #[test]
    fn local_root_prefers_root_path_over_url() {
        let r = repo("https://github.com/acme/demo.git", Some("/home/u/demo"));
        assert_eq!(r.local_root(), Some("/home/u/demo"));
        // root_path wins even when the url is also a file:// URL.
        let r = repo("file:///elsewhere/demo", Some("/home/u/demo"));
        assert_eq!(r.local_root(), Some("/home/u/demo"));
    }

    #[test]
    fn local_root_falls_back_to_file_url_for_pre_migration_rows() {
        // Old rows: file:// identity, root_path never written (None).
        let r = repo("file:///home/u/demo", None);
        assert_eq!(r.local_root(), Some("/home/u/demo"));
        // '' from the column default behaves like None.
        let r = repo("file:///home/u/demo", Some(""));
        assert_eq!(r.local_root(), Some("/home/u/demo"));
    }

    #[test]
    fn local_root_is_none_for_remote_identity_without_working_tree() {
        // The data-loss guard: a server-side repo must never look local.
        let r = repo("https://github.com/acme/demo.git", None);
        assert_eq!(r.local_root(), None);
        let r = repo("git@github.com:acme/demo.git", None);
        assert_eq!(r.local_root(), None);
    }
}
