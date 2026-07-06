// nestweaver-schema: core types and data structures

pub mod confidence;
pub mod edges;
pub mod limits;
pub mod nodes;
pub mod repo_url;
pub mod uid;
pub mod version;

pub use confidence::{Language, MatchType, confidence_score};
pub use edges::{ALL_SYMBOL_EDGE_TYPES, CrossRepoLinkType, EdgeEvidence, EdgeType, ResolvedEdge};
pub use limits::{DEFAULT_DRAIN_CEILING_SECS, drain_ceiling_from_env, parse_drain_ceiling};
pub use nodes::{
    Contract, EntryPointKind, File, FrameworkHint, Heading, Note, NoteKind, Project, Repo,
    ResolvedType, Section, Service, Symbol, SymbolKind, Tag, TypeInfo, Vault, Visibility,
};
pub use repo_url::{normalized_repo_key, repo_name};
pub use uid::{
    PATH_PLACEHOLDER, canonical_symbol_id, contract_uid, file_uid, heading_uid,
    normalize_http_path, note_uid, project_uid, repo_uid, scope_hash, section_uid, service_uid,
    symbol_uid, tag_uid, truncated_hash, vault_uid,
};
pub use version::{core_schema_hash, effective_schema_hash};
