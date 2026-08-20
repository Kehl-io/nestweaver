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

pub const PUBLICATION_SOURCE_MANIFEST_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicationSourceFile {
    pub path: String,
    pub byte_size: u64,
    /// Platform change token used only to prove that a second capture may
    /// reuse `content_blake3`. `None` means the platform cannot make that
    /// proof, so validation hashes the file again.
    pub change_token: Option<String>,
    pub content_blake3: String,
}

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
    pub files: Vec<PublicationSourceFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicationVaultSource {
    pub uid: String,
    pub name: String,
    pub instance_id: String,
    pub root_path: String,
    pub content_blake3: String,
    pub file_count: u64,
    pub files: Vec<PublicationSourceFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicationSourceManifest {
    pub version: u32,
    pub repos: Vec<PublicationRepoSource>,
    pub vaults: Vec<PublicationVaultSource>,
}

impl PublicationSourceManifest {
    pub fn capture(store: &nestweaver_store::GraphStore) -> anyhow::Result<Self> {
        Self::capture_with_prior(store, None)
    }

    /// Re-capture source identity for the cutover gate while reusing a prior
    /// content digest only when a strong platform change token proves that the
    /// exact file is unchanged. Path enumeration and metadata checks always
    /// run again; ambiguous files are content-hashed again.
    pub fn recapture_for_validation(
        &self,
        store: &nestweaver_store::GraphStore,
    ) -> anyhow::Result<Self> {
        Self::capture_with_prior(store, Some(self))
    }

    fn capture_with_prior(
        store: &nestweaver_store::GraphStore,
        prior: Option<&Self>,
    ) -> anyhow::Result<Self> {
        let prior_repos = prior
            .map(|manifest| {
                manifest
                    .repos
                    .iter()
                    .map(|source| (source.uid.as_str(), source))
                    .collect::<std::collections::HashMap<_, _>>()
            })
            .unwrap_or_default();
        let prior_vaults = prior
            .map(|manifest| {
                manifest
                    .vaults
                    .iter()
                    .map(|source| (source.uid.as_str(), source))
                    .collect::<std::collections::HashMap<_, _>>()
            })
            .unwrap_or_default();
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
                let prior = prior_repos
                    .get(repo.uid.as_str())
                    .filter(|source| source.root_path == root.to_string_lossy())
                    .map(|source| source.files.as_slice());
                let (content_blake3, file_count, files) = hash_repo_tree(&root, prior)?;
                Ok(PublicationRepoSource {
                    uid: repo.uid,
                    url: repo.url,
                    instance_id: repo.instance_id,
                    name: repo.name,
                    root_path: root.to_string_lossy().to_string(),
                    observed_head: git_head(&root),
                    content_blake3,
                    file_count,
                    files,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let mut vaults = store
            .list_vaults(None)
            .map_err(|error| anyhow::anyhow!("list publication vaults: {error}"))?
            .into_iter()
            .map(|vault| {
                let root = canonical_source_root(Path::new(&vault.root_path), "vault")?;
                let prior = prior_vaults
                    .get(vault.uid.as_str())
                    .filter(|source| source.root_path == root.to_string_lossy())
                    .map(|source| source.files.as_slice());
                let (content_blake3, file_count, files) = hash_markdown_tree(&root, prior)?;
                Ok(PublicationVaultSource {
                    uid: vault.uid,
                    name: vault.name,
                    instance_id: vault.instance_id,
                    root_path: root.to_string_lossy().to_string(),
                    content_blake3,
                    file_count,
                    files,
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

fn hash_repo_tree(
    root: &Path,
    prior: Option<&[PublicationSourceFile]>,
) -> anyhow::Result<(String, u64, Vec<PublicationSourceFile>)> {
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
        return hash_relative_files(root, paths, prior);
    }
    let walker = ignore::WalkBuilder::new(root)
        .follow_links(false)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .require_git(false)
        .filter_entry(|entry| {
            !entry.file_type().is_some_and(|kind| kind.is_dir())
                || !entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| crate::index::SKIP_DIRS.contains(&name) || name == ".git")
        })
        .build();
    let paths = walker
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
        .filter_map(|entry| entry.path().strip_prefix(root).ok().map(Path::to_path_buf))
        .collect();
    hash_relative_files(root, paths, prior)
}

fn hash_markdown_tree(
    root: &Path,
    prior: Option<&[PublicationSourceFile]>,
) -> anyhow::Result<(String, u64, Vec<PublicationSourceFile>)> {
    hash_walk(
        root,
        |path| {
            path.extension()
                .and_then(|value| value.to_str())
                .is_some_and(|value| value.eq_ignore_ascii_case("md"))
        },
        prior,
    )
}

fn hash_walk(
    root: &Path,
    include: impl Fn(&Path) -> bool,
    prior: Option<&[PublicationSourceFile]>,
) -> anyhow::Result<(String, u64, Vec<PublicationSourceFile>)> {
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
    hash_relative_files(root, paths, prior)
}

fn hash_relative_files(
    root: &Path,
    mut paths: Vec<PathBuf>,
    prior: Option<&[PublicationSourceFile]>,
) -> anyhow::Result<(String, u64, Vec<PublicationSourceFile>)> {
    paths.sort();
    paths.dedup();
    let prior = prior
        .unwrap_or_default()
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect::<std::collections::HashMap<_, _>>();
    let mut files = Vec::with_capacity(paths.len());
    let mut hasher = blake3::Hasher::new();
    for relative in paths {
        let path = root.join(&relative);
        if !path.is_file() {
            continue;
        }
        let metadata = std::fs::metadata(&path)
            .with_context(|| format!("inspect publication source {}", path.display()))?;
        let byte_size = metadata.len();
        let change_token = strong_change_token(&metadata);
        let relative = relative.to_string_lossy().to_string();
        let reusable = prior
            .get(relative.as_str())
            .filter(|prior| {
                change_token.is_some()
                    && prior.byte_size == byte_size
                    && prior.change_token == change_token
            })
            .map(|prior| prior.content_blake3.clone());
        let content_blake3 = match reusable {
            Some(digest) => digest,
            None => hash_source_file(&path, &metadata)?,
        };
        hasher.update(&(relative.len() as u64).to_le_bytes());
        hasher.update(relative.as_bytes());
        hasher.update(&byte_size.to_le_bytes());
        hasher.update(content_blake3.as_bytes());
        files.push(PublicationSourceFile {
            path: relative,
            byte_size,
            change_token,
            content_blake3,
        });
    }
    Ok((
        hasher.finalize().to_hex().to_string(),
        files.len() as u64,
        files,
    ))
}

fn hash_source_file(path: &Path, before: &std::fs::Metadata) -> anyhow::Result<String> {
    let before_modified = before.modified().ok();
    let before_token = strong_change_token(before);
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("read publication source {}", path.display()))?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        #[cfg(test)]
        HASHED_SOURCE_BYTES.fetch_add(read as u64, std::sync::atomic::Ordering::Relaxed);
        hasher.update(&buffer[..read]);
    }
    let after = file.metadata()?;
    let path_after = std::fs::metadata(path)
        .with_context(|| format!("reinspect publication source {}", path.display()))?;
    if before.len() != after.len()
        || before_modified != after.modified().ok()
        || before_token != strong_change_token(&after)
        || before.len() != path_after.len()
        || before_modified != path_after.modified().ok()
        || before_token != strong_change_token(&path_after)
    {
        anyhow::bail!(
            "publication source changed while it was being captured: {}",
            path.display()
        );
    }
    Ok(hasher.finalize().to_hex().to_string())
}

#[cfg(unix)]
fn strong_change_token(metadata: &std::fs::Metadata) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    Some(format!(
        "unix-v1:{}:{}:{}:{}:{}:{}",
        metadata.dev(),
        metadata.ino(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec()
    ))
}

#[cfg(not(unix))]
fn strong_change_token(_metadata: &std::fs::Metadata) -> Option<String> {
    None
}

#[cfg(test)]
static HASHED_SOURCE_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(test)]
static HASH_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_source_fingerprint_is_stable_and_content_sensitive() {
        let _guard = HASH_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "alpha").unwrap();
        std::fs::write(dir.path().join("ignored.txt"), "one").unwrap();
        let first = hash_markdown_tree(dir.path(), None).unwrap();
        let second = hash_markdown_tree(dir.path(), Some(&first.2)).unwrap();
        assert_eq!(first.0, second.0);
        std::fs::write(dir.path().join("ignored.txt"), "two").unwrap();
        assert_eq!(
            hash_markdown_tree(dir.path(), Some(&second.2)).unwrap().0,
            first.0
        );
        std::fs::write(dir.path().join("a.md"), "beta").unwrap();
        assert_ne!(
            hash_markdown_tree(dir.path(), Some(&second.2)).unwrap().0,
            first.0
        );
    }

    #[cfg(unix)]
    #[test]
    fn validation_reuses_unchanged_content_and_hashes_changed_files() {
        let _guard = HASH_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.md");
        std::fs::write(&path, "alpha").unwrap();
        HASHED_SOURCE_BYTES.store(0, std::sync::atomic::Ordering::Relaxed);
        let first = hash_markdown_tree(dir.path(), None).unwrap();
        assert_eq!(
            HASHED_SOURCE_BYTES.load(std::sync::atomic::Ordering::Relaxed),
            5
        );

        HASHED_SOURCE_BYTES.store(0, std::sync::atomic::Ordering::Relaxed);
        let unchanged = hash_markdown_tree(dir.path(), Some(&first.2)).unwrap();
        assert_eq!(unchanged.0, first.0);
        assert_eq!(
            HASHED_SOURCE_BYTES.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "unchanged validation should prove equality from the strong change token"
        );

        std::fs::write(&path, "changed-content").unwrap();
        HASHED_SOURCE_BYTES.store(0, std::sync::atomic::Ordering::Relaxed);
        let changed = hash_markdown_tree(dir.path(), Some(&unchanged.2)).unwrap();
        assert_ne!(changed.0, first.0);
        assert_eq!(
            HASHED_SOURCE_BYTES.load(std::sync::atomic::Ordering::Relaxed),
            "changed-content".len() as u64
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_git_repository_uses_index_ignore_policy_and_incremental_validation() {
        let _guard = HASH_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "*.log\n").unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "fn live() {}\n").unwrap();
        std::fs::write(dir.path().join("ignored.log"), "large generated log").unwrap();
        std::fs::write(dir.path().join("target/bundle.js"), "generated").unwrap();

        let first = hash_repo_tree(dir.path(), None).unwrap();
        let paths = first
            .2
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>();
        assert!(paths.contains(&"src/lib.rs"));
        assert!(!paths.contains(&"ignored.log"));
        assert!(!paths.contains(&"target/bundle.js"));

        HASHED_SOURCE_BYTES.store(0, std::sync::atomic::Ordering::Relaxed);
        let unchanged = hash_repo_tree(dir.path(), Some(&first.2)).unwrap();
        assert_eq!(unchanged.0, first.0);
        assert_eq!(
            HASHED_SOURCE_BYTES.load(std::sync::atomic::Ordering::Relaxed),
            0
        );
    }
}
