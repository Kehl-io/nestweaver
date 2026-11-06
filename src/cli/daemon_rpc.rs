//! Daemon RPC routing, hybrid (daemon/upstream/direct) fallbacks and their payload helpers.
//!
//! Moved verbatim out of `src/main.rs` (nw-348).

use crate::*;

/// The daemon's own application-level message, when a failed RPC was
/// *answered* rather than unreachable.
///
/// nw-170: try_hybrid_json_rpc_checked treated every `hybrid.query` error as a
/// transport or config failure, so a typo'd note title came back as "refusing
/// direct fallback ... deliberately reset with `nestweaver daemon start
/// --reset`" and exit 1. `backlinks "Home"`, `cross-repo-contracts note_get`
/// and `project-context zzz-nonexistent` all told the user their daemon was
/// wedged when it had answered perfectly.
///
/// The presence of a `tonic::Status` in the error chain is the discriminator:
/// the daemon received the call and replied. `Unavailable` is excluded because
/// that is precisely the code for "I cannot serve this", and `DeadlineExceeded`
/// because a cancelled query genuinely may be worth retrying elsewhere.
///
/// The `tool <name> failed: ` prefix that `tool_error` adds is stripped, since
/// the caller already knows which command it ran.
pub(crate) fn daemon_application_error(error: &anyhow::Error) -> Option<String> {
    let status = error.chain().find_map(|cause| {
        cause.downcast_ref::<tonic::Status>().filter(|status| {
            !matches!(
                status.code(),
                tonic::Code::Unavailable | tonic::Code::DeadlineExceeded
            )
        })
    })?;
    let message = status.message();
    let message = message
        .split_once(" failed: ")
        .filter(|(head, _)| head.starts_with("tool "))
        .map_or(message, |(_, tail)| tail);
    (!message.is_empty()).then(|| message.to_string())
}

/// A running daemon's "manifest suggestions are not ready" answer, rendered
/// for the operator, or `None` for any other failure.
///
/// nw-700 (7) / nw-680: `suggest_links` answers a stale or owed manifest with
/// gRPC `Unavailable` and a JSON body (`error`, `rebuild`). `Unavailable` is
/// otherwise the transport's "I cannot serve this" code, so the CLI read the
/// daemon's ANSWER as the daemon being down and told the user to start a
/// daemon that was already running. The body is recognized by its manifest
/// error code, never by prose, and the remedy is the recovery runtime's own.
pub(crate) fn manifest_unavailable_answer(error: &anyhow::Error) -> Option<String> {
    let status = error.chain().find_map(|cause| {
        cause
            .downcast_ref::<tonic::Status>()
            .filter(|status| status.code() == tonic::Code::Unavailable)
    })?;
    let body: serde_json::Value = serde_json::from_str(status.message()).ok()?;
    let problem = body.get("error")?;
    let code = problem.get("code")?.as_str()?;
    if !matches!(
        code,
        "manifest_unavailable" | "manifest_temporarily_unavailable"
    ) {
        return None;
    }
    let text = |value: &serde_json::Value, key: &str| {
        value
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let rebuild = body.get("rebuild");
    let state = rebuild
        .map(|rebuild| text(rebuild, "state"))
        .filter(|state| !state.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    let rebuild_error = rebuild
        .and_then(|rebuild| rebuild.get("error"))
        .map(|error| text(error, "message"))
        .filter(|message| !message.is_empty());
    let mut message = format!(
        "manifest suggestions are not available yet: {}. The running daemon rebuilds them \
         itself (rebuild state: {state})",
        text(problem, "message")
    );
    if let Some(rebuild_error) = rebuild_error {
        message.push_str(&format!("; its last attempt failed: {rebuild_error}"));
    }
    message.push_str(". Retry once `nestweaver brain status` reports the manifest ready.");
    Some(message)
}

/// Env knob for the client-side RPC ceiling, in seconds. `0` disables it.
pub(crate) const RPC_TIMEOUT_ENV: &str = "NESTWEAVER_RPC_TIMEOUT_SECS";

/// Default client-side ceiling on a daemon RPC.
///
/// Deliberately generous. This is a backstop against a wedged or
/// indefinitely-blocked daemon, not a latency target — a value tight enough to
/// be a performance guard would turn slow-but-succeeding queries on a large
/// graph into failures.
pub(crate) const RPC_TIMEOUT_DEFAULT: std::time::Duration = std::time::Duration::from_secs(300);

/// How long the CLI will wait for a daemon RPC before giving up.
///
/// nw-162: there was no client-side bound at all. `--max-millis` is enforced
/// SERVER-side and does not bound the client's wall clock, so a daemon that
/// stopped responding parked the CLI in `Runtime::block_on` indefinitely — one
/// observed `regex-search` ran 8m38s before being killed, with every tokio
/// worker idle awaiting a response that never arrived.
///
/// When the caller passed `--max-millis`, the server has been asked to answer
/// within that budget, so the client waits that long plus a margin for
/// transport and scheduling; anything beyond means the daemon is not honouring
/// its own deadline. Otherwise the generous default applies.
pub(crate) fn daemon_rpc_timeout(args: &serde_json::Value) -> Option<std::time::Duration> {
    if let Some(seconds) = nestweaver_schema::env::env_parse::<u64>(RPC_TIMEOUT_ENV) {
        // An explicit 0 opts out entirely, for debugging a slow daemon.
        return (seconds > 0).then(|| std::time::Duration::from_secs(seconds));
    }
    let budget = args
        .get("max_millis")
        .and_then(serde_json::Value::as_u64)
        .map(|ms| {
            std::time::Duration::from_millis(ms).saturating_add(std::time::Duration::from_secs(30))
        });
    Some(budget.unwrap_or(RPC_TIMEOUT_DEFAULT))
}

/// Matches MCP `MAX_IDENTIFIER_LEN`. A 10k-character `--repo` used to leave
/// the CLI as gRPC PROTOCOL_ERROR rather than an honest client rejection.
pub(crate) const MAX_REPO_SELECTOR_LEN: usize = 512;

pub(crate) fn reject_oversized_repo_selectors(repos: &[String]) -> Result<(), (i32, String)> {
    for (index, selector) in repos.iter().enumerate() {
        if selector.len() > MAX_REPO_SELECTOR_LEN {
            return Err((
                EXIT_USAGE,
                format!(
                    "--repo[{index}] is {} bytes; the maximum is {MAX_REPO_SELECTOR_LEN}. \
                     The request is REJECTED rather than forwarded: an over-long selector \
                     previously failed as PROTOCOL_ERROR instead of an honest client error.",
                    selector.len()
                ),
            ));
        }
    }
    Ok(())
}

pub(crate) fn error_is_unresolved_repo_filter(error: &anyhow::Error) -> bool {
    if error
        .chain()
        .any(|cause| cause.is::<nestweaver_engine::node_scope::RepoFilterUnresolved>())
    {
        return true;
    }
    let stamped = error.chain().find_map(|cause| {
        cause.downcast_ref::<tonic::Status>().and_then(|status| {
            status
                .metadata()
                .get(nestweaver_engine::node_scope::NW_ERROR_CODE_METADATA_KEY)
                .and_then(|value| value.to_str().ok().map(str::to_string))
        })
    });
    if stamped.as_deref() == Some(nestweaver_engine::node_scope::REPO_FILTER_UNRESOLVED_CODE) {
        return true;
    }
    format!("{error:#}").contains("repo filter entry ")
}

/// Whether `error` is a tool-argument schema violation (nw-660).
///
/// Typed on the direct route (`ToolArgumentsInvalid` is in the chain). Across
/// gRPC the type does not survive, so the daemon stamps
/// [`nestweaver_mcp::tools::TOOL_ARGUMENTS_INVALID_CODE`] into the status
/// metadata, the same way nw-443 carries `RepoFilterUnresolved`. Both checks
/// key on the type or code, never on the message, so a genuine internal
/// failure that merely quotes an argument cannot be reclassified.
pub(crate) fn error_is_invalid_tool_arguments(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.is::<nestweaver_mcp::tools::ToolArgumentsInvalid>()
            || cause.downcast_ref::<tonic::Status>().is_some_and(|status| {
                status.code() == tonic::Code::InvalidArgument
                    && status
                        .metadata()
                        .get(nestweaver_engine::node_scope::NW_ERROR_CODE_METADATA_KEY)
                        .and_then(|value| value.to_str().ok())
                        == Some(nestweaver_mcp::tools::TOOL_ARGUMENTS_INVALID_CODE)
            })
    })
}

/// The `{status, error, repo, message[, candidates]}` envelope of an
/// unresolved repo filter: from the typed error on the direct route, or from
/// the status details the daemon attaches to it.
pub(crate) fn unresolved_repo_filter_envelope(error: &anyhow::Error) -> Option<serde_json::Value> {
    if let Some(unresolved) = error.chain().find_map(|cause| {
        cause.downcast_ref::<nestweaver_engine::node_scope::RepoFilterUnresolved>()
    }) {
        return Some(unresolved.envelope());
    }
    error.chain().find_map(|cause| {
        let status = cause.downcast_ref::<tonic::Status>()?;
        let envelope: serde_json::Value = serde_json::from_slice(status.details()).ok()?;
        envelope
            .get("status")
            .and_then(|v| v.as_str())
            .and_then(nestweaver_engine::RepoSelectorFailure::from_status)
            .map(|_| envelope)
    })
}

/// Report an unresolved repo filter and return its exit code: not found 2,
/// ambiguous 3 (the envelope lists the candidates), malformed 64.
pub(crate) fn report_unresolved_repo_filter(error: &anyhow::Error, json: bool) -> i32 {
    let mut envelope = unresolved_repo_filter_envelope(error).unwrap_or_else(|| {
        // An older daemon sends neither the type nor the details; its
        // message still names an ambiguous selector.
        let message = format!("{error:#}");
        let status = if message.to_ascii_lowercase().contains("ambiguous") {
            "ambiguous"
        } else {
            "not_found"
        };
        serde_json::json!({ "status": status, "message": message })
    });
    let failure = envelope
        .get("status")
        .and_then(|v| v.as_str())
        .and_then(nestweaver_engine::RepoSelectorFailure::from_status)
        .unwrap_or(nestweaver_engine::RepoSelectorFailure::NotFound);
    let code = match failure {
        nestweaver_engine::RepoSelectorFailure::NotFound => EXIT_NOT_FOUND,
        nestweaver_engine::RepoSelectorFailure::Ambiguous => EXIT_AMBIGUOUS,
        nestweaver_engine::RepoSelectorFailure::Malformed => EXIT_USAGE,
    };
    // `error` is the machine word on the CLI. 12.1.1 (B6): a miss reads
    // "not found", the word every other not-found envelope uses (`symbol`,
    // `impact`, `flow-trace`); `status` keeps the class (`not_found`).
    envelope["error"] = serde_json::json!(match failure {
        nestweaver_engine::RepoSelectorFailure::NotFound => "not found",
        _ => failure.status(),
    });
    let message = envelope
        .get("message")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{error:#}"));
    if json {
        println!("{envelope}");
    }
    eprintln!("{message}");
    code
}

/// Only initial transport failures may use configured remote reads. Identity,
/// configuration, restart, and unknown startup failures must stay failures.
pub(crate) fn initial_daemon_transport_unavailable(error: &anyhow::Error) -> bool {
    let context = error.to_string();
    let initial_transport = context.starts_with("failed to connect to daemon at ")
        || context == "health check failed"
        || context == "health check timed out — daemon connected but unresponsive";
    if !initial_transport
        || error.chain().any(|cause| {
            cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied)
                || cause.downcast_ref::<tonic::Status>().is_some_and(|status| {
                    matches!(
                        status.code(),
                        tonic::Code::PermissionDenied | tonic::Code::Unauthenticated
                    )
                })
        })
    {
        return false;
    }
    // A transport wrapper can hide EACCES or a policy error. Only a known
    // terminal connection failure/timeout grants remote fallback; opaque
    // wrappers and unknown leaves remain failures.
    let cause = error.root_cause();
    cause
        .downcast_ref::<tokio::time::error::Elapsed>()
        .is_some()
        || cause.downcast_ref::<tonic::Status>().is_some_and(|status| {
            matches!(
                status.code(),
                tonic::Code::Unavailable | tonic::Code::DeadlineExceeded
            )
        })
        || cause.downcast_ref::<std::io::Error>().is_some_and(|error| {
            matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::NotFound
            )
        })
}

