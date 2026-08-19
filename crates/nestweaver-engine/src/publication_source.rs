//! Exact source manifest for a full publication rebuild.
//!
//! A publication plan captures source paths and content before staging, then
//! captures them again before sealing. Equality is the cutover gate: a repo or
//! vault that moved underneath the rebuild is retried instead of publishing a
//! graph whose declared inputs are no longer reproducible.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};

pub const PUBLICATION_SOURCE_MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicationRepoSource {
    pub uid: String,
    pub url: String,
    pub instance_id: String,
    pub name: Option<String>,
    pub root_path: String,
    pub observed_head: Option<String>,
    pub content_blake3: String,
    pub file_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicationVaultSource {
    pub uid: String,
    pub name: String,
    pub instance_id: String,
    pub root_path: String,
    pub content_blake3: String,
    pub file_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicationSourceManifest {
    pub version: u32,
    pub repos: Vec<PublicationRepoSource>,
    pub vaults: Vec<PublicationVaultSource>,
}

impl PublicationSourceManifest {
    pub fn capture(store: &nestweaver_store::GraphStore) -> anyhow::Result<Self> {
        let mut repos = store
            .list_repos(None)
            .map_err(|error| anyhow::anyhow!("list publication repositories: {error}"))?
            .into_iter()
            .map(|repo| {
                let root = repo.local_root().ok_or_else(|| {
                    anyhow::anyhow!(
                        "repository '{}' has no local working-tree path; restore its source before a full publication rebuild",
                        repo.url
                    )
                })?;
                let root = canonical_source_root(Path::new(root), "repository")?;
                let (content_blake3, file_count) = hash_repo_tree(&root)?;
                Ok(PublicationRepoSource {
                    uid: repo.uid,
                    url: repo.url,
                    instance_id: repo.instance_id,
                    name: repo.name,
                    root_path: root.to_string_lossy().to_string(),
                    observed_head: git_head(&root),
                    content_blake3,
                    file_count,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let mut vaults = store
            .list_vaults(None)
            .map_err(|error| anyhow::anyhow!("list publication vaults: {error}"))?
            .into_iter()
            .map(|vault| {
                let root = canonical_source_root(Path::new(&vault.root_path), "vault")?;
                let (content_blake3, file_count) = hash_markdown_tree(&root)?;
                Ok(PublicationVaultSource {
                    uid: vault.uid,
                    name: vault.name,
                    instance_id: vault.instance_id,
                    root_path: root.to_string_lossy().to_string(),
                    content_blake3,
                    file_count,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        repos.sort_by(|left, right| left.uid.cmp(&right.uid));
        vaults.sort_by(|left, right| left.uid.cmp(&right.uid));
        Ok(Self {
            version: PUBLICATION_SOURCE_MANIFEST_VERSION,
            repos,
            vaults,
        })
    }

    pub fn fingerprint(&self) -> anyhow::Result<String> {
        Ok(crate::hash::blake3_hex_bytes(&serde_json::to_vec(self)?))
    }

    pub fn write_bound(&self, db_path: &Path) -> anyhow::Result<PathBuf> {
        let store = nestweaver_store::GraphStore::open_read_only(db_path).map_err(|error| {
            anyhow::anyhow!("open publication graph for source manifest: {error}")
        })?;
        let identity = store
            .publication_identity()
            .map_err(|error| anyhow::anyhow!("read publication identity: {error}"))?
            .ok_or_else(|| anyhow::anyhow!("publication graph has no identity"))?;
        let envelope = nestweaver_store::artifact_envelope::ArtifactEnvelope::new(
            nestweaver_store::artifact_envelope::ArtifactExpectation {
                artifact_kind: crate::publication::SOURCE_MANIFEST_ARTIFACT_KIND,
                artifact_schema_version: crate::publication::SOURCE_MANIFEST_SCHEMA_VERSION,
                identity: &identity,
                producer_version: env!("CARGO_PKG_VERSION"),
                source_graph_generation: store.graph_generation(),
                algorithm_fingerprint: crate::publication::SOURCE_MANIFEST_ALGORITHM_FINGERPRINT,
            },
            self,
        )?;
        drop(store);
        let path = crate::sidecar_path(db_path, crate::publication::SOURCE_MANIFEST_SUFFIX);
        let bytes = serde_json::to_vec_pretty(&envelope)?;
        nestweaver_store::durable_sidecar::atomic_replace_file(&path, |file| {
            use std::io::Write as _;
            file.write_all(&bytes)?;
            file.write_all(b"\n")
        })?;
        Ok(path)
    }
}

fn canonical_source_root(path: &Path, kind: &str) -> anyhow::Result<PathBuf> {
    let path = std::fs::canonicalize(path)
        .with_context(|| format!("resolve {kind} source root {}", path.display()))?;
    if !path.is_dir() {
        anyhow::bail!("{kind} source root is not a directory: {}", path.display());
    }
    Ok(path)
}

fn git_head(root: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8(output.stdout).ok())
        .flatten()
        .map(|value| value.trim().to_string())
}

fn hash_repo_tree(root: &Path) -> anyhow::Result<(String, u64)> {
    let output = std::process::Command::new("git")
        .args(["ls-files", "-co", "--exclude-standard", "-z"])
        .current_dir(root)
        .output();
    if let Ok(output) = output
        && output.status.success()
    {
        let paths = output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| PathBuf::from(String::from_utf8_lossy(path).into_owned()))
            .collect::<Vec<_>>();
        return hash_relative_files(root, paths);
    }
    hash_walk(root, |_| true)
}

fn hash_markdown_tree(root: &Path) -> anyhow::Result<(String, u64)> {
    hash_walk(root, |path| {
        path.extension()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case("md"))
    })
}

fn hash_walk(root: &Path, include: impl Fn(&Path) -> bool) -> anyhow::Result<(String, u64)> {
    let mut paths = walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| entry.file_name() != ".git")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter_map(|entry| {
            let relative = entry.path().strip_prefix(root).ok()?.to_path_buf();
            include(&relative).then_some(relative)
        })
        .collect::<Vec<_>>();
    paths.sort();
    hash_relative_files(root, paths)
}

fn hash_relative_files(root: &Path, mut paths: Vec<PathBuf>) -> anyhow::Result<(String, u64)> {
    paths.sort();
    paths.dedup();
    let mut hasher = blake3::Hasher::new();
    let mut count = 0_u64;
    let mut buffer = vec![0_u8; 1024 * 1024];
    for relative in paths {
        let path = root.join(&relative);
        if !path.is_file() {
            continue;
        }
        let relative_bytes = relative.to_string_lossy();
        hasher.update(&(relative_bytes.len() as u64).to_le_bytes());
        hasher.update(relative_bytes.as_bytes());
        let mut file = std::fs::File::open(&path)
            .with_context(|| format!("read publication source {}", path.display()))?;
        let size = file.metadata()?.len();
        hasher.update(&size.to_le_bytes());
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        count += 1;
    }
    Ok((hasher.finalize().to_hex().to_string(), count))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_source_fingerprint_is_stable_and_content_sensitive() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "alpha").unwrap();
        std::fs::write(dir.path().join("ignored.txt"), "one").unwrap();
        let first = hash_markdown_tree(dir.path()).unwrap();
        let second = hash_markdown_tree(dir.path()).unwrap();
        assert_eq!(first, second);
        std::fs::write(dir.path().join("ignored.txt"), "two").unwrap();
        assert_eq!(hash_markdown_tree(dir.path()).unwrap(), first);
        std::fs::write(dir.path().join("a.md"), "beta").unwrap();
        assert_ne!(hash_markdown_tree(dir.path()).unwrap(), first);
    }
}
