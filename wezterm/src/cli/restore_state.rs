use crate::cli::session_state::{read, resolve_path};
use clap::{Parser, ValueHint};
use config::keyassignment::SpawnTabDomain;
use config::ConfigHandle;
use mux::pane::PaneId;
use mux::tab::{PaneEntry, PaneNode, SplitDirection, SplitRequest, SplitSize};
use mux::window::WindowId;
use portable_pty::cmdbuilder::CommandBuilder;
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use wezterm_client::client::Client;

/// Restore windows, tabs, pane working directories, and an approximation of
/// the saved split topology from a native snapshot.
#[derive(Debug, Parser, Clone)]
pub struct RestoreStateCommand {
    /// Snapshot path. Defaults to the native user state directory.
    #[arg(long, value_hint = ValueHint::FilePath)]
    file: Option<PathBuf>,

    /// Override the workspace stored in the snapshot for all restored windows.
    #[arg(long)]
    workspace: Option<String>,

    /// Remove the snapshot after a successful restore. Used by automatic
    /// crash/reboot recovery so a stale snapshot is not restored repeatedly.
    #[arg(long, hide = true)]
    consume: bool,
}

#[derive(Debug, Clone)]
struct RestoredPane {
    saved: PaneEntry,
    actual: PaneId,
}

#[derive(Debug, Clone, Copy)]
enum Relation {
    Left,
    Right,
    Above,
    Below,
}

impl RestoreStateCommand {
    pub async fn run(self, client: Client, config: &ConfigHandle) -> anyhow::Result<()> {
        let path = resolve_path(self.file)?;
        let snapshot = read(&path)?;
        let codec::ListPanesResponse {
            tabs,
            tab_titles,
            window_titles,
        } = snapshot.mux;
        let mut window_ids = HashMap::<WindowId, WindowId>::new();
        let mut focused_pane = None;
        let mut restored_tabs = 0usize;
        let mut restored_panes = 0usize;

        for (root, tab_title) in tabs.into_iter().zip(tab_titles.into_iter()) {
            let Some((old_window_id, _old_tab_id)) = root.window_and_tab_ids() else {
                continue;
            };
            let Some(root_size) = root.root_size() else {
                continue;
            };
            let mut leaves = Vec::new();
            collect_leaves(root, &mut leaves);
            if leaves.is_empty() {
                continue;
            }
            leaves.sort_by_key(|entry| (entry.top_row, entry.left_col));

            let workspace = self
                .workspace
                .clone()
                .unwrap_or_else(|| leaves[0].workspace.clone());
            let first = leaves[0].clone();
            let window_id = window_ids.get(&old_window_id).copied();
            let spawned = client
                .spawn_v2(codec::SpawnV2 {
                    domain: SpawnTabDomain::DefaultDomain,
                    window_id,
                    command: restore_command(&first, config),
                    command_dir: command_dir(&first),
                    size: root_size,
                    workspace,
                })
                .await?;

            window_ids.insert(old_window_id, spawned.window_id);
            client
                .set_tab_title(codec::TabTitleChanged {
                    tab_id: spawned.tab_id,
                    title: tab_title,
                })
                .await?;
            if let Some(title) = window_titles.get(&old_window_id) {
                client
                    .set_window_title(codec::WindowTitleChanged {
                        window_id: spawned.window_id,
                        title: title.clone(),
                    })
                    .await?;
            }

            let mut restored = vec![RestoredPane {
                saved: first.clone(),
                actual: spawned.pane_id,
            }];
            let mut zoomed_panes = Vec::new();
            if first.is_zoomed_pane {
                zoomed_panes.push((spawned.tab_id, spawned.pane_id));
            }
            restored_panes += 1;
            if first.is_active_pane {
                focused_pane = Some(spawned.pane_id);
            }
            for entry in leaves.into_iter().skip(1) {
                let (target, split_request) = split_for(&entry, &restored);
                let spawned_pane = client
                    .split_pane(codec::SplitPane {
                        pane_id: target,
                        split_request,
                        command: restore_command(&entry, config),
                        command_dir: command_dir(&entry),
                        domain: SpawnTabDomain::CurrentPaneDomain,
                        move_pane_id: None,
                    })
                    .await?;

                if entry.is_active_pane {
                    focused_pane = Some(spawned_pane.pane_id);
                }
                if entry.is_zoomed_pane {
                    zoomed_panes.push((spawned_pane.tab_id, spawned_pane.pane_id));
                }
                restored.push(RestoredPane {
                    saved: entry,
                    actual: spawned_pane.pane_id,
                });
                restored_panes += 1;
            }
            for (tab_id, pane_id) in zoomed_panes {
                client
                    .set_zoomed(codec::SetPaneZoomed {
                        containing_tab_id: tab_id,
                        pane_id,
                        zoomed: true,
                    })
                    .await?;
            }
            restored_tabs += 1;
        }

        if let Some(pane_id) = focused_pane {
            client
                .set_focused_pane_id(codec::SetFocusedPane { pane_id })
                .await?;
        }

        if self.consume {
            if let Err(err) = fs::remove_file(&path) {
                // The restore has already succeeded; do not make the caller
                // restore a second session merely because cleanup failed.
                eprintln!(
                    "warning: restored the snapshot but could not remove {}: {}",
                    path.display(),
                    err
                );
            }
        }

        println!(
            "restored {restored_tabs} tabs and {restored_panes} panes from {}",
            path.display()
        );
        Ok(())
    }
}

