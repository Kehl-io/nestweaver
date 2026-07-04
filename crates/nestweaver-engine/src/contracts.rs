//! Feature F2-core: API contract graph.
//!
//! This module derives [`Contract`] nodes two ways and links code handlers to
//! them with `IMPLEMENTS_CONTRACT` edges:
//!
//! * **Declared** contracts come from spec files — OpenAPI/Swagger
//!   (`openapi.yaml|json`, `swagger.json`), Protocol Buffers (`*.proto`), and
//!   GraphQL SDL (`*.graphql`). See [`parse_spec_file`].
//! * **Code-derived** contracts are minted from framework route handlers
//!   (Spring `@*Mapping`, NestJS `@Get()`/`@Post()`) when no spec declares the
//!   route. See [`detect_handlers`].
//!
//! Both kinds are **hypotheses**, not ground truth: confidence on the incident
//! edge records match quality (1.0 exact verb+path, 0.8 base-path-inferred),
//! and [`compute_drift`] surfaces the declared-vs-implemented set difference.
//!
//! Two distinct comparisons live here — don't confuse them:
//! * [`drift_for_store`] compares, within ONE snapshot, which endpoints are
//!   *declared* (in a spec) vs *implemented* (by a handler). It is **endpoint
//!   presence only** — it does NOT look at request/response fields or types.
//! * [`diff_openapi`] compares TWO OpenAPI specs (base vs head) at the endpoint
//!   AND request/response **field/type** level, classifying each change as
//!   BREAKING or INFO. This is the "did this change break the API?" check
//!   (exposed as `nestweaver contracts diff`).
//!
//! Scope (per the F2-core adversarial review) is deliberately the trustworthy
//! lanes only: same-repo Spring/NestJS handler matching. Cross-repo
//! `CONSUMES` via fetch/axios/HTTP literals and GraphQL *consumers* are
//! explicitly out of scope here.

use nestweaver_schema::{Contract, Language, contract_uid, normalize_http_path};

/// Map the schema [`Language`] enum to the lowercase language string the
/// parser's `detect_frameworks` expects (`"java"`, `"javascript"`, ...).
/// Returns `None` for languages with no framework detector.
pub fn framework_language_str(lang: Language) -> Option<&'static str> {
    Some(match lang {
        Language::Java => "java",
        Language::Kotlin => "kotlin",
        Language::CSharp => "csharp",
        Language::Python => "python",
        Language::JavaScript => "javascript",
        Language::TypeScript => "typescript",
        Language::Ruby => "ruby",
        Language::Php => "php",
        Language::Dart => "dart",
        Language::Swift => "swift",
        Language::Go => "go",
        _ => return None,
    })
}

/// One contract surface extracted from a spec, before it is turned into a
/// [`Contract`] node (which needs a `repo_uid` and `source_path` the caller
/// supplies). `verb`/`path` are `Some` for HTTP; `operation_id` carries the
/// fully-qualified identifier for gRPC/GraphQL.
#[derive(Debug, Clone, PartialEq)]
pub struct SpecContract {
    pub kind: String,
    pub verb: Option<String>,
    pub path: Option<String>,
    pub operation_id: Option<String>,
}

impl SpecContract {
    /// Mint the UID this contract will be stored under.
    pub fn uid(&self) -> String {
        contract_uid(
            &self.kind,
            self.verb.as_deref(),
            self.path.as_deref(),
            self.operation_id.as_deref(),
        )
    }

    /// Build a full [`Contract`] node bound to a repo + source file.
    pub fn into_contract(self, repo_uid: &str, source_path: &str, confidence: f32) -> Contract {
        Contract {
            uid: self.uid(),
            kind: self.kind,
            verb: self.verb,
            path: self.path,
            operation_id: self.operation_id,
            repo_uid: repo_uid.to_string(),
            source_path: source_path.to_string(),
            confidence,
        }
    }
}

/// Return true if `file_name` (or its path) is a spec file we know how to
/// parse. Recognised: `openapi.{yaml,yml,json}`, `swagger.json`, `*.proto`,
/// `*.graphql`/`*.graphqls`/`*.gql`.
pub fn is_spec_file(path: &str) -> bool {
    spec_kind(path).is_some()
}

