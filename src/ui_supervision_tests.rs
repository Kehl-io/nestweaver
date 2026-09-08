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
                "HealthCheck" => respond!(
                    HealthCheckResponse,
                    HealthCheckRequest,
                    HealthCheckResponse::default()
                ),
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
    let peer = Peer {
        port,
        requests: requests.clone(),
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
    deadline.abort();
    server.abort();
    let error = result.expect_err("a deadline exit would conceal an infinite repair loop");
    assert!(
        error.to_string().contains("three repair attempts"),
        "{error:#}"
    );
    assert_eq!(requests.load(Ordering::SeqCst), 3);
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
