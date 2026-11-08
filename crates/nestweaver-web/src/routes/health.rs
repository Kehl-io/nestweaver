use axum::Json;
use serde_json::{Value, json};

pub async fn health() -> Json<Value> {
    Json(json!({ "status": "ok", "product_version": env!("CARGO_PKG_VERSION") }))
}
