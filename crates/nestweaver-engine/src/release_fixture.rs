//! Internal, default-off daemon acceptance hooks. No direct-store authority.
//!
//! Armed stages: committed-content, generation-save, and pre-manifest-save.
//! An activation is single-use, bound to a newly created private fixture and
//! its parent process.

use anyhow::{Context, Result, ensure};
use nestweaver_store::GraphStore;
use serde::Deserialize;
use serde_json::{Value, json};
use std::cell::RefCell;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CONTENT_STAGE: &str = "index_content_committed";
const GENERATION_STAGE: &str = "index_generation_save";
const MANIFEST_STAGE: &str = "manifest_before_save";
const MAX_RECORD_BYTES: u64 = 4096;
static FIXTURE: OnceLock<Arc<Fixture>> = OnceLock::new();
thread_local! {
    static INDEX: RefCell<Option<Arc<Operation>>> = const { RefCell::new(None) };
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Activation {
    protocol: u32,
    nonce: String,
    owner_pid: u32,
    database: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Command {
    protocol: u32,
    nonce: String,
    brain_uuid: String,
    daemon_pid: u32,
    sequence: u64,
    request_id: String,
    stage: String,
    action: String,
    repo_root: PathBuf,
    target_sha: String,
    expires_unix_ms: u64,
}

struct Fixture {
    control: PathBuf,
    root: PathBuf,
    database: PathBuf,
    activation: Activation,
    brain_uuid: OnceLock<String>,
    command_consumed: AtomicBool,
    receipts: Mutex<File>,
}

struct Operation {
    fixture: Arc<Fixture>,
    command: Command,
    repo_uid: String,
    reached: AtomicBool,
    deadline: Instant,
}

/// Scoped only to the daemon's existing blocking index worker.
pub struct IndexScope(Arc<Operation>);

fn now_ms() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .try_into()?)
}

fn lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

#[cfg(unix)]
fn private_metadata(path: &Path, directory: bool) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        !metadata.file_type().is_symlink()
            && metadata.is_dir() == directory
            && (directory || metadata.is_file())
            && metadata.mode() & 0o077 == 0
            && metadata.uid() == unsafe { libc::geteuid() },
        "fixture path must be private, owned, and free of symlinks"
    );
    Ok(())
}

#[cfg(not(unix))]
fn private_metadata(_path: &Path, _directory: bool) -> Result<()> {
    anyhow::bail!("release fixture activation requires Unix ownership checks")
}

fn new_private_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    Ok(options.open(path)?)
}

fn read_record<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    private_metadata(path, false)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    ensure!(
        file.metadata()?.len() <= MAX_RECORD_BYTES,
        "fixture record exceeds bound"
    );
    let mut bytes = Vec::new();
    file.take(MAX_RECORD_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_RECORD_BYTES,
        "fixture record exceeds bound"
    );
    Ok(serde_json::from_slice(&bytes)?)
}

impl Fixture {
    fn check_owner(&self) -> Result<()> {
        private_metadata(&self.root, true)?;
        private_metadata(&self.control, true)?;
        ensure!(
            self.root.canonicalize()? == self.root,
            "fixture root changed"
        );
        ensure!(
            self.control.canonicalize()? == self.control,
            "fixture control changed"
        );
        #[cfg(unix)]
        ensure!(
            unsafe { libc::getppid() } as u32 == self.activation.owner_pid,
            "fixture parent ownership changed"
        );
        Ok(())
    }

    fn record(&self, kind: &str, details: Value) -> Result<()> {
        let event = json!({
            "protocol": 1, "kind": kind, "nonce": self.activation.nonce,
            "daemon_pid": std::process::id(), "database": self.database,
            "brain_uuid": self.brain_uuid.get(), "time_unix_ms": now_ms()?,
            "details": details,
        });
        let mut file = self
            .receipts
            .lock()
            .map_err(|_| anyhow::anyhow!("fixture receipt lock poisoned"))?;
        serde_json::to_writer(&mut *file, &event)?;
        file.write_all(b"\n")?;
        file.flush()?;
        Ok(())
    }
}

