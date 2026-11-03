//! Live runtime diagnostics from BrainStatusJson, independent of graph counts.
use anyhow::{Context, Result};
use nestweaver_proto::{EmbeddingStatus, SearchStatus, WatcherStatus};
use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// Only fields the daemon overlays from live runtime state or debt files.
/// Deliberately excludes graph counts, configuration and cached graph annotations.
#[derive(Debug, Deserialize)]
pub struct RuntimeStatus {
    pub write_holder: String,
    pub write_holder_seconds: i64,
    pub write_queue_depth: i32,
    #[serde(default)]
    pub supervision: String,
    #[serde(default, deserialize_with = "optional_embedding")]
    pub embedding_status: Option<EmbeddingStatus>,
    #[serde(default, deserialize_with = "optional_search")]
    pub search_status: Option<SearchStatus>,
    #[serde(default, deserialize_with = "optional_watcher")]
    pub watcher: Option<WatcherStatus>,
    #[serde(skip)]
    pub code_links: Option<nestweaver_proto::CodeLinksStatus>,
    #[serde(skip)]
    pub cross_repo_links: Option<nestweaver_proto::CrossRepoLinksStatus>,
}

impl RuntimeStatus {
    pub fn from_status_json(value: &Value) -> Result<Self> {
        anyhow::ensure!(
            value.get("runtime_telemetry").and_then(Value::as_str) == Some("available"),
            "daemon did not report live runtime telemetry"
        );
        let mut status = Self::deserialize(value).context("decode daemon runtime status")?;
        for key in ["code_links", "cross_repo_links"] {
            if let Some(debt) = value.get(key).filter(|value| !value.is_null()) {
                anyhow::ensure!(
                    debt.is_object() && debt.get("pending").and_then(Value::as_bool).is_some(),
                    "invalid {key} runtime debt disclosure"
                );
            }
        }
        status.code_links = value
            .get("code_links")
            .filter(|value| value.is_object())
            .and_then(|_| nestweaver_proto::code_links_from_status_json(value));
        status.cross_repo_links = value
            .get("cross_repo_links")
            .filter(|value| value.is_object())
            .and_then(|_| nestweaver_proto::cross_repo_links_from_status_json(value));
        Ok(status)
    }
}

// Proto3 missing scalars remain unknown/default for older daemons; present
// malformed values are refused. These are runtime submessages, never counts.
fn scalar<T: serde::de::DeserializeOwned + Default>(value: &Value, key: &str) -> Result<T> {
    match value.get(key) {
        Some(field) => serde_json::from_value(field.clone())
            .with_context(|| format!("invalid runtime field {key}")),
        None => Ok(T::default()),
    }
}

fn embedding_from_json(value: &Value) -> Result<EmbeddingStatus> {
    anyhow::ensure!(
        value.is_object(),
        "expected an object for embedding runtime status"
    );
    Ok(EmbeddingStatus {
        state: scalar(value, "state")?,
        backend: scalar(value, "backend")?,
        requested_device: scalar(value, "requested_device")?,
        selected_device: scalar(value, "selected_device")?,
        model_id: scalar(value, "model_id")?,
        error: scalar(value, "error")?,
        metal_compiled: scalar(value, "metal_compiled")?,
        fallback_used: scalar(value, "fallback_used")?,
        pass_active: scalar(value, "pass_active")?,
        pass_processed: scalar(value, "pass_processed")?,
        pass_total: scalar(value, "pass_total")?,
        pass_started_at: scalar(value, "pass_started_at")?,
        pass_scope: scalar(value, "pass_scope")?,
        index_live: scalar(value, "index_live")?,
        index_tombstoned: scalar(value, "index_tombstoned")?,
        index_stored: scalar(value, "index_stored")?,
        identity_state: scalar(value, "identity_state")?,
        identity_error: scalar(value, "identity_error")?,
        identity_remediation: scalar(value, "identity_remediation")?,
        seed_active: scalar(value, "seed_active")?,
        seed_bytes_done: scalar(value, "seed_bytes_done")?,
        seed_bytes_total: scalar(value, "seed_bytes_total")?,
        seed_origin: scalar(value, "seed_origin")?,
        seed_attempt: scalar(value, "seed_attempt")?,
        seed_max_attempts: scalar(value, "seed_max_attempts")?,
        seed_next_retry_at: scalar(value, "seed_next_retry_at")?,
    })
}

