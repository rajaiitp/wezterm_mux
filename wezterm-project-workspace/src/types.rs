use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DomainKind {
    Local,
    Ssh,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DomainKey {
    pub kind: DomainKind,
    pub stable_id: String,
}

impl DomainKey {
    pub fn local() -> Self {
        Self {
            kind: DomainKind::Local,
            stable_id: "local".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub struct ProjectId(pub String);

impl ProjectId {
    pub fn from_common_dir(domain: &DomainKey, common_dir: &Path) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"wezterm-project-v1\0");
        hasher.update(format!("{:?}\0", domain.kind).as_bytes());
        hasher.update(domain.stable_id.as_bytes());
        hasher.update(b"\0");
        hasher.update(common_dir.as_os_str().as_encoded_bytes());
        let digest = hasher.finalize();
        Self(format!("project:{}", encode_hex(&digest[..12])))
    }
}

impl fmt::Display for ProjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub struct WorkspaceId(pub String);

impl WorkspaceId {
    pub fn for_worktree(domain: &DomainKey, path: &Path) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"wezterm-workspace-v1\0");
        hasher.update(format!("{:?}\0", domain.kind).as_bytes());
        hasher.update(domain.stable_id.as_bytes());
        hasher.update(b"\0");
        hasher.update(path.as_os_str().as_encoded_bytes());
        let digest = hasher.finalize();
        Self(format!("dev:{}", encode_hex(&digest[..12])))
    }
}

impl fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectIdentity {
    pub id: ProjectId,
    pub domain: DomainKey,
    pub common_dir: PathBuf,
    pub primary_root: PathBuf,
}

fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Lifecycle {
    Incomplete,
    Ready,
    Degraded,
    Stopped,
}
