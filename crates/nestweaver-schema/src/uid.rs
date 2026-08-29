use sha2::{Digest, Sha256};

use crate::repo_url::normalized_repo_key;

/// Returns the first 6 bytes (12 hex chars) of the SHA-256 hash of the input.
pub fn truncated_hash(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    hex::encode(&result[..6])
}

/// "repo:{instance}:{url_hash}"
///
/// The URL is collapsed to a scheme/credential/suffix/case-invariant identity
/// key via [`normalized_repo_key`] BEFORE hashing, so equivalent clone-URL
/// forms of the same repo (ssh vs https, `.git` suffix, trailing slash,
/// embedded credentials, host/path casing) mint the same `url_hash`. This lets
/// a repo indexed by a LOCAL daemon (ssh remote) and a SERVER (https URL)
/// reconcile at the root during merged-result dedup.
///
/// ⚠ Changing `normalized_repo_key` (or this derivation) changes every stored
/// hash — requires a full reindex.
pub fn repo_uid(instance: &str, url: &str) -> String {
    let normalized = normalized_repo_key(url);
    format!("repo:{}:{}", instance, truncated_hash(&normalized))
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

/// Every UID domain this module mints, enumerated ONCE.
///
/// nw-301: `investigate`'s `fetch_full_body` matched `sym:`/`sec:`/`note:` with
/// an `if` chain and returned `None` off the end, so `head:` and `tag:` entries
/// were a permanent dead end — no error, no retry that could ever work, and
/// `expanded: true` on an entry with no body. An `if` chain cannot be
/// exhaustive, so nothing in the compiler or the test suite could notice the
/// two missing arms.
///
/// This enum is the fix for the CLASS rather than the two cases: match on it
/// and adding a twelfth domain below without handling it is a compile error at
/// every call site, not a silent `None` at runtime. `classify_uid_domain_is_total`
/// pins that every constructor here mints something this recognises.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum UidKind {
    Repo,
    File,
    Service,
    Symbol,
    Vault,
    Note,
    Heading,
    Section,
    Tag,
    Project,
    Contract,
}

impl UidKind {
    /// Every variant, so a caller can assert over the domain instead of over
    /// the cases it happens to have seen.
    pub const ALL: &'static [UidKind] = &[
        UidKind::Repo,
        UidKind::File,
        UidKind::Service,
        UidKind::Symbol,
        UidKind::Vault,
        UidKind::Note,
        UidKind::Heading,
        UidKind::Section,
        UidKind::Tag,
        UidKind::Project,
        UidKind::Contract,
    ];

    /// The `prefix:` each domain's UIDs start with.
    pub fn prefix(self) -> &'static str {
        match self {
            UidKind::Repo => "repo:",
            UidKind::File => "file:",
            UidKind::Service => "svc:",
            UidKind::Symbol => "sym:",
            UidKind::Vault => "vlt:",
            UidKind::Note => "note:",
            UidKind::Heading => "head:",
            UidKind::Section => "sec:",
            UidKind::Tag => "tag:",
            UidKind::Project => "proj:",
            UidKind::Contract => "contract:",
        }
    }

    /// The human-facing node label, for messages that name a kind.
    pub fn label(self) -> &'static str {
        match self {
            UidKind::Repo => "Repo",
            UidKind::File => "File",
            UidKind::Service => "Service",
            UidKind::Symbol => "Symbol",
            UidKind::Vault => "Vault",
            UidKind::Note => "Note",
            UidKind::Heading => "Heading",
            UidKind::Section => "Section",
            UidKind::Tag => "Tag",
            UidKind::Project => "Project",
            UidKind::Contract => "Contract",
        }
    }

    /// Classify a UID by its domain prefix.
    ///
    /// `note:` and `head:`/`sec:` share no prefix and `repo:` is not a prefix of
    /// any other domain, so a plain longest-match is unambiguous.
    pub fn of(uid: &str) -> Option<UidKind> {
        UidKind::ALL
            .iter()
            .copied()
            .find(|kind| uid.starts_with(kind.prefix()))
    }
}

