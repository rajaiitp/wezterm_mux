use crate::cli::session_state::{resolve_path, write_atomic, SessionSnapshot};
use clap::{Parser, ValueHint};
use std::collections::HashSet;
use std::path::PathBuf;
use wezterm_client::client::Client;

/// Save the native mux topology and pane metadata to a versioned snapshot.
#[derive(Debug, Parser, Clone)]
pub struct SaveStateCommand {
    /// Snapshot path. Defaults to the native user state directory.
    #[arg(long, value_hint = ValueHint::FilePath)]
    file: Option<PathBuf>,
}

impl SaveStateCommand {
    pub async fn run(self, client: Client) -> anyhow::Result<()> {
        let path = resolve_path(self.file)?;
        let mut mux = client.list_panes().await?;
        let mut tabs = Vec::with_capacity(mux.tabs.len());
        let mut tab_titles = Vec::with_capacity(mux.tab_titles.len());
        let mut retained_windows = HashSet::new();

        for (tab, title) in mux.tabs.into_iter().zip(mux.tab_titles.into_iter()) {
            if let Some((window_id, _)) = tab.window_and_tab_ids() {
                retained_windows.insert(window_id);
            }
            tabs.push(tab);
            tab_titles.push(title);
        }
        mux.tabs = tabs;
        mux.tab_titles = tab_titles;
        mux.window_titles
            .retain(|window_id, _| retained_windows.contains(window_id));
        mux.active_tabs
            .retain(|window_id, _| retained_windows.contains(window_id));

        let snapshot = SessionSnapshot::new(mux);
        write_atomic(&path, &snapshot)?;
        println!("{}", path.display());
        Ok(())
    }
}
