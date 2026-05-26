// GitLab storage backend — stub implementation.
//
// Enable with `--features gitlab`. Each method returns an error until
// GITLAB_TOKEN and related environment variables are configured.

#[cfg(feature = "gitlab")]
use crate::backend::{SnapshotMeta, StorageBackend};
#[cfg(feature = "gitlab")]
use std::path::Path;

#[cfg(feature = "gitlab")]
pub struct GitLabBackend {
    project_id: String,
    base_url: String,
}

#[cfg(feature = "gitlab")]
impl GitLabBackend {
    pub fn new(project_id: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            project_id: project_id.into(),
            base_url: base_url.into(),
        }
    }
}

#[cfg(feature = "gitlab")]
impl StorageBackend for GitLabBackend {
    fn pull_snapshot(&self, _dest: &Path) -> Result<SnapshotMeta, anyhow::Error> {
        anyhow::bail!(
            "GitLab backend not yet configured — set GITLAB_TOKEN and \
             GITLAB_BASE_URL environment variables \
             (project: {}, url: {})",
            self.project_id,
            self.base_url,
        )
    }

    fn push_snapshot(&self, _src: &Path, _meta: &SnapshotMeta) -> Result<(), anyhow::Error> {
        anyhow::bail!(
            "GitLab backend not yet configured — set GITLAB_TOKEN and \
             GITLAB_BASE_URL environment variables \
             (project: {}, url: {})",
            self.project_id,
            self.base_url,
        )
    }

    fn list_snapshots(&self) -> Result<Vec<SnapshotMeta>, anyhow::Error> {
        anyhow::bail!(
            "GitLab backend not yet configured — set GITLAB_TOKEN and \
             GITLAB_BASE_URL environment variables \
             (project: {}, url: {})",
            self.project_id,
            self.base_url,
        )
    }
}
