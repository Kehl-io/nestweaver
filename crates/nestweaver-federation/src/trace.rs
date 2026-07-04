//! Cross-repo flow-trace boundary detection and server-span stitching.
//!
//! Pure functions over the flow_trace JSON tree: find the leaves the local
//! index could not follow (cross-repo boundaries) and graft the upstream
//! server's continuation spans back into the local result tree.

use serde_json::Value;
use tracing::debug;

/// A boundary symbol detected in a local flow_trace result.
///
/// The local trace knows the symbol name and canonical_id but cannot
/// follow the call graph past it because the target repo is not indexed
/// locally.
#[derive(Debug, Clone)]
pub struct TraceBoundary {
    /// The canonical_id of the boundary symbol.
    pub canonical_id: String,
    /// The symbol name (for display/logging).
    pub name: String,
    /// The span_id (or JSON path) of the parent node in the local trace,
    /// used for stitching the server continuation back into the tree.
    pub parent_path: Vec<String>,
}

/// Detect cross-repo boundary symbols in a flow_trace JSON result tree.
///
/// A boundary is a leaf node whose `repo_uid` differs from the root
/// node's `repo_uid` (the locally-initiated trace) **and** is not itself a
/// repo the local index knows about. These represent cross-repo call edges
/// the local daemon cannot follow — the callee resolves in another repo (a
/// `CROSS_REPO_LINK` stub) that is unresolved locally, so the upstream
/// server should continue the trace from there.
///
/// `local_repos` is the set of `repo_uid`s the LOCAL daemon has indexed (see
/// the local daemon's
/// `RepoStates`). When the local daemon indexes more
/// than one repo, a trace can legitimately resolve *into* another local repo;
/// that leaf carries a foreign `repo_uid` but is still locally followed, so
/// flagging it would emit a spurious server continuation. Excluding leaves
/// whose `repo_uid` is in `local_repos` prevents that false positive. An
/// empty set restores the prior "any foreign-repo leaf is a boundary"
/// behavior, which is the safe default when the local repo set is unknown.
///
/// Two flow_trace response shapes are handled:
/// - the standard single-root trace `{ root_uid, tree: {...} }`, and
/// - the class-expanded trace `{ root_uid, methods: [ {...}, ... ] }`
///   produced when the root symbol is a class (mirrors the `methods`
///   handling in [`stitch_server_spans`]).
///
/// Requires flow_trace output to include `repo_uid` and `canonical_id`
/// fields on each node (the detailed output format; concise traces omit
/// them and therefore yield no boundaries).
///
/// See architecture spec: cross-boundary-flow-trace.md
pub fn detect_boundaries_in_trace(
    result: &Value,
    local_repos: &std::collections::HashSet<String>,
) -> Vec<TraceBoundary> {
    let mut boundaries = Vec::new();

    if let Some(tree) = result.get("tree") {
        // Standard single-root trace.
        collect_from_root(tree, local_repos, &mut boundaries);
    } else if let Some(methods) = result.get("methods").and_then(|v| v.as_array()) {
        // Class-expanded trace: each method is its own subtree rooted in the
        // class's repo. Use the first method that carries a repo_uid as the
        // local-repo reference (all methods of a class share its repo).
        let root_repo = methods
            .iter()
            .find_map(nonempty_repo_uid)
            .unwrap_or_default();
        if root_repo.is_empty() {
            debug!(
                "detect_boundaries_in_trace: class-expanded trace lacks repo_uid, cannot detect boundaries"
            );
        } else {
            for method in methods {
                let mut path = Vec::new();
                collect_boundaries(method, &root_repo, local_repos, &mut path, &mut boundaries);
            }
        }
    } else {
        // The result itself may be a bare trace node.
        collect_from_root(result, local_repos, &mut boundaries);
    }

    debug!(
        count = boundaries.len(),
        "detect_boundaries_in_trace: found boundary nodes"
    );
    boundaries
}