/// Classify a spec file path into a parser dispatch tag.
fn spec_kind(path: &str) -> Option<SpecFileKind> {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".proto") {
        return Some(SpecFileKind::Proto);
    }
    if lower.ends_with(".graphql") || lower.ends_with(".graphqls") || lower.ends_with(".gql") {
        return Some(SpecFileKind::GraphQl);
    }
    // OpenAPI / Swagger: match by filename stem so we don't try to parse every
    // YAML/JSON file in the repo as an OpenAPI doc (that would be noisy and
    // slow). The canonical names cover the overwhelming majority of repos.
    let stem_is_openapi = lower.starts_with("openapi.") || lower.starts_with("swagger.");
    if stem_is_openapi && (lower.ends_with(".yaml") || lower.ends_with(".yml")) {
        return Some(SpecFileKind::OpenApiYaml);
    }
    if stem_is_openapi && lower.ends_with(".json") {
        return Some(SpecFileKind::OpenApiJson);
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum SpecFileKind {
    OpenApiYaml,
    OpenApiJson,
    Proto,
    GraphQl,
}

/// Parse a spec file's contents into the contracts it declares. The `path` is
/// only used to dispatch on file kind. Returns an empty vec (never an error)
/// when the file does not parse — a malformed spec must not abort indexing.
pub fn parse_spec_file(path: &str, source: &str) -> Vec<SpecContract> {
    match spec_kind(path) {
        Some(SpecFileKind::OpenApiYaml) => parse_openapi_yaml(source),
        Some(SpecFileKind::OpenApiJson) => parse_openapi_json(source),
        Some(SpecFileKind::Proto) => parse_proto(path, source),
        Some(SpecFileKind::GraphQl) => parse_graphql(source),
        None => Vec::new(),
    }
}

fn parse_openapi_yaml(source: &str) -> Vec<SpecContract> {
    match serde_yaml_ng::from_str::<openapiv3::OpenAPI>(source) {
        Ok(spec) => openapi_contracts(&spec),
        Err(e) => {
            tracing::debug!("OpenAPI YAML parse failed: {e}");
            Vec::new()
        }
    }
}

fn parse_openapi_json(source: &str) -> Vec<SpecContract> {
    match serde_json::from_str::<openapiv3::OpenAPI>(source) {
        Ok(spec) => openapi_contracts(&spec),
        Err(e) => {
            tracing::debug!("OpenAPI JSON parse failed: {e}");
            Vec::new()
        }
    }
}

fn openapi_contracts(spec: &openapiv3::OpenAPI) -> Vec<SpecContract> {
    let mut out = Vec::new();
    for (raw_path, item) in spec.paths.iter() {
        let path_item = match item {
            openapiv3::ReferenceOr::Item(pi) => pi,
            // We don't follow $ref path items (rare); skip them.
            openapiv3::ReferenceOr::Reference { .. } => continue,
        };
        let norm = normalize_http_path(raw_path);
        let ops: [(&str, &Option<openapiv3::Operation>); 7] = [
            ("GET", &path_item.get),
            ("PUT", &path_item.put),
            ("POST", &path_item.post),
            ("DELETE", &path_item.delete),
            ("OPTIONS", &path_item.options),
            ("HEAD", &path_item.head),
            ("PATCH", &path_item.patch),
        ];
        for (verb, op) in ops {
            if let Some(operation) = op {
                out.push(SpecContract {
                    kind: "http".to_string(),
                    verb: Some(verb.to_string()),
                    path: Some(norm.clone()),
                    operation_id: operation.operation_id.clone(),
                });
            }
        }
    }
    out
}

fn parse_proto(path: &str, source: &str) -> Vec<SpecContract> {
    let fd = match protox_parse::parse(path, source) {
        Ok(fd) => fd,
        Err(e) => {
            tracing::debug!("proto parse failed: {e}");
            return Vec::new();
        }
    };
    let package = fd.package(); // "" when unset.
    let mut out = Vec::new();
    for service in &fd.service {
        let svc = service.name(); // e.g. "Approvals"
        for method in &service.method {
            // Fully-qualified: "<package>.<Service>/<Method>" (package omitted
            // when empty).
            let op = if package.is_empty() {
                format!("{}/{}", svc, method.name())
            } else {
                format!("{}.{}/{}", package, svc, method.name())
            };
            out.push(SpecContract {
                kind: "grpc".to_string(),
                verb: None,
                path: None,
                operation_id: Some(op),
            });
        }
    }
    out
}

fn parse_graphql(source: &str) -> Vec<SpecContract> {
    use apollo_parser::Parser;
    use apollo_parser::cst::{self, Definition};

    let cst = Parser::new(source).parse();
    // Tolerate errors: a partial schema still yields usable definitions.
    let doc = cst.document();
    let mut out = Vec::new();

    let mut collect = |type_name: &str, fields: Option<cst::FieldsDefinition>| {
        if let Some(fields) = fields {
            for field in fields.field_definitions() {
                if let Some(name) = field.name() {
                    out.push(SpecContract {
                        kind: "graphql".to_string(),
                        verb: None,
                        path: None,
                        operation_id: Some(format!("{type_name}.{}", name.text())),
                    });
                }
            }
        }
    };

    for def in doc.definitions() {
        if let Definition::ObjectTypeDefinition(obj) = def {
            let name = obj.name().map(|n| n.text().to_string()).unwrap_or_default();
            // Only the three root operation types declare contract surfaces.
            if matches!(name.as_str(), "Query" | "Mutation" | "Subscription") {
                collect(&name, obj.fields_definition());
            }
        }
    }
    out
}

// ── F2.2: framework handler detection (Spring + NestJS) ─────────────────────

/// A route handler discovered in source, paired with the contract it serves.
#[derive(Debug, Clone, PartialEq)]
pub struct HandlerMatch {
    /// 0-based index into the symbols slice passed to [`detect_handlers`].
    pub symbol_index: usize,
    pub contract: SpecContract,
    /// 1.0 for an exact verb+path match, 0.8 for a base-path-inferred match
    /// (handler had no explicit sub-path so we used only the controller base).
    pub confidence: f32,
}

/// Lightweight view of a symbol for handler detection — avoids depending on
/// the full `RawSymbol`/`Symbol` shape so this is unit-testable in isolation.
#[derive(Debug, Clone)]
pub struct HandlerSymbol {
    pub name: String,
    pub signature: String,
    /// 1-based line where the symbol's declaration begins. Used to scan the
    /// source lines immediately *above* the declaration for a route decorator
    /// / annotation, which the parser does not fold into `signature`.
    pub start_line: u32,
}

/// Extract the controller's class-level base path from raw source.
///
/// The tree-sitter symbol signature only captures the *first* annotation on a
/// class, so a `@RestController` + `@RequestMapping("/v1")` pair loses the base
/// path. We therefore scan the raw source for the first class-level
/// `@RequestMapping(...)` (Spring) or `@Controller(...)` (NestJS) — both appear
/// before any method, so the first match is the class base. Returns `""` when
/// no base path is declared.
pub fn extract_base_path(framework: &str, source: &str) -> String {
    match framework {
        "spring" => {
            // A Spring class base path is `@RequestMapping` ABOVE the class
            // declaration. Restricting the search to the pre-class region avoids
            // mistaking a method-level `@RequestMapping(method=..., path="/foo")`
            // in a `@RestController` (no class-level mapping) for the base path —
            // which would prepend `/foo` to every route and corrupt their UIDs.
            let region_end = class_decl_byte_pos(source).unwrap_or(source.len());
            source[..region_end]
                .rfind("@RequestMapping")
                .and_then(|i| first_string_arg(&source[i..]))
                .unwrap_or_default()
        }
        // `@Controller` is only ever class-level in NestJS, so the first one is
        // the controller decorator.
        "nestjs" => source
            .find("@Controller")
            .and_then(|i| first_string_arg(&source[i..]))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// Byte offset of the first `class <Ident>` declaration in `source`, or `None`.
/// Requires a word boundary before `class` so `subclass` / `MyClass` don't match.
fn class_decl_byte_pos(source: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(rel) = source[from..].find("class ") {
        let pos = from + rel;
        let boundary_ok = pos == 0
            || !source[..pos]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_');
        if boundary_ok {
            return Some(pos);
        }
        from = pos + "class ".len();
    }
    None
}

/// Find the NestJS controller class among `class_starts` (each `(symbol_index,
/// start_line)` for a class symbol) by scanning the source lines directly above
/// each class for a `@Controller` decorator. The TS parser does not fold this
/// decorator into the class signature, so `detect_frameworks` cannot see it.
/// Returns the `symbol_index` of the first matching class, or `None`.
pub fn detect_nestjs_controller_index(
    source: &str,
    class_starts: &[(usize, u32)],
) -> Option<usize> {
    for &(idx, start_line) in class_starts {
        let block = preceding_decorator_block(source, start_line);
        if block.contains("@Controller") {
            return Some(idx);
        }
    }
    None
}

/// Detect Spring / NestJS route handlers among `symbols` and pair each with
/// the HTTP [`SpecContract`] it implements.
///
/// `framework` is the detected framework string for the file
/// (`framework_hint.framework`) — `"spring"`, `"nestjs"`, or otherwise.
/// `source` is the controller's full raw source. It is used to (a) pull the
/// class-level base path (`@RequestMapping("/v1")` / `@Controller('users')`)
/// via [`extract_base_path`], and (b) recover per-handler route decorators /
/// annotations that the parser places on lines *above* a method declaration
/// (`@Post('approvals')` over `createApproval()`), which therefore never make
/// it into the symbol `signature`.
pub fn detect_handlers(
    framework: &str,
    source: &str,
    symbols: &[HandlerSymbol],
) -> Vec<HandlerMatch> {
    match framework {
        "spring" => detect_spring_handlers(source, symbols),
        "nestjs" => detect_nestjs_handlers(source, symbols),
        _ => Vec::new(),
    }
}

/// Maximum number of source lines above a method declaration we scan looking
/// for a route decorator / annotation. Decorators sit directly above the
/// method (often with other decorators / a blank line between), so a small
/// window is enough and keeps us from straying into the previous member.
const DECORATOR_SCAN_WINDOW: usize = 10;

/// Collect the source lines immediately above a symbol's declaration, joined
/// into one string, so they can be scanned for a route decorator / annotation
/// exactly like the declaration line. `start_line` is 1-based.
///
/// The window walks upward from the line just above the declaration, stopping
/// at a block boundary (`{` or `}`, i.e. the previous statement / the class
/// opening brace) so we never attribute a sibling method's decorator to this
/// one. Lines are returned in top-to-bottom order.
fn preceding_decorator_block(source: &str, start_line: u32) -> String {
    if start_line <= 1 {
        return String::new();
    }
    let lines: Vec<&str> = source.lines().collect();
    // Declaration is at index start_line - 1; scan the lines above it.
    let decl_idx = (start_line as usize).saturating_sub(1);
    if decl_idx == 0 || decl_idx > lines.len() {
        return String::new();
    }
    let mut collected: Vec<&str> = Vec::new();
    let mut i = decl_idx; // exclusive upper bound; we look at i-1 downward.
    let lower = decl_idx.saturating_sub(DECORATOR_SCAN_WINDOW);
    while i > lower {
        i -= 1;
        let line = lines[i];
        let trimmed = line.trim();
        // A block boundary marks the end of the preceding decorator run.
        if trimmed.ends_with('{') || trimmed.ends_with('}') {
            break;
        }
        collected.push(line);
    }
    collected.reverse();
    collected.join("\n")
}

/// Extract the first string literal argument inside the *first* `(...)` of an
/// annotation/decorator, e.g. `@RequestMapping("/v1/users")` -> `/v1/users`,
/// `@Controller('users')` -> `users`, `@Post(':id')` -> `:id`. Returns `None`
/// when there is no parenthesised string literal (bare `@Post()`).
fn first_string_arg(after: &str) -> Option<String> {
    let open = after.find('(')?;
    let rest = &after[open + 1..];
    let close = rest.find(')')?;
    let inner = &rest[..close];
    // Return the FIRST quoted literal inside the annotation args. The old code
    // required the ENTIRE paren body to be a single quoted string, so it missed
    // the most common Spring forms — `@GetMapping(value = "/x")`,
    // `@RequestMapping(path="/x", method=...)`, and array literals
    // `@GetMapping({"/a","/b"})` — losing the path and manufacturing false drift
    // (declared-not-implemented AND implemented-not-declared on the same route).
    first_quoted_literal(inner)
}

/// Extract the contents of the first `"…"`, `'…'`, or `` `…` `` literal in `s`.
/// UTF-8 safe: the quote chars are ASCII so all slice boundaries are valid.
fn first_quoted_literal(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'"' || c == b'\'' || c == b'`' {
            let after = &s[i + 1..];
            let rel = after.find(c as char)?; // unterminated → None
            return Some(after[..rel].to_string());
        }
        i += 1;
    }
    None
}

/// Join a base path and a sub-path into a single normalized route.
fn join_paths(base: &str, sub: &str) -> String {
    let combined = format!(
        "{}/{}",
        base.trim_end_matches('/'),
        sub.trim_start_matches('/')
    );
    normalize_http_path(&combined)
}

/// Join base and sub paths, but detect when the sub-path already includes the
/// base prefix to avoid double-concatenation (e.g. base="/api", sub="/api/users").
fn join_paths_dedup(base: &str, sub: &str) -> String {
    if !base.is_empty() && (sub == base || sub.starts_with(&format!("{base}/"))) {
        normalize_http_path(sub)
    } else {
        join_paths(base, sub)
    }
}

fn detect_spring_handlers(source: &str, symbols: &[HandlerSymbol]) -> Vec<HandlerMatch> {
    // Class-level base path from @RequestMapping("...") on the controller. The
    // first @RequestMapping in the source is the class-level one (methods that
    // use @RequestMapping come after the class declaration).
    let base = extract_base_path("spring", source);

    let mappings: [(&str, &str); 5] = [
        ("@GetMapping", "GET"),
        ("@PostMapping", "POST"),
        ("@PutMapping", "PUT"),
        ("@DeleteMapping", "DELETE"),
        ("@PatchMapping", "PATCH"),
    ];

    let mut out = Vec::new();
    for (idx, sym) in symbols.iter().enumerate() {
        // The route annotation may be on the declaration line *or* on the
        // lines directly above it. Scan both: signature first, then the
        // preceding-line block.
        let preceding = preceding_decorator_block(source, sym.start_line);
        let scan = format!("{}\n{}", preceding, sym.signature);
        let scan = scan.as_str();
        let mut matched: Option<(&str, Option<String>)> = None;
        for (anno, verb) in mappings {
            if let Some(at) = scan.find(anno) {
                matched = Some((verb, first_string_arg(&scan[at..])));
                break;
            }
        }
        // @RequestMapping(method = RequestMethod.POST, path = "...")
        if matched.is_none()
            && let Some(at) = scan.find("@RequestMapping")
        {
            let verb = request_mapping_verb(&scan[at..]).unwrap_or("ANY");
            matched = Some((verb, first_string_arg(&scan[at..])));
        }

        if let Some((verb, sub)) = matched {
            let (path, confidence) = match sub {
                Some(s) => (join_paths_dedup(&base, &s), 1.0),
                // No sub-path → base-path-inferred (lower confidence).
                None => (
                    if base.is_empty() {
                        "/".to_string()
                    } else {
                        normalize_http_path(&base)
                    },
                    0.8,
                ),
            };
            out.push(HandlerMatch {
                symbol_index: idx,
                contract: SpecContract {
                    kind: "http".to_string(),
                    verb: Some(verb.to_string()),
                    path: Some(path),
                    operation_id: None,
                },
                confidence,
            });
        }
    }
    out
}

fn request_mapping_verb(s: &str) -> Option<&'static str> {
    // Crude but adequate: look for RequestMethod.<VERB> in the annotation text.
    for v in ["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"] {
        if s.contains(&format!("RequestMethod.{v}")) {
            return Some(match v {
                "GET" => "GET",
                "POST" => "POST",
                "PUT" => "PUT",
                "DELETE" => "DELETE",
                "PATCH" => "PATCH",
                "HEAD" => "HEAD",
                _ => "OPTIONS",
            });
        }
    }
    None
}

fn detect_nestjs_handlers(source: &str, symbols: &[HandlerSymbol]) -> Vec<HandlerMatch> {
    // Controller base path from @Controller('prefix').
    let base = extract_base_path("nestjs", source);

    let decorators: [(&str, &str); 5] = [
        ("@Get(", "GET"),
        ("@Post(", "POST"),
        ("@Put(", "PUT"),
        ("@Delete(", "DELETE"),
        ("@Patch(", "PATCH"),
    ];

    let mut out = Vec::new();
    for (idx, sym) in symbols.iter().enumerate() {
        // The route decorator may sit on the declaration line or on the lines
        // directly above it (the common `@Post('x')` over `method()` style).
        let preceding = preceding_decorator_block(source, sym.start_line);
        let scan = format!("{}\n{}", preceding, sym.signature);
        let scan = scan.as_str();
        let mut matched: Option<(&str, Option<String>)> = None;
        for (deco, verb) in decorators {
            if let Some(at) = scan.find(deco) {
                // first_string_arg expects the '(' to be findable from here.
                matched = Some((verb, first_string_arg(&scan[at + deco.len() - 1..])));
                break;
            }
        }
        if let Some((verb, sub)) = matched {
            let (path, confidence) = match sub {
                Some(s) => {
                    // If the sub-path already starts with the base path, don't
                    // join — doing so would double-concatenate the prefix (e.g.
                    // base="api", sub="/api/users" → "/api/api/users").
                    // Use a boundary check to avoid false positives like
                    // base="/rest", sub="/restore".
                    let path =
                        if !base.is_empty() && (s == base || s.starts_with(&format!("{base}/"))) {
                            normalize_http_path(&s)
                        } else {
                            join_paths(&base, &s)
                        };
                    (path, 1.0)
                }
                None => (
                    if base.is_empty() {
                        "/".to_string()
                    } else {
                        normalize_http_path(&base)
                    },
                    0.8,
                ),
            };
            out.push(HandlerMatch {
                symbol_index: idx,
                contract: SpecContract {
                    kind: "http".to_string(),
                    verb: Some(verb.to_string()),
                    path: Some(path),
                    operation_id: None,
                },
                confidence,
            });
        }
    }
    out
}

// ── F2.4: drift diagnostics ─────────────────────────────────────────────────

/// One drift finding: a contract that is declared in a spec but has no
/// implementing handler, or implemented by a handler but declared in no spec.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct DriftFinding {
    pub uid: String,
    pub kind: String,
    pub verb: Option<String>,
    pub path: Option<String>,
    pub operation_id: Option<String>,
    /// `"declared-not-implemented"` or `"implemented-not-declared"`.
    pub category: String,
}

