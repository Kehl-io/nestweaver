//! Main-thread reload channel, off-write-gate artifact seeding, and
//! bounded background auto-repair for the local embedding model cache
//! (nw-484). Split out of `server.rs` as a pure move — no behavior
//! change — once everything in this module was green there; see
//! `crates/nestweaver-daemon/src/server.rs`'s `embed`/`plan_embed` RPC
//! handlers and `run_server` for the call sites that wire this in.
//!
//! Tests for this module stay in `server.rs`'s test module: they exercise
//! the full RPC/`DaemonState` wiring (barrier-blocked fakes, the
//! `DaemonService::embed` handler, `brain_status`), not just these
//! functions in isolation, so they share that module's existing
//! `DaemonState` test fixtures rather than duplicating them here.

// `PathBuf` is only used by the embed-only `SeedPhase`/`SeedFlight` types and
// functions below (it was ungated when `SeedFlight` itself still was).
#[cfg(feature = "embed")]
use std::path::PathBuf;
// `Arc` and `Duration` are only used by the embed-only functions below
// (seeding, the reload loaders, auto-repair's backoff schedule) — every
// ungated item in this file (`SeedProgress`, `SeedProgressSnapshot`,
// `EmbeddingReloadOutcome`, `EmbeddingReloadRequest`) either avoids them or
// spells `std::sync::Arc` out fully qualified. Without the gate, a
// `--no-default-features --features metal` build (CI's Cold Metal job, and
// `cargo check -p nestweaver-daemon --no-default-features --features metal
// --all-targets`) warns on both as unused.
#[cfg(feature = "embed")]
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
#[cfg(feature = "embed")]
use std::time::Duration;

// `EmbeddingRuntimeStatus` is used by the always-compiled
// `EmbeddingReloadOutcome`; `ConnectionGuard`/`DaemonState` are used only by
// the embed-only functions below (`join_or_start_seed`,
// `clear_stale_seed_flight`, `run_embedding_cache_repair`,
// `service_embedding_reloads`), so they carry the same `embed` gate as
// `embedding_cache_dir_for_load_with`/`embedding_load_config` right below —
// same reasoning, same CI job.
use crate::server::EmbeddingRuntimeStatus;
#[cfg(feature = "embed")]
use crate::server::{
    ConnectionGuard, DaemonState, embedding_cache_dir_for_load_with, embedding_load_config,
    unix_now_seconds,
};
// Only `production_reload_loader` (`not(test)`) calls this; under a test
// build the import would otherwise be unused.
#[cfg(all(feature = "embed", not(test)))]
use crate::server::load_embedding_model_with_mode;
#[cfg(all(feature = "embed", test))]
use crate::server::write_complete_hf_cache;

// ── Main-thread reload channel (nw-484) ────────────────────────────────
//
// candle's Metal path needs the process main thread (see the constraint
// documented on `load_embedding_model_with_mode` below), so an `embed` RPC
// that just seeded a missing cache cannot construct the model itself — it
// runs on a tokio worker. Instead it hands the load to `run_server`'s main
// `block_on` loop through this channel and awaits the reply. The loop
// coalesces concurrent requests into one load and refuses new ones once
// shutdown has begun.

/// Outcome of one main-thread reload attempt, delivered to every request
/// coalesced into that attempt.
///
/// Deliberately NOT `#[cfg(feature = "embed")]`: `EmbeddingReloadRequest`
/// below carries it as its `reply` sender's generic parameter, and that type
/// is part of the general-purpose `DaemonState` test-fixture surface
/// (`test_state_with_writer_generation`, `test_state_with_authz` — used by
/// hundreds of tests with nothing to do with embedding), which always opens
/// a reload channel and hands the `Receiver<EmbeddingReloadRequest>` back to
/// its caller regardless of the `embed` feature. Splitting that return type
/// per-feature would ripple into every one of those callers for no benefit.
/// Under `--no-default-features --features metal` only `Unavailable` is ever
/// constructed (`run_server`'s `#[cfg(not(feature = "embed"))]` reply
/// branch), and even that is unreachable in practice (nothing sends into the
/// channel without `embed`) — hence the narrow, feature-scoped allow rather
/// than a blanket one.
#[cfg_attr(not(feature = "embed"), allow(dead_code))]
#[derive(Clone)]
pub(crate) enum EmbeddingReloadOutcome {
    Loaded(EmbeddingRuntimeStatus),
    Unavailable(EmbeddingRuntimeStatus),
    ShuttingDown,
}

/// One request for the main-thread reload loop to (re)load the embedding
/// model, cache-only, and report the resulting status.
pub(crate) struct EmbeddingReloadRequest {
    pub(crate) reply: tokio::sync::oneshot::Sender<EmbeddingReloadOutcome>,
}

