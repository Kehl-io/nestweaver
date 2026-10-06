//! Exercise the production supervisor against a healthy but dishonest wire peer.
use super::*;
use nestweaver_proto::*;
use std::convert::Infallible;
use std::future::{Ready, ready};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::task::{Context as TaskContext, Poll};
use tonic::body::Body;
use tonic::codegen::{Service, http};

#[derive(Clone)]
struct Reply<T>(T);
impl<R, T: Clone + Send + 'static> tonic::server::UnaryService<R> for Reply<T> {
    type Response = T;
    type Future = Ready<Result<tonic::Response<T>, tonic::Status>>;
    fn call(&mut self, _: tonic::Request<R>) -> Self::Future {
        ready(Ok(tonic::Response::new(self.0.clone())))
    }
}
#[derive(Clone)]
struct Peer {
    port: u16,
    requests: Arc<AtomicUsize>,
    stops: Arc<AtomicUsize>,
    health_delay: std::time::Duration,
    watcher_id: Option<u64>,
}
impl tonic::server::NamedService for Peer {
    const NAME: &'static str = "nestweaver.daemon.v1.NestWeaverDaemon";
}
impl Service<http::Request<Body>> for Peer {
    type Response = http::Response<Body>;
    type Error = Infallible;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;
    fn poll_ready(&mut self, _: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
    fn call(&mut self, request: http::Request<Body>) -> Self::Future {
        let peer = self.clone();
        Box::pin(async move {
            macro_rules! respond {
                ($response:ty, $request:ty, $value:expr) => {{
                    let codec = tonic_prost::ProstCodec::<$response, $request>::default();
                    tonic::server::Grpc::new(codec)
                        .unary(Reply($value), request)
                        .await
                }};
            }
            Ok(match request.uri().path().rsplit('/').next().unwrap() {
                "HealthCheck" => {
                    tokio::time::sleep(peer.health_delay).await;
                    respond!(
                        HealthCheckResponse,
                        HealthCheckRequest,
                        HealthCheckResponse {
                            watcher: peer.watcher_id.map(|id| WatcherStatus {
                                id,
                                ..Default::default()
                            }),
                            ..Default::default()
                        }
                    )
                }
                "ServeUi" => {
                    peer.requests.fetch_add(1, Ordering::SeqCst);
                    respond!(
                        ServeUiResponse,
                        ServeUiRequest,
                        ServeUiResponse {
                            ok: true,
                            port: u32::from(peer.port),
                            ..Default::default()
                        }
                    )
                }
                "StopUi" => {
                    peer.stops.fetch_add(1, Ordering::SeqCst);
                    respond!(
                        StopUiResponse,
                        StopUiRequest,
                        StopUiResponse {
                            ok: true,
                            ..Default::default()
                        }
                    )
                }
                _ => tonic::Status::unimplemented("unexpected RPC").into_http(),
            })
        })
    }
}
struct SocketCleanup(PathBuf);
impl Drop for SocketCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        if let Some(parent) = self.0.parent() {
            let _ = std::fs::remove_dir(parent);
        }
    }
}
#[test]
fn healthy_peer_cannot_trap_supervisor_in_identical_repairs() {
    supervisor_error_cleans_up(false);
}

#[test]
fn mismatched_repair_endpoint_cleans_up() {
    supervisor_error_cleans_up(true);
}

