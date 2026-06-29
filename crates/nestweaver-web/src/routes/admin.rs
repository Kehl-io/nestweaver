//! Admin REST API routes for NestWeaver server management.
//!
//! All routes under `/admin/api/` require the admin token via the
//! `AdminAuth` extractor. Covers repos, queue, drain/resume,
//! dead-letter, config reload, and full server status.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::{
    Json,
    extract::{FromRef, FromRequestParts, Path, State},
    http::{StatusCode, request::Parts},
};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

use crate::state::{AdminState, PendingDevice};

// ── Admin auth extractor ───────────────────────────────────────────────

/// Axum extractor that validates the admin token from the Authorization
/// header. Returns 401 if missing or invalid.
pub struct AdminAuth;

impl<S> FromRequestParts<S> for AdminAuth
where
    S: Send + Sync,
    Arc<AdminState>: FromRef<S>,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let admin_state = Arc::<AdminState>::from_ref(state);
        let token = parts
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        match token {
            Some(t)
                if t.as_bytes()
                    .ct_eq(admin_state.admin_token.as_bytes())
                    .into() =>
            {
                Ok(AdminAuth)
            }
            _ => Err((StatusCode::UNAUTHORIZED, "admin token required")),
        }
    }
}

// ── Response types ─────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct RepoInfo {
    pub id: String,
    pub url: String,
    pub name: String,
    pub status: String,
    pub indexed_sha: String,
    pub symbol_count: i64,
}

#[derive(Deserialize)]
pub struct AddRepoRequest {
    pub url: String,
    pub branch: Option<String>,
}

#[derive(Serialize)]
pub struct QueueInfo {
    pub depth: u32,
    pub drained: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by_priority: Option<std::collections::HashMap<String, u32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub running: Option<Vec<serde_json::Value>>,
}

#[derive(Serialize, Deserialize)]
pub struct DrainStatus {
    pub drained: bool,
    pub active_reads: u32,
    pub active_writes: u32,
}

#[derive(Serialize)]
pub struct RepoStats {
    pub total: usize,
    pub indexed: usize,
    pub stale: usize,
    pub dead_letter: usize,
}

#[derive(Serialize)]
pub struct SymbolStats {
    pub total: usize,
}

#[derive(Serialize)]
pub struct QueueStats {
    pub pending: u32,
    pub running: u32,
    pub dead_letter: usize,
}

#[derive(Serialize)]
pub struct AdminStatus {
    pub instance_id: String,
    pub uptime_seconds: u64,
    pub server_mode: bool,
    pub repo_count: usize,
    pub active_reads: u32,
    pub active_writes: u32,
    pub queue_depth: u32,
    pub drained: bool,
    pub version: String,
    // Nested shapes expected by the React admin dashboard.
    pub repos: RepoStats,
    pub symbols: SymbolStats,
    pub queue: QueueStats,
}

#[derive(Serialize)]
pub struct MessageResponse {
    pub message: String,
}

// ── Repo management ────────────────────────────────────────────────────

/// GET /admin/api/repos — list repos with status and freshness.
pub async fn list_repos(
    _auth: AdminAuth,
    State(state): State<Arc<AdminState>>,
) -> Result<Json<Vec<RepoInfo>>, (StatusCode, String)> {
    let store = state.daemon_store.clone();

    let repos = tokio::task::spawn_blocking(move || store.list_repos(None))
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("task panicked: {e}"),
            )
        })?
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("list_repos failed: {e}"),
            )
        })?;

    let store = state.daemon_store.clone();
    let repo_infos = tokio::task::spawn_blocking(move || {
        repos
            .into_iter()
            .map(|r| {
                let symbol_count = store
                    .symbol_names_by_repo(&r.uid)
                    .map(|v| v.len() as i64)
                    .unwrap_or(0);
                let name = r.name.unwrap_or_else(|| {
                    r.url
                        .strip_prefix("file://")
                        .unwrap_or(&r.url)
                        .rsplit('/')
                        .next()
                        .unwrap_or(&r.url)
                        .to_string()
                });
                RepoInfo {
                    id: r.uid,
                    url: r.url,
                    name,
                    status: "indexed".to_string(),
                    indexed_sha: r.indexed_sha,
                    symbol_count,
                }
            })
            .collect::<Vec<_>>()
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task panicked: {e}"),
        )
    })?;

    Ok(Json(repo_infos))
}

/// True if a V4 address is an internal/non-routable target we must never reach
/// (loopback, RFC1918 private, link-local, or the unspecified `0.0.0.0`, which
/// routes to loopback on some platforms).
fn v4_is_internal(v4: Ipv4Addr) -> bool {
    v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
}

/// Extract an embedded IPv4 from the IPv6 transition forms that can smuggle an
/// internal V4 target past a naive IPv6-only check:
/// - NAT64 well-known prefix `64:ff9b::/96` (embedded V4 in the low 32 bits)
/// - 6to4 `2002:V4::/16` (embedded V4 in segments 1..3)
/// - IPv4-compatible `::a.b.c.d` (low 32 bits when the high 96 bits are zero and
///   it is not the IPv4-mapped `::ffff:a.b.c.d` form, which is handled separately)
fn embedded_ipv4(v6: Ipv6Addr) -> Option<Ipv4Addr> {
    let seg = v6.segments();
    let v4_from =
        |hi: u16, lo: u16| Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8);
    // NAT64 64:ff9b::/96 — IPv4 in the low 32 bits.
    if seg[0] == 0x0064 && seg[1] == 0xff9b && seg[2..6] == [0, 0, 0, 0] {
        return Some(v4_from(seg[6], seg[7]));
    }
    // 6to4 2002:V4::/16 — IPv4 in segments 1..3.
    if seg[0] == 0x2002 {
        return Some(v4_from(seg[1], seg[2]));
    }
    // IPv4-compatible ::a.b.c.d — high 96 bits zero (excludes ::ffff:a.b.c.d,
    // whose segment 5 is 0xffff), and not the all-zero unspecified address.
    if seg[0..6] == [0, 0, 0, 0, 0, 0] && (seg[6] != 0 || seg[7] != 0) {
        return Some(v4_from(seg[6], seg[7]));
    }
    None
}

/// True if `ip` is an internal/private target SSRF must never reach. Handles
/// raw V4/V6, IPv4-mapped (`::ffff:a.b.c.d`), and the IPv6 transition forms
/// (NAT64 / 6to4 / v4-compatible) that can embed an internal V4. Pure — no I/O.
fn ip_is_internal(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4_is_internal(v4),
        IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() {
                return true;
            }
            // IPv4-mapped (::ffff:a.b.c.d) hiding an internal V4 target.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return v4_is_internal(v4);
            }
            // Unique-local fc00::/7 (is_unique_local is nightly-only).
            if (v6.segments()[0] & 0xfe00) == 0xfc00 {
                return true;
            }
            // Link-local fe80::/10 (is_unicast_link_local is nightly-only).
            if (v6.segments()[0] & 0xffc0) == 0xfe80 {
                return true;
            }
            // NAT64 / 6to4 / v4-compatible embedding an internal V4 target.
            embedded_ipv4(v6).is_some_and(v4_is_internal)
        }
    }
}

/// Parse one IPv4 "part" using the inet_aton number grammar that alternate-
/// encoding SSRF payloads abuse: hex (`0x`/`0X` prefix), octal (leading `0`),
/// or decimal. Returns the numeric value, which the caller range-checks.
fn parse_ipv4_part(part: &str) -> Option<u64> {
    if part.is_empty() {
        return None;
    }
    let (radix, digits) =
        if let Some(hex) = part.strip_prefix("0x").or_else(|| part.strip_prefix("0X")) {
            (16, hex)
        } else if part.len() > 1 && part.starts_with('0') {
            (8, &part[1..])
        } else {
            (10, part)
        };
    if digits.is_empty() {
        return None;
    }
    u64::from_str_radix(digits, radix).ok()
}

/// Parse an alternate-encoded IPv4 host into an `Ipv4Addr`. Covers the inet_aton
/// forms that `url::Url` leaves as opaque domain strings for non-special schemes
/// (git/ssh): single decimal (`2130706433`), hex (`0x7f000001`), octal
/// (`017700000001`), and 2–4 dotted parts in any of those bases. Returns `None`
/// for anything that isn't a fully-numeric host (e.g. a real DNS name).
fn parse_numeric_ipv4(host: &str) -> Option<Ipv4Addr> {
    let parts: Vec<&str> = host.split('.').collect();
    if parts.is_empty() || parts.len() > 4 {
        return None;
    }
    let nums: Vec<u64> = parts
        .iter()
        .map(|p| parse_ipv4_part(p))
        .collect::<Option<Vec<_>>>()?;
    let addr: u32 = match nums.as_slice() {
        // a — the whole 32-bit address.
        [a] => u32::try_from(*a).ok()?,
        // a.b — a is the top octet, b the low 24 bits.
        [a, b] => {
            if *a > 0xff || *b > 0x00ff_ffff {
                return None;
            }
            ((*a as u32) << 24) | (*b as u32)
        }
        // a.b.c — a, b are octets, c the low 16 bits.
        [a, b, c] => {
            if *a > 0xff || *b > 0xff || *c > 0xffff {
                return None;
            }
            ((*a as u32) << 24) | ((*b as u32) << 16) | (*c as u32)
        }
        // a.b.c.d — standard dotted quad.
        [a, b, c, d] => {
            if [a, b, c, d].iter().any(|n| **n > 0xff) {
                return None;
            }
            ((*a as u32) << 24) | ((*b as u32) << 16) | ((*c as u32) << 8) | (*d as u32)
        }
        _ => return None,
    };
    Some(Ipv4Addr::from(addr))
}

