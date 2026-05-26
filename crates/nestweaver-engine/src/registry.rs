use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryEntry {
    pub id: String,
    pub config_path: String,
    pub snapshot_path: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Registry {
    #[serde(skip)]
    path: PathBuf,
    pub instances: Vec<RegistryEntry>,
}

impl Registry {
    pub fn load_or_create(path: &Path) -> Result<Self, anyhow::Error> {
        if path.exists() {
            let contents = std::fs::read_to_string(path)?;
            let mut reg: Registry = serde_json::from_str(&contents)?;
            reg.path = path.to_path_buf();
            Ok(reg)
        } else {
            Ok(Registry {
                path: path.to_path_buf(),
                instances: Vec::new(),
            })
        }
    }

    pub fn save(&self) -> Result<(), anyhow::Error> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let contents = serde_json::to_string_pretty(self)?;
        std::fs::write(&self.path, contents)?;
        Ok(())
    }

    pub fn register(&mut self, id: &str, config_path: &str) -> Result<(), anyhow::Error> {
        if self.instances.iter().any(|e| e.id == id) {
            anyhow::bail!("instance '{}' is already registered", id);
        }
        self.instances.push(RegistryEntry {
            id: id.to_string(),
            config_path: config_path.to_string(),
            snapshot_path: None,
        });
        self.save()
    }

    pub fn remove(&mut self, id: &str) -> Result<(), anyhow::Error> {
        let len_before = self.instances.len();
        self.instances.retain(|e| e.id != id);
        if self.instances.len() == len_before {
            anyhow::bail!("instance '{}' not found in registry", id);
        }
        self.save()
    }

    pub fn list(&self) -> &[RegistryEntry] {
        &self.instances
    }

    pub fn get(&self, id: &str) -> Option<&RegistryEntry> {
        self.instances.iter().find(|e| e.id == id)
    }
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_list() {
        let dir = tempfile::tempdir().unwrap();
        let reg_path = dir.path().join("registry.json");
        let mut reg = Registry::load_or_create(&reg_path).unwrap();
        reg.register("my-instance", "/path/to/config.toml").unwrap();
        let entries = reg.list();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "my-instance");
        assert_eq!(entries[0].config_path, "/path/to/config.toml");
        assert!(entries[0].snapshot_path.is_none());
    }

    #[test]
    fn remove_instance() {
        let dir = tempfile::tempdir().unwrap();
        let reg_path = dir.path().join("registry.json");
        let mut reg = Registry::load_or_create(&reg_path).unwrap();
        reg.register("alpha", "/cfg/alpha.toml").unwrap();
        reg.remove("alpha").unwrap();
        assert!(reg.list().is_empty());
    }

    #[test]
    fn duplicate_register_errors() {
        let dir = tempfile::tempdir().unwrap();
        let reg_path = dir.path().join("registry.json");
        let mut reg = Registry::load_or_create(&reg_path).unwrap();
        reg.register("dup", "/cfg/dup.toml").unwrap();
        let result = reg.register("dup", "/cfg/other.toml");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("already registered"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn load_persists_across_instances() {
        let dir = tempfile::tempdir().unwrap();
        let reg_path = dir.path().join("registry.json");

        {
            let mut reg = Registry::load_or_create(&reg_path).unwrap();
            reg.register("persistent", "/cfg/persistent.toml").unwrap();
            reg.save().unwrap();
        }

        {
            let reg2 = Registry::load_or_create(&reg_path).unwrap();
            assert_eq!(reg2.list().len(), 1);
            assert_eq!(reg2.list()[0].id, "persistent");
            assert_eq!(reg2.list()[0].config_path, "/cfg/persistent.toml");
        }
    }

    #[test]
    fn get_by_id() {
        let dir = tempfile::tempdir().unwrap();
        let reg_path = dir.path().join("registry.json");
        let mut reg = Registry::load_or_create(&reg_path).unwrap();
        reg.register("find-me", "/cfg/find-me.toml").unwrap();
        reg.register("other", "/cfg/other.toml").unwrap();

        let entry = reg.get("find-me").expect("should find entry");
        assert_eq!(entry.id, "find-me");
        assert_eq!(entry.config_path, "/cfg/find-me.toml");

        assert!(reg.get("does-not-exist").is_none());
    }
}
