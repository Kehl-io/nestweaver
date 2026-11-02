//! Last completed graph observations for the diagnostic status RPC only.
//! Refreshes use the normal blocking dispatch and are retained by daemon shutdown.
use super::*;
use sha2::{Digest, Sha256};

const CAPACITY: usize = 8;
const MAX_AGE: Duration = Duration::from_secs(30);

#[derive(Clone, PartialEq, Eq)]
struct Key {
    principal: [u8; 32],
    policy_enabled: bool,
    args: String,
    profile: nestweaver_schema::ToolDeliveryProfile,
}

#[derive(Clone)]
struct Observation {
    value: serde_json::Value,
    generation: u64,
    publication: u64,
    captured: std::time::Instant,
    captured_unix_ms: u128,
}

struct Entry {
    key: Key,
    observation: Option<Observation>,
    reserved: bool,
}

#[derive(Default)]
pub(super) struct StatusSnapshots {
    entries: std::sync::Mutex<Vec<Entry>>,
    changed: tokio::sync::Notify,
    refresh: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

fn capture_blocked(state: &DaemonState) -> bool {
    state.write_gate.holder_snapshot().is_some()
        || state.store.index_publication_owner_snapshot().is_some()
        || state.indexing_active.load(Ordering::Acquire)
}

fn fence(state: &DaemonState) -> (u64, u64) {
    (
        state.store.graph_generation(),
        state.store.index_publication_revision(),
    )
}

fn needs_refresh(state: &DaemonState, observation: &Observation) -> bool {
    fence(state) != (observation.generation, observation.publication)
        || observation.captured.elapsed() >= MAX_AGE
}

fn stale(state: &DaemonState, observation: &Observation) -> bool {
    capture_blocked(state)
        || nestweaver_engine::index_publication::status(&state.db_path).dirty
        || needs_refresh(state, observation)
}

fn key(
    state: &DaemonState,
    extensions: &tonic::Extensions,
    args: serde_json::Value,
) -> Result<Key, Status> {
    use nestweaver_engine::authz::{Identity, VisibleRepos};
    let identity = extensions.get::<Identity>().unwrap_or(&Identity::Anonymous);
    let enabled = state.permission_source.is_enabled();
    // This is the existing status admission contract, before any graph read.
    if enabled && !matches!(identity, Identity::Admin) {
        return Err(Status::permission_denied(
            "BrainStatus requires an administrator while repository authorization is enabled",
        ));
    }
    // Administrators resolve without a repository catalogue; this preserves
    // the permission source's decision instead of substituting a caller scope.
    // A custom source returning a restricted scope still fails closed through
    // the same MCP admission used by ordinary dispatch.
    let visible = if enabled {
        state.permission_source.visible_repos(identity, &[])
    } else {
        VisibleRepos::All
    };
    let args = nestweaver_mcp::tools::admit_tool_arguments("brain_status", args, Some(&visible))
        .map_err(|error| dispatch_err_to_status("brain_status", error))?;
    let mut principal = Sha256::new();
    match identity {
        Identity::Admin => principal.update(b"admin"),
        Identity::Anonymous => principal.update(b"anonymous"),
        Identity::Token(token) => {
            principal.update(b"query:");
            principal.update(token.as_bytes());
        }
    }
    let uid = extensions
        .get::<tonic::transport::server::UdsConnectInfo>()
        .and_then(|info| info.peer_cred.as_ref())
        .map(|cred| cred.uid())
        // No UDS peer on bearer/embedded requests; authenticated Identity still
        // distinguishes those principals. The daemon's UID is stable, not PID.
        .unwrap_or_else(|| unsafe { libc::geteuid() });
    principal.update(b"uid:");
    principal.update(uid.to_le_bytes());
    Ok(Key {
        principal: principal.finalize().into(),
        policy_enabled: enabled,
        args: serde_json::to_string(&args).map_err(|e| Status::invalid_argument(e.to_string()))?,
        profile: extensions
            .get::<nestweaver_schema::ToolDeliveryProfile>()
            .copied()
            .unwrap_or_default(),
    })
}

pub(super) enum StatusArguments<'a> {
    Json(&'a str),
    Value(serde_json::Value),
}

impl DaemonService {
    /// Cached status retains the same activity, timeout and metrics contract
    /// as ordinary tool dispatch; only graph observation is asynchronous.
    pub(super) async fn guarded_status_snapshot(
        &self,
        args: StatusArguments<'_>,
        extensions: &tonic::Extensions,
        current_only: bool,
    ) -> Result<serde_json::Value, Status> {
        let _activity = ConnectionGuard::read(&self.state);
        let started = Instant::now();
        let tool = "brain_status";
        nestweaver_web::routes::metrics::GRPC_REQUESTS
            .with_label_values(&[tool])
            .inc();
        let timeout = self.state.safeguards.effective_timeout(tool, None);
        let cancel = Arc::new(AtomicBool::new(false));
        let _disconnect = arm_disconnect_cancel(cancel.clone());
        let handler = async {
            let args = match args {
                StatusArguments::Json(raw) => serde_json::from_str(raw).map_err(|error| {
                    Status::invalid_argument(format!("invalid JSON arguments: {error}"))
                })?,
                StatusArguments::Value(args) => args,
            };
            self.status_snapshot(args, extensions, current_only).await
        };
        let response = if self.state.server_mode {
            with_safeguard_cancellable(tool, &self.state.safeguards, None, cancel, handler).await
        } else {
            handler.await
        };
        let elapsed = started.elapsed();
        nestweaver_web::routes::metrics::QUERY_DURATION
            .with_label_values(&[tool])
            .observe(elapsed.as_secs_f64());
        if elapsed >= timeout.mul_f64(0.8) {
            nestweaver_web::routes::metrics::SLOW_QUERIES.inc();
        }
        if response.is_err() {
            nestweaver_web::routes::metrics::QUERY_ERRORS
                .with_label_values(&[tool])
                .inc();
        }
        response
    }