/// Parse a URL host string into an `IpAddr` if it denotes a literal address —
/// including bracketed IPv6 (`[::1]`) and the alternate IPv4 encodings
/// (decimal/hex/octal) that bypass `url::Url`'s IP detection on non-special
/// schemes. Returns `None` for genuine DNS hostnames. Pure — no DNS lookup.
fn parse_host_as_ip(host: &str) -> Option<IpAddr> {
    // `host_str()` brackets IPv6 literals (e.g. `[::1]`); strip them so the
    // address parses.
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);

    // Standard dotted-decimal IPv4 or an IPv6 literal.
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Some(ip);
    }

    // Alternate IPv4 encodings left as opaque domain strings by `url::Url`.
    parse_numeric_ipv4(host).map(IpAddr::V4)
}

/// True if ANY resolved address is internal. Split out from the DNS lookup so it
/// can be unit-tested with synthetic address lists (no live DNS). See
/// `resolve_host` for the blocking lookup that feeds this.
fn any_resolved_ip_is_internal(addrs: &[IpAddr]) -> bool {
    addrs.iter().copied().any(ip_is_internal)
}

/// Blocking DNS resolution of a hostname to every address it maps to, via the
/// system resolver (`std::net`). Returns an empty vec on resolution failure so
/// callers fail open on transient DNS errors rather than blocking legitimate
/// adds. MUST run on a blocking thread (it blocks).
fn resolve_host(host: &str) -> Vec<IpAddr> {
    use std::net::ToSocketAddrs;
    (host, 0u16)
        .to_socket_addrs()
        .map(|iter| iter.map(|sa| sa.ip()).collect())
        .unwrap_or_default()
}

/// Extract the DNS hostname from a repo URL that still needs resolve-time SSRF
/// validation — i.e. a host that is NOT a literal/encoded IP (those are checked
/// synchronously in `validate_repo_url`). Returns `None` for IP literals or URLs
/// without a host.
fn host_to_resolve(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    if parse_host_as_ip(host).is_some() {
        return None;
    }
    Some(host.to_string())
}

/// Validate a repo URL's scheme and host to prevent SSRF.
///
/// Rejects unsupported schemes (only https/http/git/ssh are allowed) and any
/// host that resolves to an internal/private target — `localhost`, the cloud
/// metadata endpoint, and private/loopback/link-local/unique-local IP ranges
/// (OWASP SSRF Prevention Cheat Sheet). Literal, alternate-encoded
/// (decimal/hex/octal), and IPv6-embedded IPv4 forms are all covered here
/// synchronously; DNS hostnames are additionally resolve-checked at add-time
/// (see `add_repo`). The returned `Err` is the user-facing message.
fn validate_repo_url(url: &str) -> Result<(), String> {
    // Validate URL scheme to prevent SSRF via file:// or other unexpected schemes.
    let allowed_schemes = ["https", "http", "git", "ssh"];
    let parsed = match url::Url::parse(url) {
        Ok(p) if allowed_schemes.contains(&p.scheme()) => p,
        Ok(parsed) => {
            return Err(format!(
                "unsupported URL scheme '{}': allowed schemes are {}",
                parsed.scheme(),
                allowed_schemes.join(", ")
            ));
        }
        Err(e) => {
            return Err(format!("invalid URL '{url}': {e}"));
        }
    };

    // SSRF prevention: reject internal/private targets (OWASP SSRF Prevention
    // Cheat Sheet). `parse_host_as_ip` also catches alternate IPv4 encodings
    // (decimal/hex/octal) that `url::Url` leaves as opaque domain strings, and
    // `ip_is_internal` unwraps IPv6-embedded IPv4 (mapped/NAT64/6to4/compatible).
    if let Some(host) = parsed.host_str() {
        if host == "localhost" || host == "metadata.google.internal" {
            return Err(format!(
                "rejected hostname '{host}': internal addresses not allowed"
            ));
        }
        if let Some(ip) = parse_host_as_ip(host)
            && ip_is_internal(ip)
        {
            return Err(format!(
                "rejected IP '{ip}': private/loopback addresses not allowed"
            ));
        }
    }

    Ok(())
}

/// Whether a config-declared repo URL is safe to enqueue during a reload.
///
/// Applies the same synchronous SSRF checks as `add_repo` (`validate_repo_url`)
/// so repos loaded from `instance.toml` can't smuggle in an internal/private
/// target that the add-repo API would reject. DNS resolution is intentionally
/// not performed here — config reload must stay non-blocking — so only the
/// synchronous checks (scheme, literal/encoded internal IPs, localhost/metadata
/// hostnames) apply; that matches add_repo's pre-resolution gate.
///
/// Exposed `pub` so the daemon's startup config-repo enqueue path
/// (`server.rs`) can gate `instance.toml`-declared repos through the same SSRF
/// check before they are cloned/indexed, not just the reload path.
pub fn config_repo_url_allowed(url: &str) -> bool {
    validate_repo_url(url).is_ok()
}

/// POST /admin/api/repos — add a new repo.
pub async fn add_repo(
    _auth: AdminAuth,
    State(state): State<Arc<AdminState>>,
    Json(req): Json<AddRepoRequest>,
) -> Result<Json<MessageResponse>, (StatusCode, String)> {
    // Validate the URL scheme + host to prevent SSRF (file://, internal
    // hostnames, private/loopback IPs, alternate IPv4 encodings, IPv6-embedded
    // IPv4). See `validate_repo_url` — this part is pure (no DNS).
    validate_repo_url(&req.url).map_err(|e| (StatusCode::BAD_REQUEST, e))?;

    // SSRF defense-in-depth: for a DNS hostname (not a literal/encoded IP),
    // resolve it and reject if ANY resolved address is internal. Catches names
    // that point at internal IPs plus basic DNS-rebinding at add-time. The
    // lookup blocks, so it runs on a blocking thread (kept out of the pure
    // `validate_repo_url`).
    //
    // TOCTOU caveat: resolution happens here at add-time, so a name could later
    // re-resolve to an internal IP at fetch time. True fetch-time enforcement
    // (validating the connected IP when the indexer clones) is out of scope and
    // tracked separately.
    if let Some(host) = host_to_resolve(&req.url) {
        let resolved = tokio::task::spawn_blocking(move || resolve_host(&host))
            .await
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("task panicked: {e}"),
                )
            })?;
        if any_resolved_ip_is_internal(&resolved) {
            return Err((
                StatusCode::BAD_REQUEST,
                "rejected hostname: resolves to an internal address".to_string(),
            ));
        }
    }

    // Derive the jobs database path from the brain database path.
    let jobs_path = nestweaver_engine::sidecar_path(&state.db_path, ".jobs.sqlite");
    let repo_url = req.url.clone();
    let branch = req.branch.clone();

    tokio::task::spawn_blocking(move || {
        let queue = nestweaver_engine::jobs::JobQueue::open(&jobs_path).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("open job queue: {e}"),
            )
        })?;
        let repo_id = nestweaver_engine::jobs::canonical_repo_id(&repo_url);
        queue
            .upsert(
                &repo_id,
                &repo_url,
                nestweaver_engine::jobs::JobTrigger::Unindexed,
                branch.as_deref(),
            )
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("enqueue job: {e}"),
                )
            })?;
        Ok::<_, (StatusCode, String)>(())
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task panicked: {e}"),
        )
    })??;

    // Persist admin-added repos into instance config so scheduler/webhook
    // allowlisting survives daemon restarts.
    if let Some(config_path) = state.config_path.clone() {
        let repo_url = req.url.clone();
        let branch = req.branch.clone();
        tokio::task::spawn_blocking(move || {
            nestweaver_engine::append_repo_to_config_file(
                &config_path,
                &repo_url,
                branch.as_deref(),
            )
        })
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("task panicked: {e}"),
            )
        })?
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("persist repo config: {e}"),
            )
        })?;
    }

    // Update live scheduler so the new repo is polled without restart.
    if let Some(ref tx) = state.scheduler_tx {
        let repo_name = nestweaver_engine::pull::repo_name_from_url(&req.url);
        let _ = tx
            .send(nestweaver_engine::scheduler::SchedulerCommand::AddRepo {
                repo_id: repo_name,
                repo_url: req.url.clone(),
                poll_override: None,
                branch: req.branch.clone(),
            })
            .await;
    }

    // Update webhook allowed repos so pushes are accepted immediately.
    let canonical = nestweaver_engine::jobs::canonical_repo_id(&req.url);
    if let Some(ref lock) = state.webhook_allowed_repos
        && let Ok(mut guard) = lock.write()
        && let Some(ref mut set) = *guard
    {
        set.insert(canonical.clone());
    }

    // Update webhook branch map if a branch was specified.
    if let Some(ref branch) = req.branch
        && let Some(ref lock) = state.webhook_repo_branches
        && let Ok(mut guard) = lock.write()
    {
        guard.insert(canonical, branch.clone());
    }

    Ok(Json(MessageResponse {
        message: format!("repo {} queued for indexing", req.url),
    }))
}

