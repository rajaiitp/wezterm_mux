use anyhow::Context;
use chrono::Utc;
use codec::ListPanesResponse;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

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

pub fn default_path() -> anyhow::Result<PathBuf> {
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

    Ok(state_dir.join("wezterm").join("herdr.json"))
}

pub fn write_atomic(path: &Path, snapshot: &SessionSnapshot) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating snapshot directory {}", parent.display()))?;
    }

    let data = serde_json::to_vec_pretty(snapshot).context("serializing session snapshot")?;
    let temp_path = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&temp_path, data)
        .with_context(|| format!("writing temporary snapshot {}", temp_path.display()))?;
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
