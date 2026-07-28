use anyhow::{Context, Result};
use candle_core::{Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config as BertConfig};
use hf_hub::{HFClient, HFClientBuilder, HFClientSync, HFError, split_id};
use std::path::PathBuf;
use tokenizers::Tokenizer;
use tracing::info;

use crate::{DeviceKind, DevicePolicy};

/// The three files required to construct the bundled BERT embedding model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelArtifacts {
    pub config: PathBuf,
    pub tokenizer: PathBuf,
    pub weights: PathBuf,
}

/// Whether artifact resolution may contact Hugging Face.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactMode {
    /// Resolve strictly from the configured cache without network access.
    CacheOnly,
    /// Download any artifacts absent from the configured cache.
    DownloadMissing,
}

/// Typed cache-only failure for a required local model artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingModelArtifactError {
    pub model_id: String,
    pub filename: String,
    pub cache_dir: PathBuf,
}

fn posix_shell_quote(value: &str) -> String {
    const SAFE_PUNCTUATION: &[u8] = b"_@%+=:,./-";
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || SAFE_PUNCTUATION.contains(&byte))
    {
        return value.to_string();
    }

    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

impl std::fmt::Display for MissingModelArtifactError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let model_id = posix_shell_quote(&self.model_id);
        let cache_dir = posix_shell_quote(&self.cache_dir.to_string_lossy());
        write!(
            formatter,
            "required embedding artifact '{}' for model '{}' is missing from configured cache '{}'; \
             run `nestweaver embed --local --model-id {} --cache-dir {}` to download missing model files into that cache",
            self.filename,
            self.model_id,
            self.cache_dir.display(),
            model_id,
            cache_dir
        )
    }
}

impl std::error::Error for MissingModelArtifactError {}

/// Resolve all local model files through the cache root in [`crate::EmbedConfig`].
pub fn resolve_model_artifacts(
    config: &crate::EmbedConfig,
    mode: ArtifactMode,
) -> Result<ModelArtifacts> {
    resolve_model_artifacts_with_builder(config, mode, HFClient::builder())
}

fn resolve_model_artifacts_with_builder(
    config: &crate::EmbedConfig,
    mode: ArtifactMode,
    builder: HFClientBuilder,
) -> Result<ModelArtifacts> {
    let async_client = builder
        .cache_dir(config.cache_dir.clone())
        .build()
        .context("failed to configure Hugging Face client")?;
    let client = HFClientSync::from_inner(async_client)
        .context("failed to create blocking Hugging Face client")?;
    let (owner, name) = split_id(&config.model_id);
    let repo = client.model(owner, name);

    let resolve = |filename: &'static str| -> Result<PathBuf> {
        repo.download_file()
            .filename(filename)
            .local_files_only(mode == ArtifactMode::CacheOnly)
            .send()
            .map_err(|source| {
                if mode == ArtifactMode::CacheOnly
                    && matches!(
                        &source,
                        HFError::LocalEntryNotFound { .. } | HFError::EntryNotFound { .. }
                    )
                {
                    anyhow::Error::new(MissingModelArtifactError {
                        model_id: config.model_id.clone(),
                        filename: filename.to_string(),
                        cache_dir: config.cache_dir.clone(),
                    })
                } else {
                    anyhow::Error::new(source).context(format!(
                        "failed to resolve embedding artifact '{filename}' for model '{}'",
                        config.model_id
                    ))
                }
            })
    };

    Ok(ModelArtifacts {
        config: resolve("config.json")?,
        tokenizer: resolve("tokenizer.json")?,
        weights: resolve("model.safetensors")?,
    })
}

fn select_resolve_and_construct<T, Selected, Select, Resolve, Construct>(
    config: &crate::EmbedConfig,
    policy: DevicePolicy,
    mode: ArtifactMode,
    select: Select,
    resolve: Resolve,
    construct: Construct,
) -> Result<T>
where
    Select: FnOnce(DevicePolicy) -> Result<Selected>,
    Resolve: FnOnce(&crate::EmbedConfig, ArtifactMode) -> Result<ModelArtifacts>,
    Construct: FnOnce(Selected, ModelArtifacts) -> Result<T>,
{
    let selected = select(policy)?;
    let artifacts = resolve(config, mode)?;
    construct(selected, artifacts)
}