pub(crate) fn validated_upstream_fallback_config(
    db_path: &Path,
    config: Option<&Path>,
    connect_error: &anyhow::Error,
) -> anyhow::Result<Option<nestweaver_client::RestartConfig>> {
    // Validate persisted intent even when the connection error is opaque;
    // discovery intentionally tolerates unreadable files and is not a guard.
    let effective = nestweaver_client::RestartConfig::for_automatic_cold_start(db_path, config)?;
    Ok(initial_daemon_transport_unavailable(connect_error).then_some(effective))
}

#[derive(Debug)]
pub(crate) struct IncompleteIndexRefusal(pub serde_json::Value);

impl std::fmt::Display for IncompleteIndexRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Repository indexing did not complete; call-graph completeness is unavailable. Re-index the affected repositories before retrying.")
    }
}
impl std::error::Error for IncompleteIndexRefusal {}

fn contains_incomplete_index_refusal(value: &serde_json::Value) -> bool {
    if value.get("refused").and_then(serde_json::Value::as_bool) == Some(true)
        && value.get("reason").and_then(serde_json::Value::as_str) == Some("repo_index_incomplete")
    {
        return true;
    }
    // These are response-envelope fields, never source/property contents.
    [
        "local_impact",
        "org_wide_impact",
        "results",
        "result",
        "structuredContent",
    ]
    .iter()
    .filter_map(|key| value.get(*key))
    .any(|nested| {
        nested.as_array().map_or_else(
            || contains_incomplete_index_refusal(nested),
            |items| items.iter().any(contains_incomplete_index_refusal),
        )
    })
}

pub(crate) fn reject_incomplete_index_response(
    value: serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    if contains_incomplete_index_refusal(&value) {
        return Err(IncompleteIndexRefusal(value).into());
    }
    Ok(value)
}

pub(crate) fn try_hybrid_json_rpc_checked(
    use_daemon: bool,
    db_path: &std::path::Path,
    config: Option<&std::path::Path>,
    rpc_name: &str,
    args: serde_json::Value,
) -> anyhow::Result<Option<serde_json::Value>> {
    try_hybrid_json_rpc_checked_inner(use_daemon, db_path, config, rpc_name, args)?
        .map(reject_incomplete_index_response)
        .transpose()
}

fn try_hybrid_json_rpc_checked_inner(
    use_daemon: bool,
    db_path: &std::path::Path,
    config: Option<&std::path::Path>,
    rpc_name: &str,
    args: serde_json::Value,
) -> anyhow::Result<Option<serde_json::Value>> {
    if !use_daemon {
        return Ok(None);
    }
    let rt = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            ensure_direct_store_fallback_allowed(db_path, config).with_context(|| {
                format!(
                    "create runtime for daemon query {rpc_name} failed ({error}); refusing direct fallback"
                )
            })?;
            return Ok(None);
        }
    };
    let start_dir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    // nw-087: a read/query against a NONEXISTENT local db must not autostart a
    // daemon that CREATES an empty store — that turns a typo'd `--db` path into a
    // silent "0 results / status: complete" success (false-green in CI). Skip the
    // local connect when the db file is absent; still try configured upstreams
    // (federated read), else report `db_not_found` without a direct store open. `index` creates dbs and does NOT route
    // through here, so it is unaffected.
    // nw-309: refuse an exists-but-not-a-database `--db` HERE, before the
    // dial. This is the one funnel every daemon-routed read passes through, so
    // one check turns a 30s boot-ceiling stall into an immediate, accurate
    // error for the whole read surface rather than for the one command that
    // was reported.
    // nw-779: admission asks the publication selector, not `Path::exists`. A
    // published brain has no file at its logical anchor, so the raw test sent
    // `symbol`, `list-services`, `affected-tests` and `investigate` down the
    // absent arm below and they reported a healthy database as not found.
    // A `CURRENT` that cannot be resolved is an error here, never "absent".
    let local_db = selected_existing_database(db_path)?;
    if let Some(local_db) = &local_db {
        require_openable_db(local_db)?;
    }
    // Absent still routes to configured upstreams (federated read) rather
    // than failing outright.
    let Some(local_db) = local_db else {
        let effective = nestweaver_client::RestartConfig::for_automatic_cold_start(db_path, config)
            .context("load explicit instance config or persisted intent before missing-DB upstream routing")?;
        let config = effective.as_path();
        if let Some(mut hybrid) = rt.block_on(
            nestweaver_client::hybrid::HybridClient::connect_existing_verified_read(
                db_path, config, &start_dir,
            ),
        )? {
            // A verified live replica serves reads even when its logical
            // anchor has no file. Keep this client; never autostart or fall
            // through to a direct store after any query failure.
            return rt
                .block_on(async {
                    match daemon_rpc_timeout(&args) {
                        Some(budget) => tokio::time::timeout(budget, hybrid.query(rpc_name, &args))
                            .await
                            .context("existing daemon read timed out; no daemon was started")?,
                        None => hybrid.query(rpc_name, &args).await,
                    }
                })
                .map(Some)
                .with_context(|| {
                    format!("existing daemon query {rpc_name} failed; refusing direct fallback")
                });
        }
        let discovered =
            nestweaver_client::discovery::discover_upstreams_with_config(&start_dir, config);
        if discovered.is_empty() {
            require_existing_db(db_path)?;
            unreachable!("missing database must return its typed diagnostic");
        }
        return match rt.block_on(nestweaver_client::hybrid::query_configured_upstreams_only(
            config, &start_dir, rpc_name, &args,
        )) {
            Ok(value) => Ok(Some(value)),
            Err(error) if config.is_some() => Err(error).with_context(|| {
                format!(
                    "explicit-config upstream query {rpc_name} failed; refusing direct fallback"
                )
            }),
            Err(error) => {
                Err(error).context("configured upstream read failed; refusing direct fallback")
            }
        };
    };
    // For a published brain named by its anchor this is the selected slot, the
    // same path `resolve_db_with_config` hands the commands that already
    // worked; the client recovers the logical instance from it
    // (`instance_anchor_database`). A path that exists is passed on unchanged.
    let db_path = local_db.as_path();
    match rt.block_on(nestweaver_client::hybrid::HybridClient::connect(
        db_path, config, &start_dir,
    )) {
        Ok(mut hybrid) => match rt.block_on(async {
            match daemon_rpc_timeout(&args) {
                Some(budget) => tokio::time::timeout(budget, hybrid.query(rpc_name, &args))
                    .await
                    .unwrap_or_else(|_| {
                        Err(anyhow::anyhow!(
                            "daemon did not answer {rpc_name} within {}s; it may be wedged or \
                             saturated. Raise or disable the ceiling with {RPC_TIMEOUT_ENV} \
                             (0 disables), and see `nestweaver daemon --db {} status`.",
                            budget.as_secs(),
                            db_path.display()
                        ))
                    }),
                None => hybrid.query(rpc_name, &args).await,
            }
        }) {
            Ok(value) => Ok(Some(value)),
            Err(e) => {
                // nw-700 (7): an owed manifest is the daemon's ANSWER, sent as
                // `Unavailable`; it must not read as the daemon being down.
                if let Some(message) = manifest_unavailable_answer(&e) {
                    return Err(anyhow::anyhow!(message));
                }
                // nw-170: the daemon answered — "no note found with title
                // 'Home'" is a valid answer, not a daemon failure. There is
                // nothing to fall back FROM, so surface it as-is instead of
                // recommending a daemon reset.
                if let Some(message) = daemon_application_error(&e) {
                    return Err(e.context(message));
                }
                ensure_direct_store_fallback_allowed(db_path, config).with_context(|| {
                    format!("hybrid query {rpc_name} failed ({e:#}); refusing direct fallback")
                })?;
                warn_daemon_bypassed(db_path, rpc_name, &format!("{e:#}"));
                Ok(None)
            }
        },
        Err(e) => {
            if let Some(effective) = validated_upstream_fallback_config(db_path, config, &e)? {
                let config = effective.as_path();
                if !nestweaver_client::discovery::discover_upstreams_with_config(&start_dir, config)
                    .is_empty()
                {
                    return rt
                        .block_on(nestweaver_client::hybrid::query_configured_upstreams_only(
                            config, &start_dir, rpc_name, &args,
                        ))
                        .map(Some)
                        .with_context(|| {
                            format!("daemon unavailable ({e:#}); upstream query {rpc_name} failed")
                        });
                }
            }
            ensure_direct_store_fallback_allowed(db_path, config).with_context(|| {
                format!("daemon query {rpc_name} unavailable ({e:#}); refusing direct fallback")
            })?;
            unreachable!("normal reads cannot fall back to direct store")
        }
    }
}

