// nestweaver-storage: persistence layer for graph snapshots

use std::path::Path;

pub mod backend;
pub mod local;
pub mod workspace;

#[cfg(feature = "s3")]
pub mod s3;

#[cfg(feature = "gitlab")]
pub mod gitlab;

pub use backend::*;
pub use workspace::WorkspaceStorage;

/// Recursively copy a directory tree from `src` to `dst`.
///
/// Symlinks are skipped with a tracing warning — following them risks
/// escaping the source tree, and recreating them is platform-specific.
pub fn copy_dir_all(src: &Path, dst: &Path) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(dst)
        .map_err(|e| anyhow::anyhow!("create_dir_all {}: {e}", dst.display()))?;
    for entry in
        std::fs::read_dir(src).map_err(|e| anyhow::anyhow!("read_dir {}: {e}", src.display()))?
    {
        let entry = entry.map_err(|e| anyhow::anyhow!("read_dir entry: {e}"))?;
        let ty = entry
            .file_type()
            .map_err(|e| anyhow::anyhow!("file_type: {e}"))?;
        let target = dst.join(entry.file_name());
        if ty.is_symlink() {
            tracing::warn!(
                path = %entry.path().display(),
                "copy_dir_all: skipping symlink"
            );
        } else if ty.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)
                .map_err(|e| anyhow::anyhow!("copy {}: {e}", entry.path().display()))?;
        }
    }
    Ok(())
}