/// Typed load failure so a caller can tell "the artifacts are missing" (the
/// only cause auto-repair may act on) from every other failure (device,
/// construction, probe, identity) without matching on status strings.
#[cfg(feature = "embed")]
#[derive(Debug, Clone, Default)]
pub(crate) struct EmbeddingLoadFailure {
    pub(crate) missing_artifact: bool,
}

/// Service one coalesced batch of reload requests from the main-loop
/// channel. Always a free function so it can be driven with an injected
/// load future under test — production passes
/// `production_reload_loader(state)` (a thin wrapper around
/// `load_embedding_model_with_mode(state, ArtifactMode::CacheOnly)`);
/// `#[cfg(test)]` production wiring instead passes
/// `unserviced_reload_loader(state)`, an honest "not serviced under libtest"
/// failure (see `run_server`), and unit tests below pass their own fakes.
///
/// `load` is a `Future` VALUE, not a closure, deliberately: constructing it
/// (calling an async fn) does not run its body until this function actually
/// polls it at step (d) below, so steps (b)/(c) short-circuiting before that
/// point means the loader never runs at all — with no HRTB gymnastics for a
/// borrow of `state` that outlives one `.await` point (a closure returning
/// `impl Future<Output = ..> + '_` cannot satisfy a `for<'a> Fn(&'a ..) ->
/// F` bound, since a single associated `F` cannot vary per call).
///
/// `load` must never download: the reload loop runs on the daemon's main
/// thread, and downloading there would defeat the whole point of moving
/// seeding to a dedicated thread (§4.1 of the nw-484 design) — the main
/// thread must stay bounded by local disk and Metal shader compilation only.
#[cfg(feature = "embed")]
pub(crate) async fn service_embedding_reloads<F>(
    state: &Arc<DaemonState>,
    first: EmbeddingReloadRequest,
    reload_rx: &mut tokio::sync::mpsc::Receiver<EmbeddingReloadRequest>,
    shutdown_sub: &mut tokio::sync::watch::Receiver<bool>,
    load: F,
) where
    F: std::future::Future<Output = Result<(), EmbeddingLoadFailure>>,
{
    // (a) Coalesce every request already queued into this one batch.
    let mut replies = vec![first.reply];
    while let Ok(next) = reload_rx.try_recv() {
        replies.push(next.reply);
    }

    // (b) Refuse outright once shutdown has begun. A reload is not a write —
    // it takes no write gate and no `ConnectionGuard` — so it must not be
    // allowed to start fresh work after the drain's admission closed.
    if state.shutdown_started.load(Ordering::SeqCst) {
        for reply in replies {
            let _ = reply.send(EmbeddingReloadOutcome::ShuttingDown);
        }
        return;
    }

    // (c) Skip a redundant load: an earlier coalesced batch (or the boot
    // load) already got there first.
    if state.embedding_runtime.status().state == "ready" {
        let status = state.embedding_runtime.status();
        for reply in replies {
            let _ = reply.send(EmbeddingReloadOutcome::Loaded(status.clone()));
        }
        return;
    }

    // (d) Load. `load` publishes ready/unavailable itself
    // (`load_embedding_model_with_mode` already calls
    // `publish_ready`/`publish_unavailable`), so this function publishes
    // nothing new — it only reports the resulting status back to callers.
    let outcome = tokio::select! {
        result = load => Some(result),
        _ = shutdown_sub.changed() => None,
    };

    // (e) Reply to every coalesced sender with the resulting status.
    match outcome {
        None => {
            for reply in replies {
                let _ = reply.send(EmbeddingReloadOutcome::ShuttingDown);
            }
        }
        Some(Ok(())) => {
            let status = state.embedding_runtime.status();
            for reply in replies {
                let _ = reply.send(EmbeddingReloadOutcome::Loaded(status.clone()));
            }
        }
        Some(Err(_failure)) => {
            let status = state.embedding_runtime.status();
            for reply in replies {
                let _ = reply.send(EmbeddingReloadOutcome::Unavailable(status.clone()));
            }
        }
    }
}

// ── Seeding: single-flight, off the write gate (nw-484) ────────────────

