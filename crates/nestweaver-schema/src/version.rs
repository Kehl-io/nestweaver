use sha2::{Digest, Sha256};

/// The canonical set of node labels, their properties, edge labels, and their properties.
/// These are sorted to ensure a stable hash regardless of insertion order.
const NODE_LABELS: &[&str] = &[
    "Contract", "File", "Heading", "Note", "Project", "Repo", "Section", "Service", "Symbol",
    "Tag", "Vault",
];

const NODE_PROPERTIES: &[&str] = &[
    "confidence",
    "content_hash",
    "created_at",
    "embedding",
    "end_line",
    "entry_point_kind",
    "file_path",
    "frontmatter",
    "heading_uid",
    "indexed_sha",
    "instance_id",
    "is_entry_point",
    "kind",
    "level",
    "modified_at",
    "name",
    "note_kind",
    "note_uid",
    "operation_id",
    "pagerank_score",
    "path",
    "repo_uid",
    "root_path",
    "signature",
    "slug",
    "source_path",
    "staleness_commits_behind",
    "start_line",
    "summary",
    "summary_hash",
    "text",
    "text_content",
    "text_hash",
    "title",
    "uid",
    "url",
    "vault_uid",
    "verb",
    "word_count",
];

const EDGE_LABELS: &[&str] = &[
    "CALLS",
    "CAUSED_BY",
    "CONTAINS",
    "CROSS_REPO_LINK",
    "DEPENDS_ON",
    "EXTENDS",
    "HEADING_HAS_SECTION",
    "HEADING_PARENT",
    "IMPLEMENTS",
    "IMPLEMENTS_CONTRACT",
    "IMPORTS",
    "MEMBER_OF",
    "NOTE_HAS_HEADING",
    "NOTE_HAS_SECTION",
    "NOTE_TAGGED_WITH",
    "PROJECT_INCLUDES_NOTE",
    "REFERENCES_CODE_NOTE_TO_SYMBOL",
    "REFERENCES_CODE_SECTION_TO_SYMBOL",
    "RELATES_TO",
    "SECTION_TAGGED_WITH",
    "SUPERSEDES",
    "VAULT_HAS_NOTE",
    "WIKILINK_TO_HEADING",
    "WIKILINK_TO_NOTE",
];

const EDGE_PROPERTIES: &[&str] = &[
    "confidence",
    "edge_type",
    "link_type",
    "source_uid",
    "target_uid",
];

fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

/// Returns the SHA-256 of all sorted node labels + properties + edge labels + properties.
/// This hash changes whenever the schema structure changes, allowing dependent systems
/// to detect when re-indexing is needed.
pub fn core_schema_hash() -> String {
    let mut parts: Vec<&str> = Vec::new();
    parts.extend_from_slice(NODE_LABELS);
    parts.extend_from_slice(NODE_PROPERTIES);
    parts.extend_from_slice(EDGE_LABELS);
    parts.extend_from_slice(EDGE_PROPERTIES);
    // Already sorted (defined as sorted constants), but sort again to be safe.
    parts.sort_unstable();
    sha256_hex(&parts.join("\n"))
}