/// A daemon RPC that must stay on THIS machine: the local daemon or nothing.
///
/// nw-690 review (B1): [`try_hybrid_json_rpc_checked`] routes by the
/// federation matrix, so a Merge tool's params reach every configured
/// upstream, and with no daemon it queries the upstreams alone. A request
/// carrying the caller's own data (`generate-guide --config/--rules-from`)
/// takes this route instead. `Ok(None)` only on the CI direct route; with the
/// daemon unreachable it refuses, naming why no upstream was asked.
pub(crate) fn try_local_daemon_json_rpc(
    db_path: &std::path::Path,
    config: Option<&std::path::Path>,
    rpc_name: &str,
    args: serde_json::Value,
) -> anyhow::Result<Option<serde_json::Value>> {
    require_existing_db(db_path)?;
    let local_only_reason = || {
        format!(
            "{rpc_name} with a caller's config or rules is served by the local daemon only and \
             is never sent to an upstream"
        )
    };
    let rt = tokio::runtime::Runtime::new()
        .with_context(|| format!("create runtime for local daemon query {rpc_name}"))?;
    let client = match rt.block_on(nestweaver_client::DaemonClient::connect(db_path, config)) {
        Ok(client) => client,
        Err(error) => {
            ensure_direct_store_fallback_allowed(db_path, config).with_context(|| {
                format!(
                    "daemon unavailable ({error:#}); {}; refusing direct fallback",
                    local_only_reason()
                )
            })?;
            return Ok(None);
        }
    };
    let mut local = nestweaver_client::hybrid::HybridClient::local_only(client);
    let answer = rt.block_on(async {
        match daemon_rpc_timeout(&args) {
            Some(budget) => tokio::time::timeout(budget, local.query_local_only(rpc_name, &args))
                .await
                .unwrap_or_else(|_| {
                    Err(anyhow::anyhow!(
                        "daemon did not answer {rpc_name} within {}s; raise or disable the \
                         ceiling with {RPC_TIMEOUT_ENV} (0 disables)",
                        budget.as_secs()
                    ))
                }),
            None => local.query_local_only(rpc_name, &args).await,
        }
    });
    match answer {
        Ok(value) => Ok(Some(value)),
        Err(error) => match daemon_application_error(&error) {
            Some(message) => Err(error.context(message)),
            None => Err(error.context(local_only_reason())),
        },
    }
}

/// Configless compatibility wrapper. Callers with an explicit config must use
/// [`try_hybrid_json_rpc_checked`] and propagate its error.
pub(crate) fn try_hybrid_json_rpc(
    use_daemon: bool,
    db_path: &std::path::Path,
    config: Option<&std::path::Path>,
    rpc_name: &str,
    args: serde_json::Value,
) -> anyhow::Result<Option<serde_json::Value>> {
    debug_assert!(
        !use_daemon || config.is_none(),
        "explicit-config callers must use try_hybrid_json_rpc_checked"
    );
    try_hybrid_json_rpc_checked(use_daemon, db_path, config, rpc_name, args)
}

pub(crate) fn hybrid_source_label(value: &serde_json::Value) -> &'static str {
    if value
        .get("_meta")
        .and_then(|meta| meta.get("sources"))
        .and_then(|sources| sources.as_array())
        .is_some_and(|sources| {
            sources
                .iter()
                .any(|source| source.as_str() == Some("server"))
        })
    {
        "daemon+hybrid"
    } else {
        "daemon"
    }
}

/// Disclose that a command could not be served by the daemon and is about to be
/// answered by the direct path instead.
///
/// nw-125: this fallback was completely silent — exit 0, nothing on stderr, a
/// result that looks authoritative. Meanwhile asking for the same bypass
/// explicitly is REFUSED, with a warning about WAL corruption and a demand for
/// `NESTWEAVER_ALLOW_NO_DAEMON=1`. The tool policed a deliberate request while
/// doing the same thing unprompted whenever the daemon was slow or down.
///
/// That matters beyond consistency: the direct path is not equivalent. It
/// returns different rankings for `investigate` (nw-120), answers `brain
/// status` with every daemon-runtime field nulled (disclosed in-band via
/// `degraded_components` and a `daemon_bypassed` warning), and — the case that
/// produced nw-126 — opens read-only, so it cannot replay a crashed daemon's
/// WAL and converts a self-healing outage into a hard failure.
///
/// Warn once per process: a single command can route several RPCs through here,
/// and repeating the paragraph per call would train the user to ignore it. The
/// cause is recorded on EVERY call (see [`last_daemon_bypass_cause`]) so the
/// direct path's structured disclosure can carry it even when the print was
/// suppressed.
pub(crate) fn warn_daemon_bypassed(db_path: &std::path::Path, rpc_name: &str, cause: &str) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static WARNED: AtomicBool = AtomicBool::new(false);
    record_daemon_bypass_cause(cause);
    if WARNED.swap(true, Ordering::Relaxed) {
        return;
    }
    // The in-band markers exist ONLY on the `brain status` direct path — do
    // not claim them for other RPCs, or a `2>/dev/null` consumer will hunt
    // stdout for markers that are not there and read their absence as "not
    // degraded".
    let marker_note = if rpc_name == "brain_status" {
        " `--json` output marks the bypass in-band via `degraded_components` and a \
         `daemon_bypassed` warning."
    } else {
        ""
    };
    eprintln!(
        "Warning: the daemon could not serve `{rpc_name}` for {} — answering from the \
         read-only direct path instead.\n  cause: {cause}\n  \
         The direct answer carries no daemon-runtime state (embedding readiness, write/index \
         queue depth).{marker_note} `nestweaver daemon --db {} status` shows the daemon's \
         side, and NESTWEAVER_DAEMON_BOOT_TIMEOUT_SECS covers a daemon that is merely slow \
         to boot.",
        db_path.display(),
        db_path.display()
    );
}

/// The cause passed to the most recent [`warn_daemon_bypassed`] call. The
/// print is once-per-process, but the structured `daemon_bypassed` warning in
/// a direct-path `--json` answer needs the cause every time, so it is stashed
/// separately rather than recovered from the suppression flag.
pub(crate) static LAST_DAEMON_BYPASS_CAUSE: std::sync::Mutex<Option<String>> =
    std::sync::Mutex::new(None);

pub(crate) fn record_daemon_bypass_cause(cause: &str) {
    if let Ok(mut slot) = LAST_DAEMON_BYPASS_CAUSE.lock() {
        *slot = Some(cause.to_string());
    }
}

/// The most recent daemon-bypass cause recorded by [`warn_daemon_bypassed`],
/// for the direct path's in-band disclosure.
pub(crate) fn last_daemon_bypass_cause() -> Option<String> {
    LAST_DAEMON_BYPASS_CAUSE.lock().ok()?.clone()
}

/// Unwrap the `{ "results": [...], "_meta": {...} }` envelope that the hybrid
/// JSON-RPC path wraps around bare-array tool results, returning the inner
/// `results` value. Bare (already-unwrapped) values — e.g. a local-daemon
/// response — pass through unchanged, so every CLI consumer that deserializes a
/// list result can route through this regardless of which path produced it.
pub(crate) fn unwrap_hybrid_payload(value: serde_json::Value) -> serde_json::Value {
    value.get("results").cloned().unwrap_or(value)
}

/// Drop the hybrid provenance `_meta` key from a daemon/hybrid object
/// response so the CLI can deserialize/print the same shape the direct path
/// produces (the direct store result has no `_meta`).
pub(crate) fn strip_hybrid_meta(mut value: serde_json::Value) -> serde_json::Value {
    if let Some(obj) = value.as_object_mut() {
        obj.remove("_meta");
    }
    value
}

/// Rebuild the direct path's `CommunityInfo` from a `clusters` tool
/// response entry, which uses `size` where the CLI/sidecar schema uses
/// `member_count`. Serializing the real struct keeps daemon --json output
/// byte-identical to the direct path (struct field order, not map order).
pub(crate) fn community_info_from_tool_json(
    c: &serde_json::Value,
) -> Option<nestweaver_engine::CommunityInfo> {
    Some(nestweaver_engine::CommunityInfo {
        id: c.get("id")?.as_u64()? as u32,
        name: c.get("name")?.as_str()?.to_string(),
        cohesion: c.get("cohesion")?.as_f64()?,
        member_count: c.get("size")?.as_u64()? as usize,
        members: serde_json::from_value(c.get("members")?.clone()).ok()?,
        key_files: serde_json::from_value(c.get("key_files")?.clone()).ok()?,
    })
}

/// Rebuild the direct path's `PatternCount` from a `count_patterns`
/// tool payload entry (`PatternCount` is Serialize-only, so this is manual —
/// same approach as [`community_info_from_tool_json`]). Serializing the real
/// struct keeps daemon --json output byte-identical to the direct path
/// (struct field order, not map order). `stale_index` defaults to false for
/// daemons that predate the field.
pub(crate) fn pattern_count_from_tool_json(
    c: &serde_json::Value,
) -> Option<nestweaver_store::regex::PatternCount> {
    Some(nestweaver_store::regex::PatternCount {
        pattern: c.get("pattern")?.as_str()?.to_string(),
        total_matches: c.get("total_matches")?.as_u64()?,
        files_matched: c.get("files_matched")?.as_u64()?,
        top_files: c
            .get("top_files")?
            .as_array()?
            .iter()
            .map(|f| {
                Some(nestweaver_store::regex::FileCount {
                    path: f.get("path")?.as_str()?.to_string(),
                    count: f.get("count")?.as_u64()?,
                })
            })
            .collect::<Option<Vec<_>>>()?,
        stale_index: c
            .get("stale_index")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        ready_scopes: c
            .get("ready_scopes")
            .and_then(|value| value.as_u64())
            .unwrap_or(0) as usize,
        dirty_scopes: c
            .get("dirty_scopes")
            .and_then(|value| value.as_u64())
            .unwrap_or(0) as usize,
        error_scopes: c
            .get("error_scopes")
            .and_then(|value| value.as_u64())
            .unwrap_or(0) as usize,
        posting_hits: c
            .get("posting_hits")
            .and_then(|value| value.as_u64())
            .unwrap_or(0) as usize,
        hydrated_candidates: c
            .get("hydrated_candidates")
            .and_then(|value| value.as_u64())
            .unwrap_or(0) as usize,
        scanned_candidates: c
            .get("scanned_candidates")
            .and_then(|value| value.as_u64())
            .unwrap_or(0) as usize,
        timings: c
            .get("timings")
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default(),
        truncated: c
            .get("truncated")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
        truncation_reason: c
            .get("truncation_reason")
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok()),
    })
}

/// `count-patterns`' arguments for the daemon's `count_patterns` tool.
///
/// `max_millis` is the tool's whole-call budget, and [`daemon_rpc_timeout`]
/// reads the same key so the client waits that long plus a margin instead of
/// giving up on a budget the caller raised.
pub(crate) fn count_patterns_rpc_args(
    patterns: &[String],
    path_prefix: Option<&str>,
    kinds: Option<&[String]>,
    max_millis: Option<u64>,
) -> serde_json::Value {
    let mut args = serde_json::json!({ "patterns": patterns });
    if let Some(prefix) = path_prefix {
        args["path_prefix"] = serde_json::json!(prefix);
    }
    if let Some(kinds) = kinds {
        args["kinds"] = serde_json::json!(kinds);
    }
    if let Some(max_millis) = max_millis {
        args["max_millis"] = serde_json::json!(max_millis);
    }
    args
}

/// The stderr warning for a `count-patterns` result whose budget ran out,
/// or `None` when every count is exact. Written in `--json` mode too: the
/// array on stdout carries `truncated` per pattern, but a script that only
/// checks for a zero exit must not read lower bounds as answers.
pub(crate) fn pattern_counts_truncation_warning(
    counts: &[nestweaver_store::regex::PatternCount],
    max_millis: Option<u64>,
) -> Option<String> {
    let cut = counts.iter().filter(|count| count.truncated).count();
    if cut == 0 {
        return None;
    }
    let budget = max_millis.unwrap_or(nestweaver_store::regex::COUNT_PATTERNS_DEFAULT_MAX_MILLIS);
    Some(format!(
        "warning: INCOMPLETE COUNTS — the {budget} ms time budget ran out, so {cut} of {} \
         pattern(s) report lower bounds, not totals. Raise --max-millis (up to 600000), \
         narrow --path-prefix/--kinds, or pass fewer patterns. Exit code 75.",
        counts.len()
    ))
}