    pub(super) fn http_brain_status_provider(
        state: Arc<DaemonState>,
    ) -> nestweaver_mcp::http::BrainStatusProvider {
        Arc::new(move |args, identity, profile| {
            let state = state.clone();
            Box::pin(async move {
                let mut request = Request::new(JsonRequest {
                    args_json: args.to_string(),
                });
                request.extensions_mut().insert(identity);
                request.extensions_mut().insert(profile);
                let response = DaemonService::new(state).brain_status_json(request).await?;
                Ok(serde_json::from_str(&response.into_inner().result_json)?)
            })
        })
    }

    pub(super) async fn status_snapshot(
        &self,
        args: serde_json::Value,
        extensions: &tonic::Extensions,
        current_only: bool,
    ) -> Result<serde_json::Value, Status> {
        let key = key(&self.state, extensions, args)?;
        let wait = self.state.status_snapshots.changed.notified();
        tokio::pin!(wait);
        wait.as_mut().enable();
        let observation = {
            let mut entries = self
                .state
                .status_snapshots
                .entries
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = entries.iter().find(|e| e.key == key) {
                entry.observation.clone()
            } else {
                if entries.len() == CAPACITY {
                    let index = entries
                        .iter()
                        .position(|entry| !entry.reserved)
                        .ok_or_else(|| {
                            Status::unavailable("status observation capacity is reserved")
                        })?;
                    entries.remove(index);
                }
                entries.push(Entry {
                    key: key.clone(),
                    observation: None,
                    reserved: false,
                });
                None
            }
        };
        // A new principal/argument frame never borrows another caller's cache.
        // Missing observations while indexing fail quickly rather than queue
        // graph reads behind a native transaction.
        self.kick_status_refresh();
        let observation = match observation {
            Some(value) => value,
            None if capture_blocked(&self.state) => {
                return Err(Status::unavailable(
                    "brain status has no completed observation for this caller while indexing",
                ));
            }
            None => {
                let _ = tokio::time::timeout(Duration::from_millis(500), wait.as_mut()).await;
                self.state
                    .status_snapshots
                    .entries
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter()
                    .find(|e| e.key == key)
                    .and_then(|e| e.observation.clone())
                    .ok_or_else(|| {
                        Status::unavailable(
                            "brain status observation is being refreshed; retry shortly",
                        )
                    })?
            }
        };
        let is_busy = capture_blocked(&self.state);
        let is_stale = is_busy || stale(&self.state, &observation);
        // The typed protobuf has no place to disclose last-observed counts.
        if current_only && is_stale {
            return Err(Status::unavailable(
                "current brain status is busy; use BrainStatusJson for explicitly last-observed counts",
            ));
        }
        let mut value = observation.value;
        value["status_observation"] = serde_json::json!({
            "cached": true,
            "stale": is_stale,
            "busy": is_busy,
            "as_of_generation": observation.generation,
            "as_of_publication_revision": observation.publication,
            "captured_unix_ms": observation.captured_unix_ms,
            "age_ms": observation.captured.elapsed().as_millis(),
            "graph_annotations_as_of_observation": true,
        });
        // These disclosures use only atomics, owned in-memory state and
        // sidecar files. Never call graph-derived annotations on a cache hit.
        value["index_publication"] = nestweaver_mcp::tools::publication_status_json(
            &nestweaver_engine::index_publication::status(&self.state.db_path),
        );
        value["index_publication_owner"] = self.state.store.index_publication_owner_snapshot()
            .map(|(token, operation, age_secs)| serde_json::json!({"token":token,"operation":operation,"age_secs":age_secs}))
            .unwrap_or(serde_json::Value::Null);
        value["code_links"] =
            nestweaver_engine::code_links::code_links_status_json(Some(&self.state.db_path));
        value["cross_repo_links"] =
            nestweaver_engine::cross_repo_links::cross_repo_links_status_json(Some(
                &self.state.db_path,
            ));
        let (skipped, near) =
            nestweaver_engine::index_md::skipped_notes_status_json(Some(&self.state.db_path));
        value["skipped_notes"] = skipped;
        value["notes_near_size_limit"] = near;
        // Only runtime progress is overlaid here; record identity and policy
        // diagnostics remain explicitly as of the fenced graph observation.
        let in_progress = vault_derivation::in_progress_json(&self.state);
        if value.get("vault_derivation").is_some() || !in_progress.is_null() {
            if !value["vault_derivation"].is_object() {
                value["vault_derivation"] = serde_json::json!({});
            }
            value["vault_derivation"]["in_progress"] = in_progress;
        }

        Ok(value)
    }

