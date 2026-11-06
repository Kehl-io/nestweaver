use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nestweaver_store::GraphStore;

use super::{CodeWatcher, code_debt, code_startup_batches};

const CALLER: &str =
    "import { selected } from '@choice/target';\nexport function caller() { return selected(); }\n";

fn aliases(target: &str) -> String {
    format!(r#"{{"compilerOptions":{{"paths":{{"@choice/*":["{target}/*"]}}}}}}"#)
}

struct ResolverStartupFixture {
    root: PathBuf,
    db: PathBuf,
    url: String,
    repo_uid: String,
    store: Arc<GraphStore>,
    _temp: tempfile::TempDir,
}

impl ResolverStartupFixture {
    fn new(configure: impl FnOnce(&Path)) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        for directory in ["src", "a", "b"] {
            fs::create_dir_all(root.join(directory)).unwrap();
        }
        let root = fs::canonicalize(root).unwrap();
        fs::write(root.join("src/main.ts"), CALLER).unwrap();
        for target in ["a", "b"] {
            fs::write(
                root.join(format!("{target}/target.ts")),
                "export function selected() {}\n",
            )
            .unwrap();
        }
        configure(&root);
        let db = temp.path().join("graph.lbug");
        let url = format!("file://{}", root.display());
        let repo_uid = nestweaver_schema::repo_uid("test", &url);
        crate::index::index_directory(&root, &db, "test", &url, "sha").unwrap();
        let store = Arc::new(GraphStore::open_or_create(&db).unwrap());
        Self {
            root,
            db,
            url,
            repo_uid,
            store,
            _temp: temp,
        }
    }

    fn watcher(&self) -> CodeWatcher {
        CodeWatcher::new(&self.db, &self.root, "test")
    }

    fn proof(&self) -> String {
        self.store
            .get_repo_resolver_inputs(&self.repo_uid)
            .unwrap()
            .expect("successful publication must persist the resolver input proof")
    }

    fn assert_full_parity(&self, phase: &str, selected: Option<&str>) {
        let caller = nestweaver_schema::symbol_uid(&self.repo_uid, "src/main.ts", "caller", 2);
        let target = selected.map(|directory| format!("{directory}/target.ts"));
        self.assert_call_parity(phase, &caller, target.as_deref());
        assert_eq!(
            fs::read_to_string(self.root.join("src/main.ts")).unwrap(),
            CALLER
        );
    }

    fn assert_call_parity(&self, phase: &str, caller: &str, target: Option<&str>) {
        let paths = |store: &GraphStore| {
            let mut paths: Vec<_> = store
                .callees_of(caller)
                .unwrap()
                .into_iter()
                .map(|symbol| symbol.file_path)
                .collect();
            paths.sort();
            paths
        };
        let expected: Vec<_> = target.into_iter().map(str::to_owned).collect();
        assert_eq!(
            paths(&self.store),
            expected,
            "{phase}: unchanged caller at readiness"
        );
        let (_, full) =
            crate::index::index_directory_in_memory(&self.root, "test", &self.url, "sha").unwrap();
        assert_eq!(
            paths(&full),
            expected,
            "{phase}: fresh index fixture control"
        );
        let edges = |store: &GraphStore| {
            let mut rows: Vec<_> = store
                .load_typed_edges()
                .unwrap()
                .into_iter()
                .map(|(source, target, kind, confidence, evidence)| {
                    (source, target, kind, confidence.to_bits(), evidence)
                })
                .collect();
            rows.sort();
            rows
        };
        assert_eq!(
            edges(&self.store),
            edges(&full),
            "{phase}: exact typed edge, property and multiplicity parity"
        );
    }

    fn restart_with_drift(&self, phase: &str, selected: Option<&str>) {
        let generation = self.store.graph_generation();
        assert!(
            code_startup_batches(&self.db, &self.root, &self.store) > 0,
            "{phase}: resolver-only drift must replay before readiness"
        );
        assert!(self.store.graph_generation() > generation);
        assert!(
            code_debt(&self.db).is_empty(),
            "{phase}: successful startup settles debt"
        );
        self.assert_full_parity(phase, selected);
    }

    fn assert_warm_no_publication(&self) {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let prepared = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&prepared);
        let leases = Arc::new(AtomicUsize::new(0));
        let admitted = Arc::clone(&leases);
        let mut watcher = self
            .watcher()
            .with_mutation_lease_factory(Arc::new(move |_| {
                admitted.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(()) as Box<dyn super::WatchMutationLease>)
            }));
        watcher.batch_plan_observer = Some(Arc::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
        }));
        let generation = self.store.graph_generation();
        let revision = self.store.index_publication_revision();
        let proof = self.proof();
        let stop = watcher.shutdown_handle();
        watcher
            .with_ready_callback(move || stop.stop())
            .run_with_store(Arc::clone(&self.store), None)
            .unwrap();
        assert_eq!(
            prepared.load(Ordering::SeqCst),
            0,
            "warm startup must skip batch preparation"
        );
        assert_eq!(
            leases.load(Ordering::SeqCst),
            0,
            "warm startup must skip mutation admission"
        );
        assert_eq!(self.store.graph_generation(), generation);
        assert_eq!(self.store.index_publication_revision(), revision);
        assert_eq!(
            self.proof(),
            proof,
            "warm startup leaves the trusted proof intact"
        );
        assert!(code_debt(&self.db).is_empty());
    }
}