fn supervisor_error_cleans_up(mismatch: bool) {
    // Read-only consumers also coordinate with tests that change XDG: the
    // supervisor resolves its socket again on every health probe.
    let _environment = crate::XDG_RUNTIME_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("fixture.lbug");
    std::fs::write(&db, "fixture identity only").unwrap();
    let instance = nestweaver_daemon::instance_id_from_db_path(&db);
    let socket = nestweaver_daemon::socket_path(&instance);
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let _cleanup = SocketCleanup(socket.clone());
    let listener = {
        let _entered = rt.enter();
        tokio::net::UnixListener::bind(&socket).unwrap()
    };
    // A bound but non-listening socket reserves a port while refusing connects.
    let reservation = {
        let _entered = rt.enter();
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket
            .bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)))
            .unwrap();
        socket
    };
    let port = reservation.local_addr().unwrap().port();
    let requests = Arc::new(AtomicUsize::new(0));
    let stops = Arc::new(AtomicUsize::new(0));
    let peer = Peer {
        port: if mismatch {
            if port == 65535 { 1 } else { port + 1 }
        } else {
            port
        },
        requests: requests.clone(),
        stops: stops.clone(),
        watcher_id: None,
        health_delay: std::time::Duration::ZERO,
    };
    let server = rt.spawn(async move {
        tonic::transport::Server::builder()
            .add_service(peer)
            .serve_with_incoming(
                tonic::codegen::tokio_stream::wrappers::UnixListenerStream::new(listener),
            )
            .await
            .unwrap();
    });
    let mut client = rt
        .block_on(nestweaver_client::DaemonClient::connect_existing(&db))
        .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let deadline = rt.spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(25)).await;
        let _ = tx.send(());
    });
    let result = supervise_ui_daemon(&rt, &db, port, false, "", &rx, &mut client);
    let result = finish_ui_supervision(result, || {
        assert!(rt.block_on(client.stop_ui()).unwrap().ok);
    });
    assert_eq!(stops.load(Ordering::SeqCst), 1);
    deadline.abort();
    server.abort();
    let error = result.expect_err("a deadline exit would conceal an infinite repair loop");
    assert!(
        error.to_string().contains(if mismatch {
            "this session supervises"
        } else {
            "three repair attempts"
        }),
        "{error:#}"
    );
    assert_eq!(
        requests.load(Ordering::SeqCst),
        if mismatch { 1 } else { 3 }
    );
}

#[test]
fn ui_port_parser_accepts_sentinel_and_boundaries_but_not_overflow() {
    // The complete CLI enum needs the same larger stack as existing parser tests.
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            for port in ["0", "1", "65535"] {
                assert!(Cli::try_parse_from(["nestweaver", "ui", "--port", port]).is_ok());
            }
            for port in ["-1", "65536", "4294967295"] {
                assert!(Cli::try_parse_from(["nestweaver", "ui", "--port", port]).is_err());
            }
        })
        .unwrap()
        .join()
        .unwrap()
}

#[test]
fn repeated_ui_health_timeouts_preserve_live_owned_listener() {
    use std::os::unix::io::AsRawFd;
    let _environment = crate::XDG_RUNTIME_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("fixture.lbug");
    std::fs::write(&db, "fixture identity only").unwrap();
    let instance = nestweaver_daemon::instance_id_from_db_path(&db);
    let socket = nestweaver_daemon::socket_path(&instance);
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let _cleanup = SocketCleanup(socket.clone());
    let pidfile = nestweaver_daemon::pidfile_path(&instance);
    let owner = std::fs::File::create(&pidfile).unwrap();
    assert_eq!(unsafe { libc::flock(owner.as_raw_fd(), libc::LOCK_EX) }, 0);
    let listener = {
        let _entered = rt.enter();
        tokio::net::UnixListener::bind(&socket).unwrap()
    };
    let ui = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = ui.local_addr().unwrap().port();
    let peer = Peer {
        port,
        requests: Arc::new(AtomicUsize::new(0)),
        stops: Arc::new(AtomicUsize::new(0)),
        watcher_id: None,
        health_delay: std::time::Duration::from_secs(60),
    };
    let server = rt.spawn(async move {
        tonic::transport::Server::builder()
            .add_service(peer)
            .serve_with_incoming(
                tonic::codegen::tokio_stream::wrappers::UnixListenerStream::new(listener),
            )
            .await
            .unwrap();
    });
    let mut client = rt
        .block_on(nestweaver_client::DaemonClient::connect_existing(&db))
        .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    // At least two 2s health timeouts; Ctrl-C must still complete promptly.
    let deadline = rt.spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(7)).await;
        let _ = tx.send(());
    });
    let began = std::time::Instant::now();
    let result = supervise_ui_daemon(&rt, &db, port, false, "", &rx, &mut client);
    server.abort();
    deadline.abort();
    let _ = std::fs::remove_file(&pidfile);
    assert!(
        result.unwrap(),
        "a timeout cannot declare a live owned daemon lost"
    );
    assert!(
        began.elapsed() < std::time::Duration::from_secs(12),
        "shutdown stalled on an unbounded reconnect"
    );
    assert!(
        ui_port_serving(port),
        "the owned UI listener must be retained"
    );
}