fn collect_leaves(node: PaneNode, leaves: &mut Vec<PaneEntry>) {
    match node {
        PaneNode::Empty => {}
        PaneNode::Leaf(entry) => leaves.push(entry),
        PaneNode::Split { left, right, .. } => {
            collect_leaves(*left, leaves);
            collect_leaves(*right, leaves);
        }
    }
}

fn command_dir(entry: &PaneEntry) -> Option<String> {
    entry
        .working_dir
        .as_ref()
        .and_then(|url| url.url.to_file_path().ok())
        .and_then(|path| path.to_str().map(ToOwned::to_owned))
}

fn shell_command(config: &ConfigHandle) -> Option<Vec<OsString>> {
    config
        .default_prog
        .clone()
        .map(|prog| prog.into_iter().map(OsString::from).collect::<Vec<_>>())
}

fn restore_command(entry: &PaneEntry, config: &ConfigHandle) -> Option<CommandBuilder> {
    let Some(process) = entry.process.as_ref() else {
        return shell_command(config).map(CommandBuilder::from_argv);
    };
    let command = if is_app(process, &["nvim", "vim", "vi", "neovim"]) {
        editor_restore_command(process)
    } else if is_app(process, &["pi"]) {
        pi_restore_command(process, entry)
    } else if is_app(process, &["claude", "codex", "tuxedo", "tuicr"]) {
        recorded_restore_command(process)
    } else {
        return shell_command(config).map(CommandBuilder::from_argv);
    };
    Some(CommandBuilder::from_argv(command))
}

fn recorded_restore_command(process: &mux::tab::PaneProcessInfo) -> Vec<OsString> {
    if !process.argv.is_empty() {
        return process.argv.iter().map(OsString::from).collect();
    }

    let executable = if process.executable.is_empty() {
        if process.name.is_empty() {
            "sh"
        } else {
            process.name.as_str()
        }
    } else {
        &process.executable
    };
    vec![OsString::from(executable)]
}

fn editor_restore_command(process: &mux::tab::PaneProcessInfo) -> Vec<OsString> {
    // Neovim panes are commonly represented by `nvim --embed`: WezTerm's
    // process inspection sees the msgpack transport, not a normal terminal
    // invocation.  Re-launch the editor in the restored PTY and remove the
    // transport-only flag.  Some platforms omit argv[0], so prepend the
    // recorded executable in that case.
    let executable = if process.executable.is_empty() {
        if process.name.is_empty() {
            OsString::from("nvim")
        } else {
            OsString::from(&process.name)
        }
    } else {
        OsString::from(&process.executable)
    };
    let mut args: Vec<OsString> = process
        .argv
        .iter()
        .filter(|arg| arg.as_str() != "--embed")
        .map(OsString::from)
        .collect();

    let argv_has_editor = args
        .first()
        .and_then(|arg| Path::new(arg).file_name())
        .and_then(|name| name.to_str())
        .map(|name| {
            ["nvim", "vim", "vi", "neovim"]
                .iter()
                .any(|editor| name == *editor || name.starts_with(&format!("{editor}.")))
        })
        .unwrap_or(false);
    if !argv_has_editor {
        args.insert(0, executable);
    }
    if args.is_empty() {
        args.push(OsString::from("nvim"));
    }
    args
}

fn is_app(process: &mux::tab::PaneProcessInfo, names: &[&str]) -> bool {
    let mut values = Vec::with_capacity(process.argv.len() + 2);
    values.push(process.name.as_str());
    values.push(process.executable.as_str());
    values.extend(process.argv.iter().map(String::as_str));

    values.iter().any(|value| {
        let value = value.to_ascii_lowercase();
        let basename = Path::new(&value)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(value.as_str());
        names.iter().any(|name| {
            basename == *name
                || basename.starts_with(&format!("{name}."))
                || basename == format!("{name}.exe")
        })
    })
}

fn pi_restore_command(process: &mux::tab::PaneProcessInfo, entry: &PaneEntry) -> Vec<OsString> {
    if process
        .argv
        .iter()
        .any(|arg| arg == "--session" || arg == "--no-session" || arg == "--fork")
    {
        return process.argv.iter().map(OsString::from).collect();
    }

    if let Some(path) = latest_pi_session(entry, process) {
        return vec![
            OsString::from("pi"),
            OsString::from("--session"),
            path.into_os_string(),
        ];
    }

    vec![OsString::from("pi"), OsString::from("-c")]
}

