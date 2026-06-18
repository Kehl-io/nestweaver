use anyhow::{Context, Result};
use candle_core::{Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config as BertConfig};
use hf_hub::api::sync::Api;
use tokenizers::Tokenizer;
use tracing::info;

pub struct LocalModel {
    model: BertModel,
    tokenizer: Tokenizer,
    device: Device,
    dimension: usize,
}

impl LocalModel {
    pub fn load(config: &crate::EmbedConfig) -> Result<Self> {
        let device = select_device();
        info!(device = ?device, model = %config.model_id, "Loading embedding model");

        let api = Api::new()?;
        let repo = api.model(config.model_id.clone());

        let config_path = repo
            .get("config.json")
            .context("Failed to download config.json")?;
        let tokenizer_path = repo
            .get("tokenizer.json")
            .context("Failed to download tokenizer.json")?;
        let weights_path = repo
            .get("model.safetensors")
            .context("Failed to download model weights")?;

        let config_str = std::fs::read_to_string(&config_path)?;
        let bert_config: BertConfig = serde_json::from_str(&config_str)?;
        let dimension = bert_config.hidden_size;

        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow::anyhow!("Failed to load tokenizer: {e}"))?;

        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(
                &[weights_path],
                candle_core::DType::F32,
                &device,
            )?
        };
        let model = BertModel::load(vb, &bert_config)?;

        info!(dimension, "Embedding model loaded");
        Ok(Self {
            model,
            tokenizer,
            device,
            dimension,
        })
    }

    pub fn dimension(&self) -> usize {
        self.dimension
    }

    pub fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let mut all_embeddings = Vec::with_capacity(texts.len());

        for text in texts {
            let encoding = self
                .tokenizer
                .encode(*text, true)
                .map_err(|e| anyhow::anyhow!("Tokenization failed: {e}"))?;

            let ids = encoding.get_ids().to_vec();
            let type_ids = encoding.get_type_ids().to_vec();
            let attention_mask: Vec<u32> = encoding.get_attention_mask().to_vec();
            let len = ids.len();

            let input_ids = Tensor::new(ids, &self.device)?.unsqueeze(0)?;
            let token_type_ids = Tensor::new(type_ids, &self.device)?.unsqueeze(0)?;
            let attention_mask_tensor = Tensor::new(attention_mask, &self.device)?
                .to_dtype(candle_core::DType::F32)?
                .unsqueeze(0)?;

            let output =
                self.model
                    .forward(&input_ids, &token_type_ids, Some(&attention_mask_tensor))?;

            // Mean pooling over non-padding tokens
            let mask_expanded = attention_mask_tensor
                .unsqueeze(2)?
                .broadcast_as(output.shape())?;
            let masked = (output * mask_expanded)?;
            let summed = masked.sum(1)?;
            let count = Tensor::new(vec![len as f32], &self.device)?
                .unsqueeze(0)?
                .broadcast_as(summed.shape())?;
            let mean_pooled = (summed / count)?;

            // L2 normalize
            let norm = mean_pooled.sqr()?.sum(1)?.sqrt()?;
            let norm_expanded = norm.unsqueeze(1)?.broadcast_as(mean_pooled.shape())?;
            let normalized = (mean_pooled / norm_expanded)?;

            let embedding: Vec<f32> = normalized.squeeze(0)?.to_vec1()?;
            all_embeddings.push(embedding);
        }

        Ok(all_embeddings)
    }
}

fn select_device() -> Device {
    #[cfg(feature = "metal")]
    {
        if let Ok(device) = Device::new_metal(0) {
            return device;
        }
    }
    Device::Cpu
}