/// Render `count-patterns` results, identically for the daemon and direct
/// routes. Returns `true` when any count is truncated (a lower bound), which
/// the caller turns into exit 75.
pub(crate) fn print_pattern_counts(
    counts: &[nestweaver_store::regex::PatternCount],
    json: bool,
    max_millis: Option<u64>,
) -> anyhow::Result<bool> {
    if json {
        println!("{}", serde_json::to_string_pretty(counts)?);
    } else {
        for c in counts {
            println!(
                "'{}': {} match(es) across {} file(s)",
                c.pattern, c.total_matches, c.files_matched
            );
            for f in &c.top_files {
                println!("    {} ({})", f.path, f.count);
            }
            println!(
                "    regex: {} ready / {} scanned / {} errors; {} posting hits, {} hydrated, {} verified; {} ms",
                c.ready_scopes,
                c.dirty_scopes,
                c.error_scopes,
                c.posting_hits,
                c.hydrated_candidates,
                c.scanned_candidates,
                c.timings.total_ms,
            );
            if c.truncated {
                println!(
                    "    (incomplete: the time budget ran out; these counts are lower bounds — raise --max-millis)"
                );
            }
        }
        // Surface in-band staleness: the daemon's stderr warning cannot reach
        // us, and the direct path's is a once-per-process latch that may
        // already have fired inside the store.
        if counts.iter().any(|c| c.stale_index) {
            print_stale_index_note();
        }
    }
    let warning = pattern_counts_truncation_warning(counts, max_millis);
    if let Some(warning) = &warning {
        eprintln!("{warning}");
    }
    Ok(warning.is_some())
}

/// One-line staleness note rendered (text mode only) when a search
/// reports that it bypassed a stale trigram posting table. JSON mode carries
/// the signal in-band via the `stale_index` field instead.
pub(crate) fn print_stale_index_note() {
    println!(
        "(one or more trigram scopes are stale — dirty scopes were scanned safely; if this \
         persists, rebuild with `index --with-trigrams`)"
    );
}

pub(crate) fn print_regex_execution_note(res: &nestweaver_store::regex::RegexSearchResult) {
    println!(
        "(regex scopes: {} ready, {} scanned, {} errors; {} posting hits, {} hydrated, {} verified; {} ms total [plan {}, hydrate {}, verify {}])",
        res.ready_scopes,
        res.dirty_scopes,
        res.error_scopes,
        res.posting_hits,
        res.hydrated_candidates,
        res.scanned_candidates,
        res.timings.total_ms,
        res.timings.planning_ms,
        res.timings.hydration_ms,
        res.timings.verification_ms,
    );
}

/// The text renderer's column suffix for a regex hit (nw-549): `:COL` after a
/// `path:line` location, so several hits on one line read as distinct places.
/// Empty for a note location (no line) or an older daemon that sent no column.
pub(crate) fn regex_column_suffix(m: &nestweaver_store::regex::RegexMatch) -> String {
    let has_line = m
        .location
        .rsplit_once(':')
        .is_some_and(|(_, tail)| !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()));
    match m.column {
        Some(column) if has_line => format!(":{column}"),
        _ => String::new(),
    }
}

pub(crate) fn regex_truncation_label(
    reason: Option<nestweaver_store::regex::RegexTruncationReason>,
) -> &'static str {
    use nestweaver_store::regex::RegexTruncationReason;
    match reason {
        Some(RegexTruncationReason::ResultLimit) => "result limit reached",
        Some(RegexTruncationReason::Deadline) => "time budget reached",
        Some(RegexTruncationReason::CandidateCap) => "candidate safety cap reached",
        Some(RegexTruncationReason::UndecodableRows) => {
            "corpus rows could not be read and were skipped — re-index to repair"
        }
        Some(RegexTruncationReason::Unknown) | None => "search budget reached",
    }
}

/// nw-271: propagates instead of returning an empty Vec.
///
/// `unwrap_or_default()` here made a malformed daemon payload indistinguishable
/// from "no candidates matched" — a fact about this process presented as a fact
/// about the graph.
pub(crate) fn hybrid_search_candidates_from_value(
    value: serde_json::Value,
) -> anyhow::Result<Vec<nestweaver_engine::SymbolCandidate>> {
    serde_json::from_value(unwrap_hybrid_payload(value))
        .context("decode search candidates from the daemon")
}

/// Catalogue routing is independent of an established graph connection.
/// Keep the request cursor and correlation at the actual hybrid wire seam.
pub(crate) fn hybrid_catalogue_reply(
    request: &nestweaver_mcp::protocol::Request,
    lite: bool,
) -> serde_json::Value {
    let id = request.id.clone().unwrap_or(serde_json::Value::Null);
    let cursor = request
        .params
        .as_ref()
        .and_then(|params| params.get("cursor"))
        .and_then(serde_json::Value::as_str);
    match nestweaver_mcp::tools::tool_list_page(lite, cursor) {
        Ok(page) => serde_json::json!({"jsonrpc":"2.0","id":id,"result":page}),
        Err(message) => {
            serde_json::json!({"jsonrpc":"2.0","id":id,"error":{"code":-32602,"message":message}})
        }
    }
}

/// Run the MCP stdio server using HybridClient for query routing.
///
/// Read-only queries are dispatched through `HybridClient::query()` which
/// applies fallback/merge/primary routing across upstream servers. Write
/// operations (brain_add_source, brain_remove_source, prune_stale) go
/// through the standard gRPC path.
pub(crate) fn dispatch_hybrid_mcp_request(
    hybrid: &mut nestweaver_client::hybrid::HybridClient,
    rt: &tokio::runtime::Runtime,
    lite: bool,
    write_tools: &std::collections::HashSet<&str>,
    request: &nestweaver_mcp::protocol::Request,
    cancel: &nestweaver_mcp::session::CancelFlag,
) -> serde_json::Value {
    hybrid.set_delivery_profile(nestweaver_schema::ToolDeliveryProfile::BoundedMcp);
    let id = request.id.clone().unwrap_or(serde_json::Value::Null);
    match request.method.as_str() {
        "initialize" => serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": "nestweaver-brain",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            }
        }),
        "notifications/initialized" | "initialized" => serde_json::json!({
            "jsonrpc": "2.0", "id": id, "result": null
        }),
        "tools/list" => hybrid_catalogue_reply(request, lite),
        "tools/call" => {
            let params = request.params.as_ref().unwrap_or(&serde_json::Value::Null);
            let name = params
                .get("name")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            let empty = serde_json::Value::Object(serde_json::Map::new());
            let raw_arguments = params.get("arguments").unwrap_or(&empty);
            // brain_search has no argument aliases. Borrow its input through
            // validation so an oversized query is refused before any copy.
            // Other tools still fold alias spellings before federation.
            let canonical_arguments = (name != "brain_search").then(|| {
                nestweaver_mcp::tools::canonicalize_tool_arguments(name, raw_arguments.clone())
            });
            let arguments = canonical_arguments.as_ref().unwrap_or(raw_arguments);
            if name.is_empty() {
                serde_json::json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": { "code": -32602, "message": "tools/call: 'name' is required" }
                })
            } else if let Err(error) = nestweaver_mcp::tools::enforce_tool_allowed(name) {
                serde_json::json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": nestweaver_mcp::tools::wrap_tool_error(&error.to_string()),
                })
            } else if let Err(error) =
                nestweaver_mcp::tools::validate_tool_arguments(name, arguments)
            {
                serde_json::json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": nestweaver_mcp::tools::wrap_tool_error(&error.to_string()),
                })
            } else {
                // A session outlives many requests: restart the crash-attribution clock.
                nestweaver_store::daemon_exit::note_request_start();
                let dispatched = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    if write_tools.contains(name) {
                        nestweaver_mcp::tools::dispatch_via_daemon(
                            hybrid.inner_mut(),
                            rt,
                            name,
                            arguments.clone(),
                        )
                    } else {
                        rt.block_on(nestweaver_mcp::session::await_cancellable(
                            Some(cancel),
                            hybrid.query(name, arguments),
                        ))
                    }
                }));
                match dispatched {
                    Ok(Ok(result)) => serde_json::json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": nestweaver_mcp::tools::wrap_tool_result(result),
                    }),
                    Ok(Err(error)) => serde_json::json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": nestweaver_mcp::tools::wrap_tool_failure(name, &error),
                    }),
                    Err(_) => serde_json::json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": nestweaver_mcp::tools::wrap_tool_error(
                            &format!("tool '{name}' panicked")
                        ),
                    }),
                }
            }
        }
        "ping" => serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
        method => serde_json::json!({
            "jsonrpc": "2.0", "id": id,
            "error": { "code": -32601, "message": format!("method not implemented: {method}") }
        }),
    }
}

#[cfg(test)]
pub(crate) fn process_hybrid_mcp_envelope(
    parsed: serde_json::Value,
    mut dispatch: impl FnMut(&nestweaver_mcp::protocol::Request) -> serde_json::Value,
) -> Option<serde_json::Value> {
    let mut dispatch = |request: &nestweaver_mcp::protocol::Request| {
        match nestweaver_mcp::protocol::validate_method_params(request) {
            Ok(()) => dispatch(request),
            Err(message) => {
                serde_json::json!({"jsonrpc":"2.0", "id":request.id, "error":{"code":-32602,"message":message}})
            }
        }
    };
    let invalid = |error: nestweaver_mcp::protocol::InvalidRequest| {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": error.response_id,
            "error": { "code": -32600, "message": error.message }
        })
    };
    if let serde_json::Value::Array(items) = parsed {
        if items.is_empty() {
            return Some(serde_json::json!({
                "jsonrpc": "2.0", "id": null,
                "error": { "code": -32600, "message": "empty batch array" }
            }));
        }
        let mut responses = Vec::new();
        for item in items {
            match nestweaver_mcp::protocol::validate_request(item) {
                Ok(request) => {
                    let notification = request.id.is_none();
                    let result = dispatch(&request);
                    if !notification {
                        responses.push(result);
                    }
                }
                Err(error) => responses.push(invalid(error)),
            }
        }
        (!responses.is_empty()).then_some(serde_json::Value::Array(responses))
    } else {
        match nestweaver_mcp::protocol::validate_request(parsed) {
            Ok(request) => {
                let notification = request.id.is_none();
                let result = dispatch(&request);
                (!notification).then_some(result)
            }
            Err(error) => Some(invalid(error)),
        }
    }
}

