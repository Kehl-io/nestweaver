use nestweaver_store::{GraphStore, TantivyIndex};
use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;
use tokio::sync::broadcast;

use crate::gaps_cache::GapsCache;

/// Injectable DNS resolver for the SSRF add-time hostname check in
/// `routes::admin::add_repo`.
///
/// nw-654: `add_repo` used to call `nestweaver_engine::ssrf::resolve_host`
/// directly, which performs a real, blocking DNS lookup. That made
/// `add_repo_persists_instance_config` depend on live network access —
/// a transient DNS failure on the CI runner turned into a 400 and flaked the
/// Required CI gate (it blocked v10.1.1 publication). The resolver is now a
/// field on `AdminState` instead: production wires up [`system_resolver`]
/// (real DNS, unchanged behaviour), and tests inject a synthetic function so
/// the unit test is hermetic even with networking disabled.
pub type HostResolver = Arc<dyn Fn(&str) -> Result<Vec<IpAddr>, String> + Send + Sync>;

/// The resolver every production `AdminState` must use — real DNS via
/// `nestweaver_engine::ssrf::resolve_host`. Centralized here (rather than
/// each call site writing `Arc::new(resolve_host)`) so the production
/// behaviour is declared once and every constructor CALLS it instead of
/// re-stating it.
pub fn system_resolver() -> HostResolver {
    Arc::new(nestweaver_engine::ssrf::resolve_host)
}

#[derive(Clone)]
pub struct GraphEvent {
    pub event_type: String,
    pub payload: serde_json::Value,
}

pub struct AppState {
    pub store: Arc<GraphStore>,
    pub tantivy: Option<Arc<TantivyIndex>>,
    pub event_tx: broadcast::Sender<GraphEvent>,
    pub db_path: PathBuf,
    pub file_lock: Mutex<()>,
    repo_freshness: Mutex<RepoFreshnessCache>,
    /// Lazily computed global bridge pool (uid -> raw betweenness), filled
    /// once per process by `crate::bridge::global_bridge_scores`. Cached
    /// because the engine's sampled Brandes pass is too expensive to run per
    /// request on large graphs; the data is advisory UI emphasis, so
    /// within-process staleness is acceptable.
    pub bridge_scores: OnceLock<Arc<HashMap<String, f64>>>,
    pub gaps_cache: GapsCache,
    pub manifest_recovery: OnceLock<Arc<nestweaver_engine::manifest::ManifestRecoveryRuntime>>,
    pub vault_derivation: OnceLock<VaultDerivationHttp>,
    /// Set by the daemon when this UI is served inside a process that has an
    /// idle timeout. Each HTTP request notifies it (nw-749). Absent in tests
    /// and in any router that is not tied to a daemon idle loop.
    pub idle_activity: OnceLock<std::sync::Arc<tokio::sync::Notify>>,
}

/// Local Git observation. The legacy indexed distance is not freshness proof.
#[derive(Clone, serde::Serialize)]
pub struct RepoFreshness {
    pub status: String,
    pub indexed_sha: String,
    pub current_sha: Option<String>,
    pub commits_behind: Option<u64>,
    pub commits_ahead: Option<u64>,
}

impl RepoFreshness {
    fn unknown(repo: &nestweaver_schema::Repo) -> Self {
        Self {
            status: "unknown".into(),
            indexed_sha: repo.indexed_sha.clone(),
            current_sha: None,
            commits_behind: None,
            commits_ahead: None,
        }
    }
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct RepoFreshnessKey {
    generation: u64,
    uid: String,
    indexed_sha: String,
    root: Option<String>,
}
#[derive(Default)]
struct RepoFreshnessCache {
    entries: HashMap<RepoFreshnessKey, (Instant, RepoFreshness)>,
}
const FRESHNESS_CACHE_LIMIT: usize = 512;
const FRESHNESS_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(5);

fn bounded_local_git(root: &str, args: &[&str], deadline: Instant) -> Result<Option<String>, ()> {
    let budget = deadline.saturating_duration_since(Instant::now());
    if budget.is_zero() {
        return Err(());
    }
    let mut command = std::process::Command::new("git");
    command.args(["-C", root]).args(args);
    let output =
        nestweaver_engine::git_cmd::run_git_with_timeout_and_output_limit(command, budget, 4096)
            .map_err(|_| ())?;
    Ok(output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned()))
}

