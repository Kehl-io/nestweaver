// nestweaver-schema: core types and data structures

pub mod confidence;
pub mod edges;
pub mod nodes;
pub mod uid;
pub mod version;

pub use confidence::{Language, MatchType, confidence_score};
pub use edges::{CrossRepoLinkType, EdgeType, ResolvedEdge};
pub use nodes::{
    EntryPointKind, File, FrameworkHint, Heading, Note, NoteKind, Project, Repo, ResolvedType,
    Section, Service, Symbol, SymbolKind, Tag, TypeInfo, Vault, Visibility,
};
pub use uid::{
    file_uid, heading_uid, note_uid, project_uid, repo_uid, section_uid, service_uid, symbol_uid,
    tag_uid, truncated_hash, vault_uid,
};
pub use version::{core_schema_hash, effective_schema_hash};
