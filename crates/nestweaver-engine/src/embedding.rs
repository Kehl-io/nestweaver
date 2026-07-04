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

/// Batch-embed multiple texts in a single API call. The endpoint must be
/// OpenAI-compatible (`POST /v1/embeddings` with `input` as an array).
/// Returns one embedding per input text, in the same order.
pub async fn generate_embeddings_batch(
    endpoint: &str,
    model: &str,
    texts: &[&str],
) -> Result<Vec<Vec<f32>>, anyhow::Error> {
    if texts.is_empty() {
        return Ok(vec![]);
    }
    let url = format!("{}/v1/embeddings", endpoint.trim_end_matches('/'));
    let client = reqwest::Client::new();

    #[derive(serde::Serialize)]
    struct Req<'a> {
        model: &'a str,
        input: Vec<&'a str>,
    }
    #[derive(serde::Deserialize)]
    struct Resp {
        data: Vec<RespData>,
    }
    #[derive(serde::Deserialize)]
    struct RespData {
        embedding: Vec<f32>,
        // OpenAI does NOT guarantee `data` is returned in input order; realign by
        // this field. Providers that omit it (e.g. Ollama, which is in-order)
        // default to 0, so the stable sort is a no-op and preserves order.
        #[serde(default)]
        index: usize,
    }

    let resp: Resp = client
        .post(&url)
        .json(&Req {
            model,
            input: texts.to_vec(),
        })
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    // A short/long response would silently misalign every embedding with the
    // wrong input (or drop some), corrupting the index. Fail loudly instead.
    if resp.data.len() != texts.len() {
        anyhow::bail!(
            "embedding API returned {} vectors for {} inputs",
            resp.data.len(),
            texts.len()
        );
    }
    let mut data = resp.data;
    data.sort_by_key(|d| d.index);
    Ok(data.into_iter().map(|d| d.embedding).collect())
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