#[test]
fn resolver_startup_replays_config_only_edits_before_readiness() {
    let fixture = ResolverStartupFixture::new(|root| {
        fs::create_dir(root.join("config")).unwrap();
        fs::write(
            root.join("tsconfig.json"),
            r#"{"extends":"./config/custom.settings"}"#,
        )
        .unwrap();
        fs::write(root.join("config/custom.settings"), aliases("a")).unwrap();
    });
    let custom = fixture.root.join("config/custom.settings");
    let root_config = fixture.root.join("tsconfig.json");
    fixture.assert_full_parity("initial full publication", Some("a"));
    fixture.assert_warm_no_publication();

    fs::write(&custom, aliases("b")).unwrap();
    fixture.restart_with_drift("custom extends edited while stopped", Some("b"));

    let config_dir = fixture.root.join("config");
    fs::rename(&config_dir, fixture.root.join("old-config")).unwrap();
    fixture.restart_with_drift("config ancestor moved while stopped", None);

    fs::create_dir(&config_dir).unwrap();
    fs::write(&custom, aliases("a")).unwrap();
    fixture.restart_with_drift("previously absent extends target created", Some("a"));

    fs::remove_file(&root_config).unwrap();
    fixture.restart_with_drift("root config removed while stopped", None);

    fs::write(&root_config, aliases("b")).unwrap();
    fixture.restart_with_drift("previously absent root config created", Some("b"));

    fs::write(fixture.root.join("unrelated.json"), aliases("a")).unwrap();
    fs::write(&root_config, aliases("b")).unwrap();
    fixture.assert_warm_no_publication();
    fixture.assert_full_parity("unrelated JSON and identical config bytes", Some("b"));
}

#[cfg(unix)]
#[test]
fn resolver_startup_replays_contained_alias_retarget_and_physical_deletion() {
    use std::os::unix::fs::symlink;

    let fixture = ResolverStartupFixture::new(|root| {
        fs::write(
            root.join("tsconfig.json"),
            r#"{"extends":"./logical.settings"}"#,
        )
        .unwrap();
        fs::write(root.join("first.settings"), aliases("a")).unwrap();
        fs::write(root.join("second.settings"), aliases("b")).unwrap();
        symlink("bridge.settings", root.join("logical.settings")).unwrap();
        symlink("first.settings", root.join("bridge.settings")).unwrap();
    });
    fixture.assert_full_parity("initial alias", Some("a"));
    fixture.assert_warm_no_publication();

    let bridge = fixture.root.join("bridge.settings");
    fs::remove_file(&bridge).unwrap();
    symlink("second.settings", &bridge).unwrap();
    fixture.restart_with_drift("intermediate alias retargeted while stopped", Some("b"));

    let physical = fixture.root.join("second.settings");
    fs::remove_file(&physical).unwrap();
    fixture.restart_with_drift("alias target deleted while stopped", None);

    fs::write(&physical, aliases("a")).unwrap();
    fixture.restart_with_drift("dangling alias target recreated while stopped", Some("a"));
    fs::write(fixture.root.join("first.settings"), aliases("b")).unwrap();
    fixture.assert_warm_no_publication();
}