#[test]
fn dead_ui_daemon_without_an_owner_enters_degraded_service() {
    let _environment = crate::XDG_RUNTIME_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("dead.lbug");
    std::fs::write(&db, "fixture identity only").unwrap();
    let instance = nestweaver_daemon::instance_id_from_db_path(&db);
    let socket = nestweaver_daemon::socket_path(&instance);
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let _cleanup = SocketCleanup(socket.clone());
    let listener = {
        let _entered = rt.enter();
        tokio::net::UnixListener::bind(&socket).unwrap()
    };
    let ui = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = ui.local_addr().unwrap().port();
    drop(ui);
    let peer = Peer {
        port,
        requests: Arc::new(AtomicUsize::new(0)),
        stops: Arc::new(AtomicUsize::new(0)),
        watcher_id: None,
        health_delay: std::time::Duration::ZERO,
    };
    let server = rt.spawn(async move {
        tonic::transport::Server::builder()
            .add_service(peer)
            .serve_with_incoming(
                tonic::codegen::tokio_stream::wrappers::UnixListenerStream::new(listener),
            )
            .await
            .unwrap();
    });
    let mut client = rt
        .block_on(nestweaver_client::DaemonClient::connect_existing(&db))
        .unwrap();
    server.abort();
    let _ = rt.block_on(server);
    // Leave the stale socket pathname as a killed daemon does. It is not identity.
    let (tx, rx) = std::sync::mpsc::channel();
    let observed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen = observed.clone();
    let witness = rt.spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(4)).await;
        seen.store(ui_port_serving(port), Ordering::SeqCst);
        let _ = tx.send(());
    });
    assert!(
        !supervise_ui_daemon(&rt, &db, port, false, "", &rx, &mut client).unwrap(),
        "a verified absent daemon must remain down at shutdown"
    );
    witness.abort();
    assert!(
        observed.load(Ordering::SeqCst),
        "verified absence must serve the degraded endpoint"
    );
}

#[test]
fn ui_absence_unknown_pidfile_lock_error_preserves_listener() {
    ui_absence_pidfile_fixture(|rt, db, pidfile, listener| {
        std::fs::write(pidfile, "").unwrap();
        for errno in [libc::EINTR, libc::EIO, libc::ENOLCK, libc::EOPNOTSUPP] {
            let absent = ui_daemon_absence_verified_with_pidfile_probe(
                rt,
                db,
                &anyhow::anyhow!("health endpoint refused"),
                |_| Err(std::io::Error::from_raw_os_error(errno)),
            );
            assert!(
                !absent,
                "lock observation errno {errno} cannot authorize listener takeover"
            );
            assert!(ui_port_serving(listener.local_addr().unwrap().port()));
        }
    });
}

#[test]
fn ui_absence_actual_free_and_missing_pidfiles_permit_recovery() {
    ui_absence_pidfile_fixture(|rt, db, pidfile, _listener| {
        std::fs::write(pidfile, "").unwrap();
        assert!(ui_daemon_absence_verified(
            rt,
            db,
            &anyhow::anyhow!("health endpoint refused")
        ));
        std::fs::remove_file(pidfile).unwrap();
        assert!(ui_daemon_absence_verified(
            rt,
            db,
            &anyhow::anyhow!("health endpoint refused")
        ));
    });
}