/// Called only by the feature-gated foreground-daemon argument, before open.
pub fn activate(control: &Path, database: &Path) -> Result<()> {
    ensure!(FIXTURE.get().is_none(), "fixture already activated");
    private_metadata(control, true)?;
    let control = control.canonicalize()?;
    let root = control
        .parent()
        .context("fixture control needs a parent")?
        .to_path_buf();
    private_metadata(&root, true)?;
    ensure!(
        root.parent() == Some(Path::new("/tmp").canonicalize()?.as_path())
            && root
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("nw-release-"))
            && control.file_name().is_some_and(|n| n == "fixture-control"),
        "fixture must be a newly owned /tmp/nw-release-* directory"
    );
    let expected_database = root.join("fixture.lbug");
    ensure!(
        database == expected_database,
        "fixture database path must be exact and canonical"
    );
    ensure!(
        !std::fs::read_dir(&root)?.any(|entry| entry.map_or(true, |entry| entry
            .file_name()
            .to_string_lossy()
            .starts_with("fixture.lbug"))),
        "fixture database and sidecars must not already exist"
    );
    let entries: Vec<_> = std::fs::read_dir(&control)?.collect::<std::io::Result<_>>()?;
    ensure!(
        entries.len() == 1 && entries[0].file_name() == "activation.json",
        "stale fixture controls refused"
    );
    let activation: Activation = read_record(&control.join("activation.json"))?;
    ensure!(
        activation.protocol == 1 && lower_hex(&activation.nonce, 64),
        "invalid fixture activation"
    );
    ensure!(
        activation.database == expected_database && activation.owner_pid > 1,
        "fixture ownership mismatch"
    );
    #[cfg(unix)]
    ensure!(
        unsafe { libc::getppid() } as u32 == activation.owner_pid,
        "fixture must be launched by its owner"
    );
    // create_new makes a failed or completed activation permanently single-use.
    let mut claim = new_private_file(&control.join("claimed"))?;
    writeln!(claim, "{}", std::process::id())?;
    let receipts = new_private_file(&control.join("receipts.jsonl"))?;
    let fixture = Arc::new(Fixture {
        control,
        root,
        database: expected_database,
        activation,
        brain_uuid: OnceLock::new(),
        command_consumed: AtomicBool::new(false),
        receipts: Mutex::new(receipts),
    });
    fixture.record("activation_checked", json!({}))?;
    FIXTURE
        .set(fixture)
        .map_err(|_| anyhow::anyhow!("fixture already activated"))
}

/// Recheck before the daemon can select a replica/configured database or open it.
pub fn validate_daemon_start(database: &Path, configured_or_server: bool) -> Result<()> {
    if let Some(fixture) = FIXTURE.get() {
        fixture.check_owner()?;
        ensure!(
            !configured_or_server && database == fixture.database && !database.exists(),
            "fixture requires a fresh, unconfigured local daemon"
        );
    }
    Ok(())
}

pub fn bind_daemon_store(store: &GraphStore) -> Result<()> {
    if let Some(fixture) = FIXTURE.get() {
        fixture.check_owner()?;
        ensure!(
            store.db_path() == Some(fixture.database.as_path()),
            "fixture store changed"
        );
        let identity = store
            .publication_identity()?
            .context("fixture persistent identity missing")?;
        fixture
            .brain_uuid
            .set(identity.brain_uuid)
            .map_err(|_| anyhow::anyhow!("fixture store already bound"))?;
        fixture.record("identity_bound", json!({}))?;
    }
    Ok(())
}

/// Only an exact foreground daemon index can consume the single armed command.
pub fn begin_index(
    store: &GraphStore,
    repo_root: &Path,
    repo_uid: &str,
    target_sha: &str,
) -> Result<Option<IndexScope>> {
    let Some(fixture) = FIXTURE.get() else {
        return Ok(None);
    };
    fixture.check_owner()?;
    let command_path = fixture.control.join("command.json");
    if !command_path.try_exists()? {
        return Ok(None);
    }
    ensure!(
        !fixture.command_consumed.swap(true, Ordering::SeqCst),
        "fixture command already consumed"
    );
    let command: Command = read_record(&command_path)?;
    if command.stage == MANIFEST_STAGE {
        fixture.command_consumed.store(false, Ordering::SeqCst);
        return Ok(None);
    }
    validate_command(&command, fixture, repo_root, target_sha)?;
    ensure!(
        store.db_path() == Some(fixture.database.as_path()),
        "fixture store changed"
    );
    ensure!(
        store.lookup_repo(repo_uid)?.is_none(),
        "first-content fixture requires a fresh repo"
    );
    std::fs::rename(command_path, fixture.control.join("consumed-1.json"))?;
    let remaining_ms = command
        .expires_unix_ms
        .checked_sub(now_ms()?)
        .filter(|remaining| *remaining > 0 && *remaining <= 30_000)
        .context("fixture command expired while arming")?;
    let operation = Arc::new(Operation {
        fixture: Arc::clone(fixture),
        command,
        repo_uid: repo_uid.to_owned(),
        reached: AtomicBool::new(false),
        deadline: Instant::now() + Duration::from_millis(remaining_ms),
    });
    ensure!(
        INDEX.with(|slot| slot.borrow().is_none()),
        "fixture operation already active"
    );
    INDEX.with(|slot| *slot.borrow_mut() = Some(Arc::clone(&operation)));
    let scope = IndexScope(operation);
    scope.0.event("armed", json!({"target_sha": target_sha}))?;
    Ok(Some(scope))
}