/// DELETE /admin/api/repos/:id — remove a repo.
pub async fn remove_repo(
    _auth: AdminAuth,
    State(state): State<Arc<AdminState>>,
    Path(repo_uid): Path<String>,
) -> Result<Json<MessageResponse>, (StatusCode, String)> {
    let store = state.daemon_store.clone();
    let uid = repo_uid.clone();

    // Look up the repo URL before deletion so we can clean up scheduler
    // and webhook state afterwards.
    let store_for_lookup = state.daemon_store.clone();
    let uid_for_lookup = repo_uid.clone();
    let repo_url: Option<String> = tokio::task::spawn_blocking(move || {
        store_for_lookup
            .lookup_repo(&uid_for_lookup)
            .ok()
            .flatten()
            .map(|r| r.url)
    })
    .await
    .ok()
    .flatten();

    // Purge queued jobs FIRST so no new workers can claim while we delete.
    if let Some(ref url) = repo_url {
        let canonical = nestweaver_engine::jobs::canonical_repo_id(url);
        let jobs_path = nestweaver_engine::sidecar_path(&state.db_path, ".jobs.sqlite");
        let _ = tokio::task::spawn_blocking(move || {
            if let Ok(queue) = nestweaver_engine::jobs::JobQueue::open(&jobs_path) {
                let _ = queue.cancel_repo(&canonical);
            }
        })
        .await;
    }

    // Persist the removal before deleting graph data so a failed config write
    // cannot leave the next restart re-admitting the repo silently.
    if let (Some(config_path), Some(url)) = (state.config_path.clone(), repo_url.clone()) {
        tokio::task::spawn_blocking(move || {
            nestweaver_engine::remove_repo_from_config_file(&config_path, &url)
        })
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("task panicked: {e}"),
            )
        })?
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("persist repo removal: {e}"),
            )
        })?;
    }

    // Delete graph data under write mutex. An already-claimed worker will
    // also acquire this mutex before indexing; when it runs, it checks
    // whether the repo node still exists and skips if deleted.
    let write_mutex = state.write_mutex.clone();
    tokio::task::spawn_blocking(move || {
        let _guard = write_mutex.as_ref().map(|m| m.blocking_lock());
        store
            .bulk_delete_repo_files_and_symbols(&uid)
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("bulk_delete failed: {e}"),
                )
            })?;
        store.clear_repo_derived_nodes(&uid).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("clear_derived failed: {e}"),
            )
        })?;
        store.delete_repo_node(&uid).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("delete_repo_node failed: {e}"),
            )
        })?;
        Ok::<_, (StatusCode, String)>(())
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task panicked: {e}"),
        )
    })??;

    // Remove from live scheduler.
    if let Some(ref tx) = state.scheduler_tx {
        // The scheduler seeds repos with `repo_cfg.name.unwrap_or(repo_name_from_url(...))`.
        // To match that, look up the configured name from the instance config first.
        let url_derived = repo_url
            .as_deref()
            .map(nestweaver_engine::pull::repo_name_from_url)
            .unwrap_or_else(|| repo_uid.clone());
        let sched_id = if let Some(ref config_path) = state.config_path {
            nestweaver_engine::InstanceConfig::from_file(config_path)
                .ok()
                .and_then(|cfg| {
                    let canonical = repo_url
                        .as_deref()
                        .map(nestweaver_engine::jobs::canonical_repo_id)
                        .unwrap_or_default();
                    cfg.repos
                        .iter()
                        .find(|r| nestweaver_engine::jobs::canonical_repo_id(&r.url) == canonical)
                        .and_then(|r| r.name.clone())
                })
                .unwrap_or(url_derived)
        } else {
            url_derived
        };
        let _ = tx
            .send(nestweaver_engine::scheduler::SchedulerCommand::RemoveRepo {
                repo_id: sched_id.clone(),
            })
            .await;
        // Also try the URL-derived name in case the config name didn't match
        // (e.g., repo already removed from config, or name was customized).
        let url_fallback = repo_url
            .as_deref()
            .map(nestweaver_engine::pull::repo_name_from_url)
            .unwrap_or_default();
        if !url_fallback.is_empty() && url_fallback != sched_id {
            let _ = tx
                .send(nestweaver_engine::scheduler::SchedulerCommand::RemoveRepo {
                    repo_id: url_fallback,
                })
                .await;
        }
    }

    // Remove from webhook allowed repos.
    if let Some(ref url) = repo_url {
        let canonical = nestweaver_engine::jobs::canonical_repo_id(url);
        if let Some(ref lock) = state.webhook_allowed_repos
            && let Ok(mut guard) = lock.write()
            && let Some(ref mut set) = *guard
        {
            set.remove(&canonical);
        }
        if let Some(ref lock) = state.webhook_repo_branches
            && let Ok(mut guard) = lock.write()
        {
            guard.remove(&canonical);
        }
    }

    Ok(Json(MessageResponse {
        message: format!("repo {} removed", repo_uid),
    }))
}

/// POST /admin/api/repos/:id/reindex — trigger an immediate re-index.
pub async fn trigger_reindex(
    _auth: AdminAuth,
    State(state): State<Arc<AdminState>>,
    Path(repo_uid): Path<String>,
) -> Result<Json<MessageResponse>, (StatusCode, String)> {
    let store = state.daemon_store.clone();
    let jobs_path = nestweaver_engine::sidecar_path(&state.db_path, ".jobs.sqlite");
    let uid = repo_uid.clone();
    let branch_map = state.webhook_repo_branches.clone();

    tokio::task::spawn_blocking(move || {
        // Look up the repo URL from the store.
        let repo = store
            .lookup_repo(&uid)
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("lookup repo: {e}"),
                )
            })?
            .ok_or_else(|| (StatusCode::NOT_FOUND, format!("repo {} not found", uid)))?;

        let queue = nestweaver_engine::jobs::JobQueue::open(&jobs_path).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("open job queue: {e}"),
            )
        })?;
        let repo_id = nestweaver_engine::jobs::canonical_repo_id(&repo.url);
        let branch = configured_branch_for_repo(&branch_map, &repo_id);
        queue
            .upsert(
                &repo_id,
                &repo.url,
                nestweaver_engine::jobs::JobTrigger::Webhook,
                branch.as_deref(),
            )
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("enqueue job: {e}"),
                )
            })?;
        Ok::<_, (StatusCode, String)>(())
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task panicked: {e}"),
        )
    })??;

    Ok(Json(MessageResponse {
        message: format!("reindex queued for repo {}", repo_uid),
    }))
}

fn configured_branch_for_repo(
    branch_map: &Option<Arc<std::sync::RwLock<std::collections::HashMap<String, String>>>>,
    repo_id: &str,
) -> Option<String> {
    branch_map.as_ref().and_then(|branches| {
        branches
            .read()
            .ok()
            .and_then(|map| map.get(repo_id).cloned())
    })
}

// ── Queue management ───────────────────────────────────────────────────

/// GET /admin/api/queue — queue state.
pub async fn get_queue(_auth: AdminAuth, State(state): State<Arc<AdminState>>) -> Json<QueueInfo> {
    let depth = state.indexing_queue_depth.load(Ordering::Relaxed);
    let drained = state.drained.load(Ordering::Relaxed);

    // Read actual running jobs and pending count from the SQLite job queue.
    // Show pending jobs regardless of drain state so operators can see what
    // is waiting to be processed.
    let db_path = state.db_path.clone();
    let (running_jobs, pending_count): (Option<Vec<serde_json::Value>>, Option<i64>) =
        tokio::task::spawn_blocking(move || {
            let jobs_path = nestweaver_engine::sidecar_path(&db_path, ".jobs.sqlite");
            match nestweaver_engine::jobs::JobQueue::open(&jobs_path) {
                Ok(q) => {
                    let running = q.running_jobs().ok().map(|jobs| {
                        jobs.into_iter()
                            .map(|j| {
                                serde_json::json!({
                                    "repo": j.repo,
                                    "started_at": j.started_at,
                                    "duration_s": j.duration_s,
                                })
                            })
                            .collect()
                    });
                    let pending = q.queue_depth().ok().map(|d| d.pending);
                    (running, pending)
                }
                Err(_) => (None, None),
            }
        })
        .await
        .unwrap_or((None, None));

    Json(QueueInfo {
        depth,
        drained,
        pending: pending_count,
        by_priority: None,
        running: running_jobs,
    })
}

// ── Drain/Resume ───────────────────────────────────────────────────────

/// POST /admin/api/drain — stop workers from picking new jobs.
pub async fn drain(
    _auth: AdminAuth,
    State(state): State<Arc<AdminState>>,
) -> Json<MessageResponse> {
    state.drained.store(true, Ordering::SeqCst);
    tracing::info!("admin API: workers drained");
    Json(MessageResponse {
        message: "workers drained — in-flight jobs will finish, no new jobs picked".to_string(),
    })
}

/// POST /admin/api/resume — resume normal processing.
pub async fn resume(
    _auth: AdminAuth,
    State(state): State<Arc<AdminState>>,
) -> Json<MessageResponse> {
    state.drained.store(false, Ordering::SeqCst);
    tracing::info!("admin API: workers resumed");
    Json(MessageResponse {
        message: "workers resumed".to_string(),
    })
}

/// GET /admin/api/drain/status — current drain state.
pub async fn drain_status(
    _auth: AdminAuth,
    State(state): State<Arc<AdminState>>,
) -> Json<DrainStatus> {
    Json(DrainStatus {
        drained: state.drained.load(Ordering::Relaxed),
        active_reads: state.active_reads.load(Ordering::Relaxed),
        active_writes: state.active_writes.load(Ordering::Relaxed),
    })
}

// ── Dead letter ────────────────────────────────────────────────────────

/// GET /admin/api/dead-letter — list dead-letter entries.
pub async fn list_dead_letter(
    _auth: AdminAuth,
    State(state): State<Arc<AdminState>>,
) -> Result<Json<Vec<serde_json::Value>>, (StatusCode, String)> {
    let jobs_path = nestweaver_engine::sidecar_path(&state.db_path, ".jobs.sqlite");

    let entries = tokio::task::spawn_blocking(move || {
        let queue = nestweaver_engine::jobs::JobQueue::open(&jobs_path).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("open job queue: {e}"),
            )
        })?;
        let dead = queue.dead_letters().map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("dead_letters: {e}"),
            )
        })?;
        let values: Vec<serde_json::Value> = dead
            .into_iter()
            .map(|j| {
                serde_json::json!({
                    "id": j.id,
                    "repo_id": j.repo_id,
                    "repo_url": j.repo_url,
                    // Frontend-expected fields:
                    "repo": j.repo_id,
                    "error": j.error_msg,
                    "last_attempt": j.updated_at,
                    "attempts": j.attempt,
                    // Keep original fields for backwards compat:
                    "attempt": j.attempt,
                    "max_attempts": j.max_attempts,
                    "updated_at": j.updated_at,
                })
            })
            .collect();
        Ok::<_, (StatusCode, String)>(values)
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task panicked: {e}"),
        )
    })??;

    Ok(Json(entries))
}