#[test]
fn resolver_startup_repairs_missing_and_corrupt_proofs_once() {
    let fixture = ResolverStartupFixture::new(|root| {
        fs::write(root.join("tsconfig.json"), aliases("a")).unwrap();
    });
    let original = fixture.proof();
    let config = fixture.root.join("tsconfig.json");
    for (phase, malformed, target) in [
        ("legacy missing proof", None, "a"),
        ("corrupt proof", Some("{invalid"), "b"),
    ] {
        let txn = fixture.store.begin_transaction().unwrap();
        match malformed {
            Some(value) => GraphStore::set_repo_resolver_inputs_on(&txn, &fixture.repo_uid, value),
            None => GraphStore::clear_repo_resolver_inputs_on(&txn, &fixture.repo_uid),
        }
        .unwrap();
        fixture.store.commit_transaction(&txn).unwrap();
        drop(txn);
        assert_eq!(
            fixture
                .store
                .get_repo_resolver_inputs(&fixture.repo_uid)
                .unwrap()
                .as_deref(),
            malformed
        );

        fs::write(&config, aliases(target)).unwrap();
        fixture.restart_with_drift(phase, Some(target));
        let repaired = fixture.proof();
        assert_ne!(Some(repaired.as_str()), malformed);
        if target == "b" {
            assert_ne!(
                repaired, original,
                "repair must record the new config bytes"
            );
        }
        fixture.assert_warm_no_publication();
    }
}

#[test]
fn resolver_startup_tracks_go_module_membership_hidden_by_ignore_rules() {
    const GO_CALLER: &str =
        "package main\nimport \"example.org/alias/api\"\nfunc caller() { api.Selected() }\n";
    let fixture = ResolverStartupFixture::new(|root| {
        fs::create_dir_all(root.join("nested/api")).unwrap();
        fs::write(root.join("nested/go.mod"), "module example.org/alias\n").unwrap();
        fs::write(
            root.join("nested/api/target.go"),
            "package api\nfunc Selected() {}\n",
        )
        .unwrap();
        fs::write(root.join("src/main.go"), GO_CALLER).unwrap();
    });
    let caller = nestweaver_schema::symbol_uid(&fixture.repo_uid, "src/main.go", "caller", 3);
    fixture.assert_call_parity(
        "initial nested Go module",
        &caller,
        Some("nested/api/target.go"),
    );
    fixture.assert_warm_no_publication();

    let ignore = fixture.root.join(".ignore");
    fs::write(&ignore, "/nested/go.mod\n").unwrap();
    assert!(code_startup_batches(&fixture.db, &fixture.root, &fixture.store) > 0);
    fixture.assert_call_parity("only the nested module config was hidden", &caller, None);

    fs::remove_file(&ignore).unwrap();
    assert!(code_startup_batches(&fixture.db, &fixture.root, &fixture.store) > 0);
    fixture.assert_call_parity(
        "nested module config visible again",
        &caller,
        Some("nested/api/target.go"),
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("src/main.go")).unwrap(),
        GO_CALLER
    );
    fixture.assert_warm_no_publication();
}