    fn kick_status_refresh(&self) {
        if self.state.shutdown_started.load(Ordering::Acquire) || capture_blocked(&self.state) {
            return;
        }
        let mut task = self
            .state
            .status_snapshots
            .refresh
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if task.as_ref().is_some_and(|task| !task.is_finished()) {
            return;
        }
        if !self
            .state
            .status_snapshots
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|entry| {
                entry
                    .observation
                    .as_ref()
                    .is_none_or(|observation| needs_refresh(&self.state, observation))
            })
        {
            return;
        }
        let state = self.state.clone();
        *task = Some(tokio::spawn(async move {
            DaemonService::new(state)
                .refresh_status_observations()
                .await;
        }));
    }

    async fn refresh_status_observations(&self) {
        let keys: Vec<_> = self
            .state
            .status_snapshots
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|e| {
                e.observation
                    .as_ref()
                    .is_none_or(|o| needs_refresh(&self.state, o))
            })
            .map(|e| e.key.clone())
            .collect();
        for key in keys {
            if self.state.shutdown_started.load(Ordering::Acquire) || capture_blocked(&self.state) {
                break;
            }
            let before = fence(&self.state);
            let args = match serde_json::from_str(&key.args) {
                Ok(args) => args,
                Err(_) => continue,
            };
            // Exactly one lifecycle-owned refresh at a time; inner dispatch
            // supplies all MCP thread-local context and graph annotations.
            let result = self
                .dispatch_tool_json_inner(
                    "brain_status",
                    args,
                    Arc::new(AtomicBool::new(false)),
                    nestweaver_engine::authz::VisibleRepos::All,
                    key.profile,
                )
                .await;
            if let Ok(value) = result {
                if capture_blocked(&self.state) || fence(&self.state) != before {
                    continue;
                }
                let observation = Observation {
                    value,
                    generation: before.0,
                    publication: before.1,
                    captured: std::time::Instant::now(),
                    captured_unix_ms: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis(),
                };
                if let Some(entry) = self
                    .state
                    .status_snapshots
                    .entries
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter_mut()
                    .find(|e| e.key == key)
                {
                    entry.observation = Some(observation);
                }
                self.state.status_snapshots.changed.notify_waiters();
            }
        }
    }

    pub(super) async fn prewarm_status_observations(&self) {
        // UDS clients are admitted as the local administrator. The otherwise
        // identical no-transport identity is useful to embedded callers.
        for identity in [
            nestweaver_engine::authz::Identity::Admin,
            nestweaver_engine::authz::Identity::Anonymous,
        ] {
            let mut extensions = tonic::Extensions::new();
            extensions.insert(identity);
            for profile in [
                nestweaver_schema::ToolDeliveryProfile::FullCli,
                nestweaver_schema::ToolDeliveryProfile::BoundedMcp,
            ] {
                extensions.insert(profile);
                if let Ok(key) = key(&self.state, &extensions, serde_json::json!({})) {
                    self.state
                        .status_snapshots
                        .entries
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(Entry {
                            key,
                            observation: None,
                            reserved: true,
                        });
                }
            }
        }
        self.refresh_status_observations().await;
    }
}