pub struct LocalModel {
    model: BertModel,
    tokenizer: Tokenizer,
    device: Device,
    device_kind: DeviceKind,
    dimension: usize,
}

impl LocalModel {
    /// Load using the default automatic device-selection policy.
    pub fn load(config: &crate::EmbedConfig) -> Result<Self> {
        Self::load_with_policy_and_artifact_mode(
            config,
            DevicePolicy::Auto,
            ArtifactMode::DownloadMissing,
        )
    }

    /// Load with an explicit device-selection policy.
    pub fn load_with_policy(config: &crate::EmbedConfig, policy: DevicePolicy) -> Result<Self> {
        Self::load_with_policy_and_artifact_mode(config, policy, ArtifactMode::DownloadMissing)
    }

    /// Load with explicit device and artifact-resolution policies.
    pub fn load_with_policy_and_artifact_mode(
        config: &crate::EmbedConfig,
        policy: DevicePolicy,
        mode: ArtifactMode,
    ) -> Result<Self> {
        select_resolve_and_construct(
            config,
            policy,
            mode,
            select_device,
            resolve_model_artifacts,
            |(device, device_kind), artifacts| {
                Self::from_artifacts(config, policy, device, device_kind, artifacts)
            },
        )
    }

    fn from_artifacts(
        config: &crate::EmbedConfig,
        policy: DevicePolicy,
        device: Device,
        device_kind: DeviceKind,
        artifacts: ModelArtifacts,
    ) -> Result<Self> {
        let config_str = std::fs::read_to_string(&artifacts.config)?;
        let bert_config: BertConfig = serde_json::from_str(&config_str)?;
        let dimension = bert_config.hidden_size;

        let tokenizer = Tokenizer::from_file(&artifacts.tokenizer)
            .map_err(|e| anyhow::anyhow!("Failed to load tokenizer: {e}"))?;

        info!(?device_kind, device = ?device, model = %config.model_id, "Loading embedding model");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let vb = unsafe {
                VarBuilder::from_mmaped_safetensors(
                    std::slice::from_ref(&artifacts.weights),
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
                format!("embedding model probe failed for requested device policy {policy:?}")
            }),
            Err(_) => anyhow::bail!(
                "embedding model probe panicked for requested device policy {:?}",
                policy
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DeviceKind, DevicePolicy};
    use std::cell::Cell;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    const TEST_MODEL_ID: &str = "test-owner/test-model";
    const TEST_COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
    const CACHE_ISOLATION_CHILD: &str = "NESTWEAVER_EMBED_CACHE_ISOLATION_CHILD";
    const POPULATED_CACHE_PATH: &str = "NESTWEAVER_EMBED_POPULATED_CACHE_PATH";
    const EMPTY_CACHE_PATH: &str = "NESTWEAVER_EMBED_EMPTY_CACHE_PATH";
    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let sequence = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "nestweaver-embed-{label}-{}-{sequence}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create test directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn test_config(cache_dir: &Path) -> crate::EmbedConfig {
        crate::EmbedConfig {
            model_id: TEST_MODEL_ID.to_string(),
            cache_dir: cache_dir.to_path_buf(),
            external_endpoint: None,
            external_model: None,
        }
    }

    fn write_cached_artifacts(cache_dir: &Path, omitted: Option<&str>) -> ModelArtifacts {
        // hf-hub documents refs/<revision> and snapshots/<commit>/<filename>
        // as its public cache representation. Regular files are sufficient for
        // cache-only resolution; production code never constructs these paths.
        let repo_dir = cache_dir.join("models--test-owner--test-model");
        let snapshot_dir = repo_dir.join("snapshots").join(TEST_COMMIT);
        std::fs::create_dir_all(repo_dir.join("refs")).expect("create refs");
        std::fs::create_dir_all(&snapshot_dir).expect("create snapshot");
        std::fs::write(repo_dir.join("refs").join("main"), TEST_COMMIT).expect("write ref");

        let artifacts = ModelArtifacts {
            config: snapshot_dir.join("config.json"),
            tokenizer: snapshot_dir.join("tokenizer.json"),
            weights: snapshot_dir.join("model.safetensors"),
        };
        for path in [&artifacts.config, &artifacts.tokenizer, &artifacts.weights] {
            if path.file_name().and_then(|name| name.to_str()) != omitted {
                std::fs::write(path, b"fixture").expect("write cached artifact");
            }
        }
        artifacts
    }