/// Progress of an in-flight (or most recently attempted) artifact seed,
/// surfaced through `brain_status`'s JSON route. Deliberately independent of
/// `EmbeddingRuntimeStatus`: that struct is swapped wholesale on
/// publish/unavailable, while seed progress changes continuously (bytes)
/// across attempts that do not themselves change the published status until
/// they finish.
///
/// `origin`/`attempt`/`max_attempts` describe whoever STARTED the current
/// flight, not every caller currently waiting on it. When a second caller
/// joins an in-flight download via `join_or_start_seed` (e.g. an operator's
/// `embed` RPC joining a download auto-repair already started), these
/// fields keep reporting the original starter — an `embed`-joins-auto_repair
/// case therefore still shows `seed_origin: "auto_repair"` in `brain_status`
/// for the whole download, which is honest (there is genuinely only one
/// download in flight) but can read as surprising if you only know about
/// the RPC you just called.
/// Every field `pub(crate)`: `crate::server`'s test fake seeders (barrier-
/// blocked fakes exercising `brain_status` mid-download) write these atomics
/// directly rather than through a method, to simulate hf-hub's real progress
/// callbacks without depending on this module's own `ArtifactProgressSink`
/// adapter.
#[derive(Default)]
pub(crate) struct SeedProgress {
    active: AtomicBool,
    pub(crate) bytes_done: AtomicU64,
    pub(crate) bytes_total: AtomicU64,
    origin: std::sync::Mutex<String>,
    attempt: AtomicU32,
    max_attempts: AtomicU32,
    next_retry_at: std::sync::atomic::AtomicI64,
}

/// Immutable view of [`SeedProgress`], read under `brain_status`.
/// `pub(crate)` (struct and every field): `crate::server`'s
/// `embedding_status_proto`/`embedding_status_json` read every field
/// directly to populate the typed and JSON status routes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SeedProgressSnapshot {
    pub(crate) active: bool,
    pub(crate) bytes_done: u64,
    pub(crate) bytes_total: u64,
    pub(crate) origin: String,
    pub(crate) attempt: u32,
    pub(crate) max_attempts: u32,
    pub(crate) next_retry_at: i64,
}

impl SeedProgress {
    /// `pub(crate)`: called directly by `crate::server`'s
    /// `seed_progress_sink_ordering_and_snapshot_clamp` test to arm a
    /// `SeedProgress` before exercising `SeedProgressSink`. Every caller
    /// (production and test) is itself `feature = "embed"`-gated, so this
    /// carries the same gate rather than the broader `any(.., test)` it used
    /// to — the wider gate left it "never used" (a warning, not an error)
    /// under `--no-default-features --features metal` (CI's Cold Metal job),
    /// which still builds the `lib test` target with `embed` off.
    #[cfg(feature = "embed")]
    pub(crate) fn begin(&self, origin: &str, attempt: u32, max_attempts: u32) {
        self.bytes_done.store(0, Ordering::Relaxed);
        self.bytes_total.store(0, Ordering::Relaxed);
        *self.origin.lock().unwrap_or_else(|e| e.into_inner()) = origin.to_string();
        self.attempt.store(attempt, Ordering::Relaxed);
        self.max_attempts.store(max_attempts, Ordering::Relaxed);
        self.next_retry_at.store(0, Ordering::Relaxed);
        // Set active LAST: every other field must already be correct the
        // instant a concurrent reader can observe `active == true`.
        self.active.store(true, Ordering::Release);
    }

    #[cfg(feature = "embed")]
    fn end(&self) {
        self.active.store(false, Ordering::Release);
    }

    #[cfg(feature = "embed")]
    fn set_next_retry_at(&self, unix_secs: i64) {
        self.next_retry_at.store(unix_secs, Ordering::Relaxed);
    }

    fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    /// `pub(crate)`: read directly by `crate::server`'s
    /// `embedding_status_proto`/`embedding_status_json`.
    pub(crate) fn snapshot(&self) -> SeedProgressSnapshot {
        // Read total BEFORE done, matching `SeedProgressSink::on_bytes`'s
        // write order, then clamp defensively: the two stores are still two
        // independent relaxed atomics (not one lock), so a reader landing
        // between them is possible in principle even with the matched
        // ordering. `done > total` is meaningless to every consumer of this
        // snapshot (the JSON and typed status routes both just print it), so
        // clamp here once rather than trusting every caller to.
        let bytes_total = self.bytes_total.load(Ordering::Relaxed);
        let bytes_done = self.bytes_done.load(Ordering::Relaxed);
        let bytes_done = if bytes_total > 0 {
            bytes_done.min(bytes_total)
        } else {
            bytes_done
        };
        SeedProgressSnapshot {
            active: self.is_active(),
            bytes_done,
            bytes_total,
            origin: self
                .origin
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            attempt: self.attempt.load(Ordering::Relaxed),
            max_attempts: self.max_attempts.load(Ordering::Relaxed),
            next_retry_at: self.next_retry_at.load(Ordering::Relaxed),
        }
    }
}

/// Terminal outcome of one seed attempt, broadcast to every joiner. Only
/// constructed/read by the embed-only single-flight machinery
/// (`join_or_start_seed`/`clear_stale_seed_flight`) and `DaemonState`'s
/// `embedding_seed` field, both gated the same way — see that field's own
/// comment for why it (unlike `embedding_reload_tx`) can be gated cleanly.
#[cfg(feature = "embed")]
#[derive(Clone)]
pub(crate) enum SeedPhase {
    Running,
    Done(Result<PathBuf, String>),
}

