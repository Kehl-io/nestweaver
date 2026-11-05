//! Read-only per-source vault discovery. Never infer identity from names or roots.
use std::path::Path;

use anyhow::Result;
use serde_json::{Value, json};

use crate::dedup::{VaultMappings, entity_key_for_uid, reconcile_vault_pairs_with_mappings};
use crate::discovery::UpstreamConfig;
use crate::upstream::UpstreamHandle;

/// Query the existing owner and every configured peer independently. A failed
/// source has no rows/count, rather than masquerading as an empty inventory.
/// Raw transport/config errors and endpoint URLs never enter the result.
pub async fn discover(db: &Path, config: Option<&Path>, start: &Path) -> Result<Value> {
    let effective = crate::RestartConfig::for_automatic_cold_start(db, config)?;
    let configs = crate::discovery::discover_upstreams_with_config(start, effective.as_path());
    let local = match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        query_local(db, effective.as_path()),
    )
    .await
    {
        Ok(value) => value,
        Err(_) => failed_source("local", "unavailable"),
    };
    let peers = inventory_peers(&local, configs).await?;
    Ok(json!({"local": local, "upstreams": peers,
        "identity_policy": "explicit one-to-one full-UID mappings; names and roots never bind vaults"}))
}

async fn query_local(db: &Path, config: Option<&Path>) -> Value {
    match crate::DaemonClient::connect_existing_verified_read(db, config).await {
        Ok(Some(client)) => {
            let mut local = crate::hybrid::HybridClient::local_only(client);
            source_result(
                "local",
                local
                    .query_local_only("list_vaults", &json!({"include_counts": true}))
                    .await,
            )
        }
        Ok(None) => failed_source("local", "unavailable"),
        Err(error) => failed_source("local", failure_status(&error)),
    }
}

async fn inventory_peers(local: &Value, configs: Vec<UpstreamConfig>) -> Result<Vec<Value>> {
    let mut pending = tokio::task::JoinSet::new();
    for (index, config) in configs.into_iter().enumerate() {
        pending.spawn(async move {
            let name = config
                .name
                .clone()
                .unwrap_or_else(|| format!("upstream-{}", index + 1));
            (
                index,
                config.vault_mappings.clone(),
                query_peer(&name, &config).await,
            )
        });
    }
    let mut peers = Vec::new();
    while let Some(result) = pending.join_next().await {
        let (index, mappings, mut peer) = result?;
        annotate_mappings(local, &mut peer, &mappings);
        peer["source_index"] = json!(index);
        peers.push(peer);
    }
    peers.sort_by_key(|peer| peer["source_index"].as_u64());
    Ok(peers)
}

async fn query_peer(name: &str, config: &UpstreamConfig) -> Value {
    let Ok(handle) = UpstreamHandle::from_config(config) else {
        return failed_source(name, "configuration_error");
    };
    let mut client = handle.client();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        nestweaver_federation::dispatch::dispatch_json_rpc_authed_profile(
            &mut client,
            "list_vaults",
            &json!({"include_counts": true}),
            handle.auth_token(),
            nestweaver_schema::ToolDeliveryProfile::FullCli,
        ),
    )
    .await;
    match result {
        Ok(result) => source_result(name, result),
        Err(_) => failed_source(name, "unavailable"),
    }
}

fn failure_status(error: &anyhow::Error) -> &'static str {
    if error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<tonic::Status>())
        .any(|status| {
            matches!(
                status.code(),
                tonic::Code::PermissionDenied | tonic::Code::Unauthenticated
            )
        })
    {
        "forbidden"
    } else {
        "unavailable"
    }
}

fn failed_source(name: &str, status: &str) -> Value {
    json!({"source": name, "status": status, "vaults": [], "vault_count": null})
}