    #[test]
    fn cache_only_resolves_complete_configured_cache() {
        let cache = TestDir::new("complete-cache");
        let expected = write_cached_artifacts(cache.path(), None);

        let actual = resolve_model_artifacts_with_builder(
            &test_config(cache.path()),
            ArtifactMode::CacheOnly,
            HFClient::builder().endpoint("http://127.0.0.1:9"),
        )
        .expect("a complete configured cache must resolve with an unreachable endpoint");

        assert_eq!(actual, expected);
    }

    #[test]
    fn cache_only_missing_artifact_is_typed_and_actionable() {
        let cache = TestDir::new("missing-cache");
        write_cached_artifacts(cache.path(), Some("model.safetensors"));

        let err = resolve_model_artifacts_with_builder(
            &test_config(cache.path()),
            ArtifactMode::CacheOnly,
            HFClient::builder().endpoint("http://127.0.0.1:9"),
        )
        .expect_err("a missing required artifact must fail without contacting the endpoint");
        let missing = err
            .downcast_ref::<MissingModelArtifactError>()
            .expect("cache-only misses must preserve their error type");

        assert_eq!(missing.model_id, TEST_MODEL_ID);
        assert_eq!(missing.filename, "model.safetensors");
        assert_eq!(missing.cache_dir, cache.path());
        assert!(err.to_string().contains("nestweaver embed --local"));
    }

    #[cfg(unix)]
    #[test]
    fn missing_artifact_remediation_round_trips_posix_shell_arguments() {
        for (model_id, cache_dir) in [
            ("owner/safe-model-1", "/tmp/safe-cache-1"),
            ("owner/model with spaces", "/tmp/cache with spaces"),
            ("owner/model's", "/tmp/Kory's cache"),
        ] {
            let remediation = MissingModelArtifactError {
                model_id: model_id.to_string(),
                filename: "model.safetensors".to_string(),
                cache_dir: PathBuf::from(cache_dir),
            }
            .to_string();
            let command = remediation
                .split('`')
                .nth(1)
                .expect("remediation must contain a shell command");
            let script = format!("set -- {command}; printf '%s\\n%s\\n%s\\n' \"$#\" \"$5\" \"$7\"");
            let output = Command::new("sh")
                .args(["-c", &script])
                .output()
                .expect("run remediation through a POSIX shell");

            assert!(
                output.status.success(),
                "shell rejected remediation for {model_id:?} and {cache_dir:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                String::from_utf8_lossy(&output.stdout),
                format!("7\n{model_id}\n{cache_dir}\n")
            );
        }
    }