/// The in-flight (or most recently finished) seed download, so a second
/// caller joins instead of starting a duplicate download.
#[cfg(feature = "embed")]
pub(crate) struct SeedFlight {
    key: (String, PathBuf),
    rx: tokio::sync::watch::Receiver<SeedPhase>,
}

/// Downloads any missing local model artifacts into the configured cache.
/// Injected on [`DaemonState`] so tests never reach the real Hugging Face
/// endpoint or the developer's real platform model cache (nw-483): production
/// wraps `nestweaver_embed::resolve_model_artifacts_with_progress(..,
/// DownloadMissing, ..)`, tests substitute a fake built on
/// `write_complete_hf_cache`. No new env var — this is a seam on the
/// injected seeder, exactly as the nw-484 design specifies.
///
/// Takes an owned `Arc<SeedProgress>` (not `&SeedProgress`): the production
/// implementation builds an `hf_hub::progress::Progress` handler out of it,
/// and hf-hub's `Progress::new` requires `'static` (the handler may be
/// cloned across background transfer tasks) — a borrowed reference cannot
/// satisfy that.
#[cfg(feature = "embed")]
pub(crate) type ArtifactSeeder = Arc<
    dyn Fn(&nestweaver_embed::EmbedConfig, Arc<SeedProgress>) -> anyhow::Result<()> + Send + Sync,
>;

/// Adapts [`SeedProgress`]'s byte counters to
/// [`nestweaver_embed::ArtifactProgressSink`], so `production_artifact_seeder`
/// can hand hf-hub's real per-file transfer events straight through to
/// `brain_status`'s `seed_bytes_done`/`seed_bytes_total`.
///
/// `pub(crate)` (struct and its one field): `crate::server`'s
/// `seed_progress_sink_ordering_and_snapshot_clamp` test constructs this
/// directly to pin the store-ordering contract described on its `on_bytes`.
#[cfg(feature = "embed")]
pub(crate) struct SeedProgressSink(pub(crate) Arc<SeedProgress>);

#[cfg(feature = "embed")]
impl nestweaver_embed::ArtifactProgressSink for SeedProgressSink {
    fn on_bytes(&self, done: u64, total: u64) {
        // Total BEFORE done: `SeedProgress::snapshot()` reads both without a
        // shared lock (it's called from the JSON/typed status routes, on a
        // different thread than this seed thread), so a reader racing this
        // store must never observe a stale `total` paired with the NEW
        // `done` — that pairing is exactly `done > total`. Storing total
        // first means the only possible race window shows the OLD (still
        // internally consistent, if momentarily stale) total next to the
        // new done; `snapshot()` also clamps defensively on top of this.
        self.0.bytes_total.store(total, Ordering::Relaxed);
        self.0.bytes_done.store(done, Ordering::Relaxed);
    }
}

/// Production artifact seeder: resolve, downloading anything missing, with a
/// real progress sink wired to `seed_bytes_done`/`seed_bytes_total`. Never
/// touches the write gate — see `join_or_start_seed`.
///
/// `cfg(not(test))`, mirroring `production_reload_loader` right below: its
/// only caller is `run_server`'s `DaemonState` construction, which is itself
/// split the same way so an in-process `run_server` under libtest can never
/// reach the real Hugging Face endpoint.
#[cfg(all(feature = "embed", not(test)))]
pub(crate) fn production_artifact_seeder(
    config: &nestweaver_embed::EmbedConfig,
    progress: Arc<SeedProgress>,
) -> anyhow::Result<()> {
    let sink: Arc<dyn nestweaver_embed::ArtifactProgressSink> =
        Arc::new(SeedProgressSink(progress));
    nestweaver_embed::resolve_model_artifacts_with_progress(
        config,
        nestweaver_embed::ArtifactMode::DownloadMissing,
        Some(sink),
    )
    .map(|_artifacts| ())
}

/// Default test artifact seeder (nw-483/nw-484): writes the offline fixture
/// cache and succeeds, exactly like the design's "success" fake. Every test
/// `DaemonState` gets this by default instead of `production_artifact_seeder`
/// — measured empirically, `resolve_model_artifacts(.., DownloadMissing)`
/// against `write_complete_hf_cache`'s hand-built fixture still reaches the
/// real Hugging Face endpoint (its ref/commit resolution is not satisfied by
/// `local_files_only` alone the way `CacheOnly` mode's `ArtifactMode` check
/// is), so any test reaching the seed path through the real seeder hits the
/// network. Only `run_server`'s production path may use
/// `production_artifact_seeder`.
#[cfg(all(feature = "embed", test))]
pub(crate) fn fake_default_test_seeder(
    config: &nestweaver_embed::EmbedConfig,
    _progress: Arc<SeedProgress>,
) -> anyhow::Result<()> {
    write_complete_hf_cache(&config.cache_dir);
    Ok(())
}

