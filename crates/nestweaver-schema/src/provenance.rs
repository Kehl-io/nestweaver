//! The single spelling of result provenance (`_meta`).
//!
//! Provenance answers three questions a caller cannot answer any other way:
//! *what scope was this computed over*, *which sources contributed*, and *which
//! repos were stale when it was computed*. Before nw-315 the product had
//! **four** authors of that answer and **three** incompatible spellings:
//!
//! * `src/main.rs` (`local_result_meta` / `attach_local_meta`) — the CLI direct
//!   route, unprefixed keys, on the payload;
//! * `nestweaver-federation::results` (`inject_provenance` / `set_stale_repos`)
//!   — the CLI daemon route, unprefixed keys, on the payload;
//! * `nestweaver-mcp::http` (`add_provenance_metadata`) — namespaced
//!   `nestweaver.io/*` keys, on the OUTER `tools/call` envelope;
//! * MCP over stdio — nothing at all, while `SERVER_INSTRUCTIONS` promised the
//!   agent that "Results include `_meta.sources`".
//!
//! Four authors is why the fourth one was missing: nothing named the property,
//! so nothing could notice its absence. This module is that name. It lives in
//! `nestweaver-schema` because that is the only crate every provenance author
//! already depends on — `nestweaver-federation` depends on schema + proto and
//! `nestweaver-mcp` depends on federation only behind the `daemon` feature, so
//! a helper in either of those could not serve both.
//!
//! The spelling is the unprefixed one, on the PAYLOAD, because that is what the
//! server's own `initialize` instructions promise and what both CLI routes
//! already emit.

use serde_json::Value;

/// The key provenance is written under. One constant so a fifth author cannot
/// invent a fifth spelling without deleting this line.
pub const META_KEY: &str = "_meta";

/// The scope label for a result computed against the local graph with no
/// upstreams involved.
pub const SCOPE_LOCAL: &str = "local";

/// The source label for the local graph.
pub const SOURCE_LOCAL: &str = "local";

/// The scope implied by a source list when the caller has no better label.
///
/// One source is that source's own name; more than one is a hybrid answer.
/// Matches what `nestweaver-federation` has always done — it is lifted here
/// rather than restated.
pub fn derived_scope<'a>(sources: &[&'a str]) -> &'a str {
    if sources.len() > 1 {
        "hybrid"
    } else {
        sources.first().copied().unwrap_or(SCOPE_LOCAL)
    }
}

/// Build the provenance object itself.
pub fn provenance(scope: &str, sources: &[&str], stale_repos: &[String]) -> Value {
    serde_json::json!({
        "scope": scope,
        "sources": sources,
        "stale_repos": stale_repos,
    })
}

/// Write provenance onto a payload, replacing whatever was there.
///
/// Used by the layer that knows the most about where the answer came from — the
/// federation client and the HTTP boundary both overwrite the tool layer's
/// honest-but-narrow local stamp with the federated truth.
///
/// A non-object payload is left alone; see [`set_or_wrap`] for the callers that
/// need a bare array to become addressable.
pub fn set(result: &mut Value, scope: &str, sources: &[&str], stale_repos: &[String]) {
    if let Some(obj) = result.as_object_mut() {
        obj.insert(META_KEY.to_string(), provenance(scope, sources, stale_repos));
    }
}

/// Write provenance only if the payload does not already carry it.
///
/// This is the tool layer's stamp. It must not clobber a richer verdict that a
/// federating caller has already written, and it must not be silently skipped
/// either — a payload with no `_meta` at all is the nw-315 defect.
pub fn ensure(result: &mut Value, scope: &str, sources: &[&str], stale_repos: &[String]) {
    if let Some(obj) = result.as_object_mut() {
        obj.entry(META_KEY)
            .or_insert_with(|| provenance(scope, sources, stale_repos));
    }
}

/// Like [`set`], but wraps a bare array under `results` first.
///
/// A few legacy JSON RPCs (notably `search_symbols`) still return a bare array,
/// which has nowhere to put a `_meta` key. Callers still need to know where the
/// data came from, so preserve the array under `results` rather than dropping
/// the provenance.
pub fn set_or_wrap(result: &mut Value, scope: &str, sources: &[&str], stale_repos: &[String]) {
    if result.is_array() {
        let items = result.take();
        *result = serde_json::json!({
            "results": items,
            META_KEY: provenance(scope, sources, stale_repos),
        });
    } else {
        set(result, scope, sources, stale_repos);
    }
}

/// Update only the `stale_repos` leg, creating `_meta` if absent.
///
/// The staleness verdict is computed by a background health check in the
/// federation client and is not available to the tool layer, so it is applied
/// as a separate enrichment pass rather than folded into the stamp.
pub fn set_stale_repos(result: &mut Value, stale_repos: &[String]) {
    if let Some(obj) = result.as_object_mut() {
        let meta = obj
            .entry(META_KEY)
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        if let Some(meta_obj) = meta.as_object_mut() {
            meta_obj.insert(
                "stale_repos".to_string(),
                serde_json::to_value(stale_repos).unwrap_or(Value::Null),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_legs_are_always_present_together() {
        let mut payload = serde_json::json!({ "answer": 1 });
        ensure(&mut payload, SCOPE_LOCAL, &[SOURCE_LOCAL], &[]);
        let meta = &payload[META_KEY];
        for leg in ["scope", "sources", "stale_repos"] {
            assert!(
                meta.get(leg).is_some(),
                "provenance without `{leg}` is not provenance: {payload}"
            );
        }
        assert_eq!(meta["sources"], serde_json::json!(["local"]));
    }

    #[test]
    fn ensure_does_not_clobber_a_richer_verdict() {
        let mut payload = serde_json::json!({ "answer": 1 });
        set(
            &mut payload,
            "federated",
            &["daemon", "upstream"],
            &["repo-a".to_string()],
        );
        ensure(&mut payload, SCOPE_LOCAL, &[SOURCE_LOCAL], &[]);
        assert_eq!(payload[META_KEY]["scope"], serde_json::json!("federated"));
        assert_eq!(
            payload[META_KEY]["stale_repos"],
            serde_json::json!(["repo-a"])
        );
    }

    #[test]
    fn a_bare_array_is_wrapped_rather_than_losing_its_provenance() {
        let mut payload = serde_json::json!([1, 2, 3]);
        set_or_wrap(&mut payload, SCOPE_LOCAL, &[SOURCE_LOCAL], &[]);
        assert_eq!(payload["results"], serde_json::json!([1, 2, 3]));
        assert_eq!(payload[META_KEY]["scope"], serde_json::json!("local"));
    }

    #[test]
    fn stale_repos_can_be_enriched_onto_an_existing_stamp() {
        let mut payload = serde_json::json!({ "answer": 1 });
        ensure(&mut payload, SCOPE_LOCAL, &[SOURCE_LOCAL], &[]);
        set_stale_repos(&mut payload, &["repo-b".to_string()]);
        assert_eq!(
            payload[META_KEY]["stale_repos"],
            serde_json::json!(["repo-b"])
        );
        assert_eq!(payload[META_KEY]["scope"], serde_json::json!("local"));
    }

    #[test]
    fn scope_is_derived_the_same_way_everywhere() {
        assert_eq!(derived_scope(&["local"]), "local");
        assert_eq!(derived_scope(&["local", "server"]), "hybrid");
        assert_eq!(derived_scope(&[]), "local");
    }
}
