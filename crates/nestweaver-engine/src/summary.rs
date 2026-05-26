use sha2::{Digest, Sha256};
use std::path::Path;

pub fn service_content_hash(symbol_hashes: &[&str]) -> String {
    let mut hasher = Sha256::new();
    let mut sorted: Vec<&str> = symbol_hashes.to_vec();
    sorted.sort();
    for h in sorted {
        hasher.update(h.as_bytes());
    }
    hex::encode(hasher.finalize())
}

pub fn cache_summary(cache_dir: &Path, hash: &str, summary: &str) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(cache_dir)?;
    std::fs::write(cache_dir.join(format!("{}.txt", hash)), summary)?;
    Ok(())
}

pub fn load_cached_summary(cache_dir: &Path, hash: &str) -> Option<String> {
    let path = cache_dir.join(format!("{}.txt", hash));
    std::fs::read_to_string(path).ok()
}

pub async fn generate_summary(
    endpoint: &str,
    model: &str,
    symbols: &[nestweaver_schema::Symbol],
) -> Result<String, anyhow::Error> {
    let signatures: Vec<&str> = symbols.iter().map(|s| s.signature.as_str()).collect();
    let files: Vec<&str> = symbols.iter().map(|s| s.file_path.as_str()).collect();

    let prompt = format!(
        "Describe this code module in 2-3 sentences. What does it do, what are its entry points, \
         and what are its key dependencies?\n\nSignatures:\n{}\n\nFiles:\n{}",
        signatures.join("\n"),
        files.join("\n")
    );

    let client = reqwest::Client::new();
    let response = client
        .post(format!(
            "{}/v1/chat/completions",
            endpoint.trim_end_matches('/')
        ))
        .json(&serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": prompt}],
            "max_tokens": 200,
        }))
        .send()
        .await?
        .error_for_status()?
        .json::<serde_json::Value>()
        .await?;

    let content = response["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("unexpected response format from LLM endpoint"))?
        .to_string();

    Ok(content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_content_hash_is_deterministic() {
        let a = service_content_hash(&["hash1", "hash2"]);
        let b = service_content_hash(&["hash1", "hash2"]);
        assert_eq!(a, b);
    }

    #[test]
    fn service_content_hash_is_order_independent() {
        let a = service_content_hash(&["hash1", "hash2"]);
        let b = service_content_hash(&["hash2", "hash1"]);
        assert_eq!(a, b); // sorted internally
    }

    #[test]
    fn cache_and_load_summary() {
        let dir = tempfile::tempdir().unwrap();
        cache_summary(dir.path(), "abc123", "Test summary").unwrap();
        let loaded = load_cached_summary(dir.path(), "abc123");
        assert_eq!(loaded, Some("Test summary".to_string()));
    }

    #[test]
    fn load_missing_summary_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_cached_summary(dir.path(), "missing"), None);
    }

    #[tokio::test]
    async fn generate_summary_calls_endpoint() {
        let mock_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({
                    "choices": [{"message": {"content": "This service handles user authentication."}}]
                }),
            ))
            .mount(&mock_server)
            .await;

        let symbols = vec![nestweaver_schema::Symbol {
            uid: "sym:test".into(),
            name: "auth".into(),
            kind: nestweaver_schema::SymbolKind::Function,
            repo_uid: "repo:test".into(),
            file_path: "src/auth.js".into(),
            start_line: 1,
            signature: "function auth()".into(),
            summary: None,
            content_hash: "abc".into(),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: nestweaver_schema::Visibility::Inferred,
            type_info: None,
            framework_hint: None,
        }];

        let summary = generate_summary(&mock_server.uri(), "test-model", &symbols)
            .await
            .unwrap();
        assert!(summary.contains("authentication"));
    }
}
