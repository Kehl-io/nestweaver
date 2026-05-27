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

impl fmt::Display for SymbolKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SymbolKind::Function => write!(f, "Function"),
            SymbolKind::Class => write!(f, "Class"),
            SymbolKind::Method => write!(f, "Method"),
            SymbolKind::Interface => write!(f, "Interface"),
            SymbolKind::Trait => write!(f, "Trait"),
            SymbolKind::Enum => write!(f, "Enum"),
            SymbolKind::Module => write!(f, "Module"),
            SymbolKind::Extension => write!(f, "Extension"),
            SymbolKind::Constant => write!(f, "Constant"),
            SymbolKind::Property => write!(f, "Property"),
            SymbolKind::TypeAlias => write!(f, "TypeAlias"),
            SymbolKind::Variable => write!(f, "Variable"),
        }
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
}

impl fmt::Display for NoteKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NoteKind::General => write!(f, "General"),
            NoteKind::Prd => write!(f, "PRD"),
            NoteKind::Design => write!(f, "Design"),
            NoteKind::Meeting => write!(f, "Meeting"),
            NoteKind::Journal => write!(f, "Journal"),
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
    pub created_at: Option<String>,
    pub modified_at: Option<String>,
    pub pagerank_score: Option<f64>,
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