pub(crate) fn run_mcp_hybrid(
    mut hybrid: nestweaver_client::hybrid::HybridClient,
    rt: tokio::runtime::Runtime,
    lite: bool,
    track_interactions: bool,
    _db_path: &Path,
) -> anyhow::Result<()> {
    hybrid.set_delivery_profile(nestweaver_schema::ToolDeliveryProfile::BoundedMcp);
    nestweaver_mcp::tools::set_lite_mode(lite);

    // Start the background maintenance task (active upstream health recovery +
    // off-hot-path staleness refresh). Tied to `hybrid`'s lifetime: it is
    // cancelled when `hybrid` drops at the end of this function. Must run
    // inside the runtime context, hence `enter()`.
    {
        let _guard = rt.enter();
        hybrid.start_maintenance();
    }

    // Interaction tracking uses the MCP crate's private record_interaction
    // helper. In hybrid mode, the HybridClient dispatches queries itself so
    // we skip interaction tracking here. Standard MCP tools/call still tracks
    // via the daemon proxy path.
    let _ = track_interactions;

    tracing::info!("brain MCP server ready on stdio (hybrid routing mode)");

    // Write tools that must bypass hybrid routing.
    //
    // Taken from `MUTATING_TOOLS`, not restated. This was a hardcoded literal
    // holding three of the six, and the three it omitted —
    // `compact_embeddings`, `set_extension`, `brain_memory_consolidate` — were
    // therefore routed as ordinary reads into `hybrid.query`, reached
    // `dispatch_json_rpc`, found no arm, and failed with "unsupported tool for
    // JSON dispatch". They were still advertised in `tools/list`, so an agent
    // with an upstream configured saw the tools, called them, and got an
    // internal-sounding error every time.
    //
    // `MUTATING_TOOLS` calls itself "the SINGLE canonical list" and every other
    // consumer — the daemon's `json_rpc!` gate, the read-only refusals in
    // tools.rs — already reads it. This was the only place that restated it,
    // and a second copy cannot be kept in sync by discipline: the tools were
    // added to the canonical list and nobody knew to add them here.
    let write_tools: std::collections::HashSet<&str> = nestweaver_mcp::http::MUTATING_TOOLS
        .iter()
        .copied()
        .collect();

    nestweaver_mcp::session::run_stdio(|request, cancel| {
        dispatch_hybrid_mcp_request(&mut hybrid, &rt, lite, &write_tools, request, cancel)
    })
}

#[cfg(test)]
mod hybrid_catalogue_tests {
    use super::*;
    use serde_json::{Value, json};

    // Compare schema positions independently of wire compaction. Defaults,
    // examples, constraints, and properties named description remain data.
    fn assert_wire_tool_contract(wire: &Value, full: &Value) {
        fn schema(wire: &Value, full: &Value) {
            let Some(expected) = full.as_object() else {
                assert_eq!(wire, full);
                return;
            };
            let actual = wire.as_object().expect("schema object");
            assert_eq!(
                actual.len(),
                expected.len()
                    - usize::from(
                        expected.contains_key("description") && !actual.contains_key("description")
                    )
            );
            for (key, value) in expected {
                if key == "description" {
                    if let Some(got) = actual.get(key) {
                        let text = got.as_str().expect("description string");
                        assert!(text.len() <= 100);
                        assert!(!text.is_empty());
                    }
                    continue;
                }
                let got = actual
                    .get(key)
                    .unwrap_or_else(|| panic!("schema constraint removed: {key}"));
                match key.as_str() {
                    "properties" | "patternProperties" | "$defs" | "definitions"
                    | "dependentSchemas" => {
                        let names = value.as_object().unwrap();
                        let observed = got.as_object().unwrap();
                        assert_eq!(observed.len(), names.len());
                        for (name, child) in names {
                            schema(
                                observed.get(name).expect("property/definition retained"),
                                child,
                            );
                        }
                    }
                    "allOf" | "anyOf" | "oneOf" | "prefixItems" => {
                        let items = value.as_array().unwrap();
                        let observed = got.as_array().unwrap();
                        assert_eq!(items.len(), observed.len());
                        for (child, expected) in observed.iter().zip(items) {
                            schema(child, expected);
                        }
                    }
                    "items"
                    | "additionalProperties"
                    | "unevaluatedProperties"
                    | "additionalItems"
                    | "not"
                    | "if"
                    | "then"
                    | "else"
                    | "contains"
                    | "propertyNames"
                    | "unevaluatedItems" => {
                        if let Some(items) = value.as_array() {
                            let observed = got.as_array().unwrap();
                            assert_eq!(observed.len(), items.len());
                            for (child, expected) in observed.iter().zip(items) {
                                schema(child, expected);
                            }
                        } else {
                            schema(got, value);
                        }
                    }
                    "dependencies" => {
                        let expected = value.as_object().unwrap();
                        let actual = got.as_object().unwrap();
                        assert_eq!(actual.len(), expected.len());
                        for (name, child) in expected {
                            let observed = actual.get(name).expect("dependency retained");
                            if child.is_object() {
                                schema(observed, child);
                            } else {
                                assert_eq!(observed, child);
                            }
                        }
                    }
                    _ => assert_eq!(got, value, "literal/constraint fidelity: {key}"),
                }
            }
        }
        assert_eq!(
            wire.as_object().unwrap().len(),
            full.as_object().unwrap().len()
        );
        for (name, parameter) in full["inputSchema"]["properties"].as_object().unwrap() {
            let authored = parameter["description"]
                .as_str()
                .expect("authored parameter guidance");
            assert!(!authored.is_empty(), "missing source guidance: {name}");
            let caption = wire["inputSchema"]["properties"][name]["description"]
                .as_str()
                .expect("wire parameter guidance retained");
            assert!(
                !caption.trim().is_empty() && caption.len() <= 100,
                "invalid guidance: {name}"
            );
        }
        for (key, value) in full.as_object().unwrap() {
            match key.as_str() {
                "inputSchema" => schema(&wire[key], value),
                "description" => {
                    let summary = wire[key]
                        .as_str()
                        .unwrap()
                        .strip_suffix(" _meta.stale_repos=federation lag. See stale_check.")
                        .expect("wire description must disclose federation staleness");
                    assert!(summary.len() <= 200, "compact summary exceeds its budget");
                }
                _ => assert_eq!(&wire[key], value),
            }
        }
    }

    #[test]
    fn first_page_contains_complete_hybrid_catalogue_and_cursor_errors_are_correlated() {
        let full = nestweaver_mcp::tools::tool_list(false);
        let expected = full["tools"].as_array().unwrap();
        assert_eq!(expected.len(), 43, "complete default hybrid profile");
        let request = nestweaver_mcp::protocol::validate_request(json!({
            "jsonrpc":"2.0", "id":"page-0", "method":"tools/list", "params":{}
        }))
        .unwrap();
        let reply = hybrid_catalogue_reply(&request, false);
        assert_eq!(reply["jsonrpc"], "2.0");
        assert_eq!(reply["id"], "page-0");
        assert!(reply.get("error").is_none(), "{reply}");
        let page = &reply["result"];
        assert!(
            nestweaver_mcp::output_budget::escaped_size(page)
                <= nestweaver_mcp::output_budget::CATALOGUE_BYTES
        );
        assert!(
            page.get("nextCursor").is_none(),
            "first-page discovery must be complete"
        );
        let actual = page["tools"].as_array().unwrap();
        assert_eq!(actual.len(), expected.len());
        let mut names = std::collections::HashSet::new();
        for (wire, full) in actual.iter().zip(expected) {
            assert!(
                names.insert(wire["name"].as_str().unwrap()),
                "duplicate tool: {wire}"
            );
            assert_wire_tool_contract(wire, full);
        }
        let request = nestweaver_mcp::protocol::validate_request(json!({"jsonrpc":"2.0","id":"bad-cursor","method":"tools/list","params":{"cursor":"broken"}})).unwrap();
        let error = hybrid_catalogue_reply(&request, false);
        assert_eq!(error["jsonrpc"], "2.0");
        assert_eq!(error["id"], "bad-cursor");
        assert_eq!(error["error"]["code"], -32602);
    }
}

#[cfg(test)]
mod incomplete_index_response_tests {
    use super::*;
    #[test]
    fn refused_single_and_federated_tiers_cannot_be_rendered_as_empty_success() {
        let refusal = serde_json::json!({"refused":true,"reason":"repo_index_incomplete","incomplete_repos":[{"repo_uid":"repo:other"}]});
        for payload in [
            refusal.clone(),
            serde_json::json!({"tier":"two","local_impact":refusal.clone()}),
            serde_json::json!({"org_wide_impact":{"results":[refusal]}}),
        ] {
            let error = reject_incomplete_index_response(payload.clone()).unwrap_err();
            assert_eq!(
                error.downcast_ref::<IncompleteIndexRefusal>().unwrap().0,
                payload
            );
        }
        let empty = serde_json::json!({"impact_nodes":[],"total":0});
        assert_eq!(
            reject_incomplete_index_response(empty.clone()).unwrap(),
            empty
        );
        let source_property =
            serde_json::json!({"properties":{"refused":true,"reason":"repo_index_incomplete"}});
        assert!(reject_incomplete_index_response(source_property).is_ok());
    }
}

#[cfg(test)]
mod replica_existing_read_tests {
    use super::*;
    use std::os::fd::AsRawFd;
    use std::sync::{Arc, Mutex};