/// Best-effort extraction of a panic payload's message, for a diagnostic
/// string only — never used for control flow.
#[cfg(feature = "embed")]
fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// Clears `state.embedding_seed` ONLY if it still holds the flight for
/// `key`. Used after observing a closed seed-result channel (a joiner's
/// `wait_for` returned `Err`): without this, a stale `SeedFlight` whose
/// `rx` still reads `Running` (the last value anyone ever sent) would make
/// every future `join_or_start_seed` call for this model believe a download
/// is still in progress and join a channel nothing will ever complete on —
/// wedging recovery for this model until the daemon restarts. Checking the
/// key first avoids clobbering a NEWER flight a concurrent caller may have
/// already installed.
#[cfg(feature = "embed")]
pub(crate) fn clear_stale_seed_flight(state: &Arc<DaemonState>, key: &(String, PathBuf)) {
    let mut guard = state
        .embedding_seed
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if guard.as_ref().is_some_and(|flight| &flight.key == key) {
        *guard = None;
    }
}

/// Join an in-flight seed download for this exact `(model_id, cache_dir)`,
/// or start a new one on a dedicated `nw-embed-seed` thread.
///
/// Never `spawn_blocking`: the tokio runtime's `Drop` waits forever on a
/// blocking task, so a hung download would pin process exit indefinitely
/// (nw-126-shaped escape-hatch problem). A bare `std::thread` is abandoned on
/// exit instead; the partial `.incomplete` file hf-hub leaves behind is safe
/// to resume or overwrite.
///
/// Holds a `ConnectionGuard::read` for the life of the download — enough to
/// suppress an idle-timeout exit while bytes are moving, without being a
/// drain signal (`active_writes` is untouched, so a shutdown drain is never
/// extended by a download in flight).
///
/// Returns `Err` only when the OS refuses to spawn the thread at all — in
/// that case no flight is left behind (the just-installed `SeedFlight` is
/// rolled back) so a caller's retry starts clean rather than joining a
/// phantom "Running" download nothing will ever finish.
///
/// The spawned thread reports its outcome through an RAII guard
/// (`FinishOnDrop`) rather than a plain post-call send, specifically so a
/// PANIC inside `seeder` still reports failure: `Drop::drop` runs during an
/// unwind exactly as it does on a normal return, so `progress.end()` and a
/// `Done(Err(..))` send are guaranteed exactly once regardless of how the
/// thread's closure exits. Without this, a panicking seeder would unwind
/// past both the progress-clear and the channel send, leaving
/// `seed_active` stuck `true` forever and every future joiner blocked on a
/// channel that will never receive `Done` (this was a real defect, fixed
/// before nw-484 shipped — see `daemon_embed_recovers_after_a_seeder_panic`).
#[cfg(feature = "embed")]
pub(crate) fn join_or_start_seed(
    state: &Arc<DaemonState>,
    config: &nestweaver_embed::EmbedConfig,
    origin: &str,
    attempt: u32,
    max_attempts: u32,
) -> std::io::Result<tokio::sync::watch::Receiver<SeedPhase>> {
    let key = (config.model_id.clone(), config.cache_dir.clone());
    let mut guard = state
        .embedding_seed
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(flight) = guard.as_ref()
        && flight.key == key
        && matches!(*flight.rx.borrow(), SeedPhase::Running)
    {
        return Ok(flight.rx.clone());
    }

    let (tx, rx) = tokio::sync::watch::channel(SeedPhase::Running);
    *guard = Some(SeedFlight {
        key: key.clone(),
        rx: rx.clone(),
    });
    drop(guard);

    state
        .embedding_seed_progress
        .begin(origin, attempt, max_attempts);
    let seeder = Arc::clone(&state.artifact_seeder);
    let progress = Arc::clone(&state.embedding_seed_progress);
    let cache_dir = config.cache_dir.clone();
    let config = config.clone();
    let read_admission = ConnectionGuard::read(state);
    let spawned = std::thread::Builder::new()
        .name("nw-embed-seed".to_string())
        .spawn(move || {
            /// Guarantees a `Done` outcome is always sent and progress is
            /// always cleared, even when the thread unwinds from a panic
            /// (`Drop::drop` runs during unwinding, not just on normal
            /// return). `outcome` starts `None`; if the seeder call panics
            /// before setting it, the drop impl supplies a generic failure
            /// so joiners still get a real answer instead of hanging.
            struct FinishOnDrop {
                progress: Arc<SeedProgress>,
                cache_dir: PathBuf,
                tx: Option<tokio::sync::watch::Sender<SeedPhase>>,
                outcome: Option<Result<(), String>>,
            }
            impl Drop for FinishOnDrop {
                fn drop(&mut self) {
                    self.progress.end();
                    let outcome = self.outcome.take().unwrap_or_else(|| {
                        Err("seed thread exited without reporting an outcome \
                             (this indicates a panic outside the caught region)"
                            .to_string())
                    });
                    let phase = match outcome {
                        Ok(()) => SeedPhase::Done(Ok(self.cache_dir.clone())),
                        Err(error) => SeedPhase::Done(Err(error)),
                    };
                    if let Some(tx) = self.tx.take() {
                        let _ = tx.send(phase);
                    }
                }
            }

            let mut finish = FinishOnDrop {
                progress: Arc::clone(&progress),
                cache_dir,
                tx: Some(tx),
                outcome: None,
            };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                seeder(&config, Arc::clone(&progress))
            }));
            finish.outcome = Some(match result {
                Ok(Ok(())) => Ok(()),
                Ok(Err(error)) => Err(format!("{error:#}")),
                Err(panic) => Err(format!(
                    "seed thread panicked: {}",
                    panic_payload_message(&*panic)
                )),
            });
            drop(read_admission);
            // `finish` drops here (end of scope), which is what actually
            // sends `Done` and clears progress — see `FinishOnDrop` above.
        });

    match spawned {
        Ok(_join_handle) => Ok(rx),
        Err(error) => {
            clear_stale_seed_flight(state, &key);
            state.embedding_seed_progress.end();
            Err(error)
        }
    }
}

