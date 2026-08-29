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

/// Failure while validating a user-supplied `since` filter value.
#[derive(Debug, thiserror::Error)]
#[error(
    "invalid 'since' value '{input}': expected an ISO 8601 timestamp \
     (2026-01-31 or 2026-01-31T00:00:00Z)"
)]
pub struct ParseSinceError {
    /// The value the caller supplied, quoted back so the error is actionable.
    pub input: String,
}

/// Validate a user-supplied `since` filter and normalise it to the exact shape
/// `modified_at` is stored in: `YYYY-MM-DDTHH:MM:SSZ`, UTC.
///
/// nw-295. `since` used to be handed straight to
/// `WHERE n.modified_at >= $since`, and `modified_at` is a String column, so
/// that predicate is a LEXICOGRAPHIC byte comparison, not a temporal one. It
/// can never fail, so an unparseable value had nowhere to surface: `'g'`
/// (0x67) sorts above every stored timestamp's leading `'2'` (0x32), which
/// made `since: "garbage"` byte-identical to `since: "2099-12-31"` — both
/// matched no note and silently deleted every Note and Section from the
/// answer. The failure direction is the harmful one: it does not no-op, it
/// narrows the result toward emptiness while reporting success.
///
/// Two shapes are accepted, and BOTH must keep working:
///
/// - a bare `YYYY-MM-DD` date, which is not RFC 3339 but works correctly today
///   and is the natural thing for an agent to send. It is widened to
///   `T00:00:00Z`. An RFC-3339-only validator would reject a currently-working
///   input, which would be a regression dressed as a fix.
/// - a full RFC 3339 timestamp, including one with a non-UTC offset. That last
///   case is why this returns a NORMALISED string rather than validating in
///   place: `2026-01-01T00:00:00+02:00` compared bytewise against a `Z`-suffixed
///   column is wrong by the offset, so converting to UTC here makes the
///   downstream lexicographic comparison temporally correct.
///
/// # Errors
/// Returns [`ParseSinceError`] when `input` is not one of those two shapes, or
/// is not a real calendar date.
pub fn parse_since(input: &str) -> Result<String, ParseSinceError> {
    use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};

    let fail = || ParseSinceError {
        input: input.to_string(),
    };
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(fail());
    }

    // A bare date is widened rather than special-cased downstream, so exactly
    // one shape reaches the comparison.
    let candidate = if trimmed.len() == 10 && trimmed.as_bytes()[4] == b'-' {
        format!("{trimmed}T00:00:00Z")
    } else {
        trimmed.to_string()
    };

    let parsed = OffsetDateTime::parse(&candidate, &Rfc3339).map_err(|_| fail())?;
    let utc = parsed.to_offset(UtcOffset::UTC);
    Ok(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        utc.year(),
        u8::from(utc.month()),
        utc.day(),
        utc.hour(),
        utc.minute(),
        utc.second(),
    ))
}

#[cfg(test)]
mod parse_since_tests {
    use super::parse_since;

    /// The values the QA hit. Each of these used to be silently HONOURED and
    /// each emptied the Note/Section half of the result set.
    #[test]
    fn an_unparseable_since_is_rejected_rather_than_honoured() {
        for bad in [
            "garbage",
            "",
            "   ",
            "yesterday",
            "2026-13-45",
            "not-a-date",
            "31/01/2026",
            "2099",
            "2026-01",
        ] {
            let error = parse_since(bad)
                .expect_err("an unparseable `since` must be an error, not a filter");
            let message = error.to_string();
            assert!(
                message.contains("since"),
                "the error must name the offending parameter: {message}"
            );
            assert!(
                message.contains("ISO 8601"),
                "the error must state the expected format, as `kinds` and `scope` do: {message}"
            );
        }
    }

    /// A bare date is not RFC 3339 but works today, so rejecting it would be a
    /// regression. It must widen to the stored shape.
    #[test]
    fn a_bare_date_is_accepted_and_widened() {
        assert_eq!(parse_since("2026-01-31").unwrap(), "2026-01-31T00:00:00Z");
    }

