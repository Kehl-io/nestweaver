// nestweaver-engine: orchestrates parsing, resolution, and graph construction

use std::path::{Path, PathBuf};

/// Canonical sidecar path: appends `suffix` to the database path.
///
/// All sidecars live alongside the database file using the convention
/// `<db><suffix>`, e.g. `data.lbug.pagerank.json` for suffix `.pagerank.json`.
///
/// This uses `OsStr::push` (append) rather than `Path::with_extension` (replace)
/// so the `.lbug` stem is preserved and backup globs like `data.lbug*` capture
/// every sidecar.
pub fn sidecar_path(db_path: &Path, suffix: &str) -> PathBuf {
    let mut s = db_path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Migrate a sidecar from the old `with_extension` naming convention to the
/// new `push` convention. If the old-convention path exists and the
/// new-convention path does not, renames the old file to the new location.
///
/// Returns `true` if a migration rename was performed.
pub fn migrate_sidecar(db_path: &Path, old_extension: &str, new_suffix: &str) -> bool {
    let old_path = db_path.with_extension(old_extension);
    let new_path = sidecar_path(db_path, new_suffix);
    if old_path.exists() && !new_path.exists() {
        std::fs::rename(&old_path, &new_path).is_ok()
    } else {
        false
    }
}

pub mod agent_guide;
pub mod blast_radius;
pub mod brainignore;
pub mod bridges;
pub mod cluster_dispatch;
pub mod clustering;
pub mod config;
pub mod cross_domain;
pub mod dead_code;
pub mod embedding;
pub mod export;
pub mod extensions;
pub mod git_diff;
pub mod html_to_md;
pub mod hubs;
pub mod index;
pub mod index_md;
pub mod manifest;
pub mod mcp_client;
pub mod process;
pub mod project;
pub mod pull;
pub mod query;
pub mod recency;
pub mod registry;
pub mod snapshot;
pub mod suggest;
pub mod summaries;
pub mod summary;
pub mod vector_search;
pub mod watch_code;
pub mod watcher;

pub use agent_guide::{generate_agents_md, generate_cursor_rule, generate_guide, generate_skill};
pub use blast_radius::{
    AffectedCluster, AffectedSymbol as BlastAffectedSymbol, BlastRadiusResult, ChangedSymbol,
    analyze_blast_radius, changed_files_from_git,
};
pub use brainignore::{is_ignored, load_brain_ignore};
pub use bridges::{BridgeNode, attach_communities, find_bridge_nodes};
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
pub use dead_code::{
    DeadCodeConfidence, DeadCodeResult, UnreachableSymbol, detect_dead_code,
    detect_dead_code_with_confidence,
};
pub use export::{export_cypher, export_graphml, export_mermaid};
pub use extensions::{
    ExtensionStore, get_all_properties, get_last_indexed_at, get_property, load_extensions,
    query_by_property, record_last_indexed_at, save_extensions, set_property,
};
pub use hubs::{HubNode, attach_cluster_ids, find_hub_nodes};
pub use index::{
    CachedFileMeta, FileMetaCache, IncrementalResult, IndexResult, incremental_index,
    index_directory, index_directory_in_memory, index_directory_with_options, load_filemeta_cache,
    save_filemeta_cache,
};
pub use index_md::{
    MarkdownIndexResult, MarkdownSinceResult, index_markdown_directory,
    index_markdown_directory_in_memory, index_markdown_directory_since,
    index_markdown_directory_since_with_ignore, index_markdown_directory_with_ignore,
    load_alias_sidecar,
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
    build_context, build_context_with_intent, build_feature_context, generate_repo_map, list_repos,
    list_services, lookup_symbol, search_symbols,
};
pub use recency::parse_iso8601_to_epoch;
pub use registry::*;
pub use snapshot::*;
pub use suggest::{
    Confidence, SuggestedFeature, SuggestedLink, Suggestions, discover_symbol_level_links,
    materialize_declared_links, persist_cross_repo_links, suggest_links,
};
pub use summaries::{
    Summary, SummaryLevel, SummaryStore, filter_by_target, generate_summaries, load_summaries,
    render_text, save_summaries, truncate_to_budget,
};
pub use watch_code::CodeWatcher;
pub use watcher::{BrainWatcher, ShutdownHandle, UpdateOutcome};
