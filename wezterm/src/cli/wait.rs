use anyhow::bail;
use clap::Parser;
use mux::pane::PaneId;
use mux::tab::PaneNode;
use smol::Timer;
use std::time::{Duration, Instant};
use wezterm_client::client::Client;

/// Wait until a pane exits or disappears from the mux.
#[derive(Debug, Parser, Clone, Copy)]
pub struct WaitCommand {
    /// Specify the target pane. The default is the current pane based on
    /// WEZTERM_PANE.
    #[arg(long)]
    pane_id: Option<PaneId>,

    /// Fail if the pane has not exited within this many seconds.
    #[arg(long)]
    timeout: Option<u64>,

    /// Polling interval in milliseconds.
    #[arg(long, default_value_t = 100)]
    poll_interval_ms: u64,
}

impl WaitCommand {
    pub async fn run(self, client: Client) -> anyhow::Result<()> {
        let pane_id = client.resolve_pane_id(self.pane_id).await?;
        wait_for_pane(&client, pane_id, self.timeout, self.poll_interval_ms).await?;
        println!("{pane_id}");
        Ok(())
    }
}

pub async fn wait_for_pane(
    client: &Client,
    pane_id: PaneId,
    timeout_secs: Option<u64>,
    poll_interval_ms: u64,
) -> anyhow::Result<()> {
    let started = Instant::now();
    let poll_interval = Duration::from_millis(poll_interval_ms.max(10));
    let timeout = timeout_secs.map(Duration::from_secs);

    loop {
        let status = client
            .get_pane_exit_status(codec::GetPaneExitStatus { pane_id })
            .await?;
        if status.exit_code.is_some() || status.signal.is_some() || !status.is_alive {
            return Ok(());
        }

        // A pane may disappear between the status request and the next poll;
        // treat that as successful completion rather than surfacing a race.
        let panes = client.list_panes().await?;
        if !pane_exists(&panes, pane_id) {
            return Ok(());
        }

        if let Some(timeout) = timeout {
            if started.elapsed() >= timeout {
                bail!("timed out waiting for pane {pane_id}");
            }
        }

        Timer::after(poll_interval).await;
    }
}

pub(crate) fn pane_exists(panes: &codec::ListPanesResponse, target: PaneId) -> bool {
    contains_pane(&panes.tabs, target)
}

fn contains_pane(tabs: &[PaneNode], target: PaneId) -> bool {
    tabs.iter().any(|tab| contains_pane_node(tab, target))
}

fn contains_pane_node(node: &PaneNode, target: PaneId) -> bool {
    match node {
        PaneNode::Empty => false,
        PaneNode::Leaf(entry) => entry.pane_id == target,
        PaneNode::Split { left, right, .. } => {
            contains_pane_node(left, target) || contains_pane_node(right, target)
        }
    }
}