    // An owned real UDS/HTTP2 endpoint backed by an actual read-only graph.
    // Runtime authority is a genuinely held pidfile flock, never a bypass.
    #[derive(Clone)]
    struct ReadEndpoint {
        health: Arc<Mutex<nestweaver_proto::HealthCheckResponse>>,
        store: Arc<nestweaver_store::GraphStore>,
        requests: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
    }
    impl tonic::server::NamedService for ReadEndpoint {
        const NAME: &'static str = "nestweaver.daemon.v1.NestWeaverDaemon";
    }
    impl tonic::codegen::Service<tonic::codegen::http::Request<tonic::body::Body>> for ReadEndpoint {
        type Response = tonic::codegen::http::Response<tonic::body::Body>;
        type Error = std::convert::Infallible;
        type Future = std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
        >;
        fn poll_ready(
            &mut self,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn call(
            &mut self,
            request: tonic::codegen::http::Request<tonic::body::Body>,
        ) -> Self::Future {
            let endpoint = self.clone();
            Box::pin(async move {
                match request.uri().path() {
                    "/nestweaver.daemon.v1.NestWeaverDaemon/HealthCheck" => {
                        struct Health(ReadEndpoint);
                        impl tonic::server::UnaryService<nestweaver_proto::HealthCheckRequest> for Health {
                            type Response = nestweaver_proto::HealthCheckResponse;
                            type Future = std::future::Ready<
                                Result<tonic::Response<Self::Response>, tonic::Status>,
                            >;
                            fn call(
                                &mut self,
                                _: tonic::Request<nestweaver_proto::HealthCheckRequest>,
                            ) -> Self::Future {
                                let mut health = self.0.health.lock().unwrap().clone();
                                if let Err(error) = self.0.store.verify_backing_file() {
                                    health.backing_file_valid = Some(false);
                                    health.backing_file_error = error.to_string();
                                }
                                std::future::ready(Ok(tonic::Response::new(health)))
                            }
                        }
                        Ok(tonic::server::Grpc::new(tonic_prost::ProstCodec::default())
                            .unary(Health(endpoint), request)
                            .await)
                    }
                    "/nestweaver.daemon.v1.NestWeaverDaemon/BrainStatusJson"
                    | "/nestweaver.daemon.v1.NestWeaverDaemon/BrainOrphanDocuments"
                    | "/nestweaver.daemon.v1.NestWeaverDaemon/BrainTopicClusters"
                    | "/nestweaver.daemon.v1.NestWeaverDaemon/BrainTagGraph"
                    | "/nestweaver.daemon.v1.NestWeaverDaemon/BrainDocStats"
                    | "/nestweaver.daemon.v1.NestWeaverDaemon/BrainDiff" => {
                        let operation = request.uri().path().to_string();
                        struct Status(ReadEndpoint, String);
                        impl tonic::server::UnaryService<nestweaver_proto::JsonRequest> for Status {
                            type Response = nestweaver_proto::JsonResponse;
                            type Future = std::future::Ready<
                                Result<tonic::Response<Self::Response>, tonic::Status>,
                            >;
                            fn call(
                                &mut self,
                                request: tonic::Request<nestweaver_proto::JsonRequest>,
                            ) -> Self::Future {
                                self.0.requests.lock().unwrap().push((
                                    self.1.clone(),
                                    serde_json::from_str(&request.into_inner().args_json).unwrap(),
                                ));
                                let repos = self.0.store.list_repos(None).unwrap().len();
                                std::future::ready(Ok(tonic::Response::new(nestweaver_proto::JsonResponse {
                                    result_json: serde_json::json!({"replica_read": true, "repos": repos}).to_string(),
                                })))
                            }
                        }
                        Ok(tonic::server::Grpc::new(tonic_prost::ProstCodec::default())
                            .unary(Status(endpoint, operation), request)
                            .await)
                    }
                    "/nestweaver.daemon.v1.NestWeaverDaemon/GetProjectContext" => {
                        struct Project(ReadEndpoint);
                        impl tonic::server::UnaryService<nestweaver_proto::ProjectContextRequest> for Project {
                            type Response = nestweaver_proto::ProjectContextResponse;
                            type Future = std::future::Ready<
                                Result<tonic::Response<Self::Response>, tonic::Status>,
                            >;
                            fn call(
                                &mut self,
                                request: tonic::Request<nestweaver_proto::ProjectContextRequest>,
                            ) -> Self::Future {
                                let request = request.into_inner();
                                self.0.requests.lock().unwrap().push(("GetProjectContext".into(), serde_json::json!({
                                    "project": request.project, "token_budget": request.token_budget,
                                    "include_components": request.include_components,
                                })));
                                std::future::ready(Ok(tonic::Response::new(nestweaver_proto::ProjectContextResponse {
                                    result_json: serde_json::json!({"project": request.project, "connected": [], "replica_read": true}).to_string(),
                                })))
                            }
                        }
                        Ok(tonic::server::Grpc::new(tonic_prost::ProstCodec::default())
                            .unary(Project(endpoint), request)
                            .await)
                    }
                    "/nestweaver.daemon.v1.NestWeaverDaemon/Search" => {
                        struct Search(ReadEndpoint);
                        impl tonic::server::UnaryService<nestweaver_proto::BrainSearchRequest> for Search {
                            type Response = nestweaver_proto::BrainSearchResponse;
                            type Future = std::future::Ready<
                                Result<tonic::Response<Self::Response>, tonic::Status>,
                            >;
                            fn call(
                                &mut self,
                                request: tonic::Request<nestweaver_proto::BrainSearchRequest>,
                            ) -> Self::Future {
                                let request = request.into_inner();
                                self.0.requests.lock().unwrap().push(("Search".into(), serde_json::json!({
                                    "query": request.query, "limit": request.limit, "prf": request.prf,
                                })));
                                let results: Vec<_> = self
                                    .0
                                    .store
                                    .list_notes(None)
                                    .unwrap()
                                    .into_iter()
                                    .filter(|note| note.title.contains(&request.query))
                                    .take(request.limit.max(0) as usize)
                                    .map(|note| nestweaver_proto::SearchResultItem {
                                        uid: note.uid,
                                        kind: "Note".into(),
                                        title: note.title,
                                        score: 1.0,
                                        location: Some(note.file_path),
                                        ..Default::default()
                                    })
                                    .collect();
                                std::future::ready(Ok(tonic::Response::new(
                                    nestweaver_proto::BrainSearchResponse {
                                        query: request.query,
                                        engine: "readonly-fixture".into(),
                                        total_matches: results.len() as i32,
                                        returned_matches: results.len() as i32,
                                        total_matches_relation: "eq".into(),
                                        results,
                                        ..Default::default()
                                    },
                                )))
                            }
                        }
                        Ok(tonic::server::Grpc::new(tonic_prost::ProstCodec::default())
                            .unary(Search(endpoint), request)
                            .await)
                    }
                    _ => Ok(tonic::Status::unimplemented("fixture read route").into_http()),
                }
            })
        }
    }