#[test]
fn ui_absence_actual_held_pidfile_retains_listener() {
    use std::os::fd::AsRawFd;
    ui_absence_pidfile_fixture(|rt, db, pidfile, _listener| {
        let holder = std::fs::File::create(pidfile).unwrap();
        assert_eq!(
            unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        assert!(!ui_daemon_absence_verified(
            rt,
            db,
            &anyhow::anyhow!("health endpoint refused")
        ));
    });
}

fn ui_absence_pidfile_fixture(
    check: impl FnOnce(&tokio::runtime::Runtime, &Path, &Path, &std::net::TcpListener),
) {
    let _environment = crate::XDG_RUNTIME_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("ownership.lbug");
    std::fs::write(&db, "fixture identity only").unwrap();
    let instance = nestweaver_daemon::instance_id_from_db_path(&db);
    let socket = nestweaver_daemon::socket_path(&instance);
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let _cleanup = SocketCleanup(socket.clone());
    let stale_listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    drop(stale_listener);
    let pidfile = nestweaver_daemon::pidfile_path(&instance);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    check(&rt, &db, &pidfile, &listener);
    let _ = std::fs::remove_file(pidfile);
}

#[test]
fn slow_watcher_health_does_not_terminate_controller() {
    let _environment = crate::XDG_RUNTIME_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("slow-watcher.lbug");
    std::fs::write(&db, "fixture identity only").unwrap();
    let instance = nestweaver_daemon::instance_id_from_db_path(&db);
    let socket = nestweaver_daemon::socket_path(&instance);
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let _cleanup = SocketCleanup(socket.clone());
    let listener = {
        let _entered = rt.enter();
        tokio::net::UnixListener::bind(&socket).unwrap()
    };
    let requests = Arc::new(AtomicUsize::new(0));
    let stops = Arc::new(AtomicUsize::new(0));
    let peer = Peer {
        port: 1,
        requests: requests.clone(),
        stops: stops.clone(),
        watcher_id: Some(17),
        health_delay: std::time::Duration::from_secs(10),
    };
    let server = rt.spawn(async move {
        tonic::transport::Server::builder()
            .add_service(peer)
            .serve_with_incoming(
                tonic::codegen::tokio_stream::wrappers::UnixListenerStream::new(listener),
            )
            .await
            .unwrap();
    });
    let mut client = rt
        .block_on(nestweaver_client::DaemonClient::connect_existing(&db))
        .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let deadline = rt.spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(7)).await;
        let _ = tx.send(());
    });
    let started = std::time::Instant::now();
    let result = wait_for_daemon_watcher(&rt, &mut client, 17, &db, &rx);
    server.abort();
    deadline.abort();
    assert!(
        result.is_ok(),
        "live accepting peer is inconclusive on timeout: {result:?}"
    );
    assert!(started.elapsed() >= std::time::Duration::from_secs(7));
    assert_eq!(
        requests.load(Ordering::SeqCst),
        0,
        "no watcher replacement/autostart"
    );
    assert_eq!(
        stops.load(Ordering::SeqCst),
        0,
        "no unconditional successor stop"
    );
}

#[test]
fn watcher_controller_terminates_on_actual_death_and_explicit_displacement() {
    let _environment = crate::XDG_RUNTIME_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    for died in [false, true] {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut peer_runtime = Some(tokio::runtime::Runtime::new().unwrap());
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("owned-watcher.lbug");
        std::fs::write(&db, "fixture identity only").unwrap();
        let socket =
            nestweaver_daemon::socket_path(&nestweaver_daemon::instance_id_from_db_path(&db));
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let _cleanup = SocketCleanup(socket.clone());
        let listener = {
            let _entered = peer_runtime.as_ref().unwrap().enter();
            tokio::net::UnixListener::bind(&socket).unwrap()
        };
        let requests = Arc::new(AtomicUsize::new(0));
        let stops = Arc::new(AtomicUsize::new(0));
        let peer = Peer {
            port: 1,
            requests: requests.clone(),
            stops: stops.clone(),
            health_delay: std::time::Duration::ZERO,
            watcher_id: Some(18),
        };
        let server = peer_runtime.as_ref().unwrap().spawn(async move {
            tonic::transport::Server::builder()
                .add_service(peer)
                .serve_with_incoming(
                    tonic::codegen::tokio_stream::wrappers::UnixListenerStream::new(listener),
                )
                .await
                .unwrap();
        });
        let mut client = rt
            .block_on(nestweaver_client::DaemonClient::connect_existing(&db))
            .unwrap();
        if died {
            // Own and stop every listener and established connection task.
            peer_runtime
                .take()
                .unwrap()
                .shutdown_timeout(std::time::Duration::from_secs(1));
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let deadline = rt.spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(8)).await;
            let _ = tx.send(());
        });
        let result = wait_for_daemon_watcher(&rt, &mut client, 17, &db, &rx);
        server.abort();
        deadline.abort();
        let error = result.expect_err("death/displacement must terminate owned controller");
        assert!(
            error.to_string().contains(if died {
                "no longer running"
            } else {
                "displaced or stopped"
            }),
            "{error:#}"
        );
        assert_eq!(requests.load(Ordering::SeqCst), 0);
        assert_eq!(
            stops.load(Ordering::SeqCst),
            0,
            "successor must never receive stop"
        );
    }
}
