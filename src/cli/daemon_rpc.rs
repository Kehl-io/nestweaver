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
    if let Some(seconds) = std::env::var(RPC_TIMEOUT_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
    {
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

/// Whether the daemon may serve a request that named `--config`.
///
/// It may not, and this is a SECURITY boundary, not a convenience. nw-316's
/// research settled it: `InstanceConfig` carries `authz`, and
/// `build_daemon_permission_source` derives the daemon's ENTIRE permission
/// source from it, with `None` meaning every caller is `VisibleRepos::All`. A
/// client that could forward its own config into request handling could supply
/// its own authorization policy, or omit it and be promoted to see everything --
/// a total bypass of nw-403 and nw-415. Most of the struct is also
/// process-lifetime rather than request-scoped (`embedding` names a model the
/// daemon loaded at BOOT, against vectors already on disk), so "forward the
/// config" silently splits into keys that can be honoured and keys that cannot,
/// with nothing telling the caller which half it got.
///
/// So `--config` joins `--no-tests` and `--prefer-instance` in forcing the
/// direct route: the caller gets exactly the config they named, and a shared
/// multi-client daemon can never be reconfigured by one client. A caller who
/// genuinely needs a different config served remotely wants a second daemon
/// INSTANCE, which the instance model already supports.
///
/// `tool_brain_guide` already refuses a caller-supplied `config` for the same
/// reason and says so in its schema; this is that precedent applied to the
/// route that has an alternative.
pub(crate) fn daemon_may_serve(use_daemon: bool, config: Option<&std::path::Path>) -> bool {
    use_daemon && config.is_none()
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
    // `error` is the machine word on the CLI, as it always was.
    envelope["error"] = serde_json::json!(failure.status());
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

pub(crate) fn try_hybrid_json_rpc_checked(
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
    require_openable_db(db_path).or_else(|error| {
        if db_path.exists() {
            Err(error)
        } else {
            // Absent is handled by the arm below, which still routes to
            // configured upstreams (federated read) rather than failing.
            Ok(())
        }
    })?;
    if !db_path.exists() {
        let effective = nestweaver_client::RestartConfig::for_automatic_cold_start(db_path, config)
            .context("load explicit instance config or persisted intent before missing-DB upstream routing")?;
        let config = effective.as_path();
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
    }
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
    })
}

/// One-line staleness note rendered (text mode only) when a search
/// reports that it bypassed a stale trigram posting table. JSON mode carries
/// the signal in-band via the `stale_index` field instead.
pub(crate) fn print_stale_index_note() {
    println!(
        "(one or more trigram scopes are stale — only dirty scopes were scanned; refresh with \
         `index --with-trigrams`, or set `[indexing] with_trigrams = true` so indexing keeps \
         them fresh)"
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

#[cfg(test)]
mod hybrid_catalogue_tests {
    use super::*;
    use serde_json::{Value, json};
    #[test]
    fn request_cursor_pages_intact_hybrid_catalogue_and_errors_are_correlated() {
        let expected = nestweaver_mcp::tools::tool_list(false)["tools"]
            .as_array()
            .unwrap()
            .clone();
        let mut actual = Vec::new();
        let mut params = json!({});
        for index in 0..100 {
            let request = nestweaver_mcp::protocol::validate_request(json!({"jsonrpc":"2.0","id":format!("page-{index}"),"method":"tools/list","params":params})).unwrap();
            let reply = hybrid_catalogue_reply(&request, false);
            assert_eq!(reply["id"], format!("page-{index}"));
            let page = &reply["result"];
            assert!(nestweaver_mcp::output_budget::escaped_size(page) <= 32_000);
            let tools = page["tools"].as_array().unwrap();
            assert!(tools.len() <= 8);
            actual.extend(tools.iter().cloned());
            if let Some(cursor) = page.get("nextCursor").and_then(Value::as_str) {
                params = json!({"cursor":cursor});
            } else {
                break;
            }
        }
        assert_eq!(actual, expected);
        let request = nestweaver_mcp::protocol::validate_request(json!({"jsonrpc":"2.0","id":"bad-cursor","method":"tools/list","params":{"cursor":"broken"}})).unwrap();
        let error = hybrid_catalogue_reply(&request, false);
        assert_eq!(error["id"], "bad-cursor");
        assert_eq!(error["error"]["code"], -32602);
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
            let params = request.params.clone().unwrap_or(serde_json::Value::Null);
            let name = params
                .get("name")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
            // nw-558: fold alias spellings before the federation legs read
            // the canonical keys.
            let arguments = nestweaver_mcp::tools::canonicalize_tool_arguments(name, arguments);
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
                nestweaver_mcp::tools::validate_tool_arguments(name, &arguments)
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
                            hybrid.query(name, &arguments),
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