    /// Normalisation is the point, not a side effect: `modified_at` is stored
    /// as `...Z` and compared BYTEWISE, so an offset timestamp left as-is is
    /// wrong by the offset.
    #[test]
    fn an_offset_timestamp_is_normalised_to_utc() {
        assert_eq!(
            parse_since("2026-01-31T02:00:00+02:00").unwrap(),
            "2026-01-31T00:00:00Z"
        );
        assert_eq!(
            parse_since("2026-01-31T00:00:00Z").unwrap(),
            "2026-01-31T00:00:00Z"
        );
    }

    /// The normalised output must sort the same way the stored column does,
    /// which is the property the whole filter rests on.
    #[test]
    fn the_normalised_shape_sorts_like_the_stored_column() {
        let stored = "2026-08-27T22:58:58Z";
        assert!(parse_since("2026-01-01").unwrap().as_str() < stored);
        assert!(parse_since("2099-12-31").unwrap().as_str() > stored);
    }
}

/// Failure while resolving a user-supplied path.
#[derive(Debug, thiserror::Error)]
pub enum ResolveUserPathError {
    /// An empty path would otherwise resolve to the process working directory,
    /// which is never a safe interpretation of user input.
    #[error("cannot resolve an empty user path; configure a non-empty path")]
    EmptyPath,
    /// `~` requires a discoverable home directory; never reinterpret it as a
    /// relative path when one is unavailable.
    #[error(
        "cannot expand user path '{input}': no home directory is available; configure an absolute path"
    )]
    HomeDirectoryUnavailable { input: String },
    /// Resolving an ordinary relative path requires the process working
    /// directory.
    #[error("cannot resolve relative user path '{input}' against the current directory: {source}")]
    CurrentDirectoryUnavailable {
        input: String,
        #[source]
        source: std::io::Error,
    },
}

/// Resolve a user-supplied path string to an absolute [`PathBuf`].
///
/// - A leading `~/` (or a bare `~`) expands against [`dirs::home_dir`]. When no
///   home directory is known, resolution fails rather than treating `~` as a
///   relative directory.
/// - Absolute paths pass through unchanged.
/// - Relative paths are absolutized against the current working directory
///   (lexically, without touching the filesystem), so a relative configured
///   path never prints as relative in an error message.
pub fn resolve_user_path(input: &str) -> Result<PathBuf, ResolveUserPathError> {
    resolve_user_path_with_home(input, dirs::home_dir())
}

fn resolve_user_path_with_home(
    input: &str,
    home: Option<PathBuf>,
) -> Result<PathBuf, ResolveUserPathError> {
    if input.trim().is_empty() {
        return Err(ResolveUserPathError::EmptyPath);
    }
    let expanded = if input == "~" {
        match home {
            Some(home) => home,
            None => {
                return Err(ResolveUserPathError::HomeDirectoryUnavailable {
                    input: input.to_string(),
                });
            }
        }
    } else if let Some(rest) = input.strip_prefix("~/") {
        match home {
            Some(home) => home.join(rest),
            None => {
                return Err(ResolveUserPathError::HomeDirectoryUnavailable {
                    input: input.to_string(),
                });
            }
        }
    } else {
        PathBuf::from(input)
    };
    if expanded.is_absolute() {
        Ok(expanded)
    } else {
        std::path::absolute(&expanded).map_err(|source| {
            ResolveUserPathError::CurrentDirectoryUnavailable {
                input: input.to_string(),
                source,
            }
        })
    }
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

/// Resolve a user-facing repository selector deterministically.
///
/// Candidates must already be filtered for the caller's authorization scope.
/// Resolution precedence is exact UID, Unicode-lowercase display name, exact
/// local root or URL, then an unambiguous URL/root substring. Ambiguous names or
/// substrings fail instead of selecting whichever row the store returned first.
pub fn resolve_repo_selector<'a>(
    repos: &'a [nestweaver_schema::Repo],
    selector: &str,
) -> Result<&'a nestweaver_schema::Repo, anyhow::Error> {
    if selector.trim().is_empty() {
        anyhow::bail!("repository selector cannot be empty");
    }

    if let Some(repo) = repos.iter().find(|repo| repo.uid == selector) {
        return Ok(repo);
    }

    let needle = selector.to_lowercase();
    let names = repos
        .iter()
        .filter(|repo| repo_display_name(repo).to_lowercase() == needle)
        .collect::<Vec<_>>();
    match names.as_slice() {
        [repo] => return Ok(*repo),
        [] => {}
        _ => {
            let candidates = names
                .iter()
                .map(|repo| format!("{} ({})", repo_display_name(repo), repo.uid))
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!(
                "repository selector '{selector}' is ambiguous; use an exact UID: {candidates}"
            );
        }
    }

    let exact_locations = repos
        .iter()
        .filter(|repo| {
            repo.url == selector || repo.local_root().is_some_and(|root| root == selector)
        })
        .collect::<Vec<_>>();
    match exact_locations.as_slice() {
        [repo] => return Ok(*repo),
        [] => {}
        _ => {
            let candidates = exact_locations
                .iter()
                .map(|repo| format!("{} ({})", repo_display_name(repo), repo.uid))
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!(
                "repository selector '{selector}' is ambiguous; use an exact UID: {candidates}"
            );
        }
    }

    let matches = repos
        .iter()
        .filter(|repo| {
            repo.url.contains(selector)
                || repo
                    .local_root()
                    .is_some_and(|root| root.contains(selector))
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [repo] => Ok(*repo),
        [] => anyhow::bail!("repo '{selector}' not found in graph"),
        _ => {
            let candidates = matches
                .iter()
                .map(|repo| format!("{} ({})", repo_display_name(repo), repo.uid))
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!(
                "repository selector '{selector}' is ambiguous; use an exact UID: {candidates}"
            )
        }
    }
}

