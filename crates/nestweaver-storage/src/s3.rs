// S3 storage backend — stub implementation.
//
// Enable with `--features s3`. Each method returns an error until AWS_*
// environment variables and a real implementation are provided.

#[cfg(feature = "s3")]
use crate::backend::{SnapshotMeta, StorageBackend};
#[cfg(feature = "s3")]
use std::path::Path;

#[cfg(feature = "s3")]
pub struct S3Backend {
    bucket: String,
    prefix: String,
}

#[cfg(feature = "s3")]
impl S3Backend {
    pub fn new(bucket: impl Into<String>, prefix: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
            prefix: prefix.into(),
        }
    }
}

#[cfg(feature = "s3")]
impl StorageBackend for S3Backend {
    fn pull_snapshot(&self, _dest: &Path) -> Result<SnapshotMeta, anyhow::Error> {
        anyhow::bail!(
            "S3 backend not yet configured — set AWS_ACCESS_KEY_ID, \
             AWS_SECRET_ACCESS_KEY, and AWS_REGION environment variables \
             (bucket: {}, prefix: {})",
            self.bucket,
            self.prefix,
        )
    }

    fn push_snapshot(&self, _src: &Path, _meta: &SnapshotMeta) -> Result<(), anyhow::Error> {
        anyhow::bail!(
            "S3 backend not yet configured — set AWS_ACCESS_KEY_ID, \
             AWS_SECRET_ACCESS_KEY, and AWS_REGION environment variables \
             (bucket: {}, prefix: {})",
            self.bucket,
            self.prefix,
        )
    }

    fn list_snapshots(&self) -> Result<Vec<SnapshotMeta>, anyhow::Error> {
        anyhow::bail!(
            "S3 backend not yet configured — set AWS_ACCESS_KEY_ID, \
             AWS_SECRET_ACCESS_KEY, and AWS_REGION environment variables \
             (bucket: {}, prefix: {})",
            self.bucket,
            self.prefix,
        )
    }
}