/// POST /admin/api/dead-letter/:id/retry — retry a dead-letter entry.
///
/// The `:id` parameter is the integer primary key from the dead-letter listing,
/// not the `repo_id` string. This matches the `id` field in the JSON returned
/// by `GET /admin/api/dead-letter`.
pub async fn retry_dead_letter(
    _auth: AdminAuth,
    State(state): State<Arc<AdminState>>,
    Path(id): Path<String>,
) -> Result<Json<MessageResponse>, (StatusCode, String)> {
    let jobs_path = nestweaver_engine::sidecar_path(&state.db_path, ".jobs.sqlite");
    let job_id: i64 = id
        .parse()
        .map_err(|_| (StatusCode::BAD_REQUEST, format!("invalid job id: {id}")))?;

    let retried = tokio::task::spawn_blocking(move || {
        let queue = nestweaver_engine::jobs::JobQueue::open(&jobs_path).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("open job queue: {e}"),
            )
        })?;
        queue.reset_dead_letter_by_id(job_id).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("reset_dead_letter: {e}"),
            )
        })
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task panicked: {e}"),
        )
    })??;

    if retried {
        Ok(Json(MessageResponse {
            message: format!("dead-letter entry {} queued for retry", id),
        }))
    } else {
        Err((
            StatusCode::NOT_FOUND,
            format!("no dead-letter entry with id {id}"),
        ))
    }
}

/// DELETE /admin/api/dead-letter/:id — dismiss a dead-letter entry.
pub async fn dismiss_dead_letter(
    _auth: AdminAuth,
    State(state): State<Arc<AdminState>>,
    Path(id): Path<String>,
) -> Result<Json<MessageResponse>, (StatusCode, String)> {
    let jobs_path = nestweaver_engine::sidecar_path(&state.db_path, ".jobs.sqlite");
    let job_id: i64 = id
        .parse()
        .map_err(|_| (StatusCode::BAD_REQUEST, format!("invalid job id: {id}")))?;

    let dismissed = tokio::task::spawn_blocking(move || {
        let queue = nestweaver_engine::jobs::JobQueue::open(&jobs_path).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("open job queue: {e}"),
            )
        })?;
        queue.dismiss_dead_letter(job_id).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("dismiss_dead_letter: {e}"),
            )
        })
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task panicked: {e}"),
        )
    })??;

    if dismissed {
        Ok(Json(MessageResponse {
            message: format!("dead-letter entry {} dismissed", id),
        }))
    } else {
        Err((
            StatusCode::NOT_FOUND,
            format!("no dead-letter entry with id {id}"),
        ))
    }
}

// ── Config reload ──────────────────────────────────────────────────────

/// POST /admin/api/reload — hot-reload instance.toml.
pub async fn reload_config(
    _auth: AdminAuth,
    State(state): State<Arc<AdminState>>,
) -> Result<Json<MessageResponse>, (StatusCode, String)> {
    let Some(ref config_path) = state.config_path else {
        return Ok(Json(MessageResponse {
            message: "no config path configured — daemon started without --config".to_string(),
        }));
    };

    let path = config_path.clone();
    let store = state.daemon_store.clone();
    let db_path = state.db_path.clone();
    let message = tokio::task::spawn_blocking(move || {
        match nestweaver_engine::InstanceConfig::from_file(&path) {
            Ok(cfg) => {
                let repo_count = cfg.repos.len();
                tracing::info!(
                    path = %path.display(),
                    repos = repo_count,
                    "config reloaded from disk"
                );

                // ── Reconcile declared repos vs indexed repos ─────────
                let jobs_path = nestweaver_engine::sidecar_path(&db_path, ".jobs.sqlite");
                let queue = nestweaver_engine::jobs::JobQueue::open(&jobs_path).ok();

                // Collect declared repo URLs from config.
                let declared_urls: std::collections::HashSet<String> =
                    cfg.repos.iter().map(|r| r.url.clone()).collect();

                // Collect indexed repo URLs from the store.
                let indexed_urls: std::collections::HashSet<String> = store
                    .list_repos(None)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|r| r.url)
                    .collect();

                let mut new_repos = 0usize;
                let mut orphaned_repos = 0usize;
                let mut skipped_repos = 0usize;

                // New repos in config but not yet indexed: enqueue.
                for r in &cfg.repos {
                    if !indexed_urls.contains(&r.url) {
                        // Config-sourced repos bypass the add_repo API and its
                        // SSRF validation, so re-run the same synchronous URL
                        // checks here and refuse to enqueue any internal/private
                        // target declared in config. DNS resolution is skipped
                        // (reload must stay non-blocking); literal/encoded
                        // internal IPs and localhost/metadata hosts are still
                        // rejected.
                        if !config_repo_url_allowed(&r.url) {
                            tracing::warn!(
                                url = %r.url,
                                "config reload: skipping repo — URL rejected by SSRF guard"
                            );
                            skipped_repos += 1;
                            continue;
                        }
                        tracing::info!(url = %r.url, "config reload: new repo — queueing for indexing");
                        if let Some(ref q) = queue {
                            let repo_id = nestweaver_engine::jobs::canonical_repo_id(&r.url);
                            let _ = q.upsert(
                                &repo_id,
                                &r.url,
                                nestweaver_engine::jobs::JobTrigger::Unindexed,
                                r.branch.as_deref(),
                            );
                        }
                        new_repos += 1;
                    }
                }

                // Indexed repos no longer in config: log warning.
                for url in &indexed_urls {
                    if !declared_urls.contains(url) {
                        tracing::warn!(
                            url = %url,
                            "config reload: repo no longer in config (orphaned)"
                        );
                        orphaned_repos += 1;
                    }
                }

                let mut msg = format!(
                    "config reloaded from {} ({} repos configured)",
                    path.display(),
                    repo_count,
                );
                if new_repos > 0 {
                    msg.push_str(&format!(", {} new repos queued", new_repos));
                }
                if orphaned_repos > 0 {
                    msg.push_str(&format!(", {} orphaned repos", orphaned_repos));
                }
                if skipped_repos > 0 {
                    msg.push_str(&format!(
                        ", {} repos skipped (rejected URL)",
                        skipped_repos
                    ));
                }
                Ok(msg)
            }
            Err(e) => {
                tracing::error!(path = %path.display(), error = %e, "config reload failed");
                Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("failed to parse config: {e}"),
                ))
            }
        }
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task panicked: {e}"),
        )
    })??;

    // Notify the live scheduler so it picks up added/removed repos
    // without a daemon restart.
    if let Some(ref tx) = state.scheduler_tx
        && let Some(ref config_path) = state.config_path
        && let Ok(cfg) = nestweaver_engine::InstanceConfig::from_file(config_path)
    {
        let repos: Vec<_> = cfg
            .repos
            .iter()
            .map(|r| {
                let repo_name = r
                    .name
                    .clone()
                    .unwrap_or_else(|| nestweaver_engine::pull::repo_name_from_url(&r.url));
                let poll_override = r.poll.as_deref().and_then(|p| match p {
                    "never" => Some(nestweaver_engine::scheduler::PollOverride::Never),
                    "manual" => Some(nestweaver_engine::scheduler::PollOverride::Manual),
                    other => nestweaver_engine::config::parse_duration(other)
                        .map(nestweaver_engine::scheduler::PollOverride::Fixed),
                });
                (repo_name, r.url.clone(), poll_override, r.branch.clone())
            })
            .collect();
        let new_min_poll = nestweaver_engine::config::parse_duration(&cfg.server.indexing.min_poll);
        let new_max_poll = nestweaver_engine::config::parse_duration(&cfg.server.indexing.max_poll);
        let _ = tx
            .send(
                nestweaver_engine::scheduler::SchedulerCommand::ReloadConfig {
                    repos,
                    min_poll: new_min_poll,
                    max_poll: new_max_poll,
                },
            )
            .await;
    }

    // Update webhook state so new/changed repos take effect without restart.
    if let Some(ref config_path) = state.config_path
        && let Ok(cfg) = nestweaver_engine::InstanceConfig::from_file(config_path)
    {
        if let Some(ref lock) = state.webhook_allowed_repos {
            let new_allowed: std::collections::HashSet<String> = cfg
                .repos
                .iter()
                .filter(|r| r.poll.as_deref() != Some("manual"))
                .map(|r| nestweaver_engine::jobs::canonical_repo_id(&r.url))
                .collect();
            if let Ok(mut guard) = lock.write() {
                *guard = Some(new_allowed);
            }
        }
        if let Some(ref lock) = state.webhook_repo_branches {
            let new_branches: std::collections::HashMap<String, String> = cfg
                .repos
                .iter()
                .filter_map(|r| {
                    r.branch.as_ref().map(|b| {
                        (
                            nestweaver_engine::jobs::canonical_repo_id(&r.url),
                            b.clone(),
                        )
                    })
                })
                .collect();
            if let Ok(mut guard) = lock.write() {
                *guard = new_branches;
            }
        }
    }

    Ok(Json(MessageResponse { message }))
}

// ── Status ─────────────────────────────────────────────────────────────

/// GET /admin/api/status — full server status.
pub async fn get_status(
    _auth: AdminAuth,
    State(state): State<Arc<AdminState>>,
) -> Json<AdminStatus> {
    let store = state.daemon_store.clone();
    let (repo_count, symbol_count) = tokio::task::spawn_blocking(move || {
        let repos = store.list_repos(None).map(|r| r.len()).unwrap_or(0);
        let symbols = store.count_symbols().unwrap_or(0);
        (repos, symbols)
    })
    .await
    .unwrap_or((0, 0));

    // Count pending/running/dead-letter entries from the job queue. The
    // persisted queue is the operator-facing source of truth, especially while
    // workers are drained and the atomic worker-depth hint is zero.
    let db_path = state.db_path.clone();
    let (pending_count, dead_letter_count, running_count) =
        tokio::task::spawn_blocking(move || {
            let jobs_path = nestweaver_engine::sidecar_path(&db_path, ".jobs.sqlite");
            let queue = nestweaver_engine::jobs::JobQueue::open(&jobs_path).ok();
            let depth = queue.as_ref().and_then(|q| q.queue_depth().ok());
            let dead = depth.as_ref().map(|d| d.dead_letter as usize).unwrap_or(0);
            let running = depth.as_ref().map(|d| d.running as u32).unwrap_or(0);
            let pending = depth.as_ref().map(|d| d.pending as u32);
            (pending, dead, running)
        })
        .await
        .unwrap_or((None, 0, 0));

    let queue_depth =
        pending_count.unwrap_or_else(|| state.indexing_queue_depth.load(Ordering::Relaxed));

    Json(AdminStatus {
        instance_id: state.instance_id.clone(),
        uptime_seconds: state.start_time.elapsed().as_secs(),
        server_mode: true,
        repo_count,
        active_reads: state.active_reads.load(Ordering::Relaxed),
        active_writes: state.active_writes.load(Ordering::Relaxed),
        queue_depth,
        drained: state.drained.load(Ordering::Relaxed),
        version: env!("CARGO_PKG_VERSION").to_string(),
        repos: RepoStats {
            total: repo_count,
            indexed: repo_count,
            stale: 0,
            dead_letter: dead_letter_count,
        },
        symbols: SymbolStats {
            total: symbol_count,
        },
        queue: QueueStats {
            pending: queue_depth,
            running: running_count,
            dead_letter: dead_letter_count,
        },
    })
}