/// Production loader for the main-thread reload channel: `CacheOnly` only —
/// the reload loop runs on the daemon's main thread and must never download
/// (§4.1 of the nw-484 design: downloading is the dedicated seed thread's
/// job, never the main thread's). A plain `fn` item (not a closure), so it is
/// zero-sized and `Copy`, cheap to pass into `service_embedding_reloads` on
/// every loop iteration.
#[cfg(all(feature = "embed", not(test)))]
pub(crate) async fn production_reload_loader(
    state: &std::sync::Arc<DaemonState>,
) -> Result<(), EmbeddingLoadFailure> {
    load_embedding_model_with_mode(state, nestweaver_embed::ArtifactMode::CacheOnly).await
}

/// `run_server`'s reload loop under `cfg(test)` only: libtest never runs a
/// test on the process main thread, so the real loader's main-thread assert
/// (`load_embedding_model_with_mode`) cannot be exercised through an
/// in-process `run_server`. Publishes an honest, typed failure instead of
/// hanging a reload requester or tripping that assert. Unit tests of the
/// reload mechanism itself (`service_embedding_reloads` called directly) use
/// their own fakes and never reach this function.
#[cfg(all(feature = "embed", test))]
pub(crate) async fn unserviced_reload_loader(
    state: &std::sync::Arc<DaemonState>,
) -> Result<(), EmbeddingLoadFailure> {
    let mut status = state.embedding_runtime.status();
    status.state = "failed".to_string();
    status.error = "model load is not serviced under libtest".to_string();
    state.embedding_runtime.publish_unavailable(status);
    Err(EmbeddingLoadFailure::default())
}

// ── Auto-repair of a known model's missing cache (nw-484 / D11) ────────

/// Whether a background cache repair should start for a boot-load failure,
/// and if so, which model it should repair for.
#[cfg(feature = "embed")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AutoRepair {
    Eligible { model_id: String },
    Skip(&'static str),
}

/// Pure decision function (design §4.4): auto-repair is eligible only for a
/// local backend, only when the operator has not opted out, only for a model
/// this database's VERIFIED persisted identity already names (never a
/// first-time download — those stay operator-initiated), and only when the
/// boot load failed with a typed missing-artifact cause. `stored_model_id`
/// must already be `None` unless the identity was verified — callers resolve
/// that (a store read) before calling this pure function.
#[cfg(feature = "embed")]
pub(crate) fn auto_repair_eligibility(
    cfg: &nestweaver_engine::config::EmbeddingConfig,
    stored_model_id: Option<&str>,
    failure: &EmbeddingLoadFailure,
) -> AutoRepair {
    if cfg.external_endpoint.is_some() {
        return AutoRepair::Skip("an external embedding backend has no local cache to repair");
    }
    if !cfg.auto_repair_cache {
        return AutoRepair::Skip("[embedding] auto_repair_cache = false");
    }
    let Some(model_id) = stored_model_id.filter(|id| !id.is_empty()) else {
        return AutoRepair::Skip("no recorded embedding identity for this database");
    };
    if !failure.missing_artifact {
        return AutoRepair::Skip("boot load failed for a reason other than a missing artifact");
    }
    AutoRepair::Eligible {
        model_id: model_id.to_string(),
    }
}

