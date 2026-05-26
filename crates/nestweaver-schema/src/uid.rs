use sha2::{Digest, Sha256};

/// Returns the first 6 bytes (12 hex chars) of the SHA-256 hash of the input.
pub fn truncated_hash(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    hex::encode(&result[..6])
}

/// "repo:{instance}:{url_hash}"
pub fn repo_uid(instance: &str, url: &str) -> String {
    format!("repo:{}:{}", instance, truncated_hash(url))
}

/// "file:{repo_uid}:{path_hash}"
pub fn file_uid(repo_uid: &str, path: &str) -> String {
    format!("file:{}:{}", repo_uid, truncated_hash(path))
}

/// "svc:{repo_uid}:{name_hash}"
pub fn service_uid(repo_uid: &str, name: &str) -> String {
    format!("svc:{}:{}", repo_uid, truncated_hash(name))
}

/// "sym:{repo_uid}:{file_path_hash}:{name_hash}:{line}"
pub fn symbol_uid(repo_uid: &str, file_path: &str, name: &str, line: u32) -> String {
    format!(
        "sym:{}:{}:{}:{}",
        repo_uid,
        truncated_hash(file_path),
        truncated_hash(name),
        line
    )
}

/// "vlt:{instance}:{root_path_hash}"
pub fn vault_uid(instance: &str, root_path: &str) -> String {
    format!("vlt:{}:{}", instance, truncated_hash(root_path))
}

/// "note:{vault_uid}:{rel_path_hash}"
pub fn note_uid(vault_uid: &str, rel_path: &str) -> String {
    format!("note:{}:{}", vault_uid, truncated_hash(rel_path))
}

/// "head:{note_uid}:{slug_hash}:{line}"
///
/// Embeds the line number so two headings with the same slug (e.g. two
/// `## Notes` sections in the same note) get distinct UIDs.
pub fn heading_uid(note_uid: &str, slug: &str, line: u32) -> String {
    format!("head:{}:{}:{}", note_uid, truncated_hash(slug), line)
}

/// "sec:{note_uid}:{start_line}:{content_hash_short}"
///
/// `content_hash_short` is the first 6 hex chars (3 bytes) of the section
/// text's SHA-256, keeping UIDs stable across edits that don't change the
/// section content while cache-busting when they do.
pub fn section_uid(note_uid: &str, start_line: u32, content_hash_short: &str) -> String {
    format!("sec:{}:{}:{}", note_uid, start_line, content_hash_short)
}

/// "tag:{vault_uid}:{name_hash}" — name is lowercased before hashing.
pub fn tag_uid(vault_uid: &str, name: &str) -> String {
    format!("tag:{}:{}", vault_uid, truncated_hash(&name.to_lowercase()))
}

/// "proj:{instance}:{name_hash}"
pub fn project_uid(instance: &str, name: &str) -> String {
    format!("proj:{}:{}", instance, truncated_hash(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncated_hash_is_12_hex_chars() {
        let h = truncated_hash("hello world");
        assert_eq!(h.len(), 12, "expected 12 hex chars, got: {h}");
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn truncated_hash_is_deterministic() {
        let a = truncated_hash("same input");
        let b = truncated_hash("same input");
        assert_eq!(a, b);
    }

    #[test]
    fn truncated_hash_differs_for_different_inputs() {
        let a = truncated_hash("input one");
        let b = truncated_hash("input two");
        assert_ne!(a, b);
    }

    #[test]
    fn repo_uid_format() {
        let uid = repo_uid("prod", "https://github.com/acme/repo");
        assert!(uid.starts_with("repo:prod:"), "got: {uid}");
        // suffix should be 12 hex chars
        let suffix = uid.strip_prefix("repo:prod:").unwrap();
        assert_eq!(suffix.len(), 12);
    }

    #[test]
    fn symbol_uid_format() {
        let ruid = repo_uid("prod", "https://github.com/acme/repo");
        let uid = symbol_uid(&ruid, "src/main.rs", "my_func", 42);
        // format: sym:{repo_uid}:{file_hash}:{name_hash}:{line}
        assert!(uid.starts_with("sym:"), "got: {uid}");
        // Verify the uid ends with ":42" (line number) and does NOT contain raw name
        assert!(
            uid.ends_with(":42"),
            "expected uid to end with ':42'; got: {uid}"
        );
        assert!(
            !uid.contains("my_func"),
            "uid should not contain raw name; got: {uid}"
        );
        // The two hash segments before the line number should each be 12 hex chars
        let trimmed = uid.strip_prefix("sym:").unwrap();
        // trimmed = "{repo_uid}:{file_hash}:{name_hash}:42"
        // repo_uid itself contains colons, so find last two colons before the final ":42"
        let without_line = trimmed.strip_suffix(":42").unwrap();
        let name_hash = without_line.split(':').next_back().unwrap();
        assert_eq!(
            name_hash.len(),
            12,
            "name hash should be 12 hex chars; got: {name_hash}"
        );
        assert!(
            name_hash.chars().all(|c| c.is_ascii_hexdigit()),
            "name hash should be hex; got: {name_hash}"
        );
        // Verify it's consistent (deterministic)
        let uid2 = symbol_uid(&ruid, "src/main.rs", "my_func", 42);
        assert_eq!(uid, uid2, "symbol_uid should be deterministic");
    }

    #[test]
    fn file_uid_format() {
        let ruid = repo_uid("local", "https://example.com/repo");
        let uid = file_uid(&ruid, "src/lib.rs");
        assert!(uid.starts_with("file:"), "got: {uid}");
    }

    #[test]
    fn service_uid_format() {
        let ruid = repo_uid("local", "https://example.com/repo");
        let uid = service_uid(&ruid, "my-service");
        assert!(uid.starts_with("svc:"), "got: {uid}");
    }
}