pub mod admin;
pub mod affected_tests;
pub mod agent_guide;
mod artifact_sidecar;
pub mod atomic_changes;
pub mod authz;
pub mod backup;
pub mod bare_clone;
pub mod blast_radius;
pub mod blast_radius_sarif;
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
pub mod contract_change;
pub mod contracts;
pub mod cpu_throttle;
pub mod cross_domain;
pub mod dead_code;
pub mod diff_impact;
pub mod drain;
pub mod embedding;
pub mod eval;
pub mod export;
pub mod export_graph;
pub mod extensions;
pub mod format_comment;
pub mod git_activity;
pub mod git_cmd;
pub mod git_diff;
pub mod guide_rules;
pub mod hash;
pub mod html_to_md;
pub mod hubs;
pub mod index;
pub mod index_limits;
pub mod index_md;
pub mod index_publication;
pub mod interactions;
pub mod investigate;
pub mod jobs;
pub mod manifest;
pub mod mcp_client;
pub mod parse_pool;
pub mod parsed_cache;
pub mod process;
pub mod project;
pub mod publication;
pub mod publication_operation;
pub mod publication_source;
pub mod publication_state;
pub mod pull;
pub mod query;
pub mod read_symbols;
pub mod recency;
pub mod registry;
pub mod repo_head;
pub mod rerank;
pub mod resolution_cache;
pub mod resolver_generation;
pub mod rts_eval;
pub mod scheduler;
pub mod signature_diff;
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
pub mod write_gate;