// ── Device-flow authentication (OAuth 2.0 Device Grant, RFC 8628) ──────

/// How long a device grant stays valid before it must be re-requested.
const DEVICE_CODE_TTL_SECS: u64 = 600;
/// Minimum interval (seconds) the client should wait between token polls.
const DEVICE_POLL_INTERVAL_SECS: u64 = 5;
/// Unambiguous alphabet for user codes — omits easily confused characters
/// (0/O, 1/I/L) so codes are easy to read aloud and type.
const USER_CODE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

/// Per-IP request budget (per minute) for the unauthenticated `/auth/device`
/// and `/auth/token` endpoints. A legitimate device-flow client polls `/token`
/// every `DEVICE_POLL_INTERVAL_SECS` (~12/min), so this leaves ample headroom
/// while throttling floods.
pub const AUTH_RATE_PER_MIN: u64 = 60;
/// Upper bound on the number of distinct client keys the auth rate limiter
/// tracks. Prevents the limiter map from becoming its own unbounded-growth DoS
/// when an attacker rotates source IPs.
pub const AUTH_RATE_MAX_KEYS: usize = 4096;
/// Max request body accepted on the `/auth` router. Device-flow bodies are a
/// tiny JSON object (`device_code`/`user_code`); 4 KiB is generous.
pub const AUTH_BODY_LIMIT_BYTES: usize = 4096;

struct AuthTokenBucket {
    tokens: f64,
    last_refill: std::time::Instant,
}

/// Bounded, per-client token-bucket rate limiter for the public device-flow
/// endpoints. Mirrors the MCP `HttpRateLimiter` token-bucket math but caps the
/// number of tracked keys so a flood of distinct source IPs can't turn the
/// limiter itself into an unbounded-growth leak (we'd just move the DoS).
///
/// Pure and synchronous so the refill/eviction logic is unit-testable with an
/// injectable clock.
pub struct AuthRateLimiter {
    buckets: std::sync::Mutex<std::collections::HashMap<String, AuthTokenBucket>>,
    capacity: f64,
    refill_per_sec: f64,
    max_keys: usize,
    clock: Arc<dyn Fn() -> std::time::Instant + Send + Sync>,
}

impl AuthRateLimiter {
    pub fn new(requests_per_min: u64, max_keys: usize) -> Self {
        Self::new_with_clock(
            requests_per_min,
            max_keys,
            Arc::new(std::time::Instant::now),
        )
    }

    fn new_with_clock(
        requests_per_min: u64,
        max_keys: usize,
        clock: Arc<dyn Fn() -> std::time::Instant + Send + Sync>,
    ) -> Self {
        Self {
            buckets: std::sync::Mutex::new(std::collections::HashMap::new()),
            capacity: requests_per_min as f64,
            refill_per_sec: requests_per_min as f64 / 60.0,
            max_keys,
            clock,
        }
    }

    /// Consume one token for `key`. Returns `true` if the request is allowed.
    ///
    /// When the tracked-key cap is reached and `key` is new, fully-refilled
    /// (idle) buckets are evicted first; if the map is still full the request is
    /// rejected rather than inserting an unbounded new key.
    pub fn check(&self, key: &str) -> bool {
        let now = (self.clock)();
        let mut buckets = self.buckets.lock().unwrap();

        if buckets.len() >= self.max_keys && !buckets.contains_key(key) {
            // Drop buckets that have fully refilled — they carry no state worth
            // keeping and freeing them keeps the map bounded.
            let cap = self.capacity;
            let refill = self.refill_per_sec;
            buckets.retain(|_, b| {
                let elapsed = now.duration_since(b.last_refill).as_secs_f64();
                (b.tokens + elapsed * refill) < cap
            });
            if buckets.len() >= self.max_keys {
                return false;
            }
        }

        let bucket = buckets
            .entry(key.to_string())
            .or_insert_with(|| AuthTokenBucket {
                tokens: self.capacity,
                last_refill: now,
            });
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        bucket.last_refill = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Derive the rate-limit key for an inbound `/auth` request.
///
/// Prefers the direct peer address (`ConnectInfo`, unspoofable) when the server
/// wired it; otherwise falls back to a reverse-proxy-supplied client IP
/// (`X-Forwarded-For`/`X-Real-IP`); if neither is available the key collapses to
/// a single global bucket, which degrades the per-IP limit to a global rate cap
/// on `/auth` (the documented fallback when no peer-addr source exists).
fn auth_rate_limit_key(req: &axum::extract::Request) -> String {
    if let Some(ci) = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
    {
        return format!("ip:{}", ci.0.ip());
    }
    if let Some(ip) = req
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return format!("xff:{ip}");
    }
    if let Some(ip) = req
        .headers()
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return format!("xrip:{ip}");
    }
    "global".to_string()
}

/// Axum middleware enforcing [`AuthRateLimiter`] on the public device-flow
/// endpoints. On rejection, the token endpoint returns the RFC 8628 `slow_down`
/// error (so polling clients back off); other endpoints get a plain 429.
pub async fn auth_rate_limit(
    limiter: Arc<AuthRateLimiter>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let key = auth_rate_limit_key(&req);
    if !limiter.check(&key) {
        if req.uri().path().ends_with("/token") {
            // RFC 8628 §3.5: tell polling clients to slow down.
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(DeviceErrorResponse {
                    error: "slow_down".to_string(),
                }),
            )
                .into_response();
        }
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "rate limit exceeded; slow down".to_string(),
        )
            .into_response();
    }
    next.run(req).await
}

#[derive(Serialize)]
pub struct DeviceAuthResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: String,
    pub expires_in: u64,
    pub interval: u64,
}

#[derive(Deserialize)]
pub struct DeviceTokenRequest {
    pub device_code: String,
}

#[derive(Serialize)]
pub struct DeviceTokenResponse {
    pub access_token: String,
}

#[derive(Serialize)]
pub struct DeviceErrorResponse {
    pub error: String,
}

#[derive(Deserialize)]
pub struct DeviceApproveRequest {
    pub user_code: String,
}

/// Number of characters in a generated user code.
const USER_CODE_LEN: usize = 8;
/// Hard cap on concurrently-pending device grants. The map is also TTL-pruned,
/// but the cap bounds memory against an unauthenticated flood on `/auth/device`
/// (the endpoint is public, so without this it could grow without bound).
const MAX_PENDING_DEVICES: usize = 1024;
/// Bound on how many times we re-roll a colliding `user_code` before giving up.
/// With a 30^8 space and ≤1024 pending grants, a single roll almost never
/// collides; the cap just guarantees termination.
const USER_CODE_MAX_ATTEMPTS: usize = 16;

/// Generate a short, human-readable user code (8 chars from an unambiguous
/// uppercase-alnum alphabet). Randomness comes from v4 UUIDs (getrandom-backed)
/// so we don't pull in an extra RNG dependency.
///
/// Bytes are mapped to the alphabet by **rejection sampling**, not `% len`: the
/// alphabet has 30 symbols and 256 is not a multiple of 30, so a plain modulo
/// would bias the first 16 symbols. We discard any byte ≥ the largest multiple
/// of the alphabet length that fits in a `u8` (240), leaving a uniform mapping.
fn generate_user_code() -> String {
    let alpha_len = USER_CODE_ALPHABET.len() as u16; // 30
    // Largest multiple of the alphabet length that fits in a u8 (240). Bytes at
    // or above this are rejected to avoid modulo bias.
    let reject_threshold = (256 / alpha_len * alpha_len) as u8;

    let mut out = String::with_capacity(USER_CODE_LEN);
    while out.len() < USER_CODE_LEN {
        // Pull a fresh batch of CSPRNG bytes; UUID v4 is getrandom-backed.
        for &b in uuid::Uuid::new_v4().into_bytes().iter() {
            if out.len() >= USER_CODE_LEN {
                break;
            }
            if b < reject_threshold {
                out.push(USER_CODE_ALPHABET[(b % alpha_len as u8) as usize] as char);
            }
        }
    }
    out
}

/// Generate a `user_code` that is unique among the currently-pending grants
/// (compared canonically). Returns `None` if a unique code couldn't be found
/// within `USER_CODE_MAX_ATTEMPTS` rolls (practically impossible at our cap).
fn generate_unique_user_code(
    map: &std::collections::HashMap<String, PendingDevice>,
) -> Option<String> {
    let taken: std::collections::HashSet<String> = map
        .values()
        .map(|p| normalize_user_code(&p.user_code))
        .collect();
    for _ in 0..USER_CODE_MAX_ATTEMPTS {
        let code = generate_user_code();
        if !taken.contains(&normalize_user_code(&code)) {
            return Some(code);
        }
    }
    None
}