fn optional_embedding<'de, D: Deserializer<'de>>(
    input: D,
) -> std::result::Result<Option<EmbeddingStatus>, D::Error> {
    Option::<Value>::deserialize(input)?
        .as_ref()
        .map(embedding_from_json)
        .transpose()
        .map_err(serde::de::Error::custom)
}

fn search_from_json(value: &Value) -> Result<SearchStatus> {
    anyhow::ensure!(
        value.is_object(),
        "expected an object for search runtime status"
    );
    Ok(SearchStatus {
        reader_available: scalar(value, "reader_available")?,
        writer_available: scalar(value, "writer_available")?,
        current: scalar(value, "current")?,
        reconciliation_debt: scalar(value, "reconciliation_debt")?,
        error: scalar(value, "error")?,
        debt_operation: scalar(value, "debt_operation")?,
        debt_created_at: scalar(value, "debt_created_at")?,
    })
}

fn optional_search<'de, D: Deserializer<'de>>(
    input: D,
) -> std::result::Result<Option<SearchStatus>, D::Error> {
    Option::<Value>::deserialize(input)?
        .as_ref()
        .map(search_from_json)
        .transpose()
        .map_err(serde::de::Error::custom)
}

fn watcher_from_json(value: &Value) -> Result<WatcherStatus> {
    anyhow::ensure!(
        value.is_object(),
        "expected an object for watcher runtime status"
    );
    Ok(WatcherStatus {
        id: scalar(value, "id")?,
        kind: scalar(value, "kind")?,
        target: scalar(value, "target")?,
        controller_pid: scalar(value, "controller_pid")?,
        started_unix_seconds: scalar(value, "started_unix_seconds")?,
        age_seconds: scalar(value, "age_seconds")?,
    })
}

