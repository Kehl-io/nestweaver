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