/// Result of a drift analysis: the two set-difference buckets.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct DriftReport {
    pub declared_not_implemented: Vec<DriftFinding>,
    pub implemented_not_declared: Vec<DriftFinding>,
}

impl DriftReport {
    pub fn is_clean(&self) -> bool {
        self.declared_not_implemented.is_empty() && self.implemented_not_declared.is_empty()
    }
}

/// Compute the declared/implemented set difference.
///
/// * `declared` — contracts that came from spec files (confidence 1.0).
/// * `code_derived` — contracts minted from handlers because no spec declared
///   them (these are, by construction, implemented-but-undeclared).
/// * `implemented_uids` — the set of contract UIDs that have at least one
///   incident `IMPLEMENTS_CONTRACT` edge.
///
/// A declared contract with no incident implements edge is
/// *declared-but-not-implemented*. A code-derived contract is
/// *implemented-but-undeclared*.
pub fn compute_drift(
    declared: &[Contract],
    code_derived: &[Contract],
    implemented_uids: &std::collections::HashSet<String>,
) -> DriftReport {
    let mut report = DriftReport::default();

    let declared_uids: std::collections::HashSet<&str> =
        declared.iter().map(|c| c.uid.as_str()).collect();

    for c in declared {
        if !implemented_uids.contains(&c.uid) {
            report.declared_not_implemented.push(DriftFinding {
                uid: c.uid.clone(),
                kind: c.kind.clone(),
                verb: c.verb.clone(),
                path: c.path.clone(),
                operation_id: c.operation_id.clone(),
                category: "declared-not-implemented".to_string(),
            });
        }
    }

    for c in code_derived {
        // A code-derived contract is undeclared unless a spec also declares the
        // same UID (in which case it isn't drift — it's a normal match).
        if !declared_uids.contains(c.uid.as_str()) {
            report.implemented_not_declared.push(DriftFinding {
                uid: c.uid.clone(),
                kind: c.kind.clone(),
                verb: c.verb.clone(),
                path: c.path.clone(),
                operation_id: c.operation_id.clone(),
                category: "implemented-not-declared".to_string(),
            });
        }
    }

    report
}

