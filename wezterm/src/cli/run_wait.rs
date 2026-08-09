use crate::cli::resolve_relative_cwd;
use clap::{Parser, ValueHint};
use config::keyassignment::SpawnTabDomain;
use config::ConfigHandle;
use mux::pane::PaneId;
use mux::window::WindowId;
use portable_pty::cmdbuilder::CommandBuilder;
use std::ffi::OsString;
use std::time::{Duration, Instant};
use wezterm_client::client::Client;

/// Spawn a command in a pane and wait for that pane to exit.
///
/// Completion uses the native mux exit-status RPC, including the short-lived
/// exit-status cache used when the configured exit behavior removes the pane.
#[derive(Debug, Parser, Clone)]
pub struct RunWaitCommand {
    /// Specify the current pane. The pane determines the target window and
    /// domain when --window-id and --domain-name are not provided.
    #[arg(long)]
    pane_id: Option<PaneId>,

    #[arg(long)]
    domain_name: Option<String>,

    /// Specify the window into which to spawn a tab.
    #[arg(long, conflicts_with_all = ["workspace", "new_window"])]
    window_id: Option<WindowId>,

    /// Spawn into a new window rather than a new tab.
    #[arg(long)]
    new_window: bool,

    /// Workspace name when creating a new window.
    #[arg(long, requires = "new_window")]
    workspace: Option<String>,

    /// Current working directory for the spawned program.
    #[arg(long, value_parser, value_hint = ValueHint::DirPath)]
    cwd: Option<OsString>,

    /// Fail if the pane has not exited within this many seconds.
    #[arg(long)]
    timeout: Option<u64>,

    /// Polling interval in milliseconds.
    #[arg(long, default_value_t = 100)]
    poll_interval_ms: u64,

    /// Program and arguments to execute.
    #[arg(value_parser, value_hint = ValueHint::CommandWithArguments, num_args = 1..)]
    prog: Vec<OsString>,
}

impl RunWaitCommand {
    pub async fn run(self, client: Client, config: &ConfigHandle) -> anyhow::Result<()> {
        let window_id = if self.new_window {
            None
        } else {
            match self.window_id {
                Some(window_id) => Some(window_id),
                None => {
                    let pane_id = client.resolve_pane_id(self.pane_id).await?;
                    find_window_for_pane(&client, pane_id).await?
                }
            }
        };

        let workspace = self
            .workspace
            .as_deref()
            .unwrap_or(
                config
                    .default_workspace
                    .as_deref()
                    .unwrap_or(mux::DEFAULT_WORKSPACE),
            )
            .to_string();

        let spawned = client
            .spawn_v2(codec::SpawnV2 {
                domain: self
                    .domain_name
                    .map_or(SpawnTabDomain::DefaultDomain, |name| {
                        SpawnTabDomain::DomainName(name)
                    }),
                window_id,
                create_workspace: false,
                command: Some(CommandBuilder::from_argv(self.prog)),
                command_dir: resolve_relative_cwd(self.cwd)?,
                size: config.initial_size(0, None),
                workspace,
            })
            .await?;

        let completion = wait_for_completion(
            &client,
            spawned.pane_id,
            self.timeout,
            self.poll_interval_ms,
        )
        .await?;

        println!(
            "{}",
            serde_json::json!({
                "pane_id": spawned.pane_id,
                "exit_code": completion.exit_code,
                "signal": completion.signal,
            })
        );
        if let Some(code) = completion.exit_code {
            if code != 0 {
                anyhow::bail!("pane {} exited with code {}", spawned.pane_id, code);
            }
        } else if let Some(signal) = completion.signal {
            anyhow::bail!("pane {} terminated by {}", spawned.pane_id, signal);
        }
        Ok(())
    }
}

#[derive(Debug)]
struct Completion {
    exit_code: Option<u32>,
    signal: Option<String>,
}

async fn wait_for_completion(
    client: &Client,
    pane_id: PaneId,
    timeout_secs: Option<u64>,
    poll_interval_ms: u64,
) -> anyhow::Result<Completion> {
    let started = Instant::now();
    let poll_interval = Duration::from_millis(poll_interval_ms.max(10));
    let timeout = timeout_secs.map(Duration::from_secs);
    // A pane-removal notification and the exit-status cache update are
    // delivered on the same mux thread but can be observed by a client in
    // adjacent polls. Give the cache a short grace period before reporting an
    // unknown status.
    let mut missing_status_polls = 0u8;

    loop {
        let status = client
            .get_pane_exit_status(codec::GetPaneExitStatus { pane_id })
            .await?;
        if status.exit_code.is_some() || status.signal.is_some() {
            return Ok(Completion {
                exit_code: status.exit_code,
                signal: status.signal,
            });
        }
        if !status.is_alive {
            if missing_status_polls < 5 {
                missing_status_polls += 1;
                smol::Timer::after(poll_interval).await;
                continue;
            }
            return Ok(Completion {
                exit_code: None,
                signal: None,
            });
        }
        missing_status_polls = 0;

        if let Some(timeout) = timeout {
            if started.elapsed() >= timeout {
                anyhow::bail!("timed out waiting for pane {pane_id}");
            }
        }
        smol::Timer::after(poll_interval).await;
    }
}

async fn find_window_for_pane(
    client: &Client,
    pane_id: PaneId,
) -> anyhow::Result<Option<WindowId>> {
    let panes = client.list_panes().await?;
    for tabroot in panes.tabs {
        let mut cursor = tabroot.into_tree().cursor();
        loop {
            if let Some(entry) = cursor.leaf_mut() {
                if entry.pane_id == pane_id {
                    return Ok(Some(entry.window_id));
                }
            }
            match cursor.preorder_next() {
                Ok(next) => cursor = next,
                Err(_) => break,
            }
        }
    }
    Ok(None)
}