#[cfg(unix)]
#[test]
fn resolver_startup_failed_replay_preserves_proof_and_retries_without_an_edit() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use super::{ShutdownHandle, WatchMutationLease, WatchMutationLeaseFactory, wait_until};

    struct RunningWatcher {
        stop: ShutdownHandle,
        thread: Option<std::thread::JoinHandle<anyhow::Result<()>>>,
    }

    impl Drop for RunningWatcher {
        fn drop(&mut self) {
            self.stop.stop();
            if let Some(thread) = self.thread.take() {
                let result = thread.join();
                if !std::thread::panicking() {
                    result.unwrap().unwrap();
                }
            }
        }
    }

    let fixture = ResolverStartupFixture::new(|root| {
        fs::write(root.join("tsconfig.json"), aliases("a")).unwrap();
    });
    let old_proof = fixture.proof();
    let old_generation = fixture.store.graph_generation();
    fs::write(fixture.root.join("tsconfig.json"), aliases("b")).unwrap();

    let admit = Arc::new(AtomicBool::new(false));
    let permit = Arc::clone(&admit);
    let failures = Arc::new(AtomicUsize::new(0));
    let rejected = Arc::clone(&failures);
    let factory: WatchMutationLeaseFactory = Arc::new(move |label| {
        if label == "watch_code_batch" && !permit.load(Ordering::Acquire) {
            rejected.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!("injected resolver startup replay failure");
        }
        Ok(Box::new(()) as Box<dyn WatchMutationLease>)
    });
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let watcher = fixture
        .watcher()
        .with_mutation_lease_factory(factory)
        .with_reconcile_retry_base(Duration::from_millis(50))
        .with_ready_callback(move || {
            let _ = ready_tx.send(());
        });
    let stop = watcher.shutdown_handle();
    let store = Arc::clone(&fixture.store);
    let running = RunningWatcher {
        stop,
        thread: Some(std::thread::spawn(move || {
            watcher.run_with_store(store, None)
        })),
    };
    ready_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("failed replay is disclosed at readiness");
    assert!(failures.load(Ordering::SeqCst) > 0);
    assert_eq!(fixture.store.graph_generation(), old_generation);
    assert_eq!(
        fixture.proof(),
        old_proof,
        "failed replay cannot certify new inputs"
    );
    assert!(
        code_debt(&fixture.db)
            .iter()
            .any(|(_, reason)| reason.contains("injected resolver startup replay failure"))
    );

    admit.store(true, Ordering::Release);
    wait_until("startup resolver retry publishes and clears debt", || {
        fixture.store.graph_generation() > old_generation && code_debt(&fixture.db).is_empty()
    });
    drop(running);
    assert_ne!(fixture.proof(), old_proof);
    fixture.assert_full_parity("retry without another file event", Some("b"));
    fixture.assert_warm_no_publication();
}

#[test]
fn resolver_startup_removes_last_workspace_source_and_its_proof() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir(&root).unwrap();
    let root = fs::canonicalize(root).unwrap();
    let config = root.join("tsconfig.json");
    let typescript = root.join("last.ts");
    fs::write(&config, aliases("a")).unwrap();
    fs::write(&typescript, "export function obsolete() {}\n").unwrap();
    fs::write(root.join("keep.rs"), "pub fn survivor() {}\n").unwrap();
    let db = temp.path().join("graph.lbug");
    let url = format!("file://{}", root.display());
    let repo_uid = nestweaver_schema::repo_uid("test", &url);
    crate::index::index_directory(&root, &db, "test", &url, "sha").unwrap();
    let store = Arc::new(GraphStore::open_or_create(&db).unwrap());
    assert!(store.get_repo_resolver_inputs(&repo_uid).unwrap().is_some());
    assert_eq!(code_startup_batches(&db, &root, &store), 0);

    fs::remove_file(&typescript).unwrap();
    fs::write(&config, aliases("b")).unwrap();
    assert!(code_startup_batches(&db, &root, &store) > 0);
    let files: Vec<_> = store
        .list_files_by_repo(&repo_uid)
        .unwrap()
        .into_iter()
        .map(|(_, path)| path)
        .collect();
    assert_eq!(files, vec!["keep.rs"]);
    let symbols: Vec<_> = store
        .lookup_symbols_by_repo(&repo_uid)
        .unwrap()
        .into_iter()
        .map(|symbol| symbol.name)
        .collect();
    assert_eq!(symbols, vec!["survivor"]);
    assert!(store.get_repo_resolver_inputs(&repo_uid).unwrap().is_none());
    assert!(code_debt(&db).is_empty());

    let generation = store.graph_generation();
    let revision = store.index_publication_revision();
    assert_eq!(code_startup_batches(&db, &root, &store), 0);
    assert_eq!(store.graph_generation(), generation);
    assert_eq!(store.index_publication_revision(), revision);
    assert!(code_debt(&db).is_empty());
}