/// Run drift analysis against a [`nestweaver_store::GraphStore`], optionally
/// scoped to a single repo UID.
///
/// Contracts whose `source_path` is a spec file are treated as **declared**;
/// the rest are **code-derived**. A declared contract with no incident
/// `IMPLEMENTS_CONTRACT` edge is *declared-not-implemented*; a code-derived
/// contract is *implemented-not-declared*.
/// Resolve a repo filter (an exact repo UID **or** a case-insensitive display
/// name) to its repo UID. `list_contracts` filters by exact UID, and repo UIDs
/// are hashed — so an MCP client passing the natural human name (`payments-svc`)
/// would match nothing and get a false "clean" report. Resolve names here so the
/// MCP and CLI front-ends agree. An unmatched value is passed through unchanged
/// (an explicit unknown UID still yields an empty result, as before).
fn resolve_repo_uid(
    store: &nestweaver_store::GraphStore,
    filter: Option<&str>,
) -> Result<Option<String>, nestweaver_store::StoreError> {
    let Some(filter) = filter else {
        return Ok(None);
    };
    let repos = store.list_repos(None)?;
    if let Some(r) = repos.iter().find(|r| r.uid == filter) {
        return Ok(Some(r.uid.clone()));
    }
    let needle = filter.to_lowercase();
    if let Some(r) = repos
        .iter()
        .find(|r| crate::repo_display_name(r).to_lowercase() == needle)
    {
        return Ok(Some(r.uid.clone()));
    }
    Ok(Some(filter.to_string()))
}

pub fn drift_for_store(
    store: &nestweaver_store::GraphStore,
    repo_uid: Option<&str>,
) -> Result<DriftReport, nestweaver_store::StoreError> {
    let repo_uid = resolve_repo_uid(store, repo_uid)?;
    let all = store.list_contracts(repo_uid.as_deref())?;
    let implemented: std::collections::HashSet<String> = store
        .list_implemented_contract_uids()?
        .into_iter()
        .collect();

    let (declared, code_derived): (Vec<Contract>, Vec<Contract>) =
        all.into_iter().partition(|c| is_spec_file(&c.source_path));

    Ok(compute_drift(&declared, &code_derived, &implemented))
}

// ── Field-level spec-vs-spec breaking-change diff (F2) ──────────────────────
//
// `drift_for_store` only compares endpoint *presence*. This compares two
// versions of an OpenAPI spec (base vs head) at the endpoint AND request/response
// FIELD/TYPE level, classifying each difference as BREAKING or INFO — the "did
// this change break the API?" check. Scope: OpenAPI (yaml/json), application/json
// bodies, object root schemas; `$ref`s are resolved against components.schemas
// (depth-capped for cycles). Non-object / composed (oneOf/allOf) schemas are
// treated opaquely (empty field set), a conservative choice that avoids false
// breakage.
//
// Known limitations (top-level fields only, by design — keeps verdicts trustworthy
// rather than clever): nested-object and array-item field changes are NOT recursed
// (a false negative — conservative), and a property's type is a coarse name (a
// `$ref` is named by its target), so refactoring an inline object into a named
// `$ref` of the same shape reads as a type change (a benign false positive a human
// reviews). It is not a full OpenAPI diff engine — it catches the common,
// high-value top-level breaks.

