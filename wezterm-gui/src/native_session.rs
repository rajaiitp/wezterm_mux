use anyhow::Context;
use chrono::Utc;
use codec::ListPanesResponse;
use mux::Mux;
use promise::spawn::spawn;
use serde::Serialize;
use smol::Timer;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DEFAULT_INTERVAL_SECONDS: u64 = 300;

fn enabled(name: &str, default: bool) -> bool {
    match env::var(name).ok().as_deref() {
        Some("0") | Some("false") | Some("no") => false,
        Some("1") | Some("true") | Some("yes") => true,
        _ => default,
    }
}

fn state_path() -> anyhow::Result<PathBuf> {
    let state_dir = if let Some(path) = env::var_os("XDG_STATE_HOME") {
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

#[derive(Debug, Serialize)]
struct SessionSnapshot {
    version: u32,
    created_at: String,
    mux: ListPanesResponse,
}

fn executable() -> anyhow::Result<PathBuf> {
    let configured = env::var_os("WEZTERM_EXECUTABLE").map(PathBuf::from);
    let current = configured.clone().unwrap_or(env::current_exe()?);
    let parent = current
        .parent()
        .context("GUI executable has no parent directory")?;
    let is_cli = current
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name != "wezterm-gui" && name != "wezterm-gui.exe")
        .unwrap_or(false);
    if is_cli {
        return Ok(current);
    }

    for name in ["wezterm-herdr", "wezterm"] {
        let candidate = parent.join(name);
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    anyhow::bail!("could not find a wezterm CLI executable beside the GUI")
}

fn run_cli(path: PathBuf, args: Vec<String>) -> anyhow::Result<()> {
    let exe = executable()?;
    let output = Command::new(exe)
        .arg("cli")
        .args(args)
        .arg("--file")
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .context("running native session CLI")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "native session CLI exited with {}: {}",
            output.status,
            stderr.trim()
        );
    }
    Ok(())
}

pub async fn restore_if_available() -> anyhow::Result<bool> {
    if !enabled("WEZTERM_HERDR_NATIVE_SESSION_RESTORE", true) {
        return Ok(false);
    }
    let path = state_path()?;
    if !path.exists() {
        return Ok(false);
    }
    smol::unblock(move || {
        run_cli(
            path,
            vec!["restore-state".to_string(), "--consume".to_string()],
        )
    })
    .await?;
    Ok(true)
}

fn capture_mux() -> ListPanesResponse {
    let mux = Mux::get();
    let mut tabs = vec![];
    let mut tab_titles = vec![];
    let mut window_titles = std::collections::HashMap::new();
    let mut active_tabs = std::collections::HashMap::new();
    for window_id in mux.iter_windows() {
        if let Some(window) = mux.get_window(window_id) {
            if let Some(tab) = window.get_active() {
                active_tabs.insert(window_id, tab.tab_id());
            }
            window_titles.insert(window_id, window.get_title().to_string());
            for tab in window.iter() {
                tabs.push(tab.codec_pane_tree());
                tab_titles.push(tab.get_title());
            }
        }
    }
    ListPanesResponse {
        tabs,
        tab_titles,
        window_titles,
        active_tabs,
    }
}

fn write_atomic(path: &Path, snapshot: &SessionSnapshot) -> anyhow::Result<()> {
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
    Ok(())
}

pub fn save_now() -> anyhow::Result<()> {
    if !enabled("WEZTERM_HERDR_NATIVE_SESSION_AUTOSAVE", true) {
        return Ok(());
    }
    let path = state_path()?;
    let mux = capture_mux();
    // The final WindowRemoved notification is emitted after the last tab has
    // already been detached. Do not replace a useful restore point with an
    // empty snapshot during a normal GUI close.
    if mux.tabs.is_empty() {
        log::debug!("native session save skipped because the mux is empty");
        return Ok(());
    }
    let snapshot = SessionSnapshot {
        version: 1,
        created_at: Utc::now().to_rfc3339(),
        mux,
    };
    write_atomic(&path, &snapshot)
}

pub fn start_periodic_save() {
    if !enabled("WEZTERM_HERDR_NATIVE_SESSION_AUTOSAVE", true) {
        return;
    }
    let interval = env::var("WEZTERM_HERDR_NATIVE_SESSION_INTERVAL")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_INTERVAL_SECONDS)
        .max(10);

    spawn(async move {
        loop {
            Timer::after(Duration::from_secs(interval)).await;
            let result = smol::unblock(save_now).await;
            if let Err(err) = result {
                log::warn!("native session periodic save failed: {err:#}");
            }
        }
    })
    .detach();
}
