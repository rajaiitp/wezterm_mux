use crate::types::{DomainKey, Lifecycle, ProjectId, ProjectIdentity, WorkspaceId};
use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const REGISTRY_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRecord {
    pub identity: ProjectIdentity,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub last_opened_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryWorkspace {
    pub id: WorkspaceId,
    pub label: String,
    pub domain: DomainKey,
    pub project_id: ProjectId,
    pub worktree_path: PathBuf,
    pub branch: Option<String>,
    pub managed: bool,
    pub layout_profile: String,
    pub lifecycle: Lifecycle,
    pub last_opened_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Registry {
    pub version: u32,
    #[serde(default)]
    pub projects: BTreeMap<ProjectId, ProjectRecord>,
    #[serde(default)]
    pub workspaces: BTreeMap<WorkspaceId, RegistryWorkspace>,
    #[serde(default)]
    pub last_opened_workspace: Option<WorkspaceId>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            version: REGISTRY_VERSION,
            projects: BTreeMap::new(),
            workspaces: BTreeMap::new(),
            last_opened_workspace: None,
        }
    }
}

impl Registry {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let contents = fs::read_to_string(path)
            .with_context(|| format!("read workspace registry {}", path.display()))?;
        let registry: Self = serde_json::from_str(&contents)
            .with_context(|| format!("decode workspace registry {}", path.display()))?;
        if registry.version != REGISTRY_VERSION {
            bail!(
                "unsupported workspace registry version {}; expected {}",
                registry.version,
                REGISTRY_VERSION
            );
        }
        Ok(registry)
    }

    pub fn save_atomic(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create registry directory {}", parent.display()))?;
        }
        let data = serde_json::to_vec_pretty(self).context("encode workspace registry")?;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let temporary = path.with_extension(format!("tmp-{}-{nonce}", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options
            .open(&temporary)
            .with_context(|| format!("write temporary registry {}", temporary.display()))?;
        file.write_all(&data)
            .with_context(|| format!("write temporary registry {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("sync temporary registry {}", temporary.display()))?;
        drop(file);
        if let Err(error) = fs::rename(&temporary, path) {
            let _ = fs::remove_file(&temporary);
            return Err(error).with_context(|| {
                format!(
                    "replace workspace registry {} with {}",
                    path.display(),
                    temporary.display()
                )
            });
        }
        if let Some(parent) = path.parent() {
            let directory = OpenOptions::new()
                .read(true)
                .open(parent)
                .with_context(|| format!("open registry directory {}", parent.display()))?;
            directory
                .sync_all()
                .with_context(|| format!("sync registry directory {}", parent.display()))?;
        }
        Ok(())
    }

    pub fn remember_project(&mut self, identity: ProjectIdentity, now: u64) {
        self.projects.insert(
            identity.id.clone(),
            ProjectRecord {
                identity,
                aliases: Vec::new(),
                last_opened_at: now,
            },
        );
    }

    pub fn remember_workspace(&mut self, workspace: RegistryWorkspace) {
        self.last_opened_workspace = Some(workspace.id.clone());
        self.workspaces.insert(workspace.id.clone(), workspace);
    }

    pub fn forget_workspace(&mut self, id: &WorkspaceId) {
        self.workspaces.remove(id);
        if self.last_opened_workspace.as_ref() == Some(id) {
            self.last_opened_workspace = None;
        }
    }
}

pub fn default_registry_path() -> anyhow::Result<PathBuf> {
    let base = dirs_next::data_local_dir()
        .or_else(dirs_next::home_dir)
        .context("could not determine a user data directory")?;
    Ok(base.join("wezterm").join("project_workspaces.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DomainKey, DomainKind};
    use tempfile::tempdir;

    #[test]
    fn missing_registry_loads_empty_and_round_trips_atomically() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested/registry.json");
        let mut registry = Registry::default();
        let identity = ProjectIdentity {
            id: ProjectId("project:test".to_string()),
            domain: DomainKey {
                kind: DomainKind::Local,
                stable_id: "local".to_string(),
            },
            common_dir: PathBuf::from("/repo/.git"),
            primary_root: PathBuf::from("/repo"),
        };
        registry.remember_project(identity, 42);
        registry.save_atomic(&path).unwrap();
        let restored = Registry::load(&path).unwrap();
        assert_eq!(restored.projects.len(), 1);
        assert_eq!(
            restored.projects.values().next().unwrap().last_opened_at,
            42
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn rejects_unknown_versions() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("registry.json");
        fs::write(&path, r#"{"version":999}"#).unwrap();
        assert!(Registry::load(&path).is_err());
    }
}
