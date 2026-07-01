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

/// Return the display name for a repo: the explicit `name` override if set,
/// otherwise the URL-derived basename via [`repo_name_from_url`].
pub fn repo_display_name(repo: &nestweaver_schema::Repo) -> String {
    repo.name
        .clone()
        .unwrap_or_else(|| crate::pull::repo_name_from_url(&repo.url))
}

pub mod admin;
pub mod affected_tests;
pub mod agent_guide;
pub mod atomic_changes;
pub mod backup;
pub mod bare_clone;
pub mod blast_radius;
pub mod brain_docgraph;
pub mod brain_memory;
pub mod brainignore;
pub mod bridges;
pub mod circuit_breaker;
pub mod cluster_dispatch;
pub mod clustering;
pub mod cochange;
pub mod config;
pub mod content_reader;
pub mod contracts;
pub mod cross_domain;
pub mod dead_code;
pub mod diff_impact;
pub mod embedding;
pub mod eval;
pub mod export;
pub mod export_graph;
pub mod extensions;
pub mod format_comment;
pub mod git_activity;
pub mod git_diff;
pub mod guide_rules;
pub mod hash;
pub mod html_to_md;
pub mod hubs;
pub mod index;
pub mod index_md;
pub mod interactions;
pub mod investigate;
pub mod jobs;
pub mod manifest;
pub mod mcp_client;
pub mod parsed_cache;
pub mod process;
pub mod project;
pub mod pull;
pub mod query;
pub mod read_symbols;
pub mod recency;
pub mod registry;
pub mod rerank;
pub mod resolution_cache;
pub mod scheduler;
pub mod snapshot;
pub mod ssrf;
pub mod suggest;
pub mod summaries;
pub mod summary;
pub mod tls;
pub mod vector_search;
pub mod watch_code;
pub mod watcher;
pub mod worker;