/// The note a heading belongs to, recovered from the heading UID itself.
///
/// `heading_uid` is `head:{note_uid}:{slug_hash}:{line}` and `note_uid` contains
/// colons of its own, so the split has to come off the RIGHT. Written here,
/// beside the constructor it inverts, so the two cannot drift.
pub fn note_uid_of_heading(heading_uid: &str) -> Option<&str> {
    let rest = heading_uid.strip_prefix("head:")?;
    // `{note_uid}:{slug_hash}:{line}` — drop the last two components.
    let (without_line, _line) = rest.rsplit_once(':')?;
    let (note_uid, _slug_hash) = without_line.rsplit_once(':')?;
    if note_uid.is_empty() {
        return None;
    }
    Some(note_uid)
}

/// Canonical UID domains that can identify a top-level `brain_search` row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchEntityUidKind {
    Note,
    Tag,
    Symbol,
}

/// Parse a canonical search-entity UID and remove only its instance component.
///
/// Accepted constructor grammars are:
///
/// - `note|tag:vlt:<instance>:<12-lower-hex-vault>:<12-lower-hex-entity>`;
/// - `sym:repo:<instance>:<12-lower-hex-repo>:<12-lower-hex-file>:<12-lower-hex-name>:<u32-line>`.
///
/// Instances follow the graph UID rule: nonempty, with no whitespace. A colon
/// necessarily changes the exact component count and is therefore rejected.
/// The normalized value retains the domain plus all ownership/content
/// components so unrelated repositories, vaults, notes, tags, and symbols
/// remain distinct.
pub fn normalize_search_entity_uid(uid: &str) -> Option<(SearchEntityUidKind, String)> {
    let parts: Vec<&str> = uid.split(':').collect();
    match parts.as_slice() {
        [
            domain @ ("note" | "tag"),
            "vlt",
            instance,
            vault_hash,
            entity_hash,
        ] if valid_uid_instance(instance)
            && is_lowercase_12_hex(vault_hash)
            && is_lowercase_12_hex(entity_hash) =>
        {
            let kind = if *domain == "note" {
                SearchEntityUidKind::Note
            } else {
                SearchEntityUidKind::Tag
            };
            Some((kind, format!("{domain}:vlt:{vault_hash}:{entity_hash}")))
        }
        [
            "sym",
            "repo",
            instance,
            repo_hash,
            file_hash,
            name_hash,
            line,
        ] if valid_uid_instance(instance)
            && is_lowercase_12_hex(repo_hash)
            && is_lowercase_12_hex(file_hash)
            && is_lowercase_12_hex(name_hash)
            && is_canonical_u32(line) =>
        {
            Some((
                SearchEntityUidKind::Symbol,
                format!("sym:repo:{repo_hash}:{file_hash}:{name_hash}:{line}"),
            ))
        }
        _ => None,
    }
}

/// Validate an edit-stable canonical ID against its line-sensitive symbol UID.
///
/// The symbol UID proves repository ownership plus hashed file/name identity.
/// The canonical ID must carry the same repository hash and raw file/name
/// components whose hashes match that UID. Every possible `#` separator is
/// considered so valid paths and symbol names may themselves contain `#`.
/// The returned key is explicitly domain-prefixed to remain distinct from
/// note/tag identities.
pub fn normalize_search_symbol_canonical_id(
    symbol_uid: &str,
    canonical_id: &str,
) -> Option<String> {
    let (kind, normalized_uid) = normalize_search_entity_uid(symbol_uid)?;
    if kind != SearchEntityUidKind::Symbol {
        return None;
    }
    let uid_parts: Vec<&str> = normalized_uid.split(':').collect();
    let ["sym", "repo", repo_hash, file_hash, name_hash, _line] = uid_parts.as_slice() else {
        return None;
    };

    let (canonical_repo_hash, remainder) = canonical_id.split_once(':')?;
    let (path_and_name, scope_hash) = remainder.rsplit_once(':')?;
    if canonical_repo_hash != *repo_hash
        || !is_lowercase_12_hex(canonical_repo_hash)
        || !is_lowercase_12_hex(scope_hash)
    {
        return None;
    }

    let has_matching_path_and_name = path_and_name.match_indices('#').any(|(separator, _)| {
        let file_path = &path_and_name[..separator];
        let name = &path_and_name[separator + 1..];
        !file_path.is_empty()
            && !name.is_empty()
            && truncated_hash(file_path) == *file_hash
            && truncated_hash(name) == *name_hash
    });
    has_matching_path_and_name.then(|| format!("sym-canonical:{canonical_id}"))
}

