use anyhow::{Context, Result};
use candle_core::{Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config as BertConfig};
use hf_hub::{HFClientSync, split_id};
use tokenizers::Tokenizer;
use tracing::info;

use crate::{DeviceKind, DevicePolicy};

pub struct LocalModel {
    model: BertModel,
    tokenizer: Tokenizer,
    device: Device,
    device_kind: DeviceKind,
    dimension: usize,
}

impl LocalModel {
    pub fn load(config: &crate::EmbedConfig) -> Result<Self> {
        let (device, device_kind) = select_device(config.device_policy)?;
        let client = HFClientSync::new()?;
        let (owner, name) = split_id(&config.model_id);
        let repo = client.model(owner, name);

        let config_path = repo
            .download_file()
            .filename("config.json")
            .send()
            .context("Failed to download config.json")?;
        let tokenizer_path = repo
            .download_file()
            .filename("tokenizer.json")
            .send()
            .context("Failed to download tokenizer.json")?;
        let weights_path = repo
            .download_file()
            .filename("model.safetensors")
            .send()
            .context("Failed to download model weights")?;

        let config_str = std::fs::read_to_string(&config_path)?;
        let bert_config: BertConfig = serde_json::from_str(&config_str)?;
        let dimension = bert_config.hidden_size;

        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow::anyhow!("Failed to load tokenizer: {e}"))?;

