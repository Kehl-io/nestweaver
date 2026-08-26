//! Per-tool routing matrix — maps each MCP tool to a routing category
//! that determines how queries are dispatched across local and upstream servers.

/// How a tool's results should be routed and merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolRouting {
    /// Query both local and server in parallel, merge results via RRF.
    /// Used for: brain_search, brain_context, project_context
    Merge,
    /// Query local first. Only query server for repos not indexed locally.
    /// Used for: read_symbols, investigate, investigate_hydrate
    LocalFirst,
    /// Query server preferentially. Local overlays for uncommitted changes.
    /// Used for: hub_nodes, bridge_nodes, clusters, dead_code, cross_repo_contracts, contract_drift
    ServerPreferred,
    /// Two-tier: show local impact + org-wide impact separately.
    /// Used for: blast_radius, brain_impact, affected_tests
    TwoTier,
    /// Local for local repos, server for server-only repos. Fan out.
    /// Used for: regex_search, count_patterns
    FanOut,
    /// Local only — never query server.
    /// Used for: detect_changes
    LocalOnly,
    /// Combined view from both sources.
    /// Used for: brain_status, stale_check, brain_doc_stats
    Combined,
    /// Local-first, server continuation at boundaries.
    /// Used for: flow_trace, investigate_expand
    Continuation,
}

/// Map an MCP tool name to its routing category.
pub fn tool_routing(tool_name: &str) -> ToolRouting {
    match tool_name {
        // Search tools — merge
        "brain_search" | "brain_context" | "code_context" | "project_context"
        | "search_symbols" => ToolRouting::Merge,

        // Navigation tools — local-first
        "read_symbols" | "investigate" | "investigate_hydrate" => ToolRouting::LocalFirst,

        // Navigation tools — continuation
        "flow_trace" | "investigate_expand" => ToolRouting::Continuation,

        // Structural analysis — server-preferred
        "hub_nodes"
        | "bridge_nodes"
        | "clusters"
        | "dead_code"
        | "cross_repo_contracts"
        | "contract_drift" => ToolRouting::ServerPreferred,

        // Impact analysis — two-tier
        "blast_radius" | "brain_impact" | "affected_tests" => ToolRouting::TwoTier,

        // File-level tools — fan-out
        "regex_search" | "count_patterns" => ToolRouting::FanOut,

        // Local-only
        "detect_changes" => ToolRouting::LocalOnly,

        // Metadata — combined
        "brain_status"
        | "stale_check"
        | "brain_doc_stats"
        | "brain_tag_graph"
        | "brain_topic_clusters" => ToolRouting::Combined,

        // Vault/note tools — merge (notes may be on server vault)
        "note_get" | "backlinks" | "get_summary" | "brain_guide" | "brain_diff" => {
            ToolRouting::Merge
        }

        // Vault analysis — server-preferred (need full vault)
        "brain_broken_links" | "brain_orphan_documents" => ToolRouting::ServerPreferred,

        // Memory tools — local-only (personal vault)
        "brain_memory_lint" | "brain_memory_consolidate" | "brain_memory_related" => {
            ToolRouting::LocalOnly
        }

        // Admin/mutation tools — local-only.
        //
        // EVERY entry of `nestweaver_mcp::http::MUTATING_TOOLS` must appear
        // here: a mutation sent upstream would write to someone else's graph.
        // `compact_embeddings` was missing, so it fell to the `LocalFirst`
        // default below, reached `dispatch_json_rpc`, found no arm, and failed
        // with "unsupported tool for JSON dispatch" — advertised in
        // `tools/list` and broken for every caller with an upstream.
        //
        // This arm is deliberately a SUPERSET: `query_extensions` and the
        // memory tools are local-only reads over a personal vault, not
        // mutations. That is why it cannot simply BE the canonical list — but
        // the containment is pinned by
        // `every_mutating_tool_is_local_only` in nestweaver-mcp.
        "brain_add_source"
        | "brain_remove_source"
        | "prune_stale"
        | "set_extension"
        | "compact_embeddings"
        | "query_extensions" => ToolRouting::LocalOnly,

        // Everything else — local-first (safe default)
        _ => ToolRouting::LocalFirst,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_tools_are_merge() {
        assert_eq!(tool_routing("brain_search"), ToolRouting::Merge);
        assert_eq!(tool_routing("brain_context"), ToolRouting::Merge);
        assert_eq!(tool_routing("code_context"), ToolRouting::Merge);
        assert_eq!(
            tool_routing("compact_embeddings"),
            ToolRouting::LocalOnly,
            "a mutation must never be routed upstream"
        );
        assert_eq!(tool_routing("project_context"), ToolRouting::Merge);
        assert_eq!(tool_routing("search_symbols"), ToolRouting::Merge);
    }

    #[test]
    fn navigation_tools_are_local_first() {
        assert_eq!(tool_routing("read_symbols"), ToolRouting::LocalFirst);
        assert_eq!(tool_routing("investigate"), ToolRouting::LocalFirst);
        assert_eq!(tool_routing("investigate_hydrate"), ToolRouting::LocalFirst);
    }

    #[test]
    fn continuation_tools() {
        assert_eq!(tool_routing("flow_trace"), ToolRouting::Continuation);
        assert_eq!(
            tool_routing("investigate_expand"),
            ToolRouting::Continuation
        );
    }

    #[test]
    fn structural_tools_are_server_preferred() {
        assert_eq!(tool_routing("hub_nodes"), ToolRouting::ServerPreferred);
        assert_eq!(tool_routing("bridge_nodes"), ToolRouting::ServerPreferred);
        assert_eq!(tool_routing("clusters"), ToolRouting::ServerPreferred);
        assert_eq!(tool_routing("dead_code"), ToolRouting::ServerPreferred);
        assert_eq!(
            tool_routing("cross_repo_contracts"),
            ToolRouting::ServerPreferred
        );
        assert_eq!(tool_routing("contract_drift"), ToolRouting::ServerPreferred);
    }

    #[test]
    fn impact_tools_are_two_tier() {
        assert_eq!(tool_routing("blast_radius"), ToolRouting::TwoTier);
        assert_eq!(tool_routing("brain_impact"), ToolRouting::TwoTier);
        assert_eq!(tool_routing("affected_tests"), ToolRouting::TwoTier);
    }

    #[test]
    fn fan_out_tools() {
        assert_eq!(tool_routing("regex_search"), ToolRouting::FanOut);
        assert_eq!(tool_routing("count_patterns"), ToolRouting::FanOut);
    }

    #[test]
    fn detect_changes_is_local_only() {
        assert_eq!(tool_routing("detect_changes"), ToolRouting::LocalOnly);
    }

    #[test]
    fn metadata_tools_are_combined() {
        assert_eq!(tool_routing("brain_status"), ToolRouting::Combined);
        assert_eq!(tool_routing("stale_check"), ToolRouting::Combined);
        assert_eq!(tool_routing("brain_doc_stats"), ToolRouting::Combined);
    }

    #[test]
    fn unknown_tool_defaults_to_local_first() {
        assert_eq!(tool_routing("some_new_tool"), ToolRouting::LocalFirst);
        assert_eq!(tool_routing("future_feature"), ToolRouting::LocalFirst);
    }

    #[test]
    fn two_tier_tools_are_distinct() {
        let tools = ["blast_radius", "brain_impact", "affected_tests"];
        for tool in &tools {
            assert_eq!(
                tool_routing(tool),
                ToolRouting::TwoTier,
                "{tool} should be TwoTier"
            );
        }
    }
}
