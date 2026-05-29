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

/// Canonical placeholder substituted for every path parameter slot when
/// normalizing an HTTP route. The *name* of the slot is intentionally
/// discarded so that `/v1/users/{id}` and `/v1/users/:userId` and
/// `/v1/users/${uid}` and `/v1/users/<u>` all collapse to the same shape.
pub const PATH_PLACEHOLDER: &str = "{}";

/// Normalize an HTTP route path so that spec declarations and code-derived
/// handler routes mint and match identical [`contract_uid`]s.
///
/// Rules:
/// - Collapse every parameter slot to [`PATH_PLACEHOLDER`]. Recognised slot
///   syntaxes: OpenAPI `{id}`, Express/NestJS `:id`, JS template `${id}`,
///   and angle-bracket `<id>` (Flask/Rails-ish). The slot *name* is ignored.
/// - Ensure a single leading slash.
/// - Strip the trailing slash (except for the bare root `/`).
/// - Collapse repeated internal slashes.
pub fn normalize_http_path(path: &str) -> String {
    let trimmed = path.trim();
    let mut out = String::with_capacity(trimmed.len());
    out.push('/');
    for raw_seg in trimmed.split('/') {
        let seg = raw_seg.trim();
        if seg.is_empty() {
            continue;
        }
        let is_param = (seg.starts_with('{') && seg.ends_with('}'))
            || (seg.starts_with("${") && seg.ends_with('}'))
            || seg.starts_with(':')
            || (seg.starts_with('<') && seg.ends_with('>'));
        if is_param {
            out.push_str(PATH_PLACEHOLDER);
        } else {
            out.push_str(seg);
        }
        out.push('/');
    }
    // Strip the trailing slash we always append, unless we are at root.
    if out.len() > 1 {
        out.pop();
    }
    out
}

/// Mint a deterministic UID for a [`crate::nodes::Contract`] node.
///
/// Schemes (per F2-core spec):
/// - HTTP:    `contract:http:POST:/v1/approvals`  (verb upper-cased, path normalized)
/// - gRPC:    `contract:grpc:approvals.v1.Approvals/Create`  (fully-qualified method)
/// - GraphQL: `contract:graphql:Mutation.createApproval`     (operation id)
///
/// `verb`/`path` are only consulted for HTTP. For gRPC and GraphQL the
/// `operation_id` carries the fully-qualified identifier.
pub fn contract_uid(
    kind: &str,
    verb: Option<&str>,
    path: Option<&str>,
    operation_id: Option<&str>,
) -> String {
    match kind {
        "http" => {
            let v = verb.unwrap_or("ANY").to_ascii_uppercase();
            let p = path.map(normalize_http_path).unwrap_or_else(|| "/".into());
            format!("contract:http:{v}:{p}")
        }
        other => {
            let op = operation_id.unwrap_or("");
            format!("contract:{other}:{op}")
        }
    }
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

    #[test]
    fn normalize_http_path_collapses_param_syntaxes() {
        // All four slot syntaxes collapse to the same canonical shape,
        // ignoring slot names.
        assert_eq!(normalize_http_path("/v1/users/{id}"), "/v1/users/{}");
        assert_eq!(normalize_http_path("/v1/users/:userId"), "/v1/users/{}");
        assert_eq!(normalize_http_path("/v1/users/${uid}"), "/v1/users/{}");
        assert_eq!(normalize_http_path("/v1/users/<u>"), "/v1/users/{}");
    }

    #[test]
    fn normalize_http_path_strips_trailing_and_dedups_slashes() {
        assert_eq!(normalize_http_path("/v1/approvals/"), "/v1/approvals");
        assert_eq!(normalize_http_path("v1//approvals"), "/v1/approvals");
        assert_eq!(normalize_http_path("v1/approvals"), "/v1/approvals");
        assert_eq!(normalize_http_path("/"), "/");
        assert_eq!(normalize_http_path(""), "/");
    }

    #[test]
    fn normalize_http_path_multi_param() {
        assert_eq!(
            normalize_http_path("/orgs/{orgId}/users/:userId"),
            "/orgs/{}/users/{}"
        );
    }

    #[test]
    fn contract_uid_http_scheme() {
        let uid = contract_uid("http", Some("post"), Some("/v1/approvals/"), None);
        assert_eq!(uid, "contract:http:POST:/v1/approvals");
        // Two differently-written-but-equivalent routes mint the same UID.
        let a = contract_uid("http", Some("GET"), Some("/users/{id}"), None);
        let b = contract_uid("http", Some("get"), Some("/users/:userId"), None);
        assert_eq!(a, b, "equivalent routes must collide: {a} vs {b}");
    }

    #[test]
    fn contract_uid_grpc_scheme() {
        let uid = contract_uid("grpc", None, None, Some("approvals.v1.Approvals/Create"));
        assert_eq!(uid, "contract:grpc:approvals.v1.Approvals/Create");
    }

    #[test]
    fn contract_uid_graphql_scheme() {
        let uid = contract_uid("graphql", None, None, Some("Mutation.createApproval"));
        assert_eq!(uid, "contract:graphql:Mutation.createApproval");
    }
}