fn observe_local_git(repo: &nestweaver_schema::Repo, request_deadline: Instant) -> RepoFreshness {
    use nestweaver_engine::repo_head::is_full_sha;
    let deadline = request_deadline.min(Instant::now() + std::time::Duration::from_millis(500));
    let mut observation = RepoFreshness::unknown(repo);
    let Some(root) = repo.local_root() else {
        return observation;
    };
    match std::fs::metadata(root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            observation.status = "missing".into();
            return observation;
        }
        Err(_) => return observation,
        Ok(metadata) if !metadata.is_dir() => return observation,
        Ok(_) => {}
    }
    let head = match bounded_local_git(root, &["rev-parse", "HEAD"], deadline) {
        Ok(Some(head)) if is_full_sha(&head) => head,
        Ok(_) => {
            if matches!(std::fs::symlink_metadata(std::path::Path::new(root).join(".git")),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound)
            {
                observation.status = "untracked".into();
            }
            return observation;
        }
        Err(_) => return observation,
    };
    observation.current_sha = Some(head.clone());
    if !is_full_sha(&repo.indexed_sha) {
        return observation;
    }
    if head == repo.indexed_sha {
        observation.status = "current".into();
        observation.commits_behind = Some(0);
        observation.commits_ahead = Some(0);
        return observation;
    }
    let behind = format!("{}..{head}", repo.indexed_sha);
    let ahead = format!("{head}..{}", repo.indexed_sha);
    let counts = (
        bounded_local_git(root, &["rev-list", "--count", &behind], deadline),
        bounded_local_git(root, &["rev-list", "--count", &ahead], deadline),
    );
    let (Ok(behind), Ok(ahead)) = counts else {
        return observation;
    };
    observation.commits_behind = behind.and_then(|count| count.parse().ok());
    observation.commits_ahead = ahead.and_then(|count| count.parse().ok());
    observation.status = match (observation.commits_behind, observation.commits_ahead) {
        (Some(behind), Some(0)) if behind > 0 => "behind",
        (Some(0), Some(ahead)) if ahead > 0 => "ahead",
        (Some(behind), Some(ahead)) if behind > 0 && ahead > 0 => "diverged",
        _ => "different",
    }
    .into();
    observation
}

impl AppState {
    /// Call on blocking work, after obtaining repo rows and releasing graph
    /// connections. Bounded per-call probes and TTL avoid repeated Git work.
    pub fn repo_freshness(
        &self,
        repos: &[nestweaver_schema::Repo],
    ) -> HashMap<String, RepoFreshness> {
        let generation = self.store.graph_generation();
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        {
            let mut cache = self
                .repo_freshness
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            cache.entries.retain(|key, (at, _)| {
                key.generation == generation && at.elapsed() < FRESHNESS_CACHE_TTL
            });
        }
        repos
            .iter()
            .enumerate()
            .map(|(index, repo)| {
                let key = RepoFreshnessKey {
                    generation,
                    uid: repo.uid.clone(),
                    indexed_sha: repo.indexed_sha.clone(),
                    root: repo.local_root().map(str::to_owned),
                };
                let cached = self
                    .repo_freshness
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .entries
                    .get(&key)
                    .map(|(_, observation)| observation.clone());
                let observation = if let Some(cached) = cached {
                    cached
                } else if index >= FRESHNESS_CACHE_LIMIT || Instant::now() >= deadline {
                    RepoFreshness::unknown(repo)
                } else {
                    // Never hold the shared cache mutex or a graph connection
                    // while an owned, timeout-bounded Git process runs.
                    let observation = observe_local_git(repo, deadline);
                    let mut cache = self
                        .repo_freshness
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    if cache.entries.len() >= FRESHNESS_CACHE_LIMIT {
                        if let Some(oldest) = cache
                            .entries
                            .iter()
                            .min_by_key(|(_, (at, _))| *at)
                            .map(|(key, _)| key.clone())
                        {
                            cache.entries.remove(&oldest);
                        }
                    }
                    cache
                        .entries
                        .insert(key, (Instant::now(), observation.clone()));
                    observation
                };
                (repo.uid.clone(), observation)
            })
            .collect()
    }