fn validate_command(
    command: &Command,
    fixture: &Fixture,
    repo_root: &Path,
    target_sha: &str,
) -> Result<()> {
    ensure!(
        command.protocol == 1 && command.sequence == 1 && lower_hex(&command.request_id, 32),
        "invalid fixture command identity"
    );
    ensure!(
        command.nonce == fixture.activation.nonce
            && Some(&command.brain_uuid) == fixture.brain_uuid.get()
            && command.daemon_pid == std::process::id(),
        "fixture command owner mismatch"
    );
    ensure!(
        matches!(
            command.stage.as_str(),
            CONTENT_STAGE | GENERATION_STAGE | MANIFEST_STAGE
        ) && command.action == "return_error",
        "unsupported fixture stage or action"
    );
    let now = now_ms()?;
    ensure!(
        command.expires_unix_ms > now && command.expires_unix_ms <= now.saturating_add(30_000),
        "fixture command expired or exceeds 30-second lifetime"
    );
    ensure!(
        repo_root.canonicalize()? == command.repo_root
            && command.repo_root.parent() == Some(fixture.root.as_path()),
        "fixture repository mismatch"
    );
    ensure!(
        command.target_sha == target_sha && lower_hex(target_sha, 40),
        "fixture target HEAD mismatch"
    );
    Ok(())
}

impl Operation {
    fn event(&self, kind: &str, details: Value) -> Result<()> {
        self.fixture.record(kind, json!({"request_id": self.command.request_id, "sequence": 1, "stage": self.command.stage, "repo_uid": self.repo_uid, "evidence": details}))
    }
}

impl IndexScope {
    /// Evidence for the engine result; the ordinary RPC terminal event and
    /// writer-drain status remain separate external acceptance requirements.
    pub fn engine_completed(&self, store: &GraphStore, error: Option<&str>) -> Result<()> {
        let repo = store.lookup_repo(&self.0.repo_uid)?;
        self.0.event(
            "engine_completed",
            json!({
                "error": error, "reached": self.0.reached.load(Ordering::Acquire),
                "indexed_sha": repo.map(|r| r.indexed_sha),
                "graph_generation": store.graph_generation(),
                "publication_dirty": store.is_index_publication_dirty(),
            }),
        )
    }
}

impl Drop for IndexScope {
    fn drop(&mut self) {
        INDEX.with(|slot| *slot.borrow_mut() = None);
        if let Err(error) = self.0.event("operation_scope_finished", json!({})) {
            tracing::error!(%error, "fixture completion receipt failed");
        }
    }
}

/// Ordinary error after a successful bulk write. Never bypass the finalizer.
pub fn index_content_committed(
    store: &GraphStore,
    repo_uid: &str,
    files: usize,
    symbols: usize,
) -> Result<()> {
    let Some(operation) = INDEX.with(|slot| slot.borrow().clone()) else {
        return Ok(());
    };
    if operation.command.stage != CONTENT_STAGE {
        return Ok(());
    }
    operation.fixture.check_owner()?;
    ensure!(
        operation.repo_uid == repo_uid
            && store.db_path() == Some(operation.fixture.database.as_path()),
        "fixture operation identity changed"
    );
    ensure!(
        Instant::now() < operation.deadline,
        "fixture incomplete: command expired before committed-content stage"
    );
    ensure!(
        !operation.reached.swap(true, Ordering::SeqCst),
        "fixture stage reached twice"
    );
    let repo = store
        .lookup_repo(repo_uid)?
        .context("committed fixture repo missing")?;
    ensure!(
        files > 0 && symbols > 0 && repo.indexed_sha.is_empty(),
        "fixture incomplete: expected fresh committed content before SHA"
    );
    operation.event("reached", json!({
        "committed_files": files, "committed_symbols": symbols,
        "indexed_sha": repo.indexed_sha, "target_sha": operation.command.target_sha,
        "graph_generation": store.graph_generation(), "publication_dirty": store.is_index_publication_dirty(),
    }))?;
    operation.event(
        "error_injected",
        json!({"error_code": "fixture_content_commit_error"}),
    )?;
    anyhow::bail!("fixture_content_commit_error: ordinary error after committed content")
}