/// Severity of a spec change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum SpecChangeSeverity {
    Breaking,
    Info,
}

/// One classified difference between a base and head spec.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SpecChange {
    pub severity: SpecChangeSeverity,
    pub verb: String,
    pub path: String,
    pub detail: String,
}

/// Request/response shape of one operation, used for the field-level diff.
#[derive(Default, PartialEq, Eq)]
struct OpShape {
    /// request-body field name → type string
    req_fields: std::collections::BTreeMap<String, String>,
    /// request-body required field names
    req_required: std::collections::BTreeSet<String>,
    /// success-response field name → type string
    resp_fields: std::collections::BTreeMap<String, String>,
}

const REF_DEPTH_CAP: usize = 8;

/// Resolve a `ReferenceOr<Schema>` to a concrete `Schema`, following
/// `#/components/schemas/*` refs up to `REF_DEPTH_CAP` (guards ref cycles).
fn resolve_schema<'a>(
    spec: &'a openapiv3::OpenAPI,
    mut sref: &'a openapiv3::ReferenceOr<openapiv3::Schema>,
    mut depth: usize,
) -> Option<&'a openapiv3::Schema> {
    loop {
        match sref {
            openapiv3::ReferenceOr::Item(s) => return Some(s),
            openapiv3::ReferenceOr::Reference { reference } => {
                if depth == 0 {
                    return None;
                }
                depth -= 1;
                let name = reference.rsplit('/').next()?;
                let comps = spec.components.as_ref()?;
                sref = comps.schemas.get(name)?;
            }
        }
    }
}

/// A short type name for a property schema. A `$ref` property yields its target
/// name, so changing the referenced type is detected as a type change.
fn prop_type_name(prop: &openapiv3::ReferenceOr<Box<openapiv3::Schema>>) -> String {
    match prop {
        openapiv3::ReferenceOr::Reference { reference } => reference
            .rsplit('/')
            .next()
            .unwrap_or(reference)
            .to_string(),
        openapiv3::ReferenceOr::Item(s) => match &s.schema_kind {
            openapiv3::SchemaKind::Type(t) => match t {
                openapiv3::Type::String(_) => "string",
                openapiv3::Type::Number(_) => "number",
                openapiv3::Type::Integer(_) => "integer",
                openapiv3::Type::Object(_) => "object",
                openapiv3::Type::Array(_) => "array",
                openapiv3::Type::Boolean(_) => "boolean",
            }
            .to_string(),
            openapiv3::SchemaKind::OneOf { .. } => "oneOf".to_string(),
            openapiv3::SchemaKind::AllOf { .. } => "allOf".to_string(),
            openapiv3::SchemaKind::AnyOf { .. } => "anyOf".to_string(),
            _ => "any".to_string(),
        },
    }
}

/// Extract (field→type, required) from an object schema; empty for non-objects.
fn object_fields(
    spec: &openapiv3::OpenAPI,
    sref: &openapiv3::ReferenceOr<openapiv3::Schema>,
) -> (
    std::collections::BTreeMap<String, String>,
    std::collections::BTreeSet<String>,
) {
    let mut fields = std::collections::BTreeMap::new();
    let mut required = std::collections::BTreeSet::new();
    if let Some(schema) = resolve_schema(spec, sref, REF_DEPTH_CAP)
        && let openapiv3::SchemaKind::Type(openapiv3::Type::Object(obj)) = &schema.schema_kind
    {
        for (name, prop) in &obj.properties {
            fields.insert(name.clone(), prop_type_name(prop));
        }
        required.extend(obj.required.iter().cloned());
    }
    (fields, required)
}

/// Pull the application/json object shape from a request/response content map.
/// Generic over the map so we don't need to name `indexmap::IndexMap` directly.
fn json_shape<'a>(
    spec: &openapiv3::OpenAPI,
    content: impl IntoIterator<Item = (&'a String, &'a openapiv3::MediaType)>,
) -> Option<(
    std::collections::BTreeMap<String, String>,
    std::collections::BTreeSet<String>,
)> {
    let mut chosen: Option<&openapiv3::MediaType> = None;
    for (ct, media) in content {
        if ct == "application/json" {
            chosen = Some(media);
            break;
        }
        chosen.get_or_insert(media);
    }
    let schema = chosen?.schema.as_ref()?;
    Some(object_fields(spec, schema))
}

/// Build the per-operation shapes for a parsed OpenAPI spec.
fn spec_shapes(spec: &openapiv3::OpenAPI) -> std::collections::BTreeMap<(String, String), OpShape> {
    let mut out = std::collections::BTreeMap::new();
    for (raw_path, item) in spec.paths.iter() {
        let openapiv3::ReferenceOr::Item(pi) = item else {
            continue;
        };
        let norm = normalize_http_path(raw_path);
        let ops: [(&str, &Option<openapiv3::Operation>); 5] = [
            ("GET", &pi.get),
            ("PUT", &pi.put),
            ("POST", &pi.post),
            ("DELETE", &pi.delete),
            ("PATCH", &pi.patch),
        ];
        for (verb, op) in ops {
            let Some(op) = op else { continue };
            let mut shape = OpShape::default();
            // Request body.
            if let Some(openapiv3::ReferenceOr::Item(rb)) = &op.request_body
                && let Some((f, r)) = json_shape(spec, &rb.content)
            {
                shape.req_fields = f;
                shape.req_required = r;
            }
            // First 2xx response with a JSON body.
            for (code, resp) in &op.responses.responses {
                let is_2xx = matches!(code, openapiv3::StatusCode::Code(c) if (200..300).contains(c))
                    || matches!(code, openapiv3::StatusCode::Range(2));
                if is_2xx
                    && let openapiv3::ReferenceOr::Item(r) = resp
                    && let Some((f, _)) = json_shape(spec, &r.content)
                {
                    shape.resp_fields = f;
                    break;
                }
            }
            out.insert((verb.to_string(), norm.clone()), shape);
        }
    }
    out
}

fn parse_openapi(path: &str, source: &str) -> Option<openapiv3::OpenAPI> {
    match spec_kind(path) {
        Some(SpecFileKind::OpenApiYaml) => serde_yaml_ng::from_str(source).ok(),
        Some(SpecFileKind::OpenApiJson) => serde_json::from_str(source).ok(),
        _ => None,
    }
}