    pub fn aggregate_repo_freshness(&self, repos: &[nestweaver_schema::Repo]) -> String {
        let observations = self.repo_freshness(repos);
        aggregate_freshness(
            observations
                .values()
                .map(|observation| observation.status.as_str()),
        )
    }
}

pub fn aggregate_freshness<'a>(statuses: impl IntoIterator<Item = &'a str>) -> String {
    let statuses: Vec<_> = statuses.into_iter().collect();
    if statuses.is_empty() {
        return "unknown".into();
    }
    if statuses.iter().all(|status| *status == statuses[0]) {
        return statuses[0].into();
    }
    if statuses
        .iter()
        .any(|status| matches!(*status, "behind" | "ahead" | "diverged" | "different"))
    {
        "stale".into()
    } else {
        "unknown".into()
    }
}

pub struct VaultDerivationHttp {
    pub data_instance_id: String,
    pub read_only: bool,
    pub max_note_bytes: u64,
}

impl AppState {
    pub fn admit_vault_derivation(&self) -> Result<(), crate::error::ApiError> {
        let Some(config) = self.vault_derivation.get() else {
            return Ok(());
        };
        nestweaver_engine::markdown_derivation::admit_all_vaults(
            &self.store,
            &self.db_path,
            &config.data_instance_id,
            &[],
            config.max_note_bytes,
            config.read_only,
        )
        .map_err(|error| crate::error::ApiError::unavailable(error.to_string()))
    }

    pub fn new(store: GraphStore, tantivy: Option<TantivyIndex>, db_path: PathBuf) -> Arc<Self> {
        let (event_tx, _) = broadcast::channel(256);
        Arc::new(Self {
            store: Arc::new(store),
            tantivy: tantivy.map(Arc::new),
            event_tx,
            db_path,
            file_lock: Mutex::new(()),
            repo_freshness: Mutex::new(RepoFreshnessCache::default()),
            bridge_scores: OnceLock::new(),
            gaps_cache: GapsCache::new(),
            manifest_recovery: OnceLock::new(),
            vault_derivation: OnceLock::new(),
            idle_activity: OnceLock::new(),
        })
    }

    pub fn new_with_store(
        store: Arc<GraphStore>,
        tantivy: Option<TantivyIndex>,
        db_path: PathBuf,
    ) -> Arc<Self> {
        let (event_tx, _) = broadcast::channel(256);
        Arc::new(Self {
            store,
            tantivy: tantivy.map(Arc::new),
            event_tx,
            db_path,
            file_lock: Mutex::new(()),
            repo_freshness: Mutex::new(RepoFreshnessCache::default()),
            bridge_scores: OnceLock::new(),
            gaps_cache: GapsCache::new(),
            manifest_recovery: OnceLock::new(),
            vault_derivation: OnceLock::new(),
            idle_activity: OnceLock::new(),
        })
    }

    pub fn new_with_arc_tantivy(
        store: Arc<GraphStore>,
        tantivy: Option<Arc<TantivyIndex>>,
        db_path: PathBuf,
    ) -> Arc<Self> {
        let (event_tx, _) = broadcast::channel(256);
        Arc::new(Self {
            store,
            tantivy,
            event_tx,
            db_path,
            file_lock: Mutex::new(()),
            repo_freshness: Mutex::new(RepoFreshnessCache::default()),
            bridge_scores: OnceLock::new(),
            gaps_cache: GapsCache::new(),
            manifest_recovery: OnceLock::new(),
            vault_derivation: OnceLock::new(),
            idle_activity: OnceLock::new(),
        })
    }
}

/// A pending device-authorization grant (RFC 8628 Device Authorization Grant).
///
/// Created by `POST /auth/device`, approved by an admin via
/// `POST /auth/device/approve`, then exchanged for the org query token by the
/// developer via `POST /auth/token`.
pub struct PendingDevice {
    /// Short, human-readable code shown to the developer and approved by an
    /// admin. Stored canonicalized (uppercase alnum, no separators).
    pub user_code: String,
    /// When this grant expires and should be pruned.
    pub expires_at: Instant,
    /// Set once an admin approves; holds the granted query token. `None` while
    /// the grant is still pending.
    pub approved_token: Option<String>,
}