/// Stop admission first; join after writers drain, before authority teardown.
pub(super) async fn drain_status_refresh(state: &DaemonState) {
    let task = state
        .status_snapshots
        .refresh
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    if let Some(task) = task
        && let Err(error) = task.await
    {
        tracing::error!(%error, "status refresh task failed while draining");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cached_status_preserves_activity_metrics_and_server_deadline() {
        let state = super::super::startup_helper_tests::test_state_with_writer();
        let Ok(mut state) = Arc::try_unwrap(state) else {
            panic!("one state owner");
        };
        state.server_mode = true;
        state
            .safeguards
            .timeouts
            .insert("brain_status".into(), Duration::from_millis(25));
        let state = Arc::new(state);
        let service = DaemonService::new(state.clone());
        service.prewarm_status_observations().await;
        let requests =
            nestweaver_web::routes::metrics::GRPC_REQUESTS.with_label_values(&["brain_status"]);
        let before = requests.get();
        let _ = tokio::time::timeout(Duration::from_millis(1), state.idle_notify.notified()).await;
        let idle = state.idle_notify.notified();
        tokio::pin!(idle);
        idle.as_mut().enable();
        service
            .brain_status_json(Request::new(JsonRequest {
                args_json: "{}".into(),
            }))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_millis(100), idle)
            .await
            .expect("cache hit records client activity");
        assert!(requests.get() > before);
        assert_eq!(state.active_reads.load(Ordering::Acquire), 0);

        let entries_before = state.status_snapshots.entries.lock().unwrap().len();
        let errors =
            nestweaver_web::routes::metrics::QUERY_ERRORS.with_label_values(&["brain_status"]);
        let errors_before = errors.get();
        let invalid = service
            .brain_status_json(Request::new(JsonRequest {
                args_json: "{".into(),
            }))
            .await;
        assert_eq!(invalid.unwrap_err().code(), tonic::Code::InvalidArgument);
        assert!(requests.get() >= before + 2);
        assert!(errors.get() > errors_before);
        assert_eq!(
            state.status_snapshots.entries.lock().unwrap().len(),
            entries_before,
            "invalid JSON never admits graph work"
        );
        assert_eq!(state.active_reads.load(Ordering::Acquire), 0);

        // Occupy only the refresh lifecycle slot with a store-free pending
        // task, so an unobserved principal must await its observation. The
        // actual request wrapper must impose its configured server deadline.
        *state.status_snapshots.refresh.lock().unwrap() =
            Some(tokio::spawn(std::future::pending()));
        let request_state = state.clone();
        let request = tokio::spawn(async move {
            let mut request = Request::new(JsonRequest {
                args_json: "{}".into(),
            });
            request
                .extensions_mut()
                .insert(nestweaver_engine::authz::Identity::Token(
                    "uncaptured".into(),
                ));
            DaemonService::new(request_state)
                .brain_status_json(request)
                .await
        });
        let active = tokio::time::timeout(Duration::from_secs(1), async {
            while state.active_reads.load(Ordering::Acquire) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        let response = request.await.unwrap();
        let pending = state
            .status_snapshots
            .refresh
            .lock()
            .unwrap()
            .take()
            .unwrap();
        pending.abort();
        let _ = pending.await;
        state.shutdown_started.store(true, Ordering::Release);
        drain_status_refresh(&state).await;
        assert!(
            active.is_ok(),
            "cached request holds read accounting while awaiting observation"
        );
        assert_eq!(response.unwrap_err().code(), tonic::Code::DeadlineExceeded);
        assert_eq!(state.active_reads.load(Ordering::Acquire), 0);
        assert!(requests.get() >= before + 3);
    }
}