    struct Replica {
        dir: tempfile::TempDir,
        anchor: std::path::PathBuf,
        socket: std::path::PathBuf,
        owner: Option<std::fs::File>,
        health: Arc<Mutex<nestweaver_proto::HealthCheckResponse>>,
        requests: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
        stop: Option<tokio::sync::oneshot::Sender<()>>,
        thread: Option<std::thread::JoinHandle<()>>,
    }
    impl Replica {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let anchor = dir.path().join("absent-replica-anchor.lbug");
            let backing = dir.path().join("materialized.lbug");
            let writer = nestweaver_store::GraphStore::open_or_create(&backing).unwrap();
            let vault_uid = nestweaver_schema::vault_uid("fixture", "/fixture-vault");
            writer
                .insert_note(&nestweaver_schema::Note {
                    uid: nestweaver_schema::note_uid(&vault_uid, "RemoteTransportNote.md"),
                    vault_uid,
                    file_path: "RemoteTransportNote.md".into(),
                    title: "RemoteTransportNote".into(),
                    note_kind: nestweaver_schema::NoteKind::General,
                    word_count: 1,
                    content_hash: "fixture".into(),
                    frontmatter: None,
                    frontmatter_raw: None,
                    created_at: None,
                    modified_at: None,
                    pagerank_score: None,
                    embedding: None,
                })
                .unwrap();
            drop(writer);
            let store = Arc::new(
                nestweaver_store::GraphStore::open_read_only_without_migration(&backing).unwrap(),
            );
            let instance = nestweaver_daemon::lifecycle::instance_id_from_db_path(&anchor);
            let socket = nestweaver_daemon::lifecycle::socket_path(&instance);
            std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
            let pidfile = nestweaver_daemon::lifecycle::pidfile_path(&instance);
            // A long runtime path moves only the socket to its short fallback;
            // the pidfile and config binding still live in the runtime dir.
            std::fs::create_dir_all(pidfile.parent().unwrap()).unwrap();
            std::fs::write(&pidfile, format!("{}\n", std::process::id())).unwrap();
            let owner = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&pidfile)
                .unwrap();
            assert_eq!(
                unsafe { libc::flock(owner.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                0
            );
            nestweaver_daemon::lifecycle::write_effective_config_binding(
                &instance,
                &nestweaver_daemon::lifecycle::EffectiveConfigBinding::new(
                    std::process::id(),
                    nestweaver_daemon::lifecycle::EffectiveConfigBindingSource::CompiledDefaults,
                ),
            )
            .unwrap();
            let health = Arc::new(Mutex::new(nestweaver_proto::HealthCheckResponse {
                version: env!("CARGO_PKG_VERSION").to_string(),
                instance_id: instance,
                db_path: anchor.display().to_string(),
                pid: std::process::id(),
                backing_file_valid: Some(true),
                ..Default::default()
            }));
            let requests = Arc::new(Mutex::new(Vec::new()));
            let endpoint = ReadEndpoint {
                health: Arc::clone(&health),
                store,
                requests: Arc::clone(&requests),
            };
            let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
            listener.set_nonblocking(true).unwrap();
            let (stop, stopped) = tokio::sync::oneshot::channel();
            let (ready, wait) = std::sync::mpsc::channel();
            let thread = std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(async move {
                    let listener = tokio::net::UnixListener::from_std(listener).unwrap();
                    let (sender, receiver) = tokio::sync::mpsc::channel(4);
                    tokio::spawn(async move {
                        while let Ok((stream, _)) = listener.accept().await {
                            if sender.send(Ok::<_, std::io::Error>(stream)).await.is_err() {
                                break;
                            }
                        }
                    });
                    ready.send(()).unwrap();
                    let server = tonic::transport::Server::builder()
                        .add_service(endpoint)
                        .serve_with_incoming(tokio_stream::wrappers::ReceiverStream::new(receiver));
                    // Dropping this owned test runtime aborts all its peers,
                    // including a channel deliberately retained by the test.
                    tokio::select! {
                        result = server => result.unwrap(),
                        _ = stopped => {},
                    }
                });
            });
            wait.recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            Self {
                dir,
                anchor,
                socket,
                owner: Some(owner),
                health,
                requests,
                stop: Some(stop),
                thread: Some(thread),
            }
        }
        fn stop(&mut self) {
            if let Some(stop) = self.stop.take() {
                let _ = stop.send(());
            }
            if let Some(thread) = self.thread.take() {
                thread.join().unwrap();
            }
        }
    }
    impl Drop for Replica {
        fn drop(&mut self) {
            self.stop();
            if let Some(thread) = self.thread.take() {
                thread.join().unwrap();
            }
            drop(self.owner.take());
            let instance = nestweaver_daemon::lifecycle::instance_id_from_db_path(&self.anchor);
            let runtime = nestweaver_daemon::lifecycle::runtime_dir(&instance);
            let _ = std::fs::remove_dir_all(&runtime);
            if self.socket.parent() != Some(runtime.as_path()) {
                let _ = std::fs::remove_dir_all(self.socket.parent().unwrap());
            }
        }
    }

    #[test]
    fn real_health_resolver_skew_is_disclosed_without_replacing_the_verified_owner() {
        use std::os::unix::fs::MetadataExt;
        struct CapturedLog(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for CapturedLog {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let generation = nestweaver_engine::resolver_generation::RESOLVER_GENERATION;
        for advertised in [Some(generation - 1), None] {
            let replica = Replica::new();
            replica.health.lock().unwrap().resolver_generation = advertised;
            let instance = nestweaver_daemon::lifecycle::instance_id_from_db_path(&replica.anchor);
            let pidfile = nestweaver_daemon::lifecycle::pidfile_path(&instance);
            let binding = nestweaver_daemon::lifecycle::effective_config_binding_path(&instance);
            let original_binding = std::fs::read(&binding).unwrap();
            let original_pidfile = std::fs::read(&pidfile).unwrap();
            let original_socket_inode = std::fs::metadata(&replica.socket).unwrap().ino();
            let log = Arc::new(Mutex::new(Vec::new()));
            let captured = log.clone();
            let subscriber = tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_max_level(tracing::Level::WARN)
                .with_writer(move || CapturedLog(captured.clone()))
                .finish();
            tracing::subscriber::with_default(subscriber, || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(async {
                    // Exercise ordinary version-checking admission as well as
                    // existing-read admission against an actual UDS/HTTP2 peer.
                    let mut ordinary = tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        nestweaver_client::DaemonClient::connect(&replica.anchor, None),
                    )
                    .await
                    .unwrap()
                    .unwrap();
                    let first = ordinary.health_check().await.unwrap();
                    assert_eq!(first.version, env!("CARGO_PKG_VERSION"));
                    assert_eq!(first.resolver_generation, advertised);
                    assert_eq!(
                        nestweaver_client::daemon_resolver_skew(&first),
                        advertised.map(|_| true)
                    );
                    let mut read = nestweaver_client::DaemonClient::connect_existing_verified_read(
                        &replica.anchor,
                        None,
                    )
                    .await
                    .unwrap()
                    .unwrap();
                    let second = read.health_check().await.unwrap();
                    assert_eq!(second.pid, std::process::id());
                    assert_eq!(second.resolver_generation, advertised);
                    assert_eq!(second.version, first.version);
                });
                let answer = try_hybrid_json_rpc_checked(
                    true,
                    &replica.anchor,
                    None,
                    "brain_status",
                    serde_json::json!({}),
                )
                .unwrap()
                .unwrap();
                assert_eq!(answer["replica_read"], true);
            });
            let log = String::from_utf8(log.lock().unwrap().clone()).unwrap();
            if advertised.is_some() {
                assert!(
                    log.contains("same product version, different resolver builds"),
                    "{log}"
                );
                assert!(
                    log.contains(&format!("daemon_resolver_generation={}", generation - 1)),
                    "{log}"
                );
                assert!(
                    log.contains(&format!("client_resolver_generation={generation}")),
                    "{log}"
                );
            } else {
                assert!(
                    !log.contains("different resolver builds"),
                    "older peers must remain unknown: {log}"
                );
            }
            assert!(
                !log.contains("requesting graceful daemon restart"),
                "resolver evidence must not request replacement: {log}"
            );
            assert_eq!(
                std::fs::metadata(&replica.socket).unwrap().ino(),
                original_socket_inode
            );
            assert_eq!(std::fs::read(&binding).unwrap(), original_binding);
            assert_eq!(std::fs::read(&pidfile).unwrap(), original_pidfile);
            let contender = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&pidfile)
                .unwrap();
            assert_ne!(
                unsafe { libc::flock(contender.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                0,
                "incumbent owner lock must remain held"
            );
            let requests = replica.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(
                requests[0].0,
                "/nestweaver.daemon.v1.NestWeaverDaemon/BrainStatusJson"
            );
            assert!(
                !replica.anchor.exists(),
                "read must not materialize a direct graph"
            );
        }
    }

    #[test]
    fn project_context_named_config_reaches_its_verified_owner_without_a_direct_open() {
        std::thread::Builder::new()
            .name("project-context-cli-acceptance".into())
            .stack_size(16 * 1024 * 1024)
            .spawn(|| {
                use clap::Parser;
                let replica = Replica::new();
                let config = replica.dir.path().join("owner.toml");
                std::fs::write(
                    &config,
                    format!(
                        r#"instance_id = "replica-test"
    [snapshot_storage]
    backend = "local"
    path = "{}"
    [workspace]
    backend = "local"
    path = "{}"
    [inference]
    endpoint = "http://127.0.0.1:9"
    embedding_model = "offline"
    summary_model = "offline"
    [git]
    credential_method = "ssh"
    "#,
                        replica.dir.path().join("snapshots").display(),
                        replica.dir.path().join("workspace").display()
                    ),
                )
                .unwrap();
                let canonical = std::fs::canonicalize(&config).unwrap();
                let instance =
                    nestweaver_daemon::lifecycle::instance_id_from_db_path(&replica.anchor);
                nestweaver_daemon::lifecycle::write_effective_config_binding(
                    &instance,
                    &nestweaver_daemon::lifecycle::EffectiveConfigBinding::new(
                        std::process::id(),
                        nestweaver_daemon::lifecycle::EffectiveConfigBindingSource::Configured {
                            path: canonical.display().to_string(),
                        },
                    ),
                )
                .unwrap();
                let invoke = |config: &std::path::Path| {
                    let cli = crate::Cli::try_parse_from([
                        "nestweaver",
                        "project-context",
                        "fixture-project",
                        "--db",
                        replica.anchor.to_str().unwrap(),
                        "--config",
                        config.to_str().unwrap(),
                        "--token-budget",
                        "321",
                        "--json",
                    ])
                    .unwrap();
                    crate::run(
                        cli,
                        &crate::OutputConfig {
                            quiet: true,
                            verbose: false,
                            color: false,
                        },
                    )
                };
                assert_eq!(invoke(&config).unwrap().0, crate::EXIT_SUCCESS);
                let calls = replica.requests.lock().unwrap().clone();
                assert_eq!(
                    calls.len(),
                    1,
                    "the command must issue exactly one owning-daemon query"
                );
                assert_eq!(calls[0].0, "GetProjectContext");
                assert_eq!(calls[0].1["project"], "fixture-project");
                assert_eq!(calls[0].1["token_budget"], 321);
                assert!(!replica.anchor.exists(), "no direct graph may be created");

                let other = replica.dir.path().join("other.toml");
                std::fs::copy(&config, &other).unwrap();
                assert!(
                    invoke(&other).is_err(),
                    "a different config path must be refused"
                );
                assert_eq!(
                    replica.requests.lock().unwrap().len(),
                    1,
                    "mismatched intent must never reach dispatch"
                );
                assert!(!replica.anchor.exists());
            })
            .unwrap()
            .join()
            .unwrap();
    }

    fn actual_search(anchor: &std::path::Path) -> anyhow::Result<(i32, Option<String>)> {
        crate::cli::brain::run_brain(
            BrainCommands::Search {
                query: "RemoteTransportNote".into(),
                limit: Some(7),
                json: true,
                db: Some(anchor.to_path_buf()),
                config: None,
                prf: true,
            },
            &OutputConfig {
                quiet: true,
                verbose: false,
                color: false,
            },
            std::time::Instant::now(),
            true,
        )
    }

    #[test]
    fn actual_brain_read_commands_use_absent_replica_anchor_and_preserve_search_params() {
        let replica = Replica::new();
        let (code, stats) = actual_search(&replica.anchor).unwrap();
        assert_eq!(code, EXIT_SUCCESS);
        assert!(
            stats.unwrap().starts_with("1 results"),
            "real readonly note result required"
        );
        let db = Some(replica.anchor.clone());
        let commands = [
            BrainCommands::Orphans {
                vault: None,
                path_prefix: None,
                allow: vec![],
                limit: Some(7),
                json: true,
                db: db.clone(),
                config: None,
            },
            BrainCommands::TopicClusters {
                resolution: 0.5,
                limit: Some(7),
                json: true,
                db: db.clone(),
                config: None,
            },
            BrainCommands::TagGraph {
                tag: None,
                limit: Some(7),
                json: true,
                db: db.clone(),
                config: None,
            },
            BrainCommands::DocStats {
                top_tags_limit: 7,
                json: true,
                db: db.clone(),
                config: None,
            },
            BrainCommands::Diff {
                repo: "fixture".into(),
                since_sha: None,
                limit: Some(7),
                json: true,
                db,
                config: None,
            },
        ];
        let out = OutputConfig {
            quiet: true,
            verbose: false,
            color: false,
        };
        for command in commands {
            assert_eq!(
                crate::cli::brain::run_brain(command, &out, std::time::Instant::now(), true)
                    .unwrap()
                    .0,
                EXIT_SUCCESS
            );
        }
        let requests = replica.requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            6,
            "all six actual command branches must issue an RPC"
        );
        assert_eq!(
            requests[0],
            (
                "Search".into(),
                serde_json::json!({"query":"RemoteTransportNote", "limit":7, "prf":true})
            )
        );
        for operation in [
            "BrainOrphanDocuments",
            "BrainTopicClusters",
            "BrainTagGraph",
            "BrainDocStats",
            "BrainDiff",
        ] {
            assert!(
                requests.iter().any(|(path, _)| path.ends_with(operation)),
                "missing {operation}"
            );
        }
        assert!(!replica.anchor.exists());
    }

    #[test]
    fn actual_brain_search_bad_health_stopped_typo_and_invalid_file_never_create_store() {
        let mut replica = Replica::new();
        replica.health.lock().unwrap().backing_file_valid = Some(false);
        assert!(actual_search(&replica.anchor).is_err());
        assert!(replica.requests.lock().unwrap().is_empty());
        assert!(!replica.anchor.exists());
        replica.stop();
        assert!(actual_search(&replica.anchor).is_err());
        assert!(!replica.anchor.exists());
        let typo = replica.dir.path().join("typo.lbug");
        assert!(format!("{:#}", actual_search(&typo).unwrap_err()).contains("database not found"));
        assert!(!typo.exists());
        let invalid = replica.dir.path().join("invalid.lbug");
        std::fs::write(&invalid, b"not a database").unwrap();
        assert!(actual_search(&invalid).is_err());
        assert_eq!(std::fs::read(&invalid).unwrap(), b"not a database");
        assert!(
            !nestweaver_daemon::lifecycle::pidfile_path(
                &nestweaver_daemon::lifecycle::instance_id_from_db_path(&invalid)
            )
            .exists()
        );
    }

    #[test]
    fn absent_replica_anchor_cli_read_uses_verified_incumbent_and_refuses_bad_evidence() {
        let replica = Replica::new();
        let query = |config| {
            try_hybrid_json_rpc_checked(
                true,
                &replica.anchor,
                config,
                "brain_status",
                serde_json::json!({}),
            )
        };
        assert_eq!(query(None).unwrap().unwrap()["replica_read"], true);
        assert!(!replica.anchor.exists());
        let valid = replica.health.lock().unwrap().clone();
        for change in ["pid", "instance", "path", "version", "backing"] {
            let mut bad = valid.clone();
            match change {
                "pid" => bad.pid += 1,
                "instance" => bad.instance_id = "wrong".into(),
                "path" => {
                    bad.db_path = replica
                        .dir
                        .path()
                        .join("different.lbug")
                        .display()
                        .to_string()
                }
                "version" => bad.version = "0.0.1".into(),
                "backing" => bad.backing_file_valid = Some(false),
                _ => unreachable!(),
            }
            *replica.health.lock().unwrap() = bad;
            assert!(query(None).is_err(), "must refuse {change}");
            assert!(!replica.anchor.exists());
        }
        *replica.health.lock().unwrap() = valid;
        let config = replica.dir.path().join("other.toml");
        std::fs::write(
            &config,
            format!(
                r#"instance_id = "replica-test"
[snapshot_storage]
backend = "local"
path = "{}"
[workspace]
backend = "local"
path = "{}"
[inference]
endpoint = "http://127.0.0.1:9"
embedding_model = "offline"
summary_model = "offline"
[git]
credential_method = "ssh"
"#,
                replica.dir.path().join("snapshots").display(),
                replica.dir.path().join("workspace").display()
            ),
        )
        .unwrap();
        let error = format!("{:#}", query(Some(&config)).unwrap_err());
        assert!(error.contains("effective config"), "{error}");
        assert!(!replica.anchor.exists());
    }

    #[test]
    fn verified_replica_exit_before_query_never_autostarts_a_replacement() {
        let mut replica = Replica::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut hybrid = rt
            .block_on(
                nestweaver_client::hybrid::HybridClient::connect_existing_verified_read(
                    &replica.anchor,
                    None,
                    replica.dir.path(),
                ),
            )
            .unwrap()
            .expect("verified existing endpoint");
        replica.stop();
        let answer = rt.block_on(async {
            tokio::time::timeout(
                std::time::Duration::from_secs(3),
                hybrid.query("brain_status", &serde_json::json!({})),
            )
            .await
        });
        assert!(
            matches!(answer, Ok(Err(_))),
            "owned endpoint exited; no read may succeed"
        );
        assert!(!replica.anchor.exists());
        assert_eq!(
            std::fs::read_to_string(nestweaver_daemon::lifecycle::pidfile_path(
                &nestweaver_daemon::lifecycle::instance_id_from_db_path(&replica.anchor),
            ))
            .unwrap()
            .trim(),
            std::process::id().to_string(),
            "no replacement pidfile"
        );
        drop(hybrid);
    }

    #[test]
    fn absent_cli_typo_never_spawns_or_creates_a_database() {
        let dir = tempfile::tempdir().unwrap();
        let anchor = dir.path().join("typo.lbug");
        let instance = nestweaver_daemon::lifecycle::instance_id_from_db_path(&anchor);
        let error =
            try_hybrid_json_rpc_checked(true, &anchor, None, "brain_status", serde_json::json!({}))
                .unwrap_err();
        assert!(format!("{error:#}").contains("database not found"));
        assert!(!anchor.exists());
        assert!(!nestweaver_daemon::lifecycle::pidfile_path(&instance).exists());
        assert!(
            nestweaver_daemon::lifecycle::daemon_socket_candidates(&instance)
                .iter()
                .all(|p| !p.exists())
        );
    }
}