/// The bounded backoff schedule for auto-repair attempts 2-5 (attempt 1 is
/// immediate): 30s, 2m, 8m, 30m, each independently jittered by up to ±20%.
/// Same capped-exponential-with-jitter shape as `trigram_reconcile_backoff`,
/// spelled out as a fixed table (rather than doubling) because the schedule
/// itself is disclosed to the operator in `brain_status` text.
#[cfg(feature = "embed")]
const AUTO_REPAIR_BACKOFF_BASE_SECS: [u64; 4] = [30, 120, 480, 1800];

#[cfg(feature = "embed")]
const AUTO_REPAIR_MAX_ATTEMPTS: u32 = 5;

/// Deterministic ±20% jitter with no `rand` dependency: a fixed-round
/// SplitMix64-shaped mix of the attempt number and a per-repair-task nonce.
/// Not cryptographic — a thundering-herd-avoidance jitter has no such
/// requirement — but reproducible under a fixed seed, which is what makes
/// `daemon_auto_repair_backs_off_after_failure_without_hot_looping` a stable
/// assertion instead of a flake.
#[cfg(feature = "embed")]
fn jitter_unit_interval(seed: u64) -> f64 {
    let mut x = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    (x >> 11) as f64 / (1u64 << 53) as f64
}

/// Delay before auto-repair attempt `attempt` (2-5; attempt 1 is immediate),
/// ±20% jitter seeded by `nonce` (a value fixed for the life of one repair
/// task, so the schedule is reproducible within a run but varies run to run).
#[cfg(feature = "embed")]
fn auto_repair_delay(attempt: u32, nonce: u64) -> Duration {
    let index = (attempt.saturating_sub(2)) as usize;
    let base_secs =
        AUTO_REPAIR_BACKOFF_BASE_SECS[index.min(AUTO_REPAIR_BACKOFF_BASE_SECS.len() - 1)];
    let unit = jitter_unit_interval(nonce ^ u64::from(attempt));
    // unit is in [0, 1); map to [-0.2, +0.2] of the base.
    let jitter_fraction = (unit - 0.5) * 0.4;
    let jittered_secs = (base_secs as f64) * (1.0 + jitter_fraction);
    Duration::from_secs_f64(jittered_secs.max(0.0))
}

/// Classify a seed failure as transient (retry) or permanent (stop early).
/// Conservative: anything not positively identified as permanent is treated
/// as transient, so an unrecognized error keeps the bounded retry schedule
/// rather than silently going quiet after one attempt.
#[cfg(feature = "embed")]
fn auto_repair_error_is_permanent(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("entrynotfound")
        || lower.contains("entry not found")
        || lower.contains("repositorynotfound")
        || lower.contains("repository not found")
        || lower.contains("401")
        || lower.contains("403")
        || lower.contains("unauthorized")
        || lower.contains("forbidden")
        || lower.contains("permission denied")
        || lower.contains("no space left")
        || lower.contains("enospc")
        || lower.contains("eacces")
}

/// Spawn the bounded background repair loop for `model_id`. Callers decide
/// eligibility first (`auto_repair_eligibility`); this function always runs
/// once spawned, up to `AUTO_REPAIR_MAX_ATTEMPTS` attempts. Every attempt
/// joins the same single-flight seed coordinator the `embed` RPC uses
/// (`join_or_start_seed`), so an operator-initiated `embed` while a repair is
/// backing off starts immediately (operator intent overrides backoff) and a
/// concurrent `embed` while a repair is downloading joins that download
/// rather than racing a second one.
///
/// Neither the write gate nor `ConnectionGuard::write` is ever held: a
/// shutdown drain is never extended by this task, and it is not awaited in
/// `run_server`'s exit sequence — it selects on the shutdown broadcast and
/// returns.
#[cfg(feature = "embed")]
pub(crate) fn spawn_embedding_cache_repair(
    state: Arc<DaemonState>,
    model_id: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(run_embedding_cache_repair(state, model_id))
}

