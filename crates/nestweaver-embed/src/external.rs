use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
struct EmbeddingRequest<'a> {
    model: &'a str,
    input: Vec<&'a str>,
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingData>,
}

#[derive(Deserialize)]
struct EmbeddingData {
    embedding: Vec<f32>,
}

pub fn embed_via_api(endpoint: &str, model: &str, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
    let url = format!("{}/v1/embeddings", endpoint.trim_end_matches('/'));

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let request = EmbeddingRequest {
        model,
        input: texts.to_vec(),
    };

    let response: EmbeddingResponse = client
        .post(&url)
        .json(&request)
        .send()
        .context("Failed to reach embedding API")?
        .error_for_status()
        .context("Embedding API returned error")?
        .json()
        .context("Failed to parse embedding response")?;

    let mut embeddings: Vec<Vec<f32>> = response.data.into_iter().map(|d| d.embedding).collect();

    if embeddings.len() != texts.len() {
        anyhow::bail!(
            "API returned {} embeddings for {} inputs",
            embeddings.len(),
            texts.len()
        );
    }

    // L2 normalize
    for emb in &mut embeddings {
        let norm: f32 = emb.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in emb.iter_mut() {
                *x /= norm;
            }
        }
    }

    Ok(embeddings)
}