#[cfg(test)]
mod upstream_only_affected_tests_regressions {
    use super::*;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicU8, Ordering},
    };
    use tonic::codegen::{Service, http};

    #[derive(Clone)]
    struct Peer {
        mode: Arc<AtomicU8>,
        calls: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
    }
    impl tonic::server::NamedService for Peer {
        const NAME: &'static str = "nestweaver.daemon.v1.NestWeaverDaemon";
    }
    impl Service<http::Request<tonic::body::Body>> for Peer {
        type Response = http::Response<tonic::body::Body>;
        type Error = std::convert::Infallible;
        type Future = std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
        >;
        fn poll_ready(
            &mut self,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn call(&mut self, request: http::Request<tonic::body::Body>) -> Self::Future {
            let peer = self.clone();
            Box::pin(async move {
                assert_eq!(
                    request.headers().get("authorization").unwrap(),
                    "Bearer nw779-private-authenticated-fixture-token"
                );
                let method = request.uri().path().rsplit('/').next().unwrap().to_owned();
                if method == "HealthCheck" {
                    struct Health;
                    impl tonic::server::UnaryService<nestweaver_proto::HealthCheckRequest> for Health {
                        type Response = nestweaver_proto::HealthCheckResponse;
                        type Future = std::future::Ready<
                            Result<tonic::Response<Self::Response>, tonic::Status>,
                        >;
                        fn call(
                            &mut self,
                            _: tonic::Request<nestweaver_proto::HealthCheckRequest>,
                        ) -> Self::Future {
                            std::future::ready(Ok(tonic::Response::new(
                                nestweaver_proto::HealthCheckResponse::default(),
                            )))
                        }
                    }
                    return Ok(tonic::server::Grpc::new(tonic_prost::ProstCodec::default())
                        .unary(Health, request)
                        .await);
                }
                struct Json(Peer, String);
                impl tonic::server::UnaryService<nestweaver_proto::JsonRequest> for Json {
                    type Response = nestweaver_proto::JsonResponse;
                    type Future =
                        std::future::Ready<Result<tonic::Response<Self::Response>, tonic::Status>>;
                    fn call(
                        &mut self,
                        request: tonic::Request<nestweaver_proto::JsonRequest>,
                    ) -> Self::Future {
                        let args: serde_json::Value =
                            serde_json::from_str(&request.into_inner().args_json).unwrap();
                        self.0.calls.lock().unwrap().push((self.1.clone(), args));
                        let mode = self.0.mode.load(Ordering::SeqCst);
                        if self.1 == "ListReposJson" && mode == 1 {
                            return std::future::ready(Err(tonic::Status::permission_denied(
                                "inventory forbidden",
                            )));
                        }
                        let repo = |uid: &str| serde_json::json!({"uid":uid,"url":format!("https://example.invalid/{uid}.git"),"indexed_sha":"fixture","staleness_commits_behind":0,"instance_id":"fixture","name":uid,"root_path":null});
                        let value = match self.1.as_str() {
                            "ListReposJson" if mode == 2 => {
                                serde_json::json!([repo("first"), repo("second")])
                            }
                            "ListReposJson" => serde_json::json!([repo("first")]),
                            "AffectedTests" => serde_json::to_value(
                                nestweaver_engine::affected_tests::empty_derived_selection(),
                            )
                            .unwrap(),
                            "Investigate" => {
                                serde_json::json!({"status":"ok","upstream_fixture":true})
                            }
                            method => panic!("unexpected upstream RPC: {method}"),
                        };
                        std::future::ready(Ok(tonic::Response::new(
                            nestweaver_proto::JsonResponse {
                                result_json: value.to_string(),
                            },
                        )))
                    }
                }
                Ok(tonic::server::Grpc::new(tonic_prost::ProstCodec::default())
                    .unary(Json(peer, method), request)
                    .await)
            })
        }
    }

    /// Invoke actual CLI dispatch in a private process so environment/config
    /// discovery cannot race unrelated unit tests.
    #[test]
    fn invoke_cli_fixture() {
        if std::env::var_os("NW779_CHILD_CASE").is_none() {
            return;
        }
        std::thread::Builder::new()
            .name("nw779-cli-fixture".into())
            .stack_size(16 * 1024 * 1024)
            .spawn(invoke_cli_fixture_body)
            .unwrap()
            .join()
            .unwrap();
    }

    fn invoke_cli_fixture_body() {
        use clap::Parser;
        let Ok(case) = std::env::var("NW779_CHILD_CASE") else {
            return;
        };
        let db = PathBuf::from(std::env::var_os("NW779_CHILD_DB").unwrap());
        let args = if case == "investigate" {
            vec![
                "nestweaver",
                "investigate",
                "fixture",
                "--no-embed",
                "--json",
                "--db",
                db.to_str().unwrap(),
            ]
        } else {
            vec![
                "nestweaver",
                "affected-tests",
                "--files",
                "src/a.py",
                "--json",
                "--db",
                db.to_str().unwrap(),
            ]
        };
        let result = crate::run(
            Cli::try_parse_from(args).unwrap(),
            &OutputConfig {
                quiet: true,
                verbose: false,
                color: false,
            },
        );
        match case.as_str() {
            "forbidden" => {
                assert!(format!("{:#}", result.unwrap_err()).contains("inventory forbidden"))
            }
            "invalid" => assert!(result.is_err()),
            "ambiguous" => assert_eq!(result.unwrap().0, EXIT_USAGE),
            _ => assert_eq!(result.unwrap().0, EXIT_SUCCESS),
        }
        if case == "invalid" {
            assert_eq!(std::fs::read(&db).unwrap(), b"not a database");
        } else {
            assert!(!db.exists());
            assert!(!nestweaver_engine::publication::default_publication_root(&db).exists());
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn missing_local_routes_authenticated_inventory_and_keeps_refusals_fail_closed() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mode = Arc::new(AtomicU8::new(0));
        let peer = Peer {
            mode: mode.clone(),
            calls: calls.clone(),
        };
        let server = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(peer)
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        for (case, peer_mode, expected_methods) in [
            ("success", 0, vec!["ListReposJson", "AffectedTests"]),
            ("investigate", 0, vec!["Investigate"]),
            ("forbidden", 1, vec!["ListReposJson"]),
            ("ambiguous", 2, vec!["ListReposJson"]),
            ("invalid", 0, vec![]),
        ] {
            mode.store(peer_mode, Ordering::SeqCst);
            calls.lock().unwrap().clear();
            let dir = tempfile::tempdir().unwrap();
            let db = dir.path().join("absent.lbug");
            if case == "invalid" {
                std::fs::write(&db, b"not a database").unwrap();
            }
            let config = dir.path().join("config/nestweaver");
            std::fs::create_dir_all(&config).unwrap();
            std::fs::write(config.join("upstreams.toml"),format!("[[upstream]]\nurl = \"http://{addr}\"\ntoken = \"nw779-private-authenticated-fixture-token\"\nmode = \"primary\"\ntimeout = \"5s\"\n")).unwrap();
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child
                .args([
                    "--exact",
                    "cli::daemon_rpc::upstream_only_affected_tests_regressions::invoke_cli_fixture",
                    "--nocapture",
                ])
                .current_dir(dir.path())
                .env("NW779_CHILD_CASE", case)
                .env("NW779_CHILD_DB", &db)
                .env("HOME", dir.path().join("home"))
                .env("XDG_CONFIG_HOME", dir.path().join("config"))
                .env("XDG_DATA_HOME", dir.path().join("data"))
                .env("XDG_STATE_HOME", dir.path().join("state"))
                .env("XDG_RUNTIME_DIR", dir.path().join("runtime"));
            for key in [
                "NESTWEAVER_DB",
                "NESTWEAVER_UPSTREAM",
                "NESTWEAVER_TOKEN",
                "NESTWEAVER_NO_DAEMON",
                "NESTWEAVER_ALLOW_NO_DAEMON",
            ] {
                child.env_remove(key);
            }
            let output = tokio::task::spawn_blocking(move || {
                child
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped());
                let mut process = child.spawn().unwrap();
                let started = std::time::Instant::now();
                while process.try_wait().unwrap().is_none() {
                    if started.elapsed() > std::time::Duration::from_secs(30) {
                        process.kill().unwrap();
                        let output = process.wait_with_output().unwrap();
                        panic!(
                            "private CLI fixture timed out: {}",
                            String::from_utf8_lossy(&output.stderr)
                        );
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                process.wait_with_output().unwrap()
            })
            .await
            .unwrap();
            assert!(
                output.status.success(),
                "{case}: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("1 passed"),
                "child test filter must execute the fixture"
            );
            let observed = calls.lock().unwrap().clone();
            assert_eq!(
                observed
                    .iter()
                    .map(|(method, _)| method.as_str())
                    .collect::<Vec<_>>(),
                expected_methods,
                "{case}"
            );
            if case == "success" {
                assert_eq!(observed[1].1["repo"], "first");
                assert_eq!(
                    observed[1].1["changed_files"],
                    serde_json::json!(["src/a.py"])
                );
            }
        }
        server.abort();
    }
}
