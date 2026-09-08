//! Faulting wire peers exercise the production clients without corrupting a real store.
use nestweaver_proto::*;
use serde_json::json;
use std::convert::Infallible;
use std::future::{Ready, ready};
use std::sync::{
    Arc,
    atomic::{AtomicU8, AtomicUsize, Ordering},
};
use std::task::{Context, Poll};
use tonic::body::Body;
use tonic::codegen::{Service, http};

#[derive(Clone)]
struct Reply<T>(Result<T, tonic::Status>);
impl<R, T: Clone + Send + 'static> tonic::server::UnaryService<R> for Reply<T> {
    type Response = T;
    type Future = Ready<Result<tonic::Response<T>, tonic::Status>>;
    fn call(&mut self, _: tonic::Request<R>) -> Self::Future {
        ready(self.0.clone().map(tonic::Response::new))
    }
}
#[derive(Clone)]
struct Peer {
    mode: Arc<AtomicU8>,
    mutations: Arc<AtomicUsize>,
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
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
    fn call(&mut self, request: http::Request<Body>) -> Self::Future {
        let mode = self.mode.load(Ordering::SeqCst);
        let mutations = self.mutations.clone();
        Box::pin(async move {
            macro_rules! respond {
                ($response:ty, $request:ty, $result:expr) => {{
                    let codec = tonic_prost::ProstCodec::<$response, $request>::default();
                    let mut grpc = tonic::server::Grpc::new(codec);
                    grpc.unary(Reply($result), request).await
                }};
            }
            let response = match request.uri().path().rsplit('/').next().unwrap() {
                "HealthCheck" => respond!(
                    HealthCheckResponse,
                    HealthCheckRequest,
                    Ok(HealthCheckResponse {
                        version: "fixture".into(),
                        ..Default::default()
                    })
                ),
                "RepoStates" => respond!(
                    RepoStatesResponse,
                    RepoStatesRequest,
                    if mode == 5 {
                        Err(tonic::Status::unavailable("injected inventory failure"))
                    } else {
                        Ok(RepoStatesResponse::default())
                    }
                ),
                "ListReposJson" => respond!(
                    JsonResponse,
                    JsonRequest,
                    Ok(JsonResponse {
                        result_json: if mode == 0 { "{" } else { "[]" }.into()
                    })
                ),
                "ListVaultsJson" => respond!(
                    JsonResponse,
                    JsonRequest,
                    if mode == 1 {
                        Err(tonic::Status::unavailable("injected vault failure"))
                    } else {
                        Ok(JsonResponse {result_json: match mode {
                    2 => "{".into(),
                    4 => json!([{"uid":"vlt:fixture","name":"fixture","root_path":"fixture","instance_id":"fixture"}]).to_string(),
                    _ => "[]".into()
                }})
                    }
                ),
                "RemoveVault" => {
                    mutations.fetch_add(1, Ordering::SeqCst);
                    respond!(
                        RemoveVaultResponse,
                        RemoveVaultRequest,
                        Ok(RemoveVaultResponse {
                            committed: true,
                            ..Default::default()
                        })
                    )
                }
                _ => tonic::Status::unimplemented("unexpected RPC").into_http(),
            };
            Ok(response)
        })
    }
}
fn peer(rt: &tokio::runtime::Runtime) -> (Peer, String, tokio::task::JoinHandle<()>) {
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = port.local_addr().unwrap();
    drop(port);
    let peer = Peer {
        mode: Arc::new(AtomicU8::new(0)),
        mutations: Arc::new(AtomicUsize::new(0)),
    };
    let service = peer.clone();
    let task = rt.spawn(async move {
        tonic::transport::Server::builder()
            .add_service(service)
            .serve(addr)
            .await
            .unwrap();
    });
    let url = format!("http://{addr}");
    rt.block_on(async {
        for _ in 0..100 {
            if nest_weaver_daemon_client::NestWeaverDaemonClient::connect(url.clone())
                .await
                .is_ok()
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("fixture peer did not start");
    });
    (peer, url, task)
}
#[test]
fn source_inventory_failures_never_dispatch_removal() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (peer, url, task) = peer(&rt);
    let mut client = rt
        .block_on(nest_weaver_daemon_client::NestWeaverDaemonClient::connect(
            url,
        ))
        .unwrap();
    for (mode, expected) in [
        (0, "repository inventory decode"),
        (1, "vault inventory RPC"),
        (2, "vault inventory decode"),
        (3, "no repo or vault"),
    ] {
        peer.mode.store(mode, Ordering::SeqCst);
        let error = nestweaver_mcp::tools::dispatch_via_daemon(
            &mut client,
            &rt,
            "brain_remove_source",
            json!({"target":"fixture"}),
        )
        .unwrap_err();
        assert!(error.to_string().contains(expected), "{mode}: {error:#}");
        assert_eq!(peer.mutations.load(Ordering::SeqCst), 0);
    }
    peer.mode.store(4, Ordering::SeqCst);
    let result = nestweaver_mcp::tools::dispatch_via_daemon(
        &mut client,
        &rt,
        "brain_remove_source",
        json!({"target":"fixture"}),
    )
    .unwrap();
    assert_eq!(result["committed"], true);
    assert_eq!(peer.mutations.load(Ordering::SeqCst), 1);
    task.abort();
}
#[cfg(target_os = "linux")] // XDG confines saved configuration to the fixture.
#[test]
fn reachable_upstream_inventory_failure_is_not_saved_as_empty() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (peer, url, task) = peer(&rt);
    let dir = tempfile::tempdir().unwrap();
    for mode in [5, 6] {
        peer.mode.store(mode, Ordering::SeqCst);
        let output = assert_cmd::Command::cargo_bin("nestweaver")
            .unwrap()
            .current_dir(dir.path())
            .env("XDG_CONFIG_HOME", dir.path().join("config"))
            .env("XDG_DATA_HOME", dir.path().join("data"))
            .args([
                "connect",
                &url,
                "--token",
                "fixture-token-with-no-real-authority",
            ])
            .timeout(std::time::Duration::from_secs(15))
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        if mode == 5 {
            assert!(!output.status.success());
            assert!(stderr.contains("inventory is unavailable"), "{stderr}");
            assert!(!stderr.contains("0 repos indexed"));
            assert!(!dir.path().join("config/nestweaver/upstreams.toml").exists());
        } else {
            assert!(output.status.success(), "{stderr}");
            assert!(stderr.contains("0 repos indexed"));
        }
    }
    task.abort();
}