fn valid_uid_instance(instance: &str) -> bool {
    !instance.is_empty() && !instance.chars().any(char::is_whitespace)
}

fn is_lowercase_12_hex(value: &str) -> bool {
    value.len() == 12
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_canonical_u32(value: &str) -> bool {
    value.bytes().all(|byte| byte.is_ascii_digit())
        && value
            .parse::<u32>()
            .is_ok_and(|parsed| value == parsed.to_string())
}

/// "proj:{instance}:{name_hash}"
pub fn project_uid(instance: &str, name: &str) -> String {
    format!("proj:{}:{}", instance, truncated_hash(name))
}

/// Every domain prefix a graph-node UID minted by this module can carry.
///
/// Kept beside the constructors deliberately: a new node kind that adds a
/// constructor above and forgets this list makes [`is_node_uid`] silently
/// reject its UIDs.
const NODE_UID_PREFIXES: &[&str] = &[
    "repo:",
    "file:",
    "svc:",
    "sym:",
    "vlt:",
    "note:",
    "head:",
    "sec:",
    "tag:",
    "proj:",
    "contract:",
];

/// Upper bound on a node UID's length.
///
/// Every constructor above emits a prefix plus fixed-width hashes and a line
/// number; the only variable-length component is a `contract:` route. This
/// bound exists to reject caller-supplied strings, not to describe the
/// constructors — a 1 MB request field is not a UID however it starts.
pub const MAX_NODE_UID_LEN: usize = 1024;

/// True when `value` has the SHAPE of a graph-node UID.
///
/// This is a cheap structural filter, not a liveness check: it says "this
/// COULD name a node", never "this node exists". Use it at boundaries that
/// accept caller-supplied strings and then use them as node identities.
///
/// The boundary that needed it is `record_interaction` (nw-296), which took
/// `arguments.seeds` verbatim off the JSON-RPC wire and used each string as an
/// interaction `node_scores` key. Raw titles and a 1 MB request field were
/// written into a sidecar that has no delete path, while the strongest ranking
/// signal — `query_seed_count`, weight 0.5 — never once landed on a real node.
pub fn is_node_uid(value: &str) -> bool {
    if value.is_empty() || value.len() > MAX_NODE_UID_LEN {
        return false;
    }
    if value.chars().any(char::is_whitespace) {
        return false;
    }
    NODE_UID_PREFIXES
        .iter()
        .any(|prefix| value.len() > prefix.len() && value.starts_with(prefix))
}

/// Compute a canonical symbol ID that is instance-independent.
///
/// Format: `<repo_url_hash>:<file_path>#<name>:<scope_hash>`
///
/// The scope_hash is derived from the scope chain (module::class::method),
/// NOT from the line number. This makes the ID stable across edits that
/// shift line numbers without changing the symbol's logical position.
///
/// When the scope chain is empty (top-level symbols), the hash falls back to
/// the symbol name alone — never the line number — so that inserting blank
/// lines above a symbol does not change its identity. This stability is relied
/// on by cross-boundary flow-trace stitching and atomic-change matching.
///
/// The `repo_url` is collapsed via [`normalized_repo_key`] before hashing so
/// equivalent URL forms mint the same `repo_hash`. ⚠ Changing that
/// normalization changes every stored canonical_id — requires a full reindex.
pub fn canonical_symbol_id(
    repo_url: &str,
    file_path: &str,
    name: &str,
    scope_chain: &str,
) -> String {
    let repo_hash = truncated_hash(&normalized_repo_key(repo_url));
    let scope_hash = scope_hash(scope_chain, name);
    format!("{}:{}#{}:{}", repo_hash, file_path, name, scope_hash)
}

/// Hash the scope chain for a symbol, falling back to the name when no
/// scope chain is available.
pub fn scope_hash(scope_chain: &str, name: &str) -> String {
    if scope_chain.is_empty() {
        truncated_hash(name)
    } else {
        truncated_hash(scope_chain)
    }
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

/// Mint the repository-independent normalized shape of an API contract.
///
/// A shape is useful for discovery, but is NOT object identity: two unrelated
/// services commonly expose the same verb and path. Callers must never use a
/// bare shape to create implementation edges or satisfy per-repo drift.
///
/// Schemes (per F2-core spec):
/// - HTTP:    `contract:http:POST:/v1/approvals`  (verb upper-cased, path normalized)
/// - gRPC:    `contract:grpc:approvals.v1.Approvals/Create`  (fully-qualified method)
/// - GraphQL: `contract:graphql:Mutation.createApproval`     (operation id)
///
/// `verb`/`path` are only consulted for HTTP. For gRPC and GraphQL the
/// `operation_id` carries the fully-qualified identifier.
pub fn contract_shape_key(
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

/// Mint the historical repository-independent contract shape UID.
///
/// This public helper is retained for source compatibility. Stored
/// [`crate::nodes::Contract`] nodes must use [`scoped_contract_uid`] so two
/// repositories with the same route do not collide.
pub fn contract_uid(
    kind: &str,
    verb: Option<&str>,
    path: Option<&str>,
    operation_id: Option<&str>,
) -> String {
    contract_shape_key(kind, verb, path, operation_id)
}

/// Mint the deterministic, repository-scoped UID for a
/// [`crate::nodes::Contract`] node.
///
/// `Contract.repo_uid` is singular ownership, so the primary key carries the
/// same namespace. This mirrors File, Service, and Symbol identity and prevents
/// unrelated repositories that expose the same route from colliding globally.
pub fn scoped_contract_uid(
    repo_uid: &str,
    kind: &str,
    verb: Option<&str>,
    path: Option<&str>,
    operation_id: Option<&str>,
) -> String {
    let shape = contract_shape_key(kind, verb, path, operation_id);
    format!(
        "contract:{repo_uid}:{}",
        shape.trim_start_matches("contract:")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// nw-301. The point of `UidKind` is that it is TOTAL — every UID this
    /// module can mint must classify, or a `match` over it silently stops being
    /// exhaustive in practice while still compiling. Adding a constructor
    /// without adding a variant fails here.
    #[test]
    fn classify_uid_domain_is_total() {
        let repo = repo_uid("local", "https://github.com/acme/api.git");
        let vault = vault_uid("local", "/vault");
        let note = note_uid(&vault, "a/b.md");
        let minted = [
            (UidKind::Repo, repo.clone()),
            (UidKind::File, file_uid(&repo, "src/a.rs")),
            (UidKind::Service, service_uid(&repo, "api")),
            (UidKind::Symbol, symbol_uid(&repo, "src/a.rs", "main", 3)),
            (UidKind::Vault, vault.clone()),
            (UidKind::Note, note.clone()),
            (UidKind::Heading, heading_uid(&note, "intro", 4)),
            (UidKind::Section, section_uid(&note, 4, "abc123")),
            (UidKind::Tag, tag_uid(&vault, "Ops")),
            (UidKind::Project, project_uid("local", "nestweaver")),
            (
                UidKind::Contract,
                scoped_contract_uid(&repo, "http", Some("get"), Some("/widgets"), None),
            ),
        ];

        for (expected, uid) in &minted {
            assert_eq!(
                UidKind::of(uid),
                Some(*expected),
                "`{uid}` did not classify as {expected:?}"
            );
        }
        let covered: std::collections::BTreeSet<UidKind> =
            minted.iter().map(|(kind, _)| *kind).collect();
        assert_eq!(
            covered.len(),
            UidKind::ALL.len(),
            "a UidKind variant has no minted example here, so nothing proves its \
             prefix matches what the constructor writes"
        );
    }

    /// The heading UID embeds its note UID, which itself contains colons — so
    /// the split must come off the right. Getting this wrong is invisible: it
    /// returns a truncated-but-plausible note UID that simply looks up nothing.
    #[test]
    fn a_heading_names_the_note_it_belongs_to() {
        let vault = vault_uid("local", "/vault");
        let note = note_uid(&vault, "a/b.md");
        let heading = heading_uid(&note, "intro", 12);
        assert_eq!(note_uid_of_heading(&heading), Some(note.as_str()));
        assert_eq!(note_uid_of_heading(&note), None);
        assert_eq!(note_uid_of_heading("head:"), None);
    }

    #[test]
    fn is_node_uid_accepts_every_constructor_and_rejects_caller_text() {
        for uid in [
            repo_uid("i", "https://example.com/a.git"),
            vault_uid("i", "/vault"),
            note_uid(&vault_uid("i", "/vault"), "a.md"),
            heading_uid(&note_uid(&vault_uid("i", "/vault"), "a.md"), "slug", 3),
            section_uid(&note_uid(&vault_uid("i", "/vault"), "a.md"), 1, "abc"),
            tag_uid(&vault_uid("i", "/vault"), "t"),
            symbol_uid(&repo_uid("i", "u"), "a.rs", "f", 1),
            file_uid(&repo_uid("i", "u"), "a.rs"),
            service_uid(&repo_uid("i", "u"), "s"),
            project_uid("i", "p"),
        ] {
            assert!(
                is_node_uid(&uid),
                "constructor output must be accepted: {uid}"
            );
        }

        // The nw-296 population: raw caller text used as a node key.
        assert!(!is_node_uid("AuthService"));
        assert!(!is_node_uid(
            "Route All Write Operations Through Daemon RPC"
        ));
        assert!(!is_node_uid(""));
        assert!(!is_node_uid("note:"), "a bare prefix names no node");
        assert!(!is_node_uid(&"x".repeat(MAX_NODE_UID_LEN + 1)));
        assert!(
            !is_node_uid(&format!("note:{}", "x".repeat(MAX_NODE_UID_LEN))),
            "a UID-shaped prefix does not license an unbounded key"
        );
    }

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
    fn repo_uid_reconciles_equivalent_url_forms() {
        // The SAME repo indexed under different clone-URL forms must mint the
        // same url_hash (and thus the same repo_uid, modulo instance) so that
        // local (ssh) and server (https) results dedup at the root. Before
        // mint-time normalization, only a trailing slash was stripped, so these
        // forms hashed differently and never reconciled.
        let canonical = repo_uid("prod", "https://github.com/acme/api");
        for form in [
            "https://github.com/acme/api",
            "https://github.com/acme/api.git",
            "https://github.com/acme/api/",
            "https://GitHub.com/Acme/API",
            "https://user:token@github.com/acme/api",
            "git@github.com:acme/api.git",
            "git@github.com:acme/api",
            "ssh://git@github.com/acme/api",
            "https://github.com/acme/api?ref=main",
        ] {
            assert_eq!(
                repo_uid("prod", form),
                canonical,
                "URL form `{form}` must mint the same repo_uid as the canonical https form"
            );
        }
    }

    #[test]
    fn repo_uid_reconciles_across_instances_modulo_instance() {
        // Same repo, two instances, two URL forms: the url_hash suffix must
        // match so instance-stripping dedup collapses them.
        let local = repo_uid("local", "git@github.com:acme/api.git");
        let server = repo_uid("server", "https://github.com/acme/api");
        let local_hash = local.rsplit(':').next().unwrap();
        let server_hash = server.rsplit(':').next().unwrap();
        assert_eq!(
            local_hash, server_hash,
            "url_hash must match across equivalent forms; {local} vs {server}"
        );
    }

    #[test]
    fn canonical_id_reconciles_equivalent_url_forms() {
        // canonical_symbol_id's repo_hash must also normalize so cross-boundary
        // flow-trace/impact stitching reconciles ssh vs https forms.
        let a = canonical_symbol_id("git@github.com:acme/api.git", "src/lib.rs", "foo", "foo");
        let b = canonical_symbol_id("https://github.com/acme/api", "src/lib.rs", "foo", "foo");
        assert_eq!(a, b, "equivalent URL forms must mint the same canonical_id");
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
    fn search_entity_uid_normalization_accepts_constructor_output_across_instances() {
        let local_repo = repo_uid("local", "git@github.com:acme/api.git");
        let server_repo = repo_uid("server", "https://github.com/acme/api");
        let local_symbol = symbol_uid(&local_repo, "src/lib.rs", "needle", 42);
        let server_symbol = symbol_uid(&server_repo, "src/lib.rs", "needle", 42);
        let (local_kind, local_normalized) =
            normalize_search_entity_uid(&local_symbol).expect("constructor output must parse");
        let (server_kind, server_normalized) =
            normalize_search_entity_uid(&server_symbol).expect("constructor output must parse");
        assert_eq!(local_kind, SearchEntityUidKind::Symbol);
        assert_eq!(server_kind, SearchEntityUidKind::Symbol);
        assert_eq!(local_normalized, server_normalized);
        assert!(!local_normalized.contains("local"));
        assert!(!server_normalized.contains("server"));

        let local_vault = vault_uid("local", "/same/vault");
        let server_vault = vault_uid("server", "/same/vault");
        for (local, server, expected_kind) in [
            (
                note_uid(&local_vault, "notes/needle.md"),
                note_uid(&server_vault, "notes/needle.md"),
                SearchEntityUidKind::Note,
            ),
            (
                tag_uid(&local_vault, "Needle"),
                tag_uid(&server_vault, "Needle"),
                SearchEntityUidKind::Tag,
            ),
        ] {
            let (local_kind, local_normalized) =
                normalize_search_entity_uid(&local).expect("constructor output must parse");
            let (server_kind, server_normalized) =
                normalize_search_entity_uid(&server).expect("constructor output must parse");
            assert_eq!(local_kind, expected_kind);
            assert_eq!(server_kind, expected_kind);
            assert_eq!(local_normalized, server_normalized);
        }
    }

    #[test]
    fn search_entity_uid_normalization_rejects_noncanonical_shapes() {
        for invalid in [
            // Note/tag IDs require exactly five components.
            "note:vlt:local:0123456789ab",
            "note:vlt:local:0123456789ab:abcdef012345:extra",
            "tag:vlt:local:0123456789ab",
            // Instances are nonempty and cannot contain whitespace.
            "note:vlt::0123456789ab:abcdef012345",
            "note:vlt:local box:0123456789ab:abcdef012345",
            // Every ownership/content hash is lowercase 12-hex.
            "note:vlt:local:0123456789AB:abcdef012345",
            "note:vlt:local:0123456789ag:abcdef012345",
            "note:vlt:local:0123456789a:abcdef012345",
            "tag:vlt:local:0123456789ab:ABCDEF012345",
            // Symbol IDs require exactly seven components and three hashes.
            "sym:repo:local:0123456789ab:abcdef012345:123456789abc",
            "sym:repo:local:0123456789ab:abcdef012345:123456789abc:42:extra",
            "sym:repo::0123456789ab:abcdef012345:123456789abc:42",
            "sym:repo:local box:0123456789ab:abcdef012345:123456789abc:42",
            "sym:repo:local:0123456789ag:abcdef012345:123456789abc:42",
            "sym:repo:local:0123456789ab:ABCDEF012345:123456789abc:42",
            "sym:repo:local:0123456789ab:abcdef012345:123456789ab:42",
            // The final component must be the canonical decimal spelling of a u32.
            "sym:repo:local:0123456789ab:abcdef012345:123456789abc:",
            "sym:repo:local:0123456789ab:abcdef012345:123456789abc:-1",
            "sym:repo:local:0123456789ab:abcdef012345:123456789abc:not-a-line",
            "sym:repo:local:0123456789ab:abcdef012345:123456789abc:4294967296",
            "sym:repo:local:0123456789ab:abcdef012345:123456789abc:00",
            "sym:repo:local:0123456789ab:abcdef012345:123456789abc:007",
            "sym:repo:local:0123456789ab:abcdef012345:123456789abc:042",
            // Search identity supports only note, tag, and symbol domains.
            "head:vlt:local:0123456789ab:abcdef012345",
        ] {
            assert_eq!(
                normalize_search_entity_uid(invalid),
                None,
                "invalid UID must fail closed: {invalid}"
            );
        }
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
        let repo = "repo:test:abc123";
        assert_eq!(
            contract_uid("http", Some("post"), Some("/v1/approvals/"), None),
            "contract:http:POST:/v1/approvals",
            "the pre-v4.1 public shape helper remains source-compatible"
        );
        let uid = scoped_contract_uid(repo, "http", Some("post"), Some("/v1/approvals/"), None);
        assert_eq!(uid, "contract:repo:test:abc123:http:POST:/v1/approvals");
        // Two differently-written-but-equivalent routes mint the same UID.
        let a = scoped_contract_uid(repo, "http", Some("GET"), Some("/users/{id}"), None);
        let b = scoped_contract_uid(repo, "http", Some("get"), Some("/users/:userId"), None);
        assert_eq!(a, b, "equivalent routes must collide: {a} vs {b}");
        assert_ne!(
            a,
            scoped_contract_uid(
                "repo:test:other",
                "http",
                Some("GET"),
                Some("/users/{id}"),
                None
            ),
            "the same shape in another repo is a different Contract node"
        );
    }

    #[test]
    fn contract_uid_grpc_scheme() {
        let uid = scoped_contract_uid(
            "repo:test:abc123",
            "grpc",
            None,
            None,
            Some("approvals.v1.Approvals/Create"),
        );
        assert_eq!(
            uid,
            "contract:repo:test:abc123:grpc:approvals.v1.Approvals/Create"
        );
    }

    #[test]
    fn contract_uid_graphql_scheme() {
        let uid = scoped_contract_uid(
            "repo:test:abc123",
            "graphql",
            None,
            None,
            Some("Mutation.createApproval"),
        );
        assert_eq!(
            uid,
            "contract:repo:test:abc123:graphql:Mutation.createApproval"
        );
    }

    #[test]
    fn canonical_id_deterministic() {
        let a = canonical_symbol_id(
            "https://github.com/acme/api",
            "src/billing/webhook.rs",
            "processPayment",
            "billing::PaymentService::processPayment",
        );
        let b = canonical_symbol_id(
            "https://github.com/acme/api",
            "src/billing/webhook.rs",
            "processPayment",
            "billing::PaymentService::processPayment",
        );
        assert_eq!(a, b);
    }

    #[test]
    fn canonical_id_same_across_trailing_slash() {
        let a = canonical_symbol_id("https://github.com/acme/api/", "src/lib.rs", "foo", "foo");
        let b = canonical_symbol_id("https://github.com/acme/api", "src/lib.rs", "foo", "foo");
        assert_eq!(a, b, "trailing slash should not change canonical_id");
    }

    #[test]
    fn canonical_id_different_for_different_scopes() {
        let a = canonical_symbol_id(
            "https://github.com/acme/api",
            "src/lib.rs",
            "process",
            "ModA::ClassA::process",
        );
        let b = canonical_symbol_id(
            "https://github.com/acme/api",
            "src/lib.rs",
            "process",
            "ModB::ClassB::process",
        );
        assert_ne!(a, b, "different scopes should produce different IDs");
    }

    #[test]
    fn canonical_id_format() {
        let id = canonical_symbol_id(
            "https://github.com/acme/api",
            "src/billing/webhook.rs",
            "processPayment",
            "billing::PaymentService::processPayment",
        );
        assert!(id.contains("src/billing/webhook.rs"));
        assert!(id.contains("#processPayment:"));
        let repo_hash = id.split(':').next().unwrap();
        assert_eq!(repo_hash.len(), 12);
    }

    #[test]
    fn canonical_id_empty_scope_falls_back_to_name() {
        let a = canonical_symbol_id("https://github.com/acme/api", "src/lib.rs", "main", "");
        let b = canonical_symbol_id("https://github.com/acme/api", "src/lib.rs", "main", "");
        assert_eq!(a, b, "empty scope chain should still be deterministic");
        // The scope hash should be the hash of the bare name — never the line.
        assert!(a.ends_with(&format!(":{}", truncated_hash("main"))));
    }

    #[test]
    fn canonical_id_empty_scope_stable_across_line_shifts() {
        // Adding blank lines above a top-level symbol shifts its start line but
        // must NOT change its canonical ID — the ID is a logical identity, not a
        // position. This is the core stability guarantee the PRD depends on for
        // cross-boundary flow-trace stitching and atomic-change matching.
        // Both calls represent the same logical symbol after a line shift; the
        // line number is no longer part of the signature, so identity is stable.
        let a = canonical_symbol_id("https://github.com/acme/api", "src/lib.rs", "helper", "");
        let b = canonical_symbol_id("https://github.com/acme/api", "src/lib.rs", "helper", "");
        assert_eq!(
            a, b,
            "line shifts must not change the canonical_id of a top-level symbol"
        );
    }

    #[test]
    fn canonical_search_symbol_identity_is_edit_stable_and_validated() {
        let repo_url = "https://github.com/acme/api";
        let file_path = "src/#generated:part.rs";
        let name = "operator:#:call";
        let repo = repo_uid("local", repo_url);
        let line_7_uid = symbol_uid(&repo, file_path, name, 7);
        let line_42_uid = symbol_uid(&repo, file_path, name, 42);
        let canonical = canonical_symbol_id(repo_url, file_path, name, "module::operator:#:call");
        let expected = format!("sym-canonical:{canonical}");

        assert_eq!(
            normalize_search_symbol_canonical_id(&line_7_uid, &canonical),
            Some(expected.clone())
        );
        assert_eq!(
            normalize_search_symbol_canonical_id(&line_42_uid, &canonical),
            Some(expected)
        );
    }

    #[test]
    fn canonical_search_symbol_identity_rejects_malformed_or_mismatched_ids() {
        let repo_url = "https://github.com/acme/api";
        let other_repo_url = "https://github.com/acme/other";
        let file_path = "src/lib.rs";
        let name = "needle";
        let uid = symbol_uid(&repo_uid("local", repo_url), file_path, name, 7);
        let canonical = canonical_symbol_id(repo_url, file_path, name, "module::needle");
        let other_repo_canonical =
            canonical_symbol_id(other_repo_url, file_path, name, "module::needle");
        let bad_scope = format!("{}ABC", &canonical[..canonical.len() - 3]);
        let empty_path = canonical_symbol_id(repo_url, "", name, "module::needle");
        let empty_name = canonical_symbol_id(repo_url, file_path, "", "module::needle");

        for invalid in [
            "",
            "not-a-canonical-id",
            other_repo_canonical.as_str(),
            bad_scope.as_str(),
            empty_path.as_str(),
            empty_name.as_str(),
        ] {
            assert_eq!(
                normalize_search_symbol_canonical_id(&uid, invalid),
                None,
                "{invalid:?} must not become a proof identity"
            );
        }
        assert_eq!(
            normalize_search_symbol_canonical_id("sym:malformed", &canonical),
            None
        );
    }

    #[test]
    fn scope_hash_empty_falls_back_to_name() {
        let h = scope_hash("", "main");
        assert_eq!(h, truncated_hash("main"));
    }

    #[test]
    fn scope_hash_non_empty_uses_chain() {
        let h = scope_hash("Foo::bar", "bar");
        assert_eq!(h, truncated_hash("Foo::bar"));
        assert_ne!(h, truncated_hash("bar"));
    }
}