/// Canonicalize a user code for comparison: uppercase, keep only alphanumerics
/// (so an admin can paste `WDJB-MJHT` or `wdjb mjht` and still match).
fn normalize_user_code(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// Drop expired grants so the pending map can't grow without bound.
fn prune_expired(map: &mut std::collections::HashMap<String, PendingDevice>) {
    let now = std::time::Instant::now();
    map.retain(|_, v| v.expires_at > now);
}

/// Build an RFC 8628 token-endpoint error response (`400` + `{ "error": ... }`).
fn device_error(error: &str) -> (StatusCode, Json<DeviceErrorResponse>) {
    (
        StatusCode::BAD_REQUEST,
        Json(DeviceErrorResponse {
            error: error.to_string(),
        }),
    )
}

/// Derive the externally-visible base URL of this server from request headers,
/// honoring a reverse-proxy `X-Forwarded-Proto`. Used to build the verification
/// URIs handed back to the developer.
fn verification_base(headers: &axum::http::HeaderMap) -> String {
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("127.0.0.1");
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("http");
    format!("{scheme}://{host}")
}

/// POST /auth/device — start a device-authorization grant (no auth).
///
/// Returns a `device_code` (opaque) and a `user_code` (shown to the developer),
/// along with the verification URIs and polling parameters per RFC 8628 §3.2.
pub async fn device_authorize(
    State(state): State<Arc<AdminState>>,
    headers: axum::http::HeaderMap,
) -> Result<Json<DeviceAuthResponse>, (StatusCode, String)> {
    let device_code = uuid::Uuid::new_v4().to_string();

    let expires_at =
        std::time::Instant::now() + std::time::Duration::from_secs(DEVICE_CODE_TTL_SECS);

    let user_code = {
        let mut map = state.device_flow.write().await;
        prune_expired(&mut map);

        // Bound the pending map: the endpoint is unauthenticated, so without a
        // cap a flood could grow it without limit (TTL pruning alone lags). Once
        // pruning can't free a slot, shed load rather than grow.
        if map.len() >= MAX_PENDING_DEVICES {
            return Err((
                StatusCode::TOO_MANY_REQUESTS,
                "device authorization capacity reached; retry later".to_string(),
            ));
        }

        // Pick a code that doesn't collide with another pending grant, so an
        // admin approving a code can never match two devices.
        let Some(code) = generate_unique_user_code(&map) else {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "could not allocate a unique user code; retry later".to_string(),
            ));
        };
        map.insert(
            device_code.clone(),
            PendingDevice {
                user_code: code.clone(),
                expires_at,
                approved_token: None,
            },
        );
        code
    };

    let base = verification_base(&headers);
    let verification_uri = format!("{base}/admin");
    let verification_uri_complete = format!("{base}/admin?user_code={user_code}");

    Ok(Json(DeviceAuthResponse {
        device_code,
        user_code,
        verification_uri,
        verification_uri_complete,
        expires_in: DEVICE_CODE_TTL_SECS,
        interval: DEVICE_POLL_INTERVAL_SECS,
    }))
}

/// POST /auth/token — exchange a `device_code` for the granted token (no auth).
///
/// RFC 8628 §3.5: unknown/expired → `expired_token`; pending approval →
/// `authorization_pending`; approved → `200 { access_token }` (one-shot).
pub async fn device_token(
    State(state): State<Arc<AdminState>>,
    Json(req): Json<DeviceTokenRequest>,
) -> Result<Json<DeviceTokenResponse>, (StatusCode, Json<DeviceErrorResponse>)> {
    let mut map = state.device_flow.write().await;
    prune_expired(&mut map);

    // After pruning, a missing entry means it was never issued or has expired.
    let Some(entry) = map.get(&req.device_code) else {
        return Err(device_error("expired_token"));
    };

    match entry.approved_token.clone() {
        None => Err(device_error("authorization_pending")),
        Some(token) => {
            // Single use: remove the grant once the token is handed out.
            map.remove(&req.device_code);
            Ok(Json(DeviceTokenResponse {
                access_token: token,
            }))
        }
    }
}