    #[test]
    fn configured_cache_roots_are_isolated() {
        if std::env::var_os(CACHE_ISOLATION_CHILD).is_some() {
            let populated = PathBuf::from(
                std::env::var_os(POPULATED_CACHE_PATH).expect("populated cache child path"),
            );
            let empty =
                PathBuf::from(std::env::var_os(EMPTY_CACHE_PATH).expect("empty cache child path"));
            let expected = write_cached_artifacts(&populated, None);

            let resolved =
                resolve_model_artifacts(&test_config(&populated), ArtifactMode::CacheOnly)
                    .expect("configured populated cache must resolve");
            assert_eq!(resolved, expected);

            let err = resolve_model_artifacts(&test_config(&empty), ArtifactMode::CacheOnly)
                .expect_err("configured cache must not read the populated process-global root");
            assert!(
                err.downcast_ref::<MissingModelArtifactError>().is_some(),
                "isolated empty cache must report its own miss"
            );
            return;
        }

        let populated = TestDir::new("populated-cache");
        let empty = TestDir::new("empty-cache");
        write_cached_artifacts(populated.path(), None);

        let output = Command::new(std::env::current_exe().expect("current test executable"))
            .args([
                "--exact",
                "local::tests::configured_cache_roots_are_isolated",
                "--nocapture",
            ])
            .env(CACHE_ISOLATION_CHILD, "1")
            .env("HF_HUB_CACHE", populated.path())
            .env(POPULATED_CACHE_PATH, populated.path())
            .env(EMPTY_CACHE_PATH, empty.path())
            .output()
            .expect("run isolated cache test child");

        assert!(
            output.status.success(),
            "isolated cache child failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn one_load_resolves_once_and_constructs_from_resolved_artifacts() {
        let cache = TestDir::new("one-pass-cache");
        let config = test_config(cache.path());
        let expected = ModelArtifacts {
            config: PathBuf::from("resolved-config"),
            tokenizer: PathBuf::from("resolved-tokenizer"),
            weights: PathBuf::from("resolved-weights"),
        };
        let selection_count = Cell::new(0);
        let resolution_count = Cell::new(0);
        let construction_count = Cell::new(0);

        let loaded = select_resolve_and_construct(
            &config,
            DevicePolicy::Cpu,
            ArtifactMode::CacheOnly,
            |seen_policy| {
                selection_count.set(selection_count.get() + 1);
                assert_eq!(seen_policy, DevicePolicy::Cpu);
                Ok("selected-device")
            },
            |seen_config, seen_mode| {
                resolution_count.set(resolution_count.get() + 1);
                assert_eq!(seen_config.cache_dir, config.cache_dir);
                assert_eq!(seen_mode, ArtifactMode::CacheOnly);
                Ok(expected.clone())
            },
            |device, artifacts| {
                construction_count.set(construction_count.get() + 1);
                assert_eq!(device, "selected-device");
                assert_eq!(artifacts, expected);
                Ok("loaded")
            },
        )
        .expect("one-pass load orchestration must succeed");

        assert_eq!(loaded, "loaded");
        assert_eq!(selection_count.get(), 1);
        assert_eq!(resolution_count.get(), 1);
        assert_eq!(construction_count.get(), 1);
    }

    #[test]
    fn failed_device_selection_never_invokes_artifact_resolver() {
        let cache = TestDir::new("failed-device-selection-cache");
        let config = test_config(cache.path());
        let resolution_count = Cell::new(0);

        let result: Result<()> = select_resolve_and_construct(
            &config,
            DevicePolicy::Metal,
            ArtifactMode::DownloadMissing,
            |_| anyhow::bail!("unsupported device"),
            |_, _| {
                resolution_count.set(resolution_count.get() + 1);
                anyhow::bail!("artifact resolver must not run")
            },
            |_: (), _| Ok(()),
        );

        assert!(
            result
                .expect_err("device selection must fail")
                .to_string()
                .contains("unsupported")
        );
        assert_eq!(resolution_count.get(), 0);
    }

    #[test]
    fn one_argument_load_remains_publicly_callable() {
        let load: fn(&crate::EmbedConfig) -> Result<LocalModel> = LocalModel::load;
        let _ = load;
    }

    #[cfg(not(feature = "metal"))]
    #[test]
    fn unsupported_metal_fails_before_artifact_resolution() {
        let cache = TestDir::new("unsupported-metal-cache");
        let err = match LocalModel::load_with_policy_and_artifact_mode(
            &test_config(cache.path()),
            DevicePolicy::Metal,
            ArtifactMode::CacheOnly,
        ) {
            Err(err) => err,
            Ok(_) => panic!("unsupported Metal must fail before inspecting an empty cache"),
        };

        assert!(err.to_string().contains("Metal"));
        assert!(
            err.downcast_ref::<MissingModelArtifactError>().is_none(),
            "unsupported Metal must fail before artifact resolution"
        );
    }

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

    #[cfg(feature = "metal")]
    #[test]
    fn metal_and_auto_propagate_metal_factory_failures() {
        for policy in [DevicePolicy::Metal, DevicePolicy::Auto] {
            let err = match select_device_choice_with(policy, || -> Result<()> {
                anyhow::bail!("factory failed")
            }) {
                Err(err) => err,
                Ok(_) => panic!("Metal failure must not fall back to CPU"),
            };
            assert!(err.to_string().contains("Metal device creation failed"));
        }
    }

    #[cfg(feature = "metal")]
    #[test]
    fn metal_and_auto_propagate_metal_factory_panics() {
        for policy in [DevicePolicy::Metal, DevicePolicy::Auto] {
            let err = match select_device_choice_with(policy, || -> Result<()> {
                panic!("factory panic");
            }) {
                Err(err) => err,
                Ok(_) => panic!("Metal panic must not fall back to CPU"),
            };
            assert!(err.to_string().contains("Metal device creation panicked"));
        }
    }
}
