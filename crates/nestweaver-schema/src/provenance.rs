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
        let stamped = provenance(scope, sources, stale_repos);
        // MERGE the provenance fields into any existing `_meta`, rather than
        // replacing the object.
        //
        // Replacing it silently destroyed every other key a handler had put
        // there. nw-316 hit this first: `project_context` writes
        // `_meta.answered_by` (which instance config produced the answer), and
        // the daemon route stamps provenance AFTER the tool returns -- so the
        // disclosure survived on the direct route and vanished on the daemon
        // one. A field on one route only is exactly the drift nw-316 is about,
        // reintroduced by the fix for it, and the parity harness caught it.
        //
        // Provenance still WINS on its own keys, so a federating caller's
        // verdict is unchanged; what changes is that keys provenance does not
        // own are no longer collateral. `ensure` already documents that not
        // clobbering a richer verdict matters -- this gives `set` the same care
        // for keys outside its vocabulary.
        match (
            obj.get_mut(META_KEY).and_then(Value::as_object_mut),
            stamped,
        ) {
            (Some(existing), Value::Object(fields)) => {
                for (key, value) in fields {
                    existing.insert(key, value);
                }
            }
            (_, stamped) => {
                obj.insert(META_KEY.to_string(), stamped);
            }
        }
    }
}

/// Write provenance only if the payload does not already carry it.
///
/// This is the tool layer's stamp. It must not clobber a richer verdict that a
/// federating caller has already written, and it must not be silently skipped
/// either — a payload with no `_meta` at all is the nw-315 defect.
pub fn ensure(result: &mut Value, scope: &str, sources: &[&str], stale_repos: &[String]) {
    if let Some(obj) = result.as_object_mut() {
        let stamped = provenance(scope, sources, stale_repos);
        // Keyed on the provenance FIELDS, not on `_meta` existing.
        //
        // Testing the whole object made this skip entirely as soon as a handler
        // put anything else under `_meta`. nw-316 tripped it: once
        // `project_context` wrote `_meta.answered_by`, the direct route silently
        // stopped receiving `scope`/`sources`/`stale_repos` -- a payload with no
        // provenance at all, which is the nw-315 defect this function exists to
        // prevent, reintroduced by an unrelated key appearing beside it.
        //
        // The stated contract is unchanged: a value already present WINS, so a
        // federating caller's richer verdict is still never clobbered. What
        // changed is that "already present" is now asked per field.
        match (
            obj.get_mut(META_KEY).and_then(Value::as_object_mut),
            stamped,
        ) {
            (Some(existing), Value::Object(fields)) => {
                for (key, value) in fields {
                    existing.entry(key).or_insert(value);
                }
            }
            (_, stamped) => {
                obj.entry(META_KEY).or_insert(stamped);
            }
        }
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
    /// nw-316: `set` REPLACED `_meta`, destroying every key a handler had
    /// already put there. `project_context` writes `_meta.answered_by` naming
    /// the instance config that produced the answer; the daemon route stamps
    /// provenance after the tool returns, so the disclosure survived on the
    /// direct route and vanished on the daemon one -- a field on one route
    /// only, which is the exact drift nw-316 exists to fix.
    #[test]
    fn set_merges_rather_than_destroying_keys_it_does_not_own() {
        let mut result = serde_json::json!({
            "answer": 1,
            "_meta": { "answered_by": { "instance_id": "kept" } }
        });
        super::set(&mut result, "local", &["local"], &[]);

        assert_eq!(
            result["_meta"]["answered_by"]["instance_id"], "kept",
            "a key provenance does not own must survive the stamp: {result}"
        );
        // Provenance still wins on its own vocabulary.
        assert_eq!(result["_meta"]["scope"], "local");
        assert!(result["_meta"]["sources"].is_array());
    }

    /// nw-316: `ensure` keyed on `_meta` EXISTING, so the moment a handler put
    /// any unrelated key there it stopped stamping provenance at all -- the
    /// nw-315 defect (a payload with no provenance) reintroduced sideways. It
    /// must fill in the fields it owns while still never clobbering a value
    /// that is already there.
    #[test]
    fn ensure_stamps_alongside_an_unrelated_meta_key() {
        let mut result = serde_json::json!({
            "answer": 1,
            "_meta": { "answered_by": { "instance_id": "kept" } }
        });
        super::ensure(&mut result, "local", &["local"], &[]);

        assert_eq!(result["_meta"]["answered_by"]["instance_id"], "kept");
        assert_eq!(
            result["_meta"]["scope"], "local",
            "an unrelated key must not suppress the provenance stamp: {result}"
        );
        assert!(result["_meta"]["sources"].is_array());
        assert!(result["_meta"]["stale_repos"].is_array());
    }

    /// The counterweight: with no existing `_meta`, the stamp is still written
    /// whole. A payload with no provenance at all is the nw-315 defect.
    #[test]
    fn set_still_stamps_a_payload_with_no_existing_meta() {
        let mut result = serde_json::json!({ "answer": 1 });
        super::set(&mut result, "local", &["local"], &[]);
        assert_eq!(result["_meta"]["scope"], "local");
    }

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