/// The node's `repo_uid` as an owned `String`, or `None` when absent/empty.
fn nonempty_repo_uid(node: &Value) -> Option<String> {
    node.get("repo_uid")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Walk a single trace tree rooted at `root`, collecting boundaries against
/// the root's own `repo_uid`. A no-op when the root lacks a `repo_uid`.
fn collect_from_root(
    root: &Value,
    local_repos: &std::collections::HashSet<String>,
    out: &mut Vec<TraceBoundary>,
) {
    let Some(root_repo) = nonempty_repo_uid(root) else {
        debug!("detect_boundaries_in_trace: root node lacks repo_uid, cannot detect boundaries");
        return;
    };
    let mut path = Vec::new();
    collect_boundaries(root, &root_repo, local_repos, &mut path, out);
}

/// Recursively walk the flow_trace tree collecting boundary nodes.
fn collect_boundaries(
    node: &Value,
    root_repo: &str,
    local_repos: &std::collections::HashSet<String>,
    path: &mut Vec<String>,
    out: &mut Vec<TraceBoundary>,
) {
    let children = node.get("children").and_then(|v| v.as_array());
    let is_leaf = children.is_none_or(|c| c.is_empty());
    let repo_uid = node.get("repo_uid").and_then(|v| v.as_str()).unwrap_or("");
    let canonical_id = node
        .get("canonical_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let name = node.get("name").and_then(|v| v.as_str()).unwrap_or("");

    // A boundary: different repo than the root, is a leaf (trace couldn't
    // follow), has a canonical_id for cross-boundary matching, and is NOT a
    // repo the local index knows about. The last clause is the false-positive
    // guard: a foreign-repo leaf that the local daemon *can* resolve (another
    // locally-indexed repo) was already followed here, so continuing it on the
    // server would be a spurious round-trip.
    if is_leaf
        && !repo_uid.is_empty()
        && repo_uid != root_repo
        && !canonical_id.is_empty()
        && !local_repos.contains(repo_uid)
    {
        out.push(TraceBoundary {
            canonical_id: canonical_id.to_string(),
            name: name.to_string(),
            parent_path: path.clone(),
        });
    }

    if let Some(children) = children {
        path.push(name.to_string());
        for child in children {
            collect_boundaries(child, root_repo, local_repos, path, out);
        }
        path.pop();
    }
}

/// Stitch server-side trace spans into a local flow_trace result tree.
///
/// Given a local trace result (JSON tree) and server continuation
/// response (spans from FlowTraceContinue RPC), merge the server spans
/// into the tree at the correct boundary point.
///
/// The merge strategy:
/// 1. Find the node in the local tree matching `parent_span_id`
/// 2. Convert server spans into the same JSON tree format
/// 3. Append server subtrees as children of the boundary node
/// 4. Annotate server-sourced nodes with `"source": "server"`
pub fn stitch_server_spans(
    local_result: &mut Value,
    server_spans: &[nestweaver_proto::TraceSpanProto],
    boundary_canonical_id: &str,
    server_name: &str,
) {
    if server_spans.is_empty() {
        return;
    }

    // Build a lookup from span_id -> span for parent linkage.
    let span_map: std::collections::HashMap<&str, &nestweaver_proto::TraceSpanProto> = server_spans
        .iter()
        .map(|s| (s.span_id.as_str(), s))
        .collect();

    // Find the root span(s) — those whose canonical_id matches the boundary.
    let root_spans: Vec<&nestweaver_proto::TraceSpanProto> = server_spans
        .iter()
        .filter(|s| s.canonical_id == boundary_canonical_id)
        .collect();

    if root_spans.is_empty() {
        return;
    }

    // Build JSON subtree(s) from server spans.
    fn build_subtree(
        span: &nestweaver_proto::TraceSpanProto,
        span_map: &std::collections::HashMap<&str, &nestweaver_proto::TraceSpanProto>,
        server_name: &str,
    ) -> Value {
        let children: Vec<Value> = span
            .callee_span_ids
            .iter()
            .filter_map(|cid| span_map.get(cid.as_str()))
            .map(|child| build_subtree(child, span_map, server_name))
            .collect();

        serde_json::json!({
            "name": span.name,
            "file_path": span.file_path,
            "canonical_id": span.canonical_id,
            "source": format!("server:{}", server_name),
            "children": children,
        })
    }

    let subtrees: Vec<Value> = root_spans
        .iter()
        .map(|s| build_subtree(s, &span_map, server_name))
        .collect();

    // Find the boundary node in the local tree and inject server subtrees.
    // The boundary node is a leaf with matching canonical_id.
    fn inject_at_boundary(
        node: &mut Value,
        boundary_cid: &str,
        subtrees: &[Value],
        server_name: &str,
    ) -> bool {
        // Check if this node is the boundary (leaf with matching canonical_id).
        if let Some(cid) = node.get("canonical_id").and_then(|v| v.as_str())
            && cid == boundary_cid
        {
            // Inject children.
            if let Some(children) = node.get_mut("children") {
                if let Some(arr) = children.as_array_mut() {
                    arr.extend_from_slice(subtrees);
                }
            } else if let Some(obj) = node.as_object_mut() {
                obj.insert("children".to_string(), Value::Array(subtrees.to_vec()));
                obj.insert(
                    "boundary_crossed".to_string(),
                    Value::String(format!("-> {}", server_name)),
                );
            }
            return true;
        }

        // Recurse into children.
        if let Some(children) = node.get_mut("children")
            && let Some(arr) = children.as_array_mut()
        {
            for child in arr.iter_mut() {
                if inject_at_boundary(child, boundary_cid, subtrees, server_name) {
                    return true;
                }
            }
        }

        false
    }

    // Try to inject into the "tree" field of the response.
    if let Some(tree) = local_result.get_mut("tree") {
        inject_at_boundary(tree, boundary_canonical_id, &subtrees, server_name);
    }

    // Also try "methods" array for class-expanded traces.
    if let Some(methods) = local_result.get_mut("methods")
        && let Some(arr) = methods.as_array_mut()
    {
        for method in arr.iter_mut() {
            inject_at_boundary(method, boundary_canonical_id, &subtrees, server_name);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── Trace stitching tests ────────────────────────────────────

    #[test]
    fn stitch_server_spans_into_local_tree() {
        use nestweaver_proto::TraceSpanProto;

        // Local tree: A -> B (boundary)
        let mut local_result = json!({
            "root_uid": "uid-a",
            "root_name": "funcA",
            "max_depth": 5,
            "tree": {
                "uid": "uid-a",
                "name": "funcA",
                "file_path": "src/a.rs",
                "depth": 0,
                "children": [{
                    "uid": "uid-b",
                    "name": "funcB",
                    "canonical_id": "abc123:src/b.rs#funcB:def456",
                    "file_path": "src/b.rs",
                    "depth": 1,
                    "children": []
                }]
            }
        });

        // Server spans: B -> C -> D
        let spans = vec![
            TraceSpanProto {
                trace_id: "t1".into(),
                span_id: "span-b".into(),
                parent_span_id: None,
                canonical_id: "abc123:src/b.rs#funcB:def456".into(),
                name: "funcB".into(),
                repo_url: "https://github.com/acme/api".into(),
                file_path: "src/b.rs".into(),
                start_line: 10,
                callee_span_ids: vec!["span-c".into()],
                source: "server".into(),
            },
            TraceSpanProto {
                trace_id: "t1".into(),
                span_id: "span-c".into(),
                parent_span_id: Some("span-b".into()),
                canonical_id: "abc123:src/c.rs#funcC:ghi789".into(),
                name: "funcC".into(),
                repo_url: "https://github.com/acme/api".into(),
                file_path: "src/c.rs".into(),
                start_line: 20,
                callee_span_ids: vec![],
                source: "server".into(),
            },
        ];

        stitch_server_spans(
            &mut local_result,
            &spans,
            "abc123:src/b.rs#funcB:def456",
            "acme-server",
        );

        // Verify the boundary node now has server children.
        let tree = &local_result["tree"];
        let boundary_node = &tree["children"][0];
        assert_eq!(boundary_node["name"], "funcB");

        let stitched_children = boundary_node["children"].as_array().unwrap();
        assert!(
            !stitched_children.is_empty(),
            "boundary node should have server-sourced children"
        );

        // The stitched root should be funcB with child funcC.
        let server_root = &stitched_children[0];
        assert_eq!(server_root["name"], "funcB");
        assert!(
            server_root["source"]
                .as_str()
                .unwrap()
                .contains("acme-server")
        );

        let server_children = server_root["children"].as_array().unwrap();
        assert_eq!(server_children.len(), 1);
        assert_eq!(server_children[0]["name"], "funcC");
    }

    #[test]
    fn stitch_empty_spans_is_noop() {
        let mut local_result = json!({
            "tree": {
                "name": "funcA",
                "children": []
            }
        });
        let original = local_result.clone();

        stitch_server_spans(&mut local_result, &[], "some-cid", "server");

        assert_eq!(local_result, original);
    }

    /// Empty local-repo set: the common single-repo case where every
    /// foreign-repo leaf is a genuine cross-repo boundary.
    fn no_local_repos() -> std::collections::HashSet<String> {
        std::collections::HashSet::new()
    }

    #[test]
    fn detect_boundaries_needs_root_repo_uid() {
        // Concise / unannotated traces have no repo_uid on the root, so the
        // local-repo reference is unknown and nothing can be flagged.
        let result = json!({
            "tree": {
                "name": "funcA",
                "children": [{"name": "funcB", "children": []}]
            }
        });
        assert!(
            detect_boundaries_in_trace(&result, &no_local_repos()).is_empty(),
            "no repo_uid on root means no boundaries"
        );
    }

    #[test]
    fn detect_boundaries_flags_cross_repo_leaf() {
        // A leaf whose repo_uid differs from the root's, carrying a
        // canonical_id, is a cross-repo boundary the server should continue.
        let result = json!({
            "tree": {
                "name": "funcA",
                "repo_uid": "local-repo",
                "canonical_id": "abc:src/lib.rs#funcA:xyz",
                "children": [{
                    "name": "funcB",
                    "repo_uid": "remote-repo",
                    "canonical_id": "def:src/api.rs#funcB:uvw",
                    "children": []
                }]
            }
        });
        let boundaries = detect_boundaries_in_trace(&result, &no_local_repos());
        assert_eq!(boundaries.len(), 1);
        assert_eq!(boundaries[0].name, "funcB");
        assert_eq!(boundaries[0].canonical_id, "def:src/api.rs#funcB:uvw");
        // parent_path records the chain of node names from the root down to
        // (but excluding) the boundary, for stitching the continuation back.
        assert_eq!(boundaries[0].parent_path, vec!["funcA".to_string()]);
    }

    #[test]
    fn detect_boundaries_records_nested_parent_path() {
        // root(A) -> mid(A) -> leaf(B): the boundary is the deep leaf and its
        // parent_path is the full name chain above it.
        let result = json!({
            "tree": {
                "name": "root",
                "repo_uid": "A",
                "canonical_id": "a:root",
                "children": [{
                    "name": "mid",
                    "repo_uid": "A",
                    "canonical_id": "a:mid",
                    "children": [{
                        "name": "leaf",
                        "repo_uid": "B",
                        "canonical_id": "b:leaf",
                        "children": []
                    }]
                }]
            }
        });
        let boundaries = detect_boundaries_in_trace(&result, &no_local_repos());
        assert_eq!(boundaries.len(), 1);
        assert_eq!(boundaries[0].canonical_id, "b:leaf");
        assert_eq!(
            boundaries[0].parent_path,
            vec!["root".to_string(), "mid".to_string()]
        );
    }

    #[test]
    fn detect_boundaries_ignores_same_repo_and_uncrossable_leaves() {
        // A same-repo leaf is not a boundary; a foreign leaf without a
        // canonical_id cannot be matched on the server, so it is skipped too.
        let result = json!({
            "tree": {
                "name": "root",
                "repo_uid": "A",
                "canonical_id": "a:root",
                "children": [
                    { "name": "localChild", "repo_uid": "A", "canonical_id": "a:child", "children": [] },
                    { "name": "foreignNoCid", "repo_uid": "B", "canonical_id": "", "children": [] }
                ]
            }
        });
        assert!(
            detect_boundaries_in_trace(&result, &no_local_repos()).is_empty(),
            "same-repo leaves and canonical-id-less foreign leaves are not boundaries"
        );
    }

    #[test]
    fn detect_boundaries_requires_leaf() {
        // A foreign node that still has locally-resolved children is not a
        // leaf: the local trace already followed past it, so it is not a
        // continuation boundary (only the genuine leaf below it could be).
        let result = json!({
            "tree": {
                "name": "root",
                "repo_uid": "A",
                "canonical_id": "a:root",
                "children": [{
                    "name": "foreignWithChild",
                    "repo_uid": "B",
                    "canonical_id": "b:foreign",
                    "children": [
                        { "name": "deeperLocal", "repo_uid": "A", "canonical_id": "a:deep", "children": [] }
                    ]
                }]
            }
        });
        assert!(
            detect_boundaries_in_trace(&result, &no_local_repos()).is_empty(),
            "a foreign node with local children is not a leaf boundary"
        );
    }

    #[test]
    fn detect_boundaries_walks_class_expanded_methods() {
        // Class-expanded traces have no `tree`; each method is its own subtree
        // rooted in the class's repo. A cross-repo leaf under any method must
        // still be detected (parity with stitch_server_spans' `methods`
        // handling).
        let result = json!({
            "root_uid": "sym:repoA::Klass",
            "root_kind": "class",
            "methods": [
                {
                    "name": "methodNoCross",
                    "repo_uid": "A",
                    "canonical_id": "a:m1",
                    "children": [
                        { "name": "localHelper", "repo_uid": "A", "canonical_id": "a:h", "children": [] }
                    ]
                },
                {
                    "name": "methodCalls",
                    "repo_uid": "A",
                    "canonical_id": "a:m2",
                    "children": [
                        { "name": "remoteApi", "repo_uid": "B", "canonical_id": "b:remoteApi", "children": [] }
                    ]
                }
            ]
        });
        let boundaries = detect_boundaries_in_trace(&result, &no_local_repos());
        assert_eq!(boundaries.len(), 1);
        assert_eq!(boundaries[0].name, "remoteApi");
        assert_eq!(boundaries[0].canonical_id, "b:remoteApi");
        assert_eq!(boundaries[0].parent_path, vec!["methodCalls".to_string()]);
    }

    #[test]
    fn detect_boundaries_skips_leaf_resolvable_in_another_local_repo() {
        // Multi-repo local daemon: the trace resolves from repo A INTO repo B,
        // and the local index also has repo B indexed. That leaf carries a
        // foreign repo_uid but is locally followed, so it must NOT be flagged
        // as a cross-repo boundary (no spurious server continuation).
        let result = json!({
            "tree": {
                "name": "funcA",
                "repo_uid": "local-repo-a",
                "canonical_id": "a:src/lib.rs#funcA",
                "children": [{
                    "name": "funcB",
                    "repo_uid": "local-repo-b",
                    "canonical_id": "b:src/api.rs#funcB",
                    "children": []
                }]
            }
        });
        let local_repos: std::collections::HashSet<String> =
            ["local-repo-a".to_string(), "local-repo-b".to_string()]
                .into_iter()
                .collect();
        assert!(
            detect_boundaries_in_trace(&result, &local_repos).is_empty(),
            "a foreign-repo leaf the local index can resolve is not a boundary"
        );
    }

    #[test]
    fn detect_boundaries_flags_leaf_in_unindexed_foreign_repo() {
        // Same multi-repo daemon, but the leaf resolves into a repo the local
        // index does NOT know about. That is a genuine cross-repo edge the
        // server must continue, so it stays a boundary even when other repos
        // are indexed locally.
        let result = json!({
            "tree": {
                "name": "funcA",
                "repo_uid": "local-repo-a",
                "canonical_id": "a:src/lib.rs#funcA",
                "children": [{
                    "name": "funcRemote",
                    "repo_uid": "server-only-repo",
                    "canonical_id": "r:src/remote.rs#funcRemote",
                    "children": []
                }]
            }
        });
        // Local set has another repo (B) but NOT "server-only-repo".
        let local_repos: std::collections::HashSet<String> =
            ["local-repo-a".to_string(), "local-repo-b".to_string()]
                .into_iter()
                .collect();
        let boundaries = detect_boundaries_in_trace(&result, &local_repos);
        assert_eq!(boundaries.len(), 1);
        assert_eq!(boundaries[0].name, "funcRemote");
        assert_eq!(boundaries[0].canonical_id, "r:src/remote.rs#funcRemote");
    }
}
