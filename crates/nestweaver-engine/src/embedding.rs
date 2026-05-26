use std::path::Path;

pub async fn generate_embedding(
    endpoint: &str,
    model: &str,
    text: &str,
) -> Result<Vec<f32>, anyhow::Error> {
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{}/v1/embeddings", endpoint.trim_end_matches('/')))
        .json(&serde_json::json!({
            "model": model,
            "input": text,
        }))
        .send()
        .await?
        .error_for_status()?
        .json::<serde_json::Value>()
        .await?;

    let embedding = response["data"][0]["embedding"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("unexpected response format from embedding endpoint"))?
        .iter()
        .map(|v| v.as_f64().unwrap_or(0.0) as f32)
        .collect();

    Ok(embedding)
}

pub fn cache_embedding(
    cache_dir: &Path,
    hash: &str,
    embedding: &[f32],
) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(cache_dir)?;
    let json = serde_json::to_string(embedding)?;
    std::fs::write(cache_dir.join(format!("{}.emb.json", hash)), json)?;
    Ok(())
}

pub fn load_cached_embedding(cache_dir: &Path, hash: &str) -> Option<Vec<f32>> {
    let path = cache_dir.join(format!("{}.emb.json", hash));
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn generate_embedding_returns_vector() {
        let mock_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/embeddings"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "data": [{"embedding": vec![0.1f32; 768]}]
                })),
            )
            .mount(&mock_server)
            .await;

        let vec = generate_embedding(&mock_server.uri(), "test-model", "test text")
            .await
            .unwrap();
        assert_eq!(vec.len(), 768);
    }

    #[test]
    fn cache_and_load_embedding() {
        let dir = tempfile::tempdir().unwrap();
        let emb = vec![0.1f32, 0.2, 0.3];
        cache_embedding(dir.path(), "abc", &emb).unwrap();
        let loaded = load_cached_embedding(dir.path(), "abc").unwrap();
        assert_eq!(loaded, emb);
    }

    #[test]
    fn load_missing_embedding_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_cached_embedding(dir.path(), "missing"), None);
    }
}