#[cfg(feature = "embed")]
async fn run_embedding_cache_repair(state: Arc<DaemonState>, model_id: String) {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ (std::process::id() as u64);
    let mut shutdown_sub = state.shutdown_tx.subscribe();

    for attempt in 1..=AUTO_REPAIR_MAX_ATTEMPTS {
        if state.embedding_runtime.status().state == "ready" {
            return;
        }
        if attempt > 1 {
            let delay = auto_repair_delay(attempt, nonce);
            let next_retry_at = unix_now_seconds() + delay.as_secs() as i64;
            state
                .embedding_seed_progress
                .set_next_retry_at(next_retry_at);
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = shutdown_sub.changed() => return,
            }
            if state.shutdown_started.load(Ordering::SeqCst) {
                return;
            }
            if state.embedding_runtime.status().state == "ready" {
                return;
            }
        }

        let cfg = state
            .instance_cfg
            .as_ref()
            .map(|c| c.embedding.clone())
            .unwrap_or_default();
        let cache_dir = match embedding_cache_dir_for_load_with(
            &cfg,
            nestweaver_engine::resolve_user_path,
        ) {
            Ok(dir) => dir,
            Err(error) => {
                // Not retryable — the configured cache path itself is
                // unusable, so another attempt on the same schedule
                // would fail identically. Still, this must never exit
                // silently: an operator watching `brain_status` needs to
                // see WHY semantic search stayed degraded.
                tracing::warn!(
                    model_id = %model_id,
                    error = %error,
                    "embedding cache auto-repair stopped: cannot resolve the configured cache directory"
                );
                let mut failed = state.embedding_runtime.status();
                failed.state = "failed".to_string();
                failed.error = format!(
                    "automatic model download could not start: failed to resolve the \
                         configured embedding cache directory: {error}; run `nestweaver embed` to retry"
                );
                state.embedding_runtime.publish_unavailable(failed);
                return;
            }
        };
        let config = embedding_load_config(&cfg, cache_dir, Some(&model_id));
        let seed_key = (config.model_id.clone(), config.cache_dir.clone());

        let seed_result: Result<PathBuf, String> = match join_or_start_seed(
            &state,
            &config,
            "auto_repair",
            attempt,
            AUTO_REPAIR_MAX_ATTEMPTS,
        ) {
            Ok(mut seed_rx) => {
                tokio::select! {
                    changed = seed_rx.wait_for(|phase| matches!(phase, SeedPhase::Done(_))) => {
                        match changed {
                            Ok(phase_ref) => match &*phase_ref {
                                SeedPhase::Done(result) => result.clone(),
                                SeedPhase::Running => unreachable!("wait_for only resolves on Done"),
                            },
                            Err(_) => {
                                // Sender closed without a Done outcome.
                                // `join_or_start_seed`'s RAII guard makes this
                                // unreachable in practice; clear the stale
                                // flight defensively and treat it as a
                                // transient failure so the bounded retry
                                // schedule still applies instead of the
                                // repair silently giving up on this model
                                // until a restart.
                                tracing::warn!(
                                    model_id = %model_id,
                                    "embedding seed watch channel closed without a Done outcome \
                                     during auto-repair; clearing the flight and retrying on schedule"
                                );
                                clear_stale_seed_flight(&state, &seed_key);
                                Err("embedding seed watch channel closed unexpectedly".to_string())
                            }
                        }
                    }
                    _ = shutdown_sub.changed() => return,
                }
            }
            Err(error) => {
                tracing::warn!(
                    model_id = %model_id,
                    error = %error,
                    "embedding cache auto-repair could not spawn the seed thread; retrying on schedule"
                );
                Err(format!(
                    "failed to start embedding artifact download: {error}"
                ))
            }
        };

        match seed_result {
            Ok(_cache_dir) => {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                if state
                    .embedding_reload_tx
                    .send(EmbeddingReloadRequest { reply: reply_tx })
                    .await
                    .is_err()
                {
                    return;
                }
                let reload_outcome = tokio::select! {
                    result = reply_rx => result.ok(),
                    _ = shutdown_sub.changed() => return,
                };
                match reload_outcome {
                    Some(EmbeddingReloadOutcome::Loaded(_)) => return,
                    Some(EmbeddingReloadOutcome::ShuttingDown) => return,
                    // Loaded failed (a non-missing-artifact reload failure, or
                    // a race where the cache became invalid again): keep
                    // retrying on the same bounded schedule.
                    Some(EmbeddingReloadOutcome::Unavailable(_)) | None => {}
                }
            }
            Err(error) => {
                if auto_repair_error_is_permanent(&error) {
                    let mut failed = state.embedding_runtime.status();
                    failed.state = "failed".to_string();
                    failed.error = format!(
                        "automatic model download stopped after {attempt} attempt(s): {error}; \
                         run `nestweaver embed` to retry"
                    );
                    state.embedding_runtime.publish_unavailable(failed);
                    return;
                }
                let mut failed = state.embedding_runtime.status();
                failed.state = "failed".to_string();
                failed.error = if attempt < AUTO_REPAIR_MAX_ATTEMPTS {
                    let next_delay = auto_repair_delay(attempt + 1, nonce);
                    format!(
                        "automatic model download failed (attempt {attempt} of {AUTO_REPAIR_MAX_ATTEMPTS}): \
                         {error}; retrying in {}s; run `nestweaver embed` to retry now",
                        next_delay.as_secs()
                    )
                } else {
                    format!(
                        "automatic model download stopped after {attempt} attempt(s): {error}; \
                         run `nestweaver embed` to retry"
                    )
                };
                state.embedding_runtime.publish_unavailable(failed);
            }
        }
    }
}
