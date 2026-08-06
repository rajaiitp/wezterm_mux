use crate::cli::wait::pane_exists;
use clap::Parser;
use mux::pane::PaneId;
use regex::Regex;
use smol::Timer;
use std::time::{Duration, Instant};
use wezterm_client::client::Client;
use wezterm_term::StableRowIndex;

/// Poll a pane's recent output until a literal string or regular expression
/// matches.
#[derive(Debug, Parser, Clone)]
pub struct WatchCommand {
    /// Specify the target pane. The default is based on WEZTERM_PANE.
    #[arg(long)]
    pane_id: Option<PaneId>,

    /// Text to find, or a regular expression when --regex is used.
    pattern: String,

    /// Interpret PATTERN as a regular expression.
    #[arg(long)]
    regex: bool,

    /// Number of recent lines to inspect on each poll.
    #[arg(long, default_value_t = 200)]
    lines: usize,

    /// Fail if the pattern has not appeared within this many seconds.
    #[arg(long)]
    timeout: Option<u64>,

    /// Polling interval in milliseconds.
    #[arg(long, default_value_t = 100)]
    poll_interval_ms: u64,
}

impl WatchCommand {
    pub async fn run(self, client: Client) -> anyhow::Result<()> {
        let pane_id = client.resolve_pane_id(self.pane_id).await?;
        let matcher = if self.regex {
            Some(Regex::new(&self.pattern)?)
        } else {
            None
        };
        let started = Instant::now();
        let poll_interval = Duration::from_millis(self.poll_interval_ms.max(10));
        let timeout = self.timeout.map(Duration::from_secs);

        loop {
            let panes = client.list_panes().await?;
            if !pane_exists(&panes, pane_id) {
                anyhow::bail!("pane {pane_id} disappeared while watching");
            }

            let text = read_recent_text(&client, pane_id, self.lines).await?;
            let matched = match matcher.as_ref() {
                Some(regex) => regex.is_match(&text),
                None => text.contains(&self.pattern),
            };
            if matched {
                print!("{text}");
                return Ok(());
            }

            if let Some(timeout) = timeout {
                if started.elapsed() >= timeout {
                    anyhow::bail!("timed out waiting for {:?} in pane {pane_id}", self.pattern);
                }
            }

            Timer::after(poll_interval).await;
        }
    }
}

async fn read_recent_text(
    client: &Client,
    pane_id: PaneId,
    max_lines: usize,
) -> anyhow::Result<String> {
    let dimensions = client
        .get_dimensions(codec::GetPaneRenderableDimensions { pane_id })
        .await?
        .dimensions;
    let end = dimensions.physical_top + dimensions.viewport_rows as StableRowIndex;
    let start = if max_lines == 0 {
        end
    } else {
        dimensions
            .scrollback_top
            .max(end.saturating_sub(max_lines as StableRowIndex))
    };

    let response = client
        .get_lines(codec::GetLines {
            pane_id: pane_id.into(),
            lines: vec![start..end],
        })
        .await?;

    let (lines, _) = response.lines.extract_data();
    let mut text = String::new();
    for (_, line) in lines {
        text.push_str(line.as_str().as_ref());
        text.push('\n');
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use regex::Regex;

    #[test]
    fn literal_and_regex_matching_are_distinct() {
        let output = "ready: pane 42";
        assert!(output.contains("ready:"));
        assert!(!output.contains("ready.*42"));
        assert!(Regex::new(r"ready: pane \d+").unwrap().is_match(output));
    }
}
