use crate::cli::session_state::{resolve_path, write_atomic, SessionSnapshot};
use clap::{Parser, ValueHint};
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
        let snapshot = SessionSnapshot::new(client.list_panes().await?);
        write_atomic(&path, &snapshot)?;
        println!("{}", path.display());
        Ok(())
    }
}