/// Shared state for admin API routes. Provides access to daemon-level
/// resources (store, queue depth, drain state) that the admin API needs.
pub struct AdminState {
    pub admin_token: String,
    /// Configured org-wide query (read) token, handed to developers on
    /// device-flow approval. `None` when the server runs without query auth.
    pub auth_token: Option<String>,
    /// In-flight device-authorization grants, keyed by `device_code`.
    pub device_flow: Arc<tokio::sync::RwLock<HashMap<String, PendingDevice>>>,
    pub daemon_store: Arc<GraphStore>,
    /// Live full-text query handle, which may be writer-backed or a reader
    /// fallback. Admin repo deletion is code-only and leaves vault search
    /// untouched; indexed vault mutation repair remains daemon-owned.
    pub tantivy: Option<Arc<nestweaver_store::TantivyIndex>>,
    pub instance_id: String,
    pub start_time: Instant,
    pub active_reads: Arc<AtomicU32>,
    pub active_writes: Arc<AtomicU32>,
    /// Once set, new admin mutations must be refused. This is the daemon's
    /// shutdown-admission flag, paired with `active_writes` so the drain cannot
    /// observe zero and finish while an HTTP admin mutation starts behind it.
    pub shutdown_started: Arc<AtomicBool>,
    /// Live count of active MCP-over-HTTP sessions, shared with the MCP handler
    /// (which republishes `sessions.len()` on insert/expiry). Surfaced as the
    /// dashboard's "connected MCP clients". Zero when server mode is off.
    pub mcp_sessions: Arc<AtomicU32>,
    pub drained: Arc<AtomicBool>,
    pub indexing_queue_depth: Arc<AtomicU32>,
    /// Path to the brain database, used to derive the jobs database path.
    pub db_path: std::path::PathBuf,
    /// Shared job-queue connection, cloned from the daemon's single `JobQueue`.
    /// Admin routes MUST use this rather than opening their own connection to
    /// the jobs SQLite file: independent connections race the worker's WAL
    /// checkpoint and crash the daemon with SIGBUS on macOS. `None` in tests
    /// and non-server mode, where a transient connection is opened on demand.
    pub job_queue: Option<Arc<Mutex<nestweaver_engine::jobs::JobQueue>>>,
    /// Path to instance.toml for hot-reload. `None` when no config was supplied.
    pub config_path: Option<std::path::PathBuf>,
    /// Channel to send commands to the live poll scheduler. `None` when no
    /// scheduler is running (non-server mode or no admin token).
    pub scheduler_tx:
        Option<tokio::sync::mpsc::Sender<nestweaver_engine::scheduler::SchedulerCommand>>,
    /// Webhook allowed repos set, shared with the webhook handler via RwLock.
    /// Reload updates this so new repos are accepted without restart.
    pub webhook_allowed_repos:
        Option<Arc<std::sync::RwLock<Option<std::collections::HashSet<String>>>>>,
    /// Webhook per-repo branch map, shared with the webhook handler via RwLock.
    pub webhook_repo_branches:
        Option<Arc<std::sync::RwLock<std::collections::HashMap<String, String>>>>,
    /// Write gate shared with the daemon to prevent races between admin
    /// repo deletion and worker indexing. `None` in tests or non-server mode.
    ///
    /// A3: the gate rather than a bare mutex so this deletion stamps itself as
    /// the write-lock holder. Unlike the worker pool it sets neither
    /// `indexing_active` nor `queue_depth`, so before this it was a writer that
    /// nothing in `brain status` could see.
    pub write_gate: Option<nestweaver_engine::WriteGate>,
    /// DNS resolver used by `add_repo`'s SSRF add-time hostname check. See
    /// [`HostResolver`] / [`system_resolver`] — production wires the real
    /// resolver, tests inject a synthetic one.
    pub resolver: HostResolver,
}