fn source_result(name: &str, result: Result<Value>) -> Value {
    match result {
        Err(error) => failed_source(name, failure_status(&error)),
        Ok(value) => {
            let Some(rows) = value
                .as_array()
                .or_else(|| value.get("results").and_then(Value::as_array))
            else {
                return failed_source(name, "invalid_response");
            };
            // Keep only public inventory fields; never propagate arbitrary
            // upstream diagnostics, metadata, or error strings.
            let rows: Vec<Value> = rows
                .iter()
                .map(|row| {
                    json!({
                        "uid": row.get("uid"), "name": row.get("name"),
                        "root_path": row.get("root_path"), "notes": row.get("notes"),
                        "last_indexed": row.get("last_indexed"),
                    })
                })
                .collect();
            json!({"source": name, "status": "available", "vault_count": rows.len(), "vaults": rows})
        }
    }
}

fn annotate_mappings(local: &Value, peer: &mut Value, mappings: &VaultMappings) {
    if peer["status"] != "available" {
        return;
    }
    let local_available = local["status"] == "available";
    let local_rows = local["vaults"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let Some(rows) = peer["vaults"].as_array_mut() else {
        return;
    };
    let local_uids: std::collections::HashSet<&str> = local_rows
        .iter()
        .filter_map(|r| r["uid"].as_str())
        .collect();
    let mut locals: Vec<_> = local_rows
        .iter()
        .filter_map(|r| {
            r["uid"]
                .as_str()
                .and_then(|u| entity_key_for_uid(u, None, None))
        })
        .collect();
    let original_keys: Vec<_> = rows
        .iter()
        .map(|r| {
            r["uid"]
                .as_str()
                .and_then(|u| entity_key_for_uid(u, None, None))
        })
        .collect();
    let mut remotes: Vec<_> = original_keys.iter().flatten().cloned().collect();
    // Use the same simultaneous mapping/collision rules as real merge routing.
    reconcile_vault_pairs_with_mappings(
        &mut locals.iter_mut().collect::<Vec<_>>(),
        &mut remotes.iter_mut().collect::<Vec<_>>(),
        mappings,
    );
    let mut remote_index = 0;
    let mut unmapped = 0;
    for (row, original) in rows.iter_mut().zip(original_keys) {
        let uid = row["uid"].as_str().unwrap_or_default().to_owned();
        let canonical = matches!(
            nestweaver_schema::uid::parse_canonical_entity_uid(&uid),
            Some(nestweaver_schema::uid::CanonicalEntityUid::Vault { .. })
        );
        let reconciled = original.as_ref().and_then(|_| {
            let key = remotes.get(remote_index);
            remote_index += 1;
            key
        });
        let accepted = canonical
            && original
                .as_ref()
                .zip(reconciled)
                .is_some_and(|(before, after)| {
                    before != after && locals.iter().any(|l| l == after)
                });
        let status = if !canonical {
            "invalid_uid"
        } else if !local_available {
            "local_unavailable"
        } else if accepted {
            "mapped"
        } else if local_uids.contains(uid.as_str()) {
            "same_uid"
        } else if mappings.values().any(|r| r == &uid) {
            unmapped += 1;
            "mapping_rejected"
        } else {
            unmapped += 1;
            "unmapped"
        };
        row["mapping_status"] = json!(status);
        if accepted {
            row["local_uid"] = json!(
                reconciled
                    .and_then(|key| key.vault.as_ref())
                    .map(|vault| format!("vlt:{vault}"))
            );
        }
        if status == "unmapped" || status == "mapping_rejected" {
            row["mapping_example"] = json!({"<LOCAL_VAULT_UID>": uid});
        }
    }
    peer["unmapped_vault_count"] = if local_available {
        json!(unmapped)
    } else {
        Value::Null
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(uids: &[&str]) -> Value {
        source_result("fixture", Ok(json!(uids.iter().map(|uid| json!({"uid":uid,"name":"same title","root_path":"/same/root","notes":0})).collect::<Vec<_>>())))
    }

    #[tokio::test]
    async fn unavailable_local_owner_never_creates_a_database_or_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("not-created.lbug");
        let result = query_local(&db, None).await;
        assert_eq!(result["status"], "unavailable");
        assert!(result["vault_count"].is_null());
        assert!(!db.exists());
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    }

    #[test]
    fn roots_never_bind_and_explicit_mapping_uses_full_identity() {
        let local = source(&["vlt:local:aaaaaaaaaaaa"]);
        let mut peer = source(&["vlt:remote:bbbbbbbbbbbb", "vlt:other:cccccccccccc"]);
        annotate_mappings(&local, &mut peer, &VaultMappings::new());
        assert_eq!(peer["unmapped_vault_count"], 2);
        let mapping = VaultMappings::from([(
            "vlt:local:aaaaaaaaaaaa".into(),
            "vlt:remote:bbbbbbbbbbbb".into(),
        )]);
        annotate_mappings(&local, &mut peer, &mapping);
        assert_eq!(peer["vaults"][0]["mapping_status"], "mapped");
        assert_eq!(peer["vaults"][0]["local_uid"], "vlt:local:aaaaaaaaaaaa");
        assert_eq!(peer["vaults"][1]["mapping_status"], "unmapped");
        assert_eq!(peer["unmapped_vault_count"], 1);
    }

    #[test]
    fn ambiguous_and_colliding_mappings_are_rejected_as_in_merge() {
        let local = source(&["vlt:local:aaaaaaaaaaaa", "vlt:local:dddddddddddd"]);
        let mut peer = source(&["vlt:remote:bbbbbbbbbbbb"]);
        let mapping = VaultMappings::from([
            (
                "vlt:local:aaaaaaaaaaaa".into(),
                "vlt:remote:bbbbbbbbbbbb".into(),
            ),
            (
                "vlt:local:dddddddddddd".into(),
                "vlt:remote:bbbbbbbbbbbb".into(),
            ),
        ]);
        annotate_mappings(&local, &mut peer, &mapping);
        assert_eq!(peer["vaults"][0]["mapping_status"], "mapping_rejected");
        let mapping = VaultMappings::from([(
            "vlt:local:aaaaaaaaaaaa".into(),
            "vlt:remote:bbbbbbbbbbbb".into(),
        )]);
        let mut peer = source(&["vlt:remote:bbbbbbbbbbbb", "vlt:local:aaaaaaaaaaaa"]);
        annotate_mappings(&local, &mut peer, &mapping);
        assert_eq!(peer["vaults"][0]["mapping_status"], "mapping_rejected");
        assert_eq!(peer["vaults"][1]["mapping_status"], "same_uid");
    }

    #[test]
    fn failures_do_not_claim_empty_counts_or_leak_remote_errors() {
        for (status, expected) in [
            (tonic::Status::permission_denied("SECRET"), "forbidden"),
            (tonic::Status::unauthenticated("SECRET"), "forbidden"),
            (tonic::Status::unavailable("SECRET"), "unavailable"),
        ] {
            let result = source_result("peer", Err(status.into()));
            assert!(result["vault_count"].is_null());
            assert!(!result.to_string().contains("SECRET"));
            assert_eq!(result["status"], expected);
        }
        let mut peer = source(&["vlt:remote:bbbbbbbbbbbb"]);
        annotate_mappings(
            &failed_source("local", "unavailable"),
            &mut peer,
            &VaultMappings::new(),
        );
        assert!(peer["unmapped_vault_count"].is_null());
        assert_eq!(peer["vaults"][0]["mapping_status"], "local_unavailable");
    }

    // A real tonic HTTP/2 peer verifies dispatch and bearer metadata. No
    // generated service implementation is needed for unrelated daemon RPCs.
    #[derive(Clone)]
    struct Peer {
        token: &'static str,
        uid: &'static str,
        forbidden: bool,
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }
    impl tonic::server::NamedService for Peer {
        const NAME: &'static str = "nestweaver.daemon.v1.NestWeaverDaemon";
    }
    impl tower::Service<tonic::codegen::http::Request<tonic::body::Body>> for Peer {
        type Response = tonic::codegen::http::Response<tonic::body::Body>;
        type Error = std::convert::Infallible;
        type Future = std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
        >;
        fn poll_ready(
            &mut self,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn call(
            &mut self,
            request: tonic::codegen::http::Request<tonic::body::Body>,
        ) -> Self::Future {
            let peer = self.clone();
            Box::pin(async move {
                assert!(
                    request.uri().path().ends_with("/ListVaultsJson"),
                    "inventory must not use another peer or fallback RPC"
                );
                #[derive(Clone)]
                struct Inventory(Peer);
                impl tonic::server::UnaryService<nestweaver_proto::JsonRequest> for Inventory {
                    type Response = nestweaver_proto::JsonResponse;
                    type Future =
                        std::future::Ready<Result<tonic::Response<Self::Response>, tonic::Status>>;
                    fn call(
                        &mut self,
                        request: tonic::Request<nestweaver_proto::JsonRequest>,
                    ) -> Self::Future {
                        self.0
                            .calls
                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let auth = request
                            .metadata()
                            .get("authorization")
                            .and_then(|v| v.to_str().ok());
                        if auth != Some(self.0.token) || self.0.forbidden {
                            return std::future::ready(Err(tonic::Status::permission_denied(
                                "SECRET remote diagnostic",
                            )));
                        }
                        assert_eq!(
                            serde_json::from_str::<Value>(&request.get_ref().args_json).unwrap()["include_counts"],
                            true
                        );
                        std::future::ready(Ok(tonic::Response::new(nestweaver_proto::JsonResponse { result_json: json!([{"uid":self.0.uid,"root_path":"/peer/root","name":"peer","notes":7}]).to_string() })))
                    }
                }
                Ok(tonic::server::Grpc::new(tonic_prost::ProstCodec::default())
                    .unary(Inventory(peer), request)
                    .await)
            })
        }
    }

    #[tokio::test]
    async fn real_peers_are_independent_authenticated_and_permission_failures_are_redacted() {
        let mut tasks = Vec::new();
        let mut configs = Vec::new();
        let mut calls = Vec::new();
        for (index, forbidden) in [false, false, true].into_iter().enumerate() {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            calls.push(count.clone());
            let peer = Peer {
                token: "Bearer secret-token",
                uid: if index == 0 {
                    "vlt:first:aaaaaaaaaaaa"
                } else {
                    "vlt:second:bbbbbbbbbbbb"
                },
                forbidden,
                calls: count,
            };
            tasks.push(tokio::spawn(async move {
                tonic::transport::Server::builder()
                    .add_service(peer)
                    .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                    .await
                    .unwrap();
            }));
            configs.push(UpstreamConfig {
                name: Some(format!("peer-{index}")),
                url,
                token: Some("secret-token".into()),
                mode: Default::default(),
                repos: vec![],
                timeout: "1s".into(),
                fallback_timeout: None,
                ca_cert: None,
                vault_mappings: Default::default(),
            });
        }
        configs[0].vault_mappings.insert(
            "vlt:local:cccccccccccc".into(),
            "vlt:first:aaaaaaaaaaaa".into(),
        );
        let inventory = inventory_peers(&source(&["vlt:local:cccccccccccc"]), configs.clone())
            .await
            .unwrap();
        assert_eq!(inventory.len(), 3);
        for (index, result) in inventory.iter().enumerate() {
            assert_eq!(result["source"], format!("peer-{index}"));
            assert!(!result.to_string().contains("secret-token"));
            assert!(!result.to_string().contains("SECRET"));
            if index == 2 {
                assert_eq!(result["status"], "forbidden");
                assert!(result["vault_count"].is_null());
            } else {
                assert_eq!(result["status"], "available");
                assert_eq!(result["vault_count"], 1);
                assert_eq!(result["vaults"][0]["notes"], 7);
                assert_eq!(
                    result["vaults"][0]["mapping_status"],
                    if index == 0 { "mapped" } else { "unmapped" }
                );
            }
            assert_eq!(calls[index].load(std::sync::atomic::Ordering::SeqCst), 1);
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut wrong_auth = configs[0].clone();
        wrong_auth.token = Some("wrong-secret".into());
        let denied = query_peer("wrong-auth", &wrong_auth).await;
        assert_eq!(denied["status"], "forbidden");
        assert!(!denied.to_string().contains("SECRET"));
        assert!(!denied.to_string().contains("wrong-secret"));
        let mut offline = configs[0].clone();
        offline.url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let result = query_peer("offline", &offline).await;
        assert_eq!(result["status"], "unavailable");
        assert!(result["vault_count"].is_null());
        for task in tasks {
            task.abort();
        }
    }
}