pub use affected_tests::{
    AffectedTestFile, AffectedTestSymbol, AffectedTestsResult, ChangedSymbolRef, affected_tests,
};
pub use agent_guide::{
    ToolDocEntry, generate_agents_md, generate_agents_md_with_rules, generate_claude_md,
    generate_claude_md_with_rules, generate_cursor_rule, generate_cursor_rule_with_rules,
    generate_guide, generate_guide_with_rules, generate_skill, generate_skill_with_rules,
    generate_skill_with_tools,
};
pub use backup::{
    BackupConfig, BackupManifest, BackupRepoInfo, BackupResult, BackupSizes, RestoreConfig,
    RestoreResult, StagedBackup, backup_inspect, backup_list, backup_restore, backup_save,
    package_staged, stage_backup_from_store,
};
pub use blast_radius::{
    AffectedCluster, AffectedSymbol as BlastAffectedSymbol, BlastRadiusResult, ChangedSymbol,
    analyze_blast_radius, changed_files_from_git,
};
pub use brain_docgraph::{
    BrokenLink, CoOccurringTag, DocStats, OrphanDocument, TagCount, TagGraph, TopicCluster,
    broken_links, doc_stats, orphan_documents, tag_graph, tag_graph_all, topic_clusters,
};
pub use brain_memory::{
    ConsolidationManifest, ConsolidationProposal, Contradiction, DanglingRelationship,
    MemoryLintReport, RelatedNode, SchemaDrift, StaleNote, SupersessionChain, memory_consolidate,
    memory_lint, memory_related,
};
pub use brainignore::{is_ignored, load_brain_ignore};
pub use bridges::{BridgeNode, attach_communities, find_bridge_nodes};
pub use cluster_dispatch::{
    ClusterMember, ClusteringOutput, CommunityInfo, compute_clusters, load_clusters, save_clusters,
};
pub use cochange::{CoChangeEdge, compute_cochanges, load_cochange_sidecar, save_cochange_sidecar};
pub use config::{
    CrossDomainConfig, ExternalRefConfig, FeatureConfig, GitConfig, GlobRule, InferenceConfig,
    InstanceConfig, LinkConfig, McpServerConfig, ProjectConfig, RankingConfig, RepoConfig,
    RepoType, ResponseConfig, SchemaExtensions, SeedResolutionConfig, StorageConfig,
    WikiSourceConfig, WorkspaceConfig, append_repo_to_config_file, default_kind_priority,
    default_test_path_patterns, remove_repo_from_config_file,
};
pub use cross_domain::{
    CrossDomainResult, SymbolIndex, VaultReaders, build_symbol_index,
    build_symbol_index_with_config, discover_cross_domain_links,
    discover_cross_domain_links_for_note, discover_cross_domain_links_for_note_with_index,
    discover_cross_domain_links_for_note_with_index_and_readers,
    discover_cross_domain_links_with_config, discover_cross_domain_links_with_readers,
};
pub use dead_code::{
    DeadCodeConfidence, DeadCodeResult, UnreachableSymbol, detect_dead_code,
    detect_dead_code_with_confidence, detect_dead_code_with_manifests,
};
pub use eval::{
    EvalComparison, EvalReport, JudgedQuery, PerQueryRow, compare_reports, load_judged_queries,
    mrr, ndcg_at_k, precision_at_k, run_eval,
};
pub use export::{export_cypher, export_graphml, export_mermaid};
pub use export_graph::export_in_memory_graph;
pub use extensions::{
    ExtensionStore, get_all_properties, get_last_indexed_at, get_property, load_extensions,
    query_by_property, record_last_indexed_at, save_extensions, set_property,
};
pub use guide_rules::{
    HARD_RULES, OwnedRule, RULES_VERSION, Rule, parse_rules_override, render_owned_rules_markdown,
    render_rules_markdown,
};
pub use hubs::{HubNode, attach_cluster_ids, find_hub_nodes};
pub use index::{
    CachedFileMeta, FileMetaCache, IncrementalResult, IndexResult, incremental_index,
    incremental_index_with_name, index_directory, index_directory_in_memory,
    index_directory_with_options, index_directory_with_store,
    index_directory_with_store_cancellable, index_with_reader, index_with_reader_and_write_gate,
    load_filemeta_cache, save_filemeta_cache,
};
pub use index_md::{
    MarkdownIndexResult, MarkdownSinceResult, index_markdown_directory,
    index_markdown_directory_in_memory, index_markdown_directory_since,
    index_markdown_directory_since_with_ignore, index_markdown_directory_with_ignore,
    index_markdown_directory_with_store, index_markdown_with_reader,
    index_markdown_with_reader_and_write_gate, load_alias_sidecar,
};
pub use interactions::{
    EventType, InteractionData, InteractionStore, InteractionTracker, NodeScore,
    clear_interaction_sidecar, compute_decayed_score, interaction_sidecar_path,
    load_interaction_data, load_interaction_scores, load_node_score, save_interaction_store,
    top_uids_by_kind,
};
pub use investigate::{
    Bundle, BundleEntry, BundleStore, Domain, ExpandResult, HydrateResult, InvestigateResult,
    NeighborRef, bundle_sidecar_path, investigate, investigate_expand, investigate_hydrate,
    load_bundle, load_bundle_store, save_bundle_store,
};
pub use manifest::{ManifestInfo, load_manifest_cache, parse_manifest, save_manifest_cache};
pub use process::{
    AffectedProcess, AffectedSymbol, ChangeImpact, ProcessMember, ProcessResult, RiskLevel,
    detect_changes_impact, trace_processes,
};
pub use project::{ProjectMaterializationResult, detect_implicit_projects, materialize_projects};
pub use pull::*;
pub use query::{
    BrainContextResult, BrainNode, ContextNode, ContextResult, CrossRepoLink, EmbedQueryFn,
    FeatureContextResult, FeatureInfo, HybridSearchConfig, LinkInfo, LookupResult, SymbolCandidate,
    SymbolDetail, apply_ranking_priors, build_brain_context, build_brain_context_hybrid,
    build_brain_context_hybrid_with_aliases, build_context, build_context_with_intent,
    build_feature_context, dedup_heading_section_pairs, expand_query_with_aliases,
    explain_ranking_prior, generate_repo_map, list_repos, list_services, lookup_symbol,
    populate_inline_bodies, promote_member_notes_into_connected,
    promote_member_symbols_into_connected, search_symbols,
};
pub use recency::parse_iso8601_to_epoch;
pub use registry::*;
pub use rerank::{
    DEFAULT_TOP_N as RERANK_DEFAULT_TOP_N, LoadedModelReranker, MonotonicReranker,
    MonotonicWeights, RerankFeatures, RerankModel, Reranker, TrainingRow, export_training_rows,
    load_rerank_model, rerank, rerank_sidecar_path, select_reranker,
};
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