fn latest_pi_session(entry: &PaneEntry, process: &mux::tab::PaneProcessInfo) -> Option<PathBuf> {
    let cwd = if process.cwd.is_empty() {
        entry
            .working_dir
            .as_ref()
            .and_then(|url| url.url.to_file_path().ok())?
    } else {
        PathBuf::from(&process.cwd)
    };
    let home = dirs_next::home_dir()?;
    let encoded = cwd
        .to_string_lossy()
        .trim_start_matches('/')
        .replace('/', "-");
    let session_dir = home
        .join(".pi")
        .join("agent")
        .join("sessions")
        .join(format!("--{encoded}--"));

    fs::read_dir(session_dir)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
        .filter_map(|entry| {
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, entry.path()))
        })
        .max_by_key(|(modified, _)| {
            modified
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
        })
        .map(|(_, path)| path)
}

#[cfg(test)]
mod tests {
    use super::{editor_restore_command, recorded_restore_command};
    use mux::tab::PaneProcessInfo;
    use std::path::Path;

    fn process(executable: &str, name: &str, argv: &[&str]) -> PaneProcessInfo {
        PaneProcessInfo {
            name: name.to_string(),
            executable: executable.to_string(),
            argv: argv.iter().map(|arg| (*arg).to_string()).collect(),
            cwd: String::new(),
        }
    }

    #[test]
    fn restores_embedded_nvim_as_a_terminal_editor() {
        let command = editor_restore_command(&process(
            "/opt/homebrew/bin/nvim",
            "nvim",
            &["nvim", "--embed", "init.lua"],
        ));
        assert_eq!(command, ["nvim", "init.lua"]);
    }

    #[test]
    fn supplies_executable_when_platform_omits_argv_zero() {
        let command =
            editor_restore_command(&process("/usr/bin/nvim", "nvim", &["--embed", "init.lua"]));
        assert_eq!(command, ["/usr/bin/nvim", "init.lua"]);
    }

    #[test]
    fn falls_back_to_nvim_when_process_metadata_is_incomplete() {
        let command = editor_restore_command(&process("", "", &["--embed"]));
        assert_eq!(command, ["nvim"]);
        assert_eq!(Path::new("nvim").file_name().unwrap(), "nvim");
    }

    #[test]
    fn preserves_recorded_agent_arguments() {
        let command = recorded_restore_command(&process(
            "/opt/homebrew/bin/codex",
            "codex",
            &["codex", "--resume", "session-123"],
        ));
        assert_eq!(command, ["codex", "--resume", "session-123"]);
    }

    #[test]
    fn falls_back_to_recorded_agent_executable() {
        let command = recorded_restore_command(&process("/usr/local/bin/claude", "", &[]));
        assert_eq!(command, ["/usr/local/bin/claude"]);
    }
}

fn split_for(entry: &PaneEntry, restored: &[RestoredPane]) -> (PaneId, SplitRequest) {
    if let Some((target, relation)) = restored
        .iter()
        .filter_map(|candidate| relation(entry, &candidate.saved).map(|r| (candidate, r)))
        .min_by_key(|(candidate, _)| {
            candidate
                .saved
                .left_col
                .abs_diff(entry.left_col)
                .saturating_add(candidate.saved.top_row.abs_diff(entry.top_row))
        })
    {
        let (direction, target_is_second, size) = match relation {
            Relation::Left => (SplitDirection::Horizontal, false, entry.size.cols),
            Relation::Right => (SplitDirection::Horizontal, true, entry.size.cols),
            Relation::Above => (SplitDirection::Vertical, false, entry.size.rows),
            Relation::Below => (SplitDirection::Vertical, true, entry.size.rows),
        };
        return (
            target.actual,
            SplitRequest {
                direction,
                target_is_second,
                top_level: false,
                size: SplitSize::Cells(size.max(1)),
            },
        );
    }

    let target = &restored[0];
    (
        target.actual,
        SplitRequest {
            direction: SplitDirection::Vertical,
            target_is_second: true,
            top_level: false,
            size: SplitSize::Cells(entry.size.rows.max(1)),
        },
    )
}

fn relation(new: &PaneEntry, existing: &PaneEntry) -> Option<Relation> {
    let new_right = new.left_col + new.size.cols;
    let existing_right = existing.left_col + existing.size.cols;
    let new_bottom = new.top_row + new.size.rows;
    let existing_bottom = existing.top_row + existing.size.rows;
    let vertical_overlap = new.top_row < existing_bottom && new_bottom > existing.top_row;
    let horizontal_overlap = new.left_col < existing_right && new_right > existing.left_col;

    if new.left_col >= existing_right && vertical_overlap {
        Some(Relation::Right)
    } else if new_right <= existing.left_col && vertical_overlap {
        Some(Relation::Left)
    } else if new.top_row >= existing_bottom && horizontal_overlap {
        Some(Relation::Below)
    } else if new_bottom <= existing.top_row && horizontal_overlap {
        Some(Relation::Above)
    } else {
        None
    }
}