        info!(?device_kind, device = ?device, model = %config.model_id, "Loading embedding model");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let vb = unsafe {
                VarBuilder::from_mmaped_safetensors(
                    std::slice::from_ref(&weights_path),
                    candle_core::DType::F32,
                    &device,
                )?
            };
            let model = BertModel::load(vb, &bert_config)?;
            let candidate = Self {
                model,
                tokenizer,
                device,
                device_kind,
                dimension,
            };
            candidate.embed(&["test"])?;
            Ok::<Self, anyhow::Error>(candidate)
        }));

        match result {
            Ok(Ok(model)) => {
                info!(dimension, ?device_kind, "Embedding model loaded");
                Ok(model)
            }
            Ok(Err(err)) => Err(err).with_context(|| {
                format!(
                    "embedding model probe failed for requested device policy {policy:?}",
                    policy = config.device_policy
                )
            }),
            Err(_) => anyhow::bail!(
                "embedding model probe panicked for requested device policy {:?}",
                config.device_policy
            ),
        }
    }

    pub fn dimension(&self) -> usize {
        self.dimension
    }

    pub fn device_kind(&self) -> DeviceKind {
        self.device_kind
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

fn select_device(policy: DevicePolicy) -> Result<(Device, DeviceKind)> {
    select_device_with(policy, || {
        #[cfg(feature = "metal")]
        {
            Device::new_metal(0).map_err(Into::into)
        }
        #[cfg(not(feature = "metal"))]
        {
            anyhow::bail!("Metal support is not compiled")
        }
    })
}

fn select_device_with<F>(policy: DevicePolicy, metal_factory: F) -> Result<(Device, DeviceKind)>
where
    F: FnOnce() -> Result<Device>,
{
    match select_device_choice_with(policy, metal_factory)? {
        DeviceChoice::Cpu => Ok((Device::Cpu, DeviceKind::Cpu)),
        DeviceChoice::Metal(device) => {
            if !device.is_metal() {
                anyhow::bail!(
                    "Metal device factory returned a non-Metal device for requested device policy {policy:?}"
                );
            }
            Ok((device, DeviceKind::Metal))
        }
    }
}

#[allow(dead_code)] // `Metal` is constructed only in Metal-feature builds.
enum DeviceChoice<T> {
    Metal(T),
    Cpu,
}

fn select_device_choice_with<F, T>(
    policy: DevicePolicy,
    metal_factory: F,
) -> Result<DeviceChoice<T>>
where
    F: FnOnce() -> Result<T>,
{
    match policy {
        DevicePolicy::Cpu => Ok(DeviceChoice::Cpu),
        DevicePolicy::Auto => {
            #[cfg(feature = "metal")]
            {
                select_metal_device_choice(policy, metal_factory)
            }
            #[cfg(not(feature = "metal"))]
            {
                let _ = metal_factory;
                Ok(DeviceChoice::Cpu)
            }
        }
        DevicePolicy::Metal => {
            #[cfg(feature = "metal")]
            {
                select_metal_device_choice(policy, metal_factory)
            }
            #[cfg(not(feature = "metal"))]
            {
                let _ = metal_factory;
                anyhow::bail!(
                    "requested device policy {policy:?} requires Metal, but Metal support is not compiled"
                )
            }
        }
    }
}

#[cfg(feature = "metal")]
fn select_metal_device_choice<F, T>(
    policy: DevicePolicy,
    metal_factory: F,
) -> Result<DeviceChoice<T>>
where
    F: FnOnce() -> Result<T>,
{
    let device = std::panic::catch_unwind(std::panic::AssertUnwindSafe(metal_factory))
        .map_err(|_| {
            anyhow::anyhow!("Metal device creation panicked for requested device policy {policy:?}")
        })?
        .with_context(|| {
            format!("Metal device creation failed for requested device policy {policy:?}")
        })?;
    Ok(DeviceChoice::Metal(device))
}
/// Download a model's files (config, tokenizer, weights) into the HuggingFace cache
/// WITHOUT building the model. Lets a caller bound a cold-cache download with a timeout so
/// a slow/unreachable HuggingFace can't hang startup. A no-op (fast) when already cached.
pub fn prefetch_model(model_id: &str) -> Result<()> {
    let client = HFClientSync::new()?;
    let (owner, name) = split_id(model_id);
    let repo = client.model(owner, name);
    repo.download_file()
        .filename("config.json")
        .send()
        .context("prefetch config.json")?;
    repo.download_file()
        .filename("tokenizer.json")
        .send()
        .context("prefetch tokenizer.json")?;
    repo.download_file()
        .filename("model.safetensors")
        .send()
        .context("prefetch model.safetensors")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DeviceKind, DevicePolicy};
    use std::cell::Cell;

    #[test]
    fn metal_policy_never_returns_cpu() {
        let selected = select_device_with(DevicePolicy::Metal, || Ok(Device::Cpu));

        match selected {
            Ok((device, kind)) => {
                assert!(
                    device.is_metal(),
                    "explicit Metal must not return a CPU device"
                );
                assert_eq!(kind, DeviceKind::Metal);
            }
            Err(err) => assert!(err.to_string().contains("Metal")),
        }
    }

    #[test]
    fn cpu_policy_never_probes_metal() {
        let probed_metal = Cell::new(false);
        let selected = select_device_with(DevicePolicy::Cpu, || -> Result<Device> {
            probed_metal.set(true);
            panic!("CPU policy must not invoke the Metal factory");
        })
        .expect("CPU must select CPU without probing Metal");

        assert!(!probed_metal.get());
        assert!(selected.0.is_cpu());
        assert_eq!(selected.1, DeviceKind::Cpu);
    }

    #[cfg(not(feature = "metal"))]
    #[test]
    fn auto_policy_uses_cpu_without_metal_feature() {
        let selected = select_device_with(DevicePolicy::Auto, || -> Result<Device> {
            panic!("Auto must not invoke the Metal factory without Metal support");
        })
        .expect("Auto must select CPU when Metal is not compiled");

        assert!(selected.0.is_cpu());
        assert_eq!(selected.1, DeviceKind::Cpu);
    }

    #[cfg(feature = "metal")]
    #[test]
    fn successful_metal_factory_selects_metal() {
        let selected = select_device_choice_with(DevicePolicy::Metal, || Ok(()))
            .expect("a successful Metal factory must select Metal");

        assert!(matches!(selected, DeviceChoice::Metal(())));
    }
}