/// Compare a base and head OpenAPI spec, classifying each operation/field/type
/// difference. Returns `None` if either file is not a parseable OpenAPI spec.
pub fn diff_openapi(
    base_path: &str,
    base_src: &str,
    head_path: &str,
    head_src: &str,
) -> Option<Vec<SpecChange>> {
    let base = parse_openapi(base_path, base_src)?;
    let head = parse_openapi(head_path, head_src)?;
    let base_ops = spec_shapes(&base);
    let head_ops = spec_shapes(&head);
    let mut changes = Vec::new();

    for (key, b) in &base_ops {
        let (verb, path) = key;
        let push = |changes: &mut Vec<SpecChange>, sev, detail: String| {
            changes.push(SpecChange {
                severity: sev,
                verb: verb.clone(),
                path: path.clone(),
                detail,
            });
        };
        let Some(h) = head_ops.get(key) else {
            push(
                &mut changes,
                SpecChangeSeverity::Breaking,
                "endpoint removed".to_string(),
            );
            continue;
        };
        // Response fields: removal or type change breaks consumers.
        for (name, btype) in &b.resp_fields {
            match h.resp_fields.get(name) {
                None => push(
                    &mut changes,
                    SpecChangeSeverity::Breaking,
                    format!("response field '{name}' removed"),
                ),
                Some(htype) if htype != btype => push(
                    &mut changes,
                    SpecChangeSeverity::Breaking,
                    format!("response field '{name}' type changed ({btype} -> {htype})"),
                ),
                _ => {}
            }
        }
        for name in h.resp_fields.keys() {
            if !b.resp_fields.contains_key(name) {
                push(
                    &mut changes,
                    SpecChangeSeverity::Info,
                    format!("response field '{name}' added"),
                );
            }
        }
        // Request fields: a type change or a NEWLY-required field breaks callers.
        for (name, btype) in &b.req_fields {
            if let Some(htype) = h.req_fields.get(name)
                && htype != btype
            {
                push(
                    &mut changes,
                    SpecChangeSeverity::Breaking,
                    format!("request field '{name}' type changed ({btype} -> {htype})"),
                );
            }
        }
        for name in &h.req_required {
            let newly_required = !b.req_required.contains(name);
            if newly_required {
                push(
                    &mut changes,
                    SpecChangeSeverity::Breaking,
                    format!("request field '{name}' is now required"),
                );
            }
        }
        for name in h.req_fields.keys() {
            if !b.req_fields.contains_key(name) && !h.req_required.contains(name) {
                push(
                    &mut changes,
                    SpecChangeSeverity::Info,
                    format!("optional request field '{name}' added"),
                );
            }
        }
    }
    // Added endpoints.
    for key in head_ops.keys() {
        if !base_ops.contains_key(key) {
            changes.push(SpecChange {
                severity: SpecChangeSeverity::Info,
                verb: key.0.clone(),
                path: key.1.clone(),
                detail: "endpoint added".to_string(),
            });
        }
    }
    changes.sort_by(|a, b| {
        (a.severity as u8, &a.path, &a.verb, &a.detail).cmp(&(
            b.severity as u8,
            &b.path,
            &b.verb,
            &b.detail,
        ))
    });
    Some(changes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn first_string_arg_handles_common_spring_forms() {
        // Bare (the only form the old code handled).
        assert_eq!(
            first_string_arg("@GetMapping(\"/x\")").as_deref(),
            Some("/x")
        );
        // Named arg — the common form the old code missed.
        assert_eq!(
            first_string_arg("@GetMapping(value = \"/x\")").as_deref(),
            Some("/x")
        );
        assert_eq!(
            first_string_arg("@RequestMapping(path=\"/v1\", method=RequestMethod.GET)").as_deref(),
            Some("/v1")
        );
        // Array literal — takes the first path.
        assert_eq!(
            first_string_arg("@GetMapping({\"/a\", \"/b\"})").as_deref(),
            Some("/a")
        );
        // No string literal (e.g. only method= ...) → None, not a wrong path.
        assert_eq!(
            first_string_arg("@RequestMapping(method = RequestMethod.GET)"),
            None
        );
        // Unicode inside quotes survives.
        assert_eq!(
            first_string_arg("@GetMapping(\"/café\")").as_deref(),
            Some("/café")
        );
    }

    #[test]
    fn extract_base_path_ignores_method_level_request_mapping() {
        // Class-level @RequestMapping → that's the base path.
        let with_class = "@RestController\n@RequestMapping(\"/v1\")\npublic class C {\n  @GetMapping(\"/x\") void x() {}\n}";
        assert_eq!(extract_base_path("spring", with_class), "/v1");

        // No class-level mapping; a method-level @RequestMapping must NOT become
        // the base path.
        let method_only = "@RestController\npublic class C {\n  @RequestMapping(method = RequestMethod.GET, path = \"/foo\")\n  void foo() {}\n}";
        assert_eq!(extract_base_path("spring", method_only), "");
    }

    #[test]
    fn diff_openapi_classifies_field_level_changes() {
        let base = r#"
openapi: 3.0.0
info: {title: t, version: "1"}
paths:
  /pay:
    post:
      requestBody:
        content:
          application/json:
            schema:
              type: object
              required: [amount]
              properties:
                amount: {type: integer}
                note: {type: string}
      responses:
        '200':
          description: ok
          content:
            application/json:
              schema:
                type: object
                properties:
                  id: {type: string}
                  status: {type: string}
  /old:
    get:
      responses: {'200': {description: ok}}
"#;
        let head = r#"
openapi: 3.0.0
info: {title: t, version: "1"}
paths:
  /pay:
    post:
      requestBody:
        content:
          application/json:
            schema:
              type: object
              required: [amount, currency]
              properties:
                amount: {type: string}
                currency: {type: string}
                note: {type: string}
      responses:
        '200':
          description: ok
          content:
            application/json:
              schema:
                type: object
                properties:
                  id: {type: string}
                  extra: {type: string}
  /new:
    get:
      responses: {'200': {description: ok}}
"#;
        let changes = diff_openapi("openapi.yaml", base, "openapi.yaml", head).expect("parses");
        let breaking: Vec<&str> = changes
            .iter()
            .filter(|c| c.severity == SpecChangeSeverity::Breaking)
            .map(|c| c.detail.as_str())
            .collect();
        let info: Vec<&str> = changes
            .iter()
            .filter(|c| c.severity == SpecChangeSeverity::Info)
            .map(|c| c.detail.as_str())
            .collect();

        // Four genuine breaks.
        assert!(
            breaking
                .iter()
                .any(|d| d.contains("'amount' type changed (integer -> string)")),
            "{breaking:?}"
        );
        assert!(
            breaking
                .iter()
                .any(|d| d.contains("'currency' is now required")),
            "{breaking:?}"
        );
        assert!(
            breaking
                .iter()
                .any(|d| d.contains("response field 'status' removed")),
            "{breaking:?}"
        );
        assert!(
            breaking.iter().any(|d| d.contains("endpoint removed")),
            "{breaking:?}"
        );
        assert_eq!(breaking.len(), 4, "unexpected breaking set: {breaking:?}");

        // Compatible additions are INFO, not breaking.
        assert!(
            info.iter()
                .any(|d| d.contains("response field 'extra' added"))
        );
        assert!(info.iter().any(|d| d.contains("endpoint added")));
        // A new required field must NOT also be double-reported as "optional added".
        assert!(!info.iter().any(|d| d.contains("'currency'")), "{info:?}");
    }

    #[test]
    fn diff_openapi_no_changes_is_empty() {
        let spec = r#"
openapi: 3.0.0
info: {title: t, version: "1"}
paths:
  /x:
    get:
      responses:
        '200':
          description: ok
          content:
            application/json:
              schema: {type: object, properties: {id: {type: string}}}
"#;
        let changes = diff_openapi("openapi.yaml", spec, "openapi.yaml", spec).expect("parses");
        assert!(
            changes.is_empty(),
            "identical specs should have no changes: {changes:?}"
        );
    }

    #[test]
    fn diff_openapi_returns_none_for_non_openapi() {
        assert!(diff_openapi("notes.md", "# hi", "notes.md", "# hi").is_none());
    }

    #[test]
    fn detects_spec_files_by_name() {
        assert!(is_spec_file("api/openapi.yaml"));
        assert!(is_spec_file("openapi.json"));
        assert!(is_spec_file("swagger.json"));
        assert!(is_spec_file("proto/approvals.proto"));
        assert!(is_spec_file("schema.graphql"));
        assert!(!is_spec_file("src/config.yaml")); // not an openapi stem
        assert!(!is_spec_file("src/main.rs"));
    }

    #[test]
    fn openapi_yaml_mints_http_contract_uid() {
        let spec = r#"
openapi: 3.0.0
info: { title: t, version: "1.0" }
paths:
  /v1/approvals:
    post:
      operationId: createApproval
      responses: { "200": { description: ok } }
  /v1/users/{id}:
    get:
      operationId: getUser
      responses: { "200": { description: ok } }
"#;
        let contracts = parse_spec_file("openapi.yaml", spec);
        let uids: Vec<String> = contracts.iter().map(|c| c.uid()).collect();
        assert!(
            uids.contains(&"contract:http:POST:/v1/approvals".to_string()),
            "uids: {uids:?}"
        );
        assert!(
            uids.contains(&"contract:http:GET:/v1/users/{}".to_string()),
            "param slot must normalize; uids: {uids:?}"
        );
    }

    #[test]
    fn proto_mints_grpc_contract_uid() {
        let proto = r#"
syntax = "proto3";
package approvals.v1;
service Approvals {
  rpc Create (CreateReq) returns (CreateResp);
}
message CreateReq {}
message CreateResp {}
"#;
        let contracts = parse_spec_file("approvals.proto", proto);
        let uids: Vec<String> = contracts.iter().map(|c| c.uid()).collect();
        assert_eq!(uids, vec!["contract:grpc:approvals.v1.Approvals/Create"]);
    }

    #[test]
    fn graphql_mints_graphql_contract_uid() {
        let schema = r#"
type Mutation {
  createApproval(input: String): String
}
type Query {
  approval(id: ID!): String
}
"#;
        let contracts = parse_spec_file("schema.graphql", schema);
        let uids: Vec<String> = contracts.iter().map(|c| c.uid()).collect();
        assert!(
            uids.contains(&"contract:graphql:Mutation.createApproval".to_string()),
            "uids: {uids:?}"
        );
        assert!(
            uids.contains(&"contract:graphql:Query.approval".to_string()),
            "uids: {uids:?}"
        );
    }

    #[test]
    fn malformed_spec_yields_no_contracts() {
        assert!(parse_spec_file("openapi.yaml", "::: not yaml :::").is_empty());
        assert!(parse_spec_file("x.proto", "this is not proto").is_empty());
    }

    #[test]
    fn spring_handler_exact_match() {
        let class_sig =
            "@RestController @RequestMapping(\"/v1/approvals\") public class ApprovalsController";
        let symbols = vec![
            HandlerSymbol {
                name: "create".into(),
                signature: "@PostMapping public void create()".into(),
                start_line: 0,
            },
            HandlerSymbol {
                name: "get".into(),
                signature: "@GetMapping(\"/{id}\") public Approval get(String id)".into(),
                start_line: 0,
            },
        ];
        let matches = detect_handlers("spring", class_sig, &symbols);
        assert_eq!(matches.len(), 2);
        // create: base path only, no sub-path → base-path-inferred (0.8).
        let create = &matches[0];
        assert_eq!(create.contract.uid(), "contract:http:POST:/v1/approvals");
        assert_eq!(create.confidence, 0.8);
        // get: explicit sub-path → exact (1.0), param normalized.
        let get = &matches[1];
        assert_eq!(get.contract.uid(), "contract:http:GET:/v1/approvals/{}");
        assert_eq!(get.confidence, 1.0);
    }

    #[test]
    fn nestjs_handler_match_with_controller_prefix() {
        let class_sig = "@Controller('approvals') export class ApprovalsController";
        let symbols = vec![HandlerSymbol {
            name: "findOne".into(),
            signature: "@Get(':id') findOne(@Param('id') id: string)".into(),
            start_line: 0,
        }];
        let matches = detect_handlers("nestjs", class_sig, &symbols);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].contract.uid(), "contract:http:GET:/approvals/{}");
        assert_eq!(matches[0].confidence, 1.0);
    }

    #[test]
    fn nestjs_decorator_on_own_line_matches() {
        // The common style: @Post('approvals') sits on the line ABOVE the
        // method declaration, so it never lands in the parsed signature. The
        // detector must recover it from the preceding source lines.
        let source = "@Controller('v1')\n\
                      export class ApprovalsController {\n  \
                      @Post('approvals')\n  \
                      createApproval() { return {}; }\n\
                      }\n";
        let symbols = vec![HandlerSymbol {
            // Signature as the TS parser emits it: declaration line only.
            name: "createApproval".into(),
            signature: "createApproval() { return {}; }".into(),
            start_line: 4, // 1-based line of the declaration.
        }];
        let matches = detect_handlers("nestjs", source, &symbols);
        assert_eq!(matches.len(), 1, "matches: {matches:?}");
        assert_eq!(
            matches[0].contract.uid(),
            "contract:http:POST:/v1/approvals"
        );
        assert_eq!(matches[0].confidence, 1.0);
    }

    #[test]
    fn nestjs_multiple_handlers_each_on_own_line_all_match() {
        // QA bug A: a controller with MULTIPLE route methods, each decorator on
        // its own line, must yield an IMPLEMENTS_CONTRACT match for EVERY
        // annotated method — not just the first one.
        let source = "@Controller('v1')\n\
                      export class Api {\n  \
                      @Get('health')\n  \
                      health() { return {}; }\n  \
                      @Post('users')\n  \
                      createUser() { return {}; }\n\
                      }\n";
        let symbols = vec![
            HandlerSymbol {
                name: "health".into(),
                signature: "health() { return {}; }".into(),
                start_line: 4,
            },
            HandlerSymbol {
                name: "createUser".into(),
                signature: "createUser() { return {}; }".into(),
                start_line: 6,
            },
        ];
        let matches = detect_handlers("nestjs", source, &symbols);
        assert_eq!(
            matches.len(),
            2,
            "both handlers must match; got {matches:?}"
        );
        let uids: Vec<String> = matches.iter().map(|m| m.contract.uid()).collect();
        assert!(
            uids.contains(&"contract:http:GET:/v1/health".to_string()),
            "GET /v1/health missing; uids: {uids:?}"
        );
        assert!(
            uids.contains(&"contract:http:POST:/v1/users".to_string()),
            "POST /v1/users missing; uids: {uids:?}"
        );
    }

    #[test]
    fn spring_multiple_handlers_each_on_own_line_all_match() {
        // QA bug A (Spring side): multiple @*Mapping methods, annotations on
        // their own lines, must ALL be detected.
        let source = "@RestController\n\
                      @RequestMapping(\"/v1\")\n\
                      public class Api {\n  \
                      @GetMapping(\"/health\")\n  \
                      public Object health() { return null; }\n  \
                      @PostMapping(\"/users\")\n  \
                      public Object createUser() { return null; }\n\
                      }\n";
        let symbols = vec![
            HandlerSymbol {
                name: "health".into(),
                signature: "public Object health() { return null; }".into(),
                start_line: 5,
            },
            HandlerSymbol {
                name: "createUser".into(),
                signature: "public Object createUser() { return null; }".into(),
                start_line: 7,
            },
        ];
        let matches = detect_handlers("spring", source, &symbols);
        assert_eq!(
            matches.len(),
            2,
            "both handlers must match; got {matches:?}"
        );
        let uids: Vec<String> = matches.iter().map(|m| m.contract.uid()).collect();
        assert!(uids.contains(&"contract:http:GET:/v1/health".to_string()));
        assert!(uids.contains(&"contract:http:POST:/v1/users".to_string()));
    }

    #[test]
    fn spring_annotation_on_own_line_matches() {
        // Spring annotations on their own lines, with the class-level
        // @RequestMapping also on its own line above the class.
        let source = "@RestController\n\
                      @RequestMapping(\"/v1/approvals\")\n\
                      public class ApprovalsController {\n  \
                      @PostMapping(\"/submit\")\n  \
                      public void submit() {}\n\
                      }\n";
        let symbols = vec![HandlerSymbol {
            name: "submit".into(),
            signature: "public void submit() {}".into(),
            start_line: 5,
        }];
        let matches = detect_handlers("spring", source, &symbols);
        assert_eq!(matches.len(), 1, "matches: {matches:?}");
        assert_eq!(
            matches[0].contract.uid(),
            "contract:http:POST:/v1/approvals/submit"
        );
        assert_eq!(matches[0].confidence, 1.0);
    }

    #[test]
    fn preceding_scan_stops_at_block_boundary() {
        // A method with NO decorator must not steal the decorator of the
        // sibling method declared above it (block boundary `}` stops the scan).
        let source = "@Controller('v1')\n\
                      export class C {\n  \
                      @Post('a')\n  \
                      a() { return {}; }\n  \
                      b() { return {}; }\n\
                      }\n";
        let symbols = vec![
            HandlerSymbol {
                name: "a".into(),
                signature: "a() { return {}; }".into(),
                start_line: 4,
            },
            HandlerSymbol {
                name: "b".into(),
                signature: "b() { return {}; }".into(),
                start_line: 5,
            },
        ];
        let matches = detect_handlers("nestjs", source, &symbols);
        // Only `a` should match; `b` has no route decorator above it.
        assert_eq!(matches.len(), 1, "matches: {matches:?}");
        assert_eq!(matches[0].symbol_index, 0);
    }

    #[test]
    fn non_framework_yields_no_handlers() {
        let symbols = vec![HandlerSymbol {
            name: "helper".into(),
            signature: "public void helper()".into(),
            start_line: 0,
        }];
        assert!(detect_handlers("none", "class Helper", &symbols).is_empty());
    }

    #[test]
    fn handler_matches_spec_contract_uid() {
        // The keystone invariant: a spec-declared contract and the handler
        // that serves it mint the *same* UID, so IMPLEMENTS_CONTRACT links up.
        let spec = r#"
openapi: 3.0.0
info: { title: t, version: "1.0" }
paths:
  /v1/approvals/{id}:
    get:
      responses: { "200": { description: ok } }
"#;
        let declared = parse_spec_file("openapi.yaml", spec);
        let class_sig = "@RestController @RequestMapping(\"/v1/approvals\") class C";
        let handler_syms = vec![HandlerSymbol {
            name: "get".into(),
            signature: "@GetMapping(\"/:id\") Approval get(String id)".into(),
            start_line: 0,
        }];
        let handlers = detect_handlers("spring", class_sig, &handler_syms);
        assert_eq!(declared.len(), 1);
        assert_eq!(handlers.len(), 1);
        assert_eq!(
            declared[0].uid(),
            handlers[0].contract.uid(),
            "spec and handler must agree on UID"
        );
    }

    fn http_contract(verb: &str, path: &str, src: &str, conf: f32) -> Contract {
        SpecContract {
            kind: "http".into(),
            verb: Some(verb.into()),
            path: Some(path.into()),
            operation_id: None,
        }
        .into_contract("repo-1", src, conf)
    }

    #[test]
    fn drift_finds_both_directions() {
        let declared = vec![
            http_contract("POST", "/v1/approvals", "openapi.yaml", 1.0),
            http_contract("GET", "/v1/approvals/{}", "openapi.yaml", 1.0),
        ];
        // Only the POST is implemented.
        let mut implemented = HashSet::new();
        implemented.insert(declared[0].uid.clone());
        // A handler implements an undeclared DELETE route (code-derived).
        let code_derived = vec![http_contract(
            "DELETE",
            "/v1/approvals/{}",
            "ApprovalsController.java",
            1.0,
        )];

        let report = compute_drift(&declared, &code_derived, &implemented);
        assert_eq!(report.declared_not_implemented.len(), 1);
        assert_eq!(
            report.declared_not_implemented[0].uid,
            "contract:http:GET:/v1/approvals/{}"
        );
        assert_eq!(report.implemented_not_declared.len(), 1);
        assert_eq!(
            report.implemented_not_declared[0].uid,
            "contract:http:DELETE:/v1/approvals/{}"
        );
        assert!(!report.is_clean());
    }

    #[test]
    fn drift_clean_when_all_match() {
        let declared = vec![http_contract("POST", "/v1/approvals", "openapi.yaml", 1.0)];
        let mut implemented = HashSet::new();
        implemented.insert(declared[0].uid.clone());
        // code-derived contract whose UID equals a declared one → not drift.
        let code_derived = vec![http_contract("POST", "/v1/approvals", "C.java", 1.0)];
        let report = compute_drift(&declared, &code_derived, &implemented);
        assert!(report.is_clean(), "report: {report:?}");
    }

    #[test]
    fn language_string_mapping() {
        assert_eq!(framework_language_str(Language::Java), Some("java"));
        assert_eq!(
            framework_language_str(Language::TypeScript),
            Some("typescript")
        );
        assert_eq!(framework_language_str(Language::Rust), None);
    }
}
