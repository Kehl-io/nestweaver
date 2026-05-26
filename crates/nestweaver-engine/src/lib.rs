// nestweaver-engine: orchestrates parsing, resolution, and graph construction

pub mod agent_guide;
pub mod cluster_dispatch;
pub mod clustering;
pub mod config;
pub mod cross_domain;
pub mod embedding;
pub mod extensions;
pub mod git_diff;
pub mod index;
pub mod index_md;
pub mod manifest;
pub mod mcp_client;
pub mod process;
pub mod project;
pub mod pull;
pub mod query;
pub mod registry;
pub mod snapshot;
pub mod suggest;
pub mod summary;
pub mod vector_search;
pub mod watcher;

pub use agent_guide::generate_guide;
pub use cluster_dispatch::{
    ClusterMember, ClusteringOutput, CommunityInfo, compute_clusters, load_clusters, save_clusters,
};
pub use config::{
    CrossDomainConfig, ExternalRefConfig, FeatureConfig, GitConfig, InferenceConfig,
    InstanceConfig, LinkConfig, McpServerConfig, ProjectConfig, RepoConfig, SchemaExtensions,
    StorageConfig, WikiSourceConfig, WorkspaceConfig,
};
pub use cross_domain::{
    CrossDomainResult, SymbolIndex, build_symbol_index, build_symbol_index_with_config,
    discover_cross_domain_links, discover_cross_domain_links_for_note,
    discover_cross_domain_links_for_note_with_index, discover_cross_domain_links_with_config,
};
pub use extensions::{
    ExtensionStore, get_all_properties, get_property, load_extensions, query_by_property,
    save_extensions, set_property,
};
pub use index::{
    IncrementalResult, IndexResult, incremental_index, index_directory, index_directory_in_memory,
};
pub use index_md::{
    MarkdownIndexResult, MarkdownSinceResult, index_markdown_directory,
    index_markdown_directory_in_memory, index_markdown_directory_since, load_alias_sidecar,
};
pub use manifest::{ManifestInfo, load_manifest_cache, parse_manifest, save_manifest_cache};
pub use process::{
    AffectedProcess, AffectedSymbol, ChangeImpact, ProcessMember, ProcessResult, RiskLevel,
    detect_changes_impact, trace_processes,
};
pub use project::{ProjectMaterializationResult, detect_implicit_projects, materialize_projects};
pub use pull::*;
pub use query::{
    BrainContextResult, BrainNode, ContextNode, ContextResult, CrossRepoLink, FeatureContextResult,
    FeatureInfo, HybridSearchConfig, LinkInfo, LookupResult, SymbolCandidate, SymbolDetail,
    build_brain_context, build_brain_context_hybrid, build_brain_context_hybrid_with_aliases,
    build_context, build_feature_context, generate_repo_map, list_repos, list_services,
    lookup_symbol, search_symbols,
};
pub use registry::*;
pub use snapshot::*;
pub use suggest::{
    Confidence, SuggestedFeature, SuggestedLink, Suggestions, discover_symbol_level_links,
    materialize_declared_links, persist_cross_repo_links, suggest_links,
};
pub use watcher::{BrainWatcher, ShutdownHandle, UpdateOutcome};