pub fn index_generation_save(store: &GraphStore) -> Result<()> {
    let Some(operation) = INDEX.with(|slot| slot.borrow().clone()) else {
        return Ok(());
    };
    if operation.command.stage != GENERATION_STAGE {
        return Ok(());
    }
    operation.fixture.check_owner()?;
    ensure!(
        store.db_path() == Some(operation.fixture.database.as_path()),
        "fixture operation identity changed"
    );
    ensure!(
        Instant::now() < operation.deadline,
        "fixture incomplete: command expired before generation-save stage"
    );
    ensure!(
        !operation.reached.swap(true, Ordering::SeqCst),
        "fixture stage reached twice"
    );
    let repo = store
        .lookup_repo(&operation.repo_uid)?
        .context("generation-save fixture repo missing")?;
    ensure!(
        !repo.indexed_sha.is_empty() && repo.indexed_sha == operation.command.target_sha,
        "fixture incomplete: expected persisted SHA before generation save"
    );
    operation.event(
        "reached",
        json!({
            "indexed_sha": repo.indexed_sha,
            "target_sha": operation.command.target_sha,
            "graph_generation": store.graph_generation(),
            "publication_dirty": store.is_index_publication_dirty(),
        }),
    )?;
    operation.event(
        "error_injected",
        json!({"error_code": "fixture_generation_save_error"}),
    )?;
    anyhow::bail!("fixture_generation_save_error: ordinary error after SHA persistence")
}

pub fn manifest_before_save() -> Result<()> {
    let Some(fixture) = FIXTURE.get() else {
        return Ok(());
    };
    fixture.check_owner()?;
    let command_path = fixture.control.join("command.json");
    if !command_path.try_exists()? {
        return Ok(());
    }
    ensure!(
        !fixture.command_consumed.swap(true, Ordering::SeqCst),
        "fixture command already consumed"
    );
    let command: Command = read_record(&command_path)?;
    ensure!(
        command.stage == MANIFEST_STAGE && command.action == "return_error",
        "unsupported fixture stage or action"
    );
    std::fs::rename(command_path, fixture.control.join("consumed-1.json"))?;
    fixture.record(
        "reached",
        json!({
            "request_id": command.request_id,
            "sequence": 1,
            "stage": MANIFEST_STAGE,
        }),
    )?;
    fixture.record(
        "error_injected",
        json!({"error_code": "fixture_manifest_save_error"}),
    )?;
    anyhow::bail!("fixture_manifest_save_error: ordinary error before manifest save")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_records_reject_unknown_fields_and_invalid_hex() {
        assert!(!lower_hex("AB", 2));
        assert!(!lower_hex("ab", 64));
        assert!(lower_hex("0123abcdef", 10));
        assert!(
            serde_json::from_value::<Activation>(json!({
                "protocol": 1, "nonce": "a".repeat(64), "owner_pid": 2,
                "database": "/tmp/fixture.lbug", "extra": true,
            }))
            .is_err()
        );
    }

    #[test]
    fn command_is_bound_to_one_owner_repo_head_action_and_short_lifetime() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let repo = repo.canonicalize().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let fixture = Fixture {
            control: root_path.join("fixture-control"),
            database: root_path.join("fixture.lbug"),
            root: root_path.clone(),
            activation: Activation {
                protocol: 1,
                nonce: "a".repeat(64),
                owner_pid: 2,
                database: root_path.join("fixture.lbug"),
            },
            brain_uuid: OnceLock::from("brain-identity".to_owned()),
            command_consumed: AtomicBool::new(false),
            receipts: Mutex::new(File::create(root_path.join("receipts")).unwrap()),
        };
        let head = "b".repeat(40);
        let baseline = json!({
            "protocol": 1, "nonce": "a".repeat(64), "brain_uuid": "brain-identity",
            "daemon_pid": std::process::id(), "sequence": 1, "request_id": "c".repeat(32),
            "stage": CONTENT_STAGE, "action": "return_error", "repo_root": repo,
            "target_sha": head, "expires_unix_ms": now_ms().unwrap() + 15_000,
        });
        let good: Command = serde_json::from_value(baseline.clone()).unwrap();
        validate_command(&good, &fixture, &repo, &head).unwrap();
        for (field, value) in [
            ("protocol", json!(2)),
            ("sequence", json!(2)),
            ("nonce", json!("d".repeat(64))),
            ("brain_uuid", json!("other-brain")),
            ("daemon_pid", json!(0)),
            ("request_id", json!("not-a-request")),
            ("stage", json!("not-a-stage")),
            ("action", json!("pause")),
            ("repo_root", json!(root_path)),
            ("target_sha", json!("e".repeat(40))),
            ("expires_unix_ms", json!(0)),
            ("expires_unix_ms", json!(now_ms().unwrap() + 60_000)),
        ] {
            let mut value_with_error = baseline.clone();
            value_with_error[field] = value;
            let command: Command = serde_json::from_value(value_with_error).unwrap();
            assert!(
                validate_command(&command, &fixture, &repo, &head).is_err(),
                "{field}"
            );
        }
    }
}