pub use affected_tests::{
    AffectedTestFile, AffectedTestSymbol, AffectedTestsResult, ChangedSymbolRef, affected_tests,
};
pub use agent_guide::{
    ToolDocEntry, generate_agents_md, generate_agents_md_with_rules, generate_claude_md,
    generate_claude_md_with_rules, generate_cursor_rule, generate_cursor_rule_with_rules,
    generate_guide, generate_guide_with_rules, generate_guide_with_tools, generate_skill,
    generate_skill_with_rules, generate_skill_with_tools,
};
pub use authz::{
    Identity, PermissionSource, StaticConfigPermissionSource, VisibleRepos,
    redact_blast_radius_for_visibility,
};
pub use backup::{
    BackupConfig, BackupManifest, BackupRepoInfo, BackupResult, BackupSizes, RestoreConfig,
    RestoreResult, StagedBackup, backup_inspect, backup_list, backup_restore, backup_save,
    package_staged, stage_backup_from_store,
};
pub use bare_clone::{mint_repo_identity, read_origin_url};
pub use blast_radius::{
    AffectedCluster, AffectedSymbol as BlastAffectedSymbol, AnalysisStatus, BlastRadiusOptions,
    BlastRadiusResult, ChangedSymbol, GateState, Notification, NotificationLevel,
    analyze_blast_radius, changed_files_from_git,
};
pub use blast_radius_sarif::{append_contract_breaks_to_sarif, blast_radius_to_sarif};
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
    ClusterMember, ClusteringOutput, CommunityInfo, LARGE_GRAPH_CLUSTER_RESOLUTION,
    LARGE_GRAPH_SYMBOL_THRESHOLD, SMALL_GRAPH_CLUSTER_RESOLUTION, compute_clusters,
    default_cluster_resolution, load_clusters, save_clusters,
};
pub use cochange::{CoChangeEdge, compute_cochanges, load_cochange_sidecar, save_cochange_sidecar};
pub use config::{
    AuthzConfig, CrossDomainConfig, ExpectedBrainAdoption, ExternalRefConfig, FeatureConfig,
    GitConfig, GlobRule, InferenceConfig, InstanceConfig, LinkConfig, McpServerConfig,
    PrImpactConfig, ProjectConfig, RankingConfig, RepoConfig, RepoType, ResponseConfig,
    SchemaExtensions, SeedResolutionConfig, StorageConfig, UpstreamEntry, WikiSourceConfig,
    WorkspaceConfig, adopt_expected_brain_uuid, append_repo_to_config_file, default_kind_priority,
    default_test_path_patterns, remove_repo_from_config_file, validate_instance_id,
};
pub use contract_change::breaking_changes_from_git;
pub use cross_domain::{
    CrossDomainResult, SymbolIndex, VaultReaders, build_symbol_index,
    build_symbol_index_with_config, discover_cross_domain_links,
    discover_cross_domain_links_for_note, discover_cross_domain_links_for_note_with_index,
    discover_cross_domain_links_for_note_with_index_and_readers,
    discover_cross_domain_links_with_config, discover_cross_domain_links_with_readers,
};
pub use dead_code::{
    DeadCodeConfidence, DeadCodeResult, UnreachableSymbol, detect_dead_code,
    detect_dead_code_cancellable, detect_dead_code_with_confidence,
    detect_dead_code_with_confidence_cancellable, detect_dead_code_with_manifests,
    detect_dead_code_with_manifests_cancellable,
};
pub use eval::{
    EvalComparison, EvalReport, JudgedQuery, PerQueryRow, compare_reports, load_judged_queries,
    mrr, ndcg_at_k, precision_at_k, run_eval,
};
pub use export::{
    ExportScope, export_cypher, export_graphml, export_graphml_scoped, export_mermaid,
    export_text_format,
};
pub use export_graph::export_in_memory_graph;
pub use extensions::{
    AbortMigrationOutcome, ExtensionStore, InstanceExtensionMigration,
    InstanceMigrationFinalizerPlan, abort_instance_extension_migration,
    finalize_instance_extension_migration, get_all_properties, get_last_indexed_at, get_property,
    load_extensions, load_live_extensions, mark_instance_extension_migration_graph_applied,
    mark_instance_extension_migration_reconciled, pending_instance_extension_migration,
    prepare_instance_extension_migration, prepare_instance_extension_migration_with_finalizers,
    prepare_instance_uid_migration_with_finalizers, query_by_property,
    reconcile_deleted_extension_uids, reconcile_extension_handoffs, reconcile_extension_liveness,
    record_last_indexed_at, remove_extension_uid_durable, save_extensions, set_property,
};
pub use guide_rules::{
    HARD_RULES, OwnedRule, RULES_VERSION, Rule, parse_rules_override, render_owned_rules_markdown,
    render_rules_markdown,
};
pub use hubs::{HubNode, attach_cluster_ids, find_hub_nodes};
pub use index::{
    CachedFileMeta, DeletedEmbeddingStateReconciliation, DeletedGraphStateReconciliation,
    DeletionReconciliationError, DeletionReconciliationFailure, DeletionReconciliationStage,
    FILEMETA_VERSION, FileMetaCache, FileMetaSidecar, IncrementalResult, IndexResult,
    finalize_code_graph_deletion, incremental_index, incremental_index_with_name, index_directory,
    index_directory_in_memory, index_directory_with_options, index_directory_with_store,
    index_directory_with_store_cancellable, index_with_reader, index_with_reader_and_write_gate,
    load_filemeta_sidecar, reconcile_deleted_graph_state, remove_repo_sidecar_slices,
    save_filemeta_sidecar,
};
pub use index_md::{
    MarkdownIndexResult, MarkdownRefreshResult, MarkdownSinceResult,
    format_markdown_refresh_summary, index_markdown_directory, index_markdown_directory_in_memory,
    index_markdown_directory_since, index_markdown_directory_since_with_ignore,
    index_markdown_directory_since_with_store_and_ignore, index_markdown_directory_with_ignore,
    index_markdown_directory_with_ignore_and_deletion_count, index_markdown_directory_with_store,
    index_markdown_directory_with_store_and_deletion_count, index_markdown_with_reader,
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
pub use manifest::{
    ManifestInfo, load_manifest_cache, load_manifest_cache_for_db, manifest_cache_path,
    parse_manifest, save_manifest_cache, save_manifest_cache_for_db,
};
pub use process::{
    AffectedProcess, AffectedSymbol, ChangeImpact, ProcessMember, ProcessResult, RiskLevel,
    detect_changes_impact, trace_processes,
};
pub use project::{
    ProjectMaterializationResult, detect_implicit_projects, detect_implicit_projects_with_mode,
    materialize_projects, materialize_projects_with_lease,
};
pub use publication::*;
pub use publication_source::*;
pub use pull::*;
pub use query::{
    BrainContextResult, BrainNode, CODE_CONTEXT_DEFAULT_LIMIT, ContextNode, ContextResult,
    CrossRepoLink, EmbedModelProvider, EmbedQueryFn, FeatureContextResult, FeatureInfo,
    HybridSearchConfig, LinkInfo, LookupResult, SearchIndexProvider, SymbolCandidate, SymbolDetail,
    apply_ranking_priors, build_brain_context, build_brain_context_hybrid,
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
pub use signature_diff::{BreakKind, BreakTier, BreakingChange, diff_public_api, diff_symbol};
pub use snapshot::*;
pub use suggest::{
    Confidence, SuggestedFeature, SuggestedLink, Suggestions, discover_symbol_level_links,
    materialize_declared_links, persist_cross_repo_links, suggest_links,
};
pub use summaries::{
    DEFAULT_SYMBOL_SUMMARY_CAP, SUMMARY_DEFAULT_TOKEN_BUDGET, Summary, SummaryLevel, SummaryStore,
    SymbolSummaries, filter_by_target, generate_summaries, generate_symbol_summaries_bounded,
    load_summaries, merge_and_save_summaries, render_text, save_summaries, truncate_to_budget,
};
pub use watch_code::CodeWatcher;
pub use watcher::{
    BrainWatcher, ShutdownHandle, UpdateOutcome, WatchMutationLease, WatchMutationLeaseFactory,
    WatchMutationRefused,
};
pub use write_gate::{WriteGate, WriteLease};

#[cfg(test)]
mod resolve_user_path_tests {
    use super::*;

    #[test]
    fn tilde_slash_expands_against_home() {
        assert_eq!(
            resolve_user_path_with_home("~/cache/models", Some(PathBuf::from("/home/tester")))
                .expect("home-backed tilde path must resolve"),
            PathBuf::from("/home/tester/cache/models")
        );
    }

    #[test]
    fn bare_tilde_is_the_home_directory() {
        assert_eq!(
            resolve_user_path_with_home("~", Some(PathBuf::from("/home/tester")))
                .expect("bare tilde must resolve to home"),
            PathBuf::from("/home/tester")
        );
    }

    #[test]
    fn absolute_path_passes_through_unchanged() {
        assert_eq!(
            resolve_user_path_with_home("/var/cache/models", Some(PathBuf::from("/home/tester")))
                .expect("absolute path must resolve"),
            PathBuf::from("/var/cache/models")
        );
    }

    #[test]
    fn relative_path_is_absolutized() {
        let resolved =
            resolve_user_path_with_home("rel/cache", None).expect("relative path must resolve");
        assert!(resolved.is_absolute());
        assert!(resolved.ends_with(Path::new("rel").join("cache")));
    }

    #[test]
    fn tilde_without_home_fails_closed() {
        for input in ["~/cache", "~/", "~"] {
            let error = resolve_user_path_with_home(input, None)
                .expect_err("tilde without a home directory must fail");
            assert!(matches!(
                &error,
                ResolveUserPathError::HomeDirectoryUnavailable { input: failed }
                    if failed == input
            ));
            assert!(error.to_string().contains("configure an absolute path"));
        }
    }

    #[test]
    fn empty_path_fails_closed_without_erasing_meaningful_spaces() {
        for input in ["", " ", "\t\r\n"] {
            let error = resolve_user_path_with_home(input, Some(PathBuf::from("/home/tester")))
                .expect_err("empty user paths must not resolve to the working directory");
            assert!(matches!(error, ResolveUserPathError::EmptyPath));
            assert!(error.to_string().contains("non-empty path"));
        }

        let resolved = resolve_user_path_with_home(
            "cache dir/with spaces",
            Some(PathBuf::from("/home/tester")),
        )
        .expect("a meaningful path containing spaces must resolve");
        assert!(resolved.ends_with(Path::new("cache dir").join("with spaces")));
    }
}

#[cfg(test)]
mod repo_selector_tests {
    use super::*;
    use nestweaver_schema::Repo;

    fn repo(uid: &str, url: &str, name: Option<&str>, root: Option<&str>) -> Repo {
        Repo {
            uid: uid.to_string(),
            url: url.to_string(),
            indexed_sha: String::new(),
            staleness_commits_behind: 0,
            instance_id: "test".to_string(),
            name: name.map(str::to_owned),
            root_path: root.map(str::to_owned),
        }
    }

    #[test]
    fn selector_uses_uid_name_root_url_and_rejects_ambiguity() {
        let repos = vec![
            repo(
                "repo:exact",
                "https://example.test/org/api.git",
                Some("Überblick"),
                Some("/work/api"),
            ),
            repo(
                "repo:other",
                "https://example.test/other/api.git",
                Some("Worker"),
                Some("/work/worker"),
            ),
        ];

        assert_eq!(
            resolve_repo_selector(&repos, "repo:exact").unwrap().uid,
            "repo:exact"
        );
        assert_eq!(
            resolve_repo_selector(&repos, "überblick").unwrap().uid,
            "repo:exact"
        );
        assert_eq!(
            resolve_repo_selector(&repos, "/work/worker").unwrap().uid,
            "repo:other"
        );
        assert_eq!(
            resolve_repo_selector(&repos, "other/api").unwrap().uid,
            "repo:other"
        );
        assert!(
            resolve_repo_selector(&repos, "api.git")
                .unwrap_err()
                .to_string()
                .contains("ambiguous")
        );
        assert!(
            resolve_repo_selector(&repos, "")
                .unwrap_err()
                .to_string()
                .contains("empty")
        );
    }

    #[test]
    fn selector_rejects_duplicate_exact_display_names() {
        let repos = vec![
            repo("repo:a", "file:///a", Some("same"), Some("/a")),
            repo("repo:b", "file:///b", Some("SAME"), Some("/b")),
        ];
        let error = resolve_repo_selector(&repos, "Same")
            .unwrap_err()
            .to_string();
        assert!(error.contains("ambiguous"), "{error}");
        assert!(
            error.contains("repo:a") && error.contains("repo:b"),
            "{error}"
        );
    }

    #[test]
    fn selector_rejects_duplicate_exact_urls_and_local_roots() {
        let duplicate_url = vec![
            repo(
                "repo:a",
                "https://example.test/shared.git",
                Some("a"),
                Some("/a"),
            ),
            repo(
                "repo:b",
                "https://example.test/shared.git",
                Some("b"),
                Some("/b"),
            ),
        ];
        let error = resolve_repo_selector(&duplicate_url, "https://example.test/shared.git")
            .unwrap_err()
            .to_string();
        assert!(error.contains("ambiguous"), "{error}");
        assert!(
            error.contains("repo:a") && error.contains("repo:b"),
            "{error}"
        );

        let duplicate_root = vec![
            repo("repo:a", "file:///a", Some("a"), Some("/work/shared")),
            repo("repo:b", "file:///b", Some("b"), Some("/work/shared")),
        ];
        let error = resolve_repo_selector(&duplicate_root, "/work/shared")
            .unwrap_err()
            .to_string();
        assert!(error.contains("ambiguous"), "{error}");
        assert!(
            error.contains("repo:a") && error.contains("repo:b"),
            "{error}"
        );
    }
}
