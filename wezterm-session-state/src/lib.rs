use anyhow::Context;
use chrono::Utc;
use codec::ListPanesResponse;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const SNAPSHOT_VERSION: u32 = 1;

#[derive(Debug, Deserialize, Serialize)]
pub struct SessionSnapshot {
    pub version: u32,
    pub created_at: String,
    pub mux: ListPanesResponse,
}

impl SessionSnapshot {
    pub fn new(mux: ListPanesResponse) -> Self {
        Self {
            version: SNAPSHOT_VERSION,
            created_at: Utc::now().to_rfc3339(),
            mux,
        }
    }
}

pub fn resolve_path(path: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    match path {
        Some(path) => Ok(path),
        None => default_path(),
    }
}

pub fn state_directory() -> anyhow::Result<PathBuf> {
    let state_dir = if let Some(path) = std::env::var_os("XDG_STATE_HOME") {
        PathBuf::from(path)
    } else if cfg!(target_os = "macos") {
        dirs_next::data_local_dir()
            .context("could not determine the macOS application support directory")?
    } else {
        dirs_next::home_dir()
            .context("could not determine the home directory")?
            .join(".local")
            .join("state")
    };
    Ok(state_dir.join("wezterm"))
}

pub fn default_path() -> anyhow::Result<PathBuf> {
    Ok(state_directory()?.join("herdr.json"))
}

pub fn write_atomic(path: &Path, snapshot: &SessionSnapshot) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating snapshot directory {}", parent.display()))?;
    }

    let data = serde_json::to_vec_pretty(snapshot).context("serializing session snapshot")?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp_path = path.with_extension(format!("tmp-{}-{nonce}", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&temp_path)
        .with_context(|| format!("writing temporary snapshot {}", temp_path.display()))?;
    file.write_all(&data)
        .with_context(|| format!("writing temporary snapshot {}", temp_path.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing temporary snapshot {}", temp_path.display()))?;
    drop(file);
    if let Err(err) = fs::rename(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(err).with_context(|| {
            format!(
                "renaming temporary snapshot {} to {}",
                temp_path.display(),
                path.display()
            )
        });
    }
    if let Some(parent) = path.parent() {
        let directory = OpenOptions::new()
            .read(true)
            .open(parent)
            .with_context(|| format!("opening snapshot directory {}", parent.display()))?;
        directory
            .sync_all()
            .with_context(|| format!("syncing snapshot directory {}", parent.display()))?;
    }
    Ok(())
}

pub fn read(path: &Path) -> anyhow::Result<SessionSnapshot> {
    let data = fs::read(path).with_context(|| format!("reading snapshot {}", path.display()))?;
    let snapshot: SessionSnapshot = serde_json::from_slice(&data)
        .with_context(|| format!("decoding snapshot {}", path.display()))?;
    if snapshot.version != SNAPSHOT_VERSION {
        anyhow::bail!(
            "unsupported session snapshot version {}; expected {}",
            snapshot.version,
            SNAPSHOT_VERSION
        );
    }
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn atomic_write_round_trips_and_uses_private_file_mode() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested").join("snapshot.json");
        let snapshot = SessionSnapshot::new(ListPanesResponse {
            tabs: vec![],
            tab_titles: vec![],
            window_titles: std::collections::HashMap::new(),
            active_tabs: std::collections::HashMap::new(),
        });
        write_atomic(&path, &snapshot).unwrap();
        assert_eq!(read(&path).unwrap().version, SNAPSHOT_VERSION);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn rejects_unknown_snapshot_versions() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("snapshot.json");
        std::fs::write(&path, r#"{\"version\":999,\"created_at\":\"now\",\"mux\":{\"tabs\":[],\"tab_titles\":[],\"window_titles\":{},\"active_tabs\":{}}}"#).unwrap();
        assert!(read(&path).is_err());
    }
}