fn optional_watcher<'de, D: Deserializer<'de>>(
    input: D,
) -> std::result::Result<Option<WatcherStatus>, D::Error> {
    Option::<Value>::deserialize(input)?
        .as_ref()
        .map(watcher_from_json)
        .transpose()
        .map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[derive(Clone)]
    struct RuntimeEndpoint {
        paths: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl tonic::server::NamedService for RuntimeEndpoint {
        const NAME: &'static str = "nestweaver.daemon.v1.NestWeaverDaemon";
    }

    impl tonic::codegen::Service<tonic::codegen::http::Request<tonic::body::Body>> for RuntimeEndpoint {
        type Response = tonic::codegen::http::Response<tonic::body::Body>;
        type Error = std::convert::Infallible;
        type Future = std::future::Ready<std::result::Result<Self::Response, Self::Error>>;
        fn poll_ready(
            &mut self,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::result::Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn call(
            &mut self,
            request: tonic::codegen::http::Request<tonic::body::Body>,
        ) -> Self::Future {
            let path = request.uri().path().to_string();
            self.paths.lock().unwrap().push(path.clone());
            if path.ends_with("/GetEffectiveConfig") {
                return std::future::ready(Ok(
                    tonic::Status::unimplemented("older daemon").into_http()
                ));
            }
            if !path.ends_with("/BrainStatusJson") {
                return std::future::ready(Ok(tonic::Status::unavailable(
                    "current graph counts busy",
                )
                .into_http()));
            }
            let text = json!({"runtime_telemetry":"available", "repo_count":null,
                "status_observation":{"stale":true,"busy":true},
                "write_holder":"embed", "write_holder_seconds":95,"write_queue_depth":3,
                "embedding_status":{"state":"embedding","model_id":"actual-selected-model",
                    "pass_active":true,"pass_processed":14,"pass_total":20}})
            .to_string();
            let mut payload = vec![10];
            let mut size = text.len();
            while size >= 128 {
                payload.push((size as u8 & 127) | 128);
                size >>= 7;
            }
            payload.push(size as u8);
            payload.extend_from_slice(text.as_bytes());
            let mut frame = vec![0];
            frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            frame.extend(payload);
            std::future::ready(Ok(tonic::codegen::http::Response::builder()
                .header("content-type", "application/grpc")
                .header("grpc-status", "0")
                .body(tonic::body::Body::new(reqwest::Body::from(frame)))
                .unwrap()))
        }
    }

    #[tokio::test]
    async fn live_runtime_rpc_survives_typed_current_count_refusal() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let paths = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let endpoint = RuntimeEndpoint {
            paths: paths.clone(),
        };
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(endpoint)
                .serve_with_incoming_shutdown(
                    tokio_stream::wrappers::TcpListenerStream::new(listener),
                    async {
                        let _ = stopped.await;
                    },
                )
                .await
                .unwrap();
        });
        let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut client = crate::DaemonClient {
            inner: nestweaver_proto::nest_weaver_daemon_client::NestWeaverDaemonClient::new(
                channel,
            ),
        };
        let typed = client.brain_status().await;
        let runtime = client.brain_runtime_status().await;
        let legacy_config = client.effective_config().await;
        drop(client);
        let _ = stop.send(());
        server.await.unwrap();
        assert_eq!(
            typed
                .unwrap_err()
                .downcast_ref::<tonic::Status>()
                .unwrap()
                .code(),
            tonic::Code::Unavailable
        );
        assert_eq!(
            legacy_config
                .unwrap_err()
                .downcast_ref::<tonic::Status>()
                .unwrap()
                .code(),
            tonic::Code::Unimplemented
        );
        let runtime = runtime.unwrap();
        assert_eq!(runtime.write_holder, "embed");
        assert_eq!(runtime.write_queue_depth, 3);
        let embedding = runtime.embedding_status.unwrap();
        assert_eq!(embedding.model_id, "actual-selected-model");
        assert_eq!((embedding.pass_processed, embedding.pass_total), (14, 20));
        let paths = paths.lock().unwrap();
        assert_eq!(paths.len(), 3);
        assert!(paths[0].ends_with("/BrainStatus"));
        assert!(paths[1].ends_with("/BrainStatusJson"));
        assert!(paths[2].ends_with("/GetEffectiveConfig"));
    }

    #[test]
    fn busy_json_preserves_live_fields_without_requiring_current_graph_counts() {
        let value = json!({"runtime_telemetry":"available", "notes":null, "repo_count":null,
            "status_observation":{"stale":true,"busy":true},
            "write_holder":"embed", "write_holder_seconds":42, "write_queue_depth":3,
            "supervision":"systemd-user", "embedding_status":{"state":"embedding",
                "model_id":"selected-model", "pass_active":true,"pass_processed":14,
                "pass_total":20,"pass_scope":"all", "seed_bytes_done":123},
            "watcher":{"id":7,"kind":"code","target":"fixture","controller_pid":123},
            "search_status":{"current":false,"reconciliation_debt":true},
            "code_links":{"pending":true,"reason":"actual debt"}});
        let status = RuntimeStatus::from_status_json(&value).unwrap();
        assert_eq!(status.write_holder, "embed");
        assert_eq!(status.write_queue_depth, 3);
        let embedding = status.embedding_status.unwrap();
        assert_eq!(embedding.model_id, "selected-model");
        assert_eq!(embedding.state, "embedding");
        assert_eq!((embedding.pass_processed, embedding.pass_total), (14, 20));
        assert_eq!(embedding.seed_bytes_done, 123);
        assert_eq!(status.watcher.unwrap().controller_pid, Some(123));
        assert!(status.search_status.unwrap().reconciliation_debt);
        assert!(status.code_links.unwrap().pending);
    }

    #[test]
    fn nullable_runtime_submessages_refuse_arrays_and_scalars() {
        for field in ["embedding_status", "search_status", "watcher"] {
            for malformed in [json!([]), json!(true), json!(1), json!("invalid")] {
                let mut value = json!({"runtime_telemetry":"available", "write_holder":"",
                    "write_holder_seconds":0, "write_queue_depth":0});
                value[field] = malformed;
                assert!(RuntimeStatus::from_status_json(&value).is_err(), "{value}");
            }
        }
    }

    #[test]
    fn direct_or_malformed_runtime_is_not_live_evidence() {
        for value in [
            json!({}),
            json!({"runtime_telemetry":"unavailable"}),
            json!({"runtime_telemetry":"available","write_holder":"embed","write_holder_seconds":1,
                "write_queue_depth":"unknown"}),
            json!({"runtime_telemetry":"available","write_holder":"embed","write_holder_seconds":1,
                "write_queue_depth":0,"embedding_status":{"state":true}}),
            json!({"runtime_telemetry":"available","write_holder":"embed","write_holder_seconds":1,
                "write_queue_depth":0,"code_links":{"pending":"unknown"}}),
        ] {
            assert!(RuntimeStatus::from_status_json(&value).is_err(), "{value}");
        }
    }
}
