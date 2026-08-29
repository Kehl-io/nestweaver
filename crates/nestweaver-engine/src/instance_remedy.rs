//! One renderer for the "this database holds several instances" remedy.
//!
//! nw-310. Four sites hand-wrote the same sentence — `src/main.rs` twice,
//! `nestweaver-daemon/src/server.rs`, and `nestweaver-engine/src/query.rs` —
//! and the two written last both emitted
//! `nestweaver instance merge --from <one> --to <keep>` with the placeholders
//! unsubstituted, *while the instance names were bound on the adjacent line*.
//! A remedy the reader must hand-substitute, at the moment they are blocked
//! and confused about instance identity, is a remedy nobody runs.

/// Render a consolidation remedy naming REAL instances.
///
/// `keeper` is the instance to merge INTO. When the caller has no opinion,
/// pass `None` and the lexicographically smallest id is chosen so the
/// suggestion is deterministic — the same tie-break
/// `nestweaver-mcp/src/tools.rs` already applies after ranking by data volume,
/// which is the information these call sites do not have.
///
/// The caveat at the end is deliberate and is NOT decoration. `instance merge`
/// does not currently update the database's recorded instance identity, so a
/// later index can re-create the split (nw-264). Emitting a confident,
/// pasteable command for a known-incomplete operation would be a worse defect
/// than the placeholder it replaces, so the command is concrete and the
/// sentence around it is honest about what it does not finish.
pub fn instance_consolidation_remedy(ids: &[String], keeper: Option<&str>) -> String {
    let mut sorted: Vec<&str> = ids.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.dedup();
    let keeper = match keeper {
        Some(explicit) => explicit,
        None => match sorted.first() {
            Some(first) => first,
            None => return String::new(),
        },
    };
    let commands: Vec<String> = sorted
        .iter()
        .filter(|id| **id != keeper)
        .map(|id| format!("nestweaver instance merge --from {id} --to {keeper}"))
        .collect();
    if commands.is_empty() {
        return String::new();
    }
    format!(
        "consolidate them into `{keeper}`:\n      {}\n    \
         then re-index each repo — a merge does not yet update the database's \
         recorded instance identity, so verify with `nestweaver brain status` \
         before relying on it.",
        commands.join("\n      ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// nw-310. The refusal already holds every instance name, so the command
    /// it prints must name them too.
    #[test]
    fn the_remedy_names_real_instances_and_carries_no_placeholders() {
        let ids = vec!["two".to_string(), "one".to_string()];
        let remedy = instance_consolidation_remedy(&ids, None);

        assert!(
            !remedy.contains('<') && !remedy.contains('>'),
            "the command still carries unsubstituted placeholders: {remedy}"
        );
        assert!(
            remedy.contains("nestweaver instance merge --from two --to one"),
            "the keeper must be deterministic (lexicographically smallest): {remedy}"
        );
        // …and honest about what the merge does not finish (nw-264).
        assert!(remedy.contains("re-index"), "{remedy}");

        // One command per non-keeper, so N instances collapse in one pass.
        let three = vec!["b".to_string(), "c".to_string(), "a".to_string()];
        let remedy = instance_consolidation_remedy(&three, None);
        assert!(remedy.contains("--from b --to a"), "{remedy}");
        assert!(remedy.contains("--from c --to a"), "{remedy}");
        assert!(!remedy.contains("--from a --to"), "{remedy}");

        // A caller that knows the keeper (the user named an instance) wins.
        let remedy = instance_consolidation_remedy(&three, Some("c"));
        assert!(remedy.contains("--from a --to c"), "{remedy}");
        assert!(remedy.contains("--from b --to c"), "{remedy}");

        // Nothing to consolidate is not a remedy.
        assert!(instance_consolidation_remedy(&[], None).is_empty());
        assert!(instance_consolidation_remedy(&["only".to_string()], None).is_empty());
    }
}
