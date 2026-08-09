use mux::TEMP_WORKSPACE_PREFIX;
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

/// Cross-process workspace ownership for GUI clients. The mux remains shared,
/// but a workspace can have only one GUI owner at a time.
pub struct WorkspaceLeases {
    root: PathBuf,
    pid: u32,
    owned: HashSet<String>,
}

impl WorkspaceLeases {
    pub fn new() -> anyhow::Result<Self> {
        let root = dirs_next::home_dir()
            .ok_or_else(|| anyhow::anyhow!("could not determine home directory"))?
            .join(".local/share/wezterm/workspace-locks");
        fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            pid: std::process::id(),
            owned: HashSet::new(),
        })
    }

    fn encoded(name: &str) -> String {
        let mut encoded = String::new();
        for byte in name.bytes() {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-') {
                encoded.push(byte as char);
            } else {
                encoded.push_str(&format!("%{byte:02x}"));
            }
        }
        encoded
    }

    fn path_for(&self, name: &str) -> PathBuf {
        self.root.join(Self::encoded(name))
    }

    fn owner_pid(&self, name: &str) -> Option<u32> {
        fs::read_to_string(self.path_for(name).join("owner"))
            .ok()?
            .lines()
            .next()?
            .parse()
            .ok()
    }

    fn live_gui(pid: u32) -> bool {
        if pid == std::process::id() {
            return true;
        }
        #[cfg(unix)]
        {
            if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
                return false;
            }
            fs::read_to_string(format!("/proc/{pid}/cmdline"))
                .map(|cmd| cmd.contains("wezterm-gui"))
                .unwrap_or(false)
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    /// Atomically claim a workspace. Stale owners are reclaimed, while a live
    /// GUI owner always wins over a new client.
    pub fn claim(&mut self, name: &str) -> bool {
        if name.is_empty() || self.owned.contains(name) {
            return !name.is_empty();
        }
        let path = self.path_for(name);
        if let Some(pid) = self.owner_pid(name) {
            if pid == self.pid {
                self.owned.insert(name.to_string());
                return true;
            }
            if Self::live_gui(pid) {
                return false;
            }
            let _ = fs::remove_dir_all(&path);
        }
        if fs::create_dir(&path).is_err() {
            return false;
        }
        let result = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path.join("owner"))
            .and_then(|mut owner| writeln!(owner, "{}", self.pid));
        if result.is_err() {
            let _ = fs::remove_dir_all(path);
            return false;
        }
        self.owned.insert(name.to_string());
        true
    }

    pub fn release(&mut self, name: &str) {
        if self.owned.remove(name) {
            let _ = fs::remove_dir_all(self.path_for(name));
        }
    }

    pub fn temporary_name(&self) -> String {
        format!("{TEMP_WORKSPACE_PREFIX}{}", self.pid)
    }
}

impl Drop for WorkspaceLeases {
    fn drop(&mut self) {
        let root = self.root.clone();
        for name in self.owned.drain() {
            let _ = fs::remove_dir_all(root.join(Self::encoded(&name)));
        }
    }
}