/// Returns the SHA-256 of the core hash concatenated with an extension hash.
/// Use this when vendor or application-specific schema extensions exist.
pub fn effective_schema_hash(core_hash: &str, extension_hash: &str) -> String {
    sha256_hex(&format!("{core_hash}{extension_hash}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_schema_hash_is_deterministic() {
        let a = core_schema_hash();
        let b = core_schema_hash();
        assert_eq!(a, b);
    }

    #[test]
    fn core_schema_hash_is_64_hex_chars() {
        let h = core_schema_hash();
        assert_eq!(h.len(), 64, "expected 64-char hex SHA-256, got: {h}");
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn effective_hash_changes_with_extension() {
        let core = core_schema_hash();
        let h1 = effective_schema_hash(&core, "ext_v1");
        let h2 = effective_schema_hash(&core, "ext_v2");
        assert_ne!(h1, h2);
    }

    #[test]
    fn effective_hash_changes_with_core() {
        let h1 = effective_schema_hash("core_a", "same_ext");
        let h2 = effective_schema_hash("core_b", "same_ext");
        assert_ne!(h1, h2);
    }

    #[test]
    fn effective_hash_is_deterministic() {
        let core = core_schema_hash();
        let a = effective_schema_hash(&core, "ext");
        let b = effective_schema_hash(&core, "ext");
        assert_eq!(a, b);
    }

    /// `root_path` predates the `Repo.root_path` field (it belongs to
    /// `Vault`), so adding the field to `Repo` must NOT change the core
    /// schema hash — no re-index is required.
    #[test]
    fn root_path_is_a_known_property_and_hash_is_stable() {
        assert!(
            NODE_PROPERTIES.contains(&"root_path"),
            "root_path must be a member of NODE_PROPERTIES"
        );
        assert_eq!(
            core_schema_hash(),
            "6646465ab73216946bf3c5acd7e418e7ffeb4562d69631864acb8c5c4499a8de",
            "core schema hash drifted — adding Repo.root_path must not change it; \
             if the schema intentionally changed, update this pinned hash"
        );
    }
}

/// Describe a version mismatch between a running daemon and a client, or
/// `None` when they agree.
///
/// Direction matters and the original check ignored it. It was a bare equality
/// test whose message was hardcoded to the older-daemon case, so running an
/// older client against a NEWER installed daemon reported "the upgrade has NOT
/// been applied" — false — and then named `restart` as the remedy, which would
/// replace the live newer daemon with the older build. An operator was told to
/// perform a downgrade in the language of an upgrade.
///
/// Both versions are passed in rather than read from `CARGO_PKG_VERSION`
/// here: this lives in the schema crate so the CLI, the client and the MCP
/// server can all share one rule (the MCP crate cannot depend on the client —
/// `mcp -> client -> daemon -> mcp` is a cycle), and each caller must report
/// ITS OWN version, not this crate's.
///
/// An unparseable version on either side is reported as a plain mismatch
/// rather than guessed at: unknown ordering must not become a confident claim.
pub fn describe_version_skew(daemon_version: &str, client_version: &str) -> Option<String> {
    if daemon_version == client_version {
        return None;
    }
    let parse = |version: &str| -> Option<(u64, u64, u64)> {
        // Ignore any pre-release/build suffix; the numeric triple is what
        // orders two NestWeaver builds.
        let core = version.split(['-', '+']).next().unwrap_or(version);
        let mut parts = core.split('.');
        let mut next = || parts.next()?.parse::<u64>().ok();
        let (major, minor, patch) = (next()?, next()?, next()?);
        parts.next().is_none().then_some((major, minor, patch))
    };
    let ordering = match (parse(daemon_version), parse(client_version)) {
        (Some(daemon), Some(client)) => Some(daemon.cmp(&client)),
        _ => None,
    };
    Some(match ordering {
        Some(std::cmp::Ordering::Less) => format!(
            "the running daemon is version {daemon_version} but this client is \
             {client_version}; the upgrade has NOT been applied and its new behaviour \
             is not active."
        ),
        Some(std::cmp::Ordering::Greater) => format!(
            "the running daemon is version {daemon_version}, NEWER than this client \
             ({client_version}); restarting it would DOWNGRADE the running daemon. \
             Use the {daemon_version} binary, or stop the daemon deliberately if you \
             intend to downgrade."
        ),
        // Equal is impossible here (the strings differ), so this is the
        // unparseable case.
        _ => format!(
            "the running daemon reports version {daemon_version} and this client is \
             {client_version}; the two do not match and their order cannot be \
             determined."
        ),
    })
}

#[cfg(test)]
mod version_skew_tests {
    use super::describe_version_skew;

    /// The original check was a bare equality test whose message was hardcoded
    /// to the older-daemon direction, so a NEWER incumbent was described as
    /// "the upgrade has NOT been applied" — false — and the remedy it named
    /// (`restart`) would have replaced the live newer daemon with the older
    /// client's build. A downgrade, described as an upgrade.
    #[test]
    fn skew_is_reported_by_direction_and_never_guesses() {
        assert!(describe_version_skew("7.0.0", "7.0.0").is_none());

        let older = describe_version_skew("6.4.0", "7.0.0").expect("older daemon is skew");
        assert!(older.contains("has NOT been applied"), "{older}");

        let newer = describe_version_skew("7.1.0", "7.0.0").expect("newer daemon is skew");
        assert!(newer.contains("DOWNGRADE"), "{newer}");
        assert!(
            !newer.contains("has NOT been applied"),
            "a newer daemon is not a missing upgrade: {newer}"
        );

        // Ordering across each component, not just the patch.
        assert!(
            describe_version_skew("8.0.0", "7.9.9")
                .unwrap()
                .contains("DOWNGRADE")
        );
        assert!(
            describe_version_skew("7.9.9", "8.0.0")
                .unwrap()
                .contains("has NOT been applied")
        );

        // Unparseable: report a mismatch, but do NOT claim a direction.
        for daemon in ["", "7.0", "seven", "7.0.0.1", "7.0.x"] {
            let unknown = describe_version_skew(daemon, "7.0.0")
                .unwrap_or_else(|| panic!("{daemon:?} differs from 7.0.0"));
            assert!(
                unknown.contains("cannot be determined"),
                "{daemon:?} must not be given a direction: {unknown}"
            );
        }

        // A pre-release suffix orders by its numeric core rather than falling
        // into the unparseable branch.
        assert!(
            describe_version_skew("7.1.0-rc.1", "7.0.0")
                .unwrap()
                .contains("DOWNGRADE")
        );
    }
}
