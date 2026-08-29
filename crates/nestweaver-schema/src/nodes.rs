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

impl SymbolKind {
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
    use super::Repo;

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