/// POST /auth/device/approve — admin approves a pending grant (admin auth).
///
/// Looks up the pending grant by `user_code` and attaches the configured org
/// query token (org-wide read token per the security model). The developer's
/// next `POST /auth/token` then succeeds.
pub async fn device_approve(
    _auth: AdminAuth,
    State(state): State<Arc<AdminState>>,
    Json(req): Json<DeviceApproveRequest>,
) -> Result<Json<MessageResponse>, (StatusCode, String)> {
    let wanted = normalize_user_code(&req.user_code);
    if wanted.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "user_code required".to_string()));
    }
    // Refuse to hand out an empty token: when the server has no query token
    // configured, approval would otherwise grant `""`, silently authenticating
    // the developer as the empty (no-auth) principal.
    //
    // 409 Conflict (vs. 503): this is a misconfiguration of the approval target,
    // not a transient outage — retrying without reconfiguring the server's query
    // token will never succeed, so a 4xx is the honest class. Kept as 409 to
    // match the ambiguous-user_code conflict below and the existing test.
    let Some(granted) = state.auth_token.clone().filter(|t| !t.is_empty()) else {
        return Err((
            StatusCode::CONFLICT,
            "server has no query token configured; device flow unavailable".to_string(),
        ));
    };

    let mut map = state.device_flow.write().await;
    prune_expired(&mut map);

    // Collect every grant whose code matches. `user_code`s are generated to be
    // unique among pending grants, so >1 match means an invariant broke; treat
    // it as an error rather than approving an arbitrary device.
    let matched: Vec<String> = map
        .iter()
        .filter(|(_, entry)| normalize_user_code(&entry.user_code) == wanted)
        .map(|(device_code, _)| device_code.clone())
        .collect();

    match matched.as_slice() {
        [] => Err((
            StatusCode::NOT_FOUND,
            "no pending device with that code".to_string(),
        )),
        [device_code] => {
            if let Some(entry) = map.get_mut(device_code) {
                entry.approved_token = Some(granted);
            }
            Ok(Json(MessageResponse {
                message: "device approved".to_string(),
            }))
        }
        _ => Err((
            StatusCode::CONFLICT,
            "ambiguous user_code: multiple pending grants match".to_string(),
        )),
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use axum::{
        Router,
        routing::{get, post},
    };
    use tower::ServiceExt;

    fn test_admin_state() -> Arc<AdminState> {
        admin_state_with_auth(Some("test-query-token".to_string()))
    }

    fn admin_state_with_auth(auth_token: Option<String>) -> Arc<AdminState> {
        let dir = tempfile::tempdir().expect("create tempdir");
        let db_path = dir.path().join("test.lbug");
        let store =
            nestweaver_store::GraphStore::open_or_create(&db_path).expect("open test store");
        let db_path_clone = db_path.clone();
        // Leak the tempdir so it lives as long as the store.
        std::mem::forget(dir);
        Arc::new(AdminState {
            admin_token: "test-admin-token".to_string(),
            auth_token,
            device_flow: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            daemon_store: Arc::new(store),
            instance_id: "test".to_string(),
            start_time: std::time::Instant::now(),
            active_reads: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            active_writes: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            drained: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            indexing_queue_depth: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            db_path: db_path_clone,
            config_path: None,
            scheduler_tx: None,
            webhook_allowed_repos: None,
            webhook_repo_branches: None,
            write_mutex: None,
        })
    }

    fn test_router() -> Router {
        let state = test_admin_state();
        Router::new()
            .route("/admin/api/status", get(get_status))
            .route("/admin/api/repos", get(list_repos))
            .route("/admin/api/queue", get(get_queue))
            .route("/admin/api/drain/status", get(drain_status))
            .with_state(state)
    }

    #[tokio::test]
    async fn status_requires_admin_token() {
        let app = test_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/admin/api/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn status_with_valid_admin_token() {
        let app = test_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/admin/api/status")
                    .header("Authorization", "Bearer test-admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn configured_branch_for_repo_reads_shared_branch_map() {
        let mut map = std::collections::HashMap::new();
        map.insert("github.com/org/repo".to_string(), "release".to_string());
        let branch_map = Some(Arc::new(std::sync::RwLock::new(map)));

        assert_eq!(
            configured_branch_for_repo(&branch_map, "github.com/org/repo"),
            Some("release".to_string())
        );
        assert_eq!(configured_branch_for_repo(&branch_map, "missing"), None);
    }

    #[tokio::test]
    async fn repos_returns_json() {
        let app = test_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/admin/api/repos")
                    .header("Authorization", "Bearer test-admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn drain_status_shows_not_drained() {
        let app = test_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/admin/api/drain/status")
                    .header("Authorization", "Bearer test-admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let status: DrainStatus = serde_json::from_slice(&body).unwrap();
        assert!(!status.drained);
    }

    #[tokio::test]
    async fn status_uses_persisted_pending_queue_count() {
        let state = test_admin_state();
        let jobs_path = nestweaver_engine::sidecar_path(&state.db_path, ".jobs.sqlite");
        let queue = nestweaver_engine::jobs::JobQueue::open(&jobs_path).unwrap();
        queue
            .upsert(
                "repo-a",
                "file:///tmp/repo-a",
                nestweaver_engine::jobs::JobTrigger::Webhook,
                None,
            )
            .unwrap();

        let app = Router::new()
            .route("/admin/api/status", get(get_status))
            .with_state(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/admin/api/status")
                    .header("Authorization", "Bearer test-admin-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let status: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(status["queue_depth"], 1);
        assert_eq!(status["queue"]["pending"], 1);
    }

    #[tokio::test]
    async fn add_repo_persists_instance_config() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let config_path = dir.path().join("instance.toml");
        std::fs::write(
            &config_path,
            r#"
instance_id = "test-instance"

[snapshot_storage]
backend = "local"
path = "/tmp/snapshots"

[workspace]
backend = "local"
path = "/tmp/workspace"

[inference]
endpoint = "http://localhost:8080"
embedding_model = "text-embedding-3-small"
summary_model = "gpt-4o-mini"

[git]
credential_method = "ssh"

[[repos]]
url = "https://github.com/example/existing"
"#,
        )
        .unwrap();
        let store =
            nestweaver_store::GraphStore::open_or_create(&db_path).expect("open test store");
        let state = Arc::new(AdminState {
            admin_token: "test-admin-token".to_string(),
            auth_token: Some("test-query-token".to_string()),
            device_flow: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            daemon_store: Arc::new(store),
            instance_id: "test".to_string(),
            start_time: std::time::Instant::now(),
            active_reads: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            active_writes: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            drained: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            indexing_queue_depth: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            db_path,
            config_path: Some(config_path.clone()),
            scheduler_tx: None,
            webhook_allowed_repos: None,
            webhook_repo_branches: None,
            write_mutex: None,
        });

        let app = Router::new()
            .route("/admin/api/repos", post(add_repo))
            .with_state(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/api/repos")
                    .header("Authorization", "Bearer test-admin-token")
                    .header("Content-Type", "application/json")
                    .body(Body::from(
                        r#"{"url":"https://github.com/example/new","branch":"main"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let cfg = nestweaver_engine::InstanceConfig::from_file(&config_path).unwrap();
        assert!(cfg.repos.iter().any(|repo| {
            repo.url == "https://github.com/example/new" && repo.branch.as_deref() == Some("main")
        }));
    }

    #[test]
    fn validate_repo_url_rejects_internal_targets() {
        // Bare IPv4 hosts, wrapped in an allowed scheme, must be rejected.
        for host in [
            "127.0.0.1",
            "10.0.0.1",
            "192.168.1.1",
            "169.254.1.1",
            "0.0.0.0",
        ] {
            let url = format!("https://{host}/repo");
            assert!(
                validate_repo_url(&url).is_err(),
                "expected {url} to be rejected"
            );
        }

        // Bare IPv6 hosts (bracketed in URLs) must be rejected: loopback,
        // link-local, unique-local, and IPv4-mapped private.
        for host in ["::1", "fe80::1", "fd00::1", "fc00::1", "::ffff:192.168.1.1"] {
            let url = format!("https://[{host}]/repo");
            assert!(
                validate_repo_url(&url).is_err(),
                "expected {url} to be rejected"
            );
        }

        // Full URLs: unspecified IPv6, disallowed scheme, internal hostname.
        for url in ["http://[::]/", "file:///etc/passwd", "git://localhost/repo"] {
            assert!(
                validate_repo_url(url).is_err(),
                "expected {url} to be rejected"
            );
        }
    }

    #[test]
    fn validate_repo_url_accepts_public_https() {
        for url in [
            "https://github.com/acme/api.git",
            "https://gitlab.com/acme/widgets.git",
        ] {
            assert!(
                validate_repo_url(url).is_ok(),
                "expected {url} to be accepted, got {:?}",
                validate_repo_url(url)
            );
        }
    }

    #[test]
    fn parse_numeric_ipv4_decodes_alternate_encodings() {
        // Loopback in decimal / hex / octal — the classic SSRF bypasses.
        assert_eq!(
            parse_numeric_ipv4("2130706433"),
            Some(Ipv4Addr::new(127, 0, 0, 1))
        );
        assert_eq!(
            parse_numeric_ipv4("0x7f000001"),
            Some(Ipv4Addr::new(127, 0, 0, 1))
        );
        assert_eq!(
            parse_numeric_ipv4("017700000001"),
            Some(Ipv4Addr::new(127, 0, 0, 1))
        );
        // A public address in hex stays public.
        assert_eq!(
            parse_numeric_ipv4("0x08080808"),
            Some(Ipv4Addr::new(8, 8, 8, 8))
        );
        // Mixed-base dotted (inet_aton) forms.
        assert_eq!(
            parse_numeric_ipv4("0x7f.0.0.1"),
            Some(Ipv4Addr::new(127, 0, 0, 1))
        );
        assert_eq!(
            parse_numeric_ipv4("127.1"),
            Some(Ipv4Addr::new(127, 0, 0, 1))
        );
        // Genuine DNS names and out-of-range numbers are not IPs.
        assert_eq!(parse_numeric_ipv4("github.com"), None);
        assert_eq!(parse_numeric_ipv4("0x1_0000_0000"), None);
        assert_eq!(parse_numeric_ipv4("1.2.3.4.5"), None);
        assert_eq!(parse_numeric_ipv4("256.0.0.1"), None);
    }

    #[test]
    fn parse_host_as_ip_handles_literals_and_encodings() {
        // Alternate-encoded loopback resolves to an IpAddr we can range-check.
        for host in ["2130706433", "0x7f000001", "017700000001"] {
            assert_eq!(
                parse_host_as_ip(host),
                Some(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))),
                "host {host} should decode to 127.0.0.1"
            );
        }
        // Public hex encoding decodes to the real public address.
        assert_eq!(
            parse_host_as_ip("0x08080808"),
            Some(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)))
        );
        // Bracketed IPv6 literal (as `host_str()` returns it).
        assert_eq!(
            parse_host_as_ip("[::1]"),
            Some(IpAddr::V6(Ipv6Addr::LOCALHOST))
        );
        // Plain dotted quad.
        assert_eq!(
            parse_host_as_ip("10.0.0.1"),
            Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)))
        );
        // A real DNS name is not an IP literal.
        assert_eq!(parse_host_as_ip("github.com"), None);
    }

    #[test]
    fn ip_is_internal_flags_encoded_and_embedded_loopback() {
        // Alternate-encoded loopback (decimal/hex/octal) → internal.
        for host in ["2130706433", "0x7f000001", "017700000001"] {
            let ip = parse_host_as_ip(host).expect("decodes to an IP");
            assert!(ip_is_internal(ip), "{host} ({ip}) should be internal");
        }

        // IPv6 transition forms embedding an internal V4 → internal.
        for s in [
            "64:ff9b::7f00:1",    // NAT64 -> 127.0.0.1
            "2002:7f00:1::",      // 6to4 -> 127.0.0.1
            "::127.0.0.1",        // v4-compatible -> 127.0.0.1
            "64:ff9b::a00:1",     // NAT64 -> 10.0.0.1 (private)
            "2002:c0a8:101::",    // 6to4 -> 192.168.1.1 (private)
            "::ffff:192.168.1.1", // v4-mapped private
        ] {
            let ip: IpAddr = s.parse().expect("valid IPv6 literal");
            assert!(ip_is_internal(ip), "{s} should be internal");
        }

        // Raw internal V4/V6 literals stay flagged.
        for s in [
            "127.0.0.1",
            "10.0.0.1",
            "169.254.1.1",
            "0.0.0.0",
            "::1",
            "fe80::1",
            "fc00::1",
        ] {
            let ip: IpAddr = s.parse().expect("valid IP literal");
            assert!(ip_is_internal(ip), "{s} should be internal");
        }
    }

    #[test]
    fn ip_is_internal_allows_public_addresses() {
        // Public V4/V6 and the embedded forms carrying public V4 are allowed.
        for s in [
            "8.8.8.8",
            "1.1.1.1",
            "2001:4860:4860::8888", // Google public DNS, IPv6
            "64:ff9b::808:808",     // NAT64 -> 8.8.8.8 (public)
            "2002:0808:0808::",     // 6to4 -> 8.8.8.8 (public)
            "::8.8.8.8",            // v4-compatible -> 8.8.8.8 (public)
        ] {
            let ip: IpAddr = s.parse().expect("valid IP literal");
            assert!(!ip_is_internal(ip), "{s} should be allowed (public)");
        }
        // The decimal/hex public encoding too.
        assert!(!ip_is_internal(
            parse_host_as_ip("0x08080808").expect("decodes")
        ));
    }

    #[test]
    fn validate_repo_url_rejects_alternate_encoded_internal_hosts() {
        // git/ssh are non-special schemes, so `url::Url` keeps these numeric
        // hosts as opaque strings — exactly the bypass these checks close.
        for url in [
            "git://2130706433/repo",
            "ssh://0x7f000001/repo",
            "git://017700000001/repo",
            "git://[64:ff9b::7f00:1]/repo",
            "git://[2002:7f00:1::]/repo",
            "git://[::127.0.0.1]/repo",
        ] {
            assert!(
                validate_repo_url(url).is_err(),
                "expected {url} to be rejected"
            );
        }
    }

    #[test]
    fn any_resolved_ip_is_internal_checks_each_address() {
        // Mixed list with one internal address → rejected.
        let mixed = [
            IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)),
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
        ];
        assert!(any_resolved_ip_is_internal(&mixed));

        // All-public list → allowed.
        let public = [
            IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
            IpAddr::V6("2001:4860:4860::8888".parse().unwrap()),
        ];
        assert!(!any_resolved_ip_is_internal(&public));

        // Empty (resolution failure / fail-open) → not internal.
        assert!(!any_resolved_ip_is_internal(&[]));
    }

    #[test]
    fn host_to_resolve_skips_ip_literals() {
        // DNS names need resolve-time validation.
        assert_eq!(
            host_to_resolve("https://github.com/acme/api.git"),
            Some("github.com".to_string())
        );
        // Literal and alternate-encoded IPs are already checked synchronously,
        // so they need no DNS lookup.
        assert_eq!(host_to_resolve("https://93.184.216.34/repo"), None);
        assert_eq!(host_to_resolve("git://2130706433/repo"), None);
        assert_eq!(host_to_resolve("https://[2606:2800:220:1::1]/repo"), None);
    }

    // ── Device flow ─────────────────────────────────────────────────────

    fn device_router(state: Arc<AdminState>) -> Router {
        Router::new()
            .route("/auth/device", post(device_authorize))
            .route("/auth/token", post(device_token))
            .route("/auth/device/approve", post(device_approve))
            .with_state(state)
    }

    async fn post_json(
        app: &Router,
        uri: &str,
        token: Option<&str>,
        body: &str,
    ) -> (StatusCode, serde_json::Value) {
        let mut builder = Request::builder()
            .method("POST")
            .uri(uri)
            .header("Content-Type", "application/json");
        if let Some(t) = token {
            builder = builder.header("Authorization", format!("Bearer {t}"));
        }
        let resp = app
            .clone()
            .oneshot(builder.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    #[test]
    fn generate_user_code_is_eight_unambiguous_chars() {
        let code = generate_user_code();
        assert_eq!(code.len(), 8);
        assert!(
            code.bytes().all(|b| USER_CODE_ALPHABET.contains(&b)),
            "code {code} contains chars outside the alphabet"
        );
    }

    #[test]
    fn normalize_user_code_strips_separators_and_uppercases() {
        assert_eq!(normalize_user_code("wdjb-mjht"), "WDJBMJHT");
        assert_eq!(normalize_user_code(" ab cd "), "ABCD");
    }

    #[tokio::test]
    async fn device_flow_request_pending_approve_token() {
        let app = device_router(test_admin_state());

        // 1. Request a device code.
        let (status, auth) = post_json(&app, "/auth/device", None, "{}").await;
        assert_eq!(status, StatusCode::OK);
        let device_code = auth["device_code"].as_str().unwrap().to_string();
        let user_code = auth["user_code"].as_str().unwrap().to_string();
        assert_eq!(auth["expires_in"], 600);
        assert_eq!(auth["interval"], 5);
        assert!(
            auth["verification_uri_complete"]
                .as_str()
                .unwrap()
                .contains(&user_code)
        );

        // 2. Polling before approval → authorization_pending.
        let (status, body) = post_json(
            &app,
            "/auth/token",
            None,
            &format!(r#"{{"device_code":"{device_code}"}}"#),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "authorization_pending");

        // 3. Admin approves the user code.
        let (status, _) = post_json(
            &app,
            "/auth/device/approve",
            Some("test-admin-token"),
            &format!(r#"{{"user_code":"{user_code}"}}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // 4. Polling after approval → access token (the org query token).
        let (status, body) = post_json(
            &app,
            "/auth/token",
            None,
            &format!(r#"{{"device_code":"{device_code}"}}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["access_token"], "test-query-token");

        // 5. The grant is single-use: a second poll fails as expired.
        let (status, body) = post_json(
            &app,
            "/auth/token",
            None,
            &format!(r#"{{"device_code":"{device_code}"}}"#),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "expired_token");
    }

    #[tokio::test]
    async fn device_token_unknown_code_is_expired() {
        let app = device_router(test_admin_state());
        let (status, body) = post_json(
            &app,
            "/auth/token",
            None,
            r#"{"device_code":"does-not-exist"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "expired_token");
    }

    #[tokio::test]
    async fn device_token_after_expiry_is_expired() {
        let state = test_admin_state();
        // Insert a grant that already expired and was approved — pruning must
        // still treat it as expired.
        {
            let mut map = state.device_flow.write().await;
            map.insert(
                "expired-code".to_string(),
                PendingDevice {
                    user_code: "ABCD1234".to_string(),
                    expires_at: std::time::Instant::now() - std::time::Duration::from_secs(1),
                    approved_token: Some("test-query-token".to_string()),
                },
            );
        }
        let app = device_router(state);
        let (status, body) = post_json(
            &app,
            "/auth/token",
            None,
            r#"{"device_code":"expired-code"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "expired_token");
    }

    #[tokio::test]
    async fn device_approve_requires_admin_token() {
        let app = device_router(test_admin_state());
        let (status, _) = post_json(
            &app,
            "/auth/device/approve",
            None,
            r#"{"user_code":"ABCD1234"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn device_approve_unknown_code_is_not_found() {
        let app = device_router(test_admin_state());
        let (status, _) = post_json(
            &app,
            "/auth/device/approve",
            Some("test-admin-token"),
            r#"{"user_code":"NOSUCHCODE"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn device_authorize_rejects_past_capacity() {
        let state = test_admin_state();
        // Fill the pending map to capacity with non-expired grants.
        {
            let mut map = state.device_flow.write().await;
            for i in 0..MAX_PENDING_DEVICES {
                map.insert(
                    format!("code-{i}"),
                    PendingDevice {
                        user_code: format!("USERCODE{i}"),
                        expires_at: std::time::Instant::now()
                            + std::time::Duration::from_secs(DEVICE_CODE_TTL_SECS),
                        approved_token: None,
                    },
                );
            }
        }
        let app = device_router(state.clone());
        let (status, _) = post_json(&app, "/auth/device", None, "{}").await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

        // The map must not have grown past the cap.
        assert_eq!(state.device_flow.read().await.len(), MAX_PENDING_DEVICES);
    }

    #[tokio::test]
    async fn device_approve_rejected_when_no_query_token_configured() {
        let state = admin_state_with_auth(None);
        // Seed a pending grant so the failure is the missing token, not a miss.
        {
            let mut map = state.device_flow.write().await;
            map.insert(
                "dev-code".to_string(),
                PendingDevice {
                    user_code: "ABCD2345".to_string(),
                    expires_at: std::time::Instant::now()
                        + std::time::Duration::from_secs(DEVICE_CODE_TTL_SECS),
                    approved_token: None,
                },
            );
        }
        let app = device_router(state.clone());
        let (status, _) = post_json(
            &app,
            "/auth/device/approve",
            Some("test-admin-token"),
            r#"{"user_code":"ABCD2345"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);

        // The grant must remain unapproved (no empty token granted).
        let map = state.device_flow.read().await;
        assert!(map.get("dev-code").unwrap().approved_token.is_none());
    }

    #[tokio::test]
    async fn device_approve_empty_string_token_also_rejected() {
        // A configured-but-empty token is as unusable as None; reject it too.
        let state = admin_state_with_auth(Some(String::new()));
        {
            let mut map = state.device_flow.write().await;
            map.insert(
                "dev-code".to_string(),
                PendingDevice {
                    user_code: "ABCD2345".to_string(),
                    expires_at: std::time::Instant::now()
                        + std::time::Duration::from_secs(DEVICE_CODE_TTL_SECS),
                    approved_token: None,
                },
            );
        }
        let app = device_router(state);
        let (status, _) = post_json(
            &app,
            "/auth/device/approve",
            Some("test-admin-token"),
            r#"{"user_code":"ABCD2345"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[test]
    fn generate_user_code_is_unbiased_and_within_alphabet() {
        // Sample many codes: every byte must be in the alphabet, and across a
        // large sample every alphabet symbol should appear (a biased mapping
        // would still stay in-alphabet, so the spread check guards the sampler).
        let mut seen = std::collections::HashSet::new();
        for _ in 0..2000 {
            let code = generate_user_code();
            assert_eq!(code.len(), USER_CODE_LEN);
            for b in code.bytes() {
                assert!(
                    USER_CODE_ALPHABET.contains(&b),
                    "byte {b} outside the user-code alphabet"
                );
                seen.insert(b);
            }
        }
        assert_eq!(
            seen.len(),
            USER_CODE_ALPHABET.len(),
            "some alphabet symbols never appeared — distribution looks skewed"
        );
    }

    #[test]
    fn generate_unique_user_code_avoids_collisions() {
        let mut map = std::collections::HashMap::new();
        for i in 0..256 {
            let code = generate_unique_user_code(&map)
                .expect("should always find a unique code at this size");
            map.insert(
                format!("device-{i}"),
                PendingDevice {
                    user_code: code,
                    expires_at: std::time::Instant::now()
                        + std::time::Duration::from_secs(DEVICE_CODE_TTL_SECS),
                    approved_token: None,
                },
            );
        }
        let distinct: std::collections::HashSet<String> = map
            .values()
            .map(|p| normalize_user_code(&p.user_code))
            .collect();
        assert_eq!(distinct.len(), map.len());
    }

    #[tokio::test]
    async fn device_approve_ambiguous_user_code_is_conflict() {
        // Two pending grants sharing a code (an invariant break) must not be
        // silently approved — the handler reports a conflict.
        let state = test_admin_state();
        {
            let mut map = state.device_flow.write().await;
            for code in ["dev-a", "dev-b"] {
                map.insert(
                    code.to_string(),
                    PendingDevice {
                        user_code: "DUPCODE9".to_string(),
                        expires_at: std::time::Instant::now()
                            + std::time::Duration::from_secs(DEVICE_CODE_TTL_SECS),
                        approved_token: None,
                    },
                );
            }
        }
        let app = device_router(state);
        let (status, _) = post_json(
            &app,
            "/auth/device/approve",
            Some("test-admin-token"),
            r#"{"user_code":"DUPCODE9"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[test]
    fn config_repo_url_allowed_rejects_internal_and_accepts_public() {
        // Internal/private targets declared in config must be skipped by reload.
        assert!(!config_repo_url_allowed("http://localhost/repo.git"));
        assert!(!config_repo_url_allowed("https://127.0.0.1/repo.git"));
        assert!(!config_repo_url_allowed(
            "http://169.254.169.254/latest/meta-data"
        ));
        assert!(!config_repo_url_allowed("https://10.0.0.5/internal.git"));
        // Alternate-encoded loopback (decimal) must also be rejected.
        assert!(!config_repo_url_allowed("git://2130706433/repo"));
        // Public HTTPS repos remain allowed.
        assert!(config_repo_url_allowed("https://github.com/acme/api.git"));
    }

    #[test]
    fn auth_rate_limiter_throttles_and_stays_bounded() {
        use std::cell::Cell;
        use std::time::{Duration, Instant};

        // Frozen clock so refill is deterministic.
        let start = Instant::now();
        thread_local! {
            static NOW: Cell<Option<Instant>> = const { Cell::new(None) };
        }
        NOW.with(|n| n.set(Some(start)));
        let clock = Arc::new(|| NOW.with(|n| n.get().unwrap()));

        let limiter = AuthRateLimiter::new_with_clock(3, 2, clock);

        // Same key: first 3 allowed, 4th rejected (bucket empty, clock frozen).
        assert!(limiter.check("ip:1.2.3.4"));
        assert!(limiter.check("ip:1.2.3.4"));
        assert!(limiter.check("ip:1.2.3.4"));
        assert!(!limiter.check("ip:1.2.3.4"));

        // After enough time the bucket refills.
        NOW.with(|n| n.set(Some(start + Duration::from_secs(60))));
        assert!(limiter.check("ip:1.2.3.4"));

        // Key-cap bound: with two saturated keys, a third distinct key is
        // rejected rather than growing the map without bound.
        let bounded = AuthRateLimiter::new(3, 2);
        for key in ["ip:a", "ip:b"] {
            assert!(bounded.check(key));
            assert!(bounded.check(key));
            assert!(bounded.check(key)); // drains each bucket
        }
        assert!(!bounded.check("ip:c"));
    }
}
