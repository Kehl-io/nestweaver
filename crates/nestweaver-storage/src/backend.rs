use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotMeta {
    pub version: String,
    pub instance_id: String,
}

pub trait StorageBackend: Send + Sync {
    fn pull_snapshot(&self, dest: &Path) -> Result<SnapshotMeta, anyhow::Error>;
    fn push_snapshot(&self, src: &Path, meta: &SnapshotMeta) -> Result<(), anyhow::Error>;
    fn list_snapshots(&self) -> Result<Vec<SnapshotMeta>, anyhow::Error>;
}

pub fn create_backend(
    name: &str,
    path: Option<&str>,
) -> Result<Box<dyn StorageBackend>, anyhow::Error> {
    match name {
        "local" => {
            let p = path.ok_or_else(|| anyhow::anyhow!("local backend requires 'path'"))?;
            Ok(Box::new(crate::local::LocalBackend::new(
                std::path::Path::new(p),
            )))
        }
        #[cfg(feature = "s3")]
        "s3" => {
            let bucket =
                path.ok_or_else(|| anyhow::anyhow!("s3 backend requires 'path' (bucket/prefix)"))?;
            let (bucket, prefix) = bucket.split_once('/').unwrap_or((bucket, ""));
            Ok(Box::new(crate::s3::S3Backend::new(bucket, prefix)))
        }
        #[cfg(not(feature = "s3"))]
        "s3" => anyhow::bail!("s3 backend not compiled in — rebuild with --features s3"),
        #[cfg(feature = "gitlab")]
        "gitlab" => {
            let project_id =
                path.ok_or_else(|| anyhow::anyhow!("gitlab backend requires 'path' (project_id)"))?;
            let base_url =
                std::env::var("GITLAB_BASE_URL").unwrap_or_else(|_| "https://gitlab.com".into());
            Ok(Box::new(crate::gitlab::GitLabBackend::new(
                project_id, base_url,
            )))
        }
        #[cfg(not(feature = "gitlab"))]
        "gitlab" => {
            anyhow::bail!("gitlab backend not compiled in — rebuild with --features gitlab")
        }
        _ => anyhow::bail!("unknown storage backend: {name}"),
    }
}
