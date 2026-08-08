use anyhow::{anyhow, Context};
use clap::Args;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

#[derive(Debug, Args, Clone)]
pub struct AutomationCall {
    /// Native automation socket. Defaults to WEZTERM_AUTOMATION_SOCKET or the
    /// .automation sibling of WEZTERM_UNIX_SOCKET.
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Registered automation client ID to call.
    #[arg(long)]
    client_id: String,

    /// Callback method, for example agent.prompt or agent.abort.
    #[arg(long)]
    method: String,

    /// JSON object passed to the callback.
    #[arg(long, default_value = "{}")]
    params: String,
}

impl AutomationCall {
    pub async fn run(self) -> anyhow::Result<()> {
        #[cfg(unix)]
        {
            let socket_path = self.socket.or_else(default_socket_path).ok_or_else(|| {
                anyhow!(
                    "automation socket is not configured; pass --socket or set WEZTERM_AUTOMATION_SOCKET"
                )
            })?;
            let params: Value = serde_json::from_str(&self.params).context("parsing --params")?;
            anyhow::ensure!(params.is_object(), "--params must be a JSON object");

            let stream = std::os::unix::net::UnixStream::connect(&socket_path)
                .with_context(|| format!("connecting to {}", socket_path.display()))?;
            let mut writer = stream.try_clone()?;
            let mut reader = BufReader::new(stream);
            let mut next_id = 1u64;

            write_request(
                &mut writer,
                next_id,
                "automation.hello",
                json!({
                    "protocolVersion": 1,
                    "client": { "name": "wezterm-cli", "version": config::wezterm_version() },
                    "requestedCapabilities": ["client.callback"]
                }),
            )?;
            read_response(&mut reader, next_id)?;
            next_id += 1;

            write_request(
                &mut writer,
                next_id,
                "client.call",
                json!({
                    "targetClientId": self.client_id,
                    "method": self.method,
                    "params": params
                }),
            )?;
            let response = read_response(&mut reader, next_id)?;
            println!("{}", serde_json::to_string_pretty(&response)?);
            Ok(())
        }

        #[cfg(not(unix))]
        {
            let _ = self;
            anyhow::bail!("automation-call is not implemented on this platform")
        }
    }
}

#[cfg(unix)]
fn default_socket_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("WEZTERM_AUTOMATION_SOCKET") {
        return Some(path.into());
    }
    let mux_socket = std::env::var_os("WEZTERM_UNIX_SOCKET")?;
    let mut path = PathBuf::from(mux_socket);
    let name = path.file_name()?.to_str()?.to_string();
    path.set_file_name(format!("{name}.automation"));
    Some(path)
}

#[cfg(unix)]
fn write_request<W: Write>(
    writer: &mut W,
    id: u64,
    method: &str,
    params: Value,
) -> anyhow::Result<()> {
    let request = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params
    });
    writeln!(writer, "{}", serde_json::to_string(&request)?)?;
    writer.flush()?;
    Ok(())
}

#[cfg(unix)]
fn read_response<R: BufRead>(reader: &mut R, id: u64) -> anyhow::Result<Value> {
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            anyhow::bail!("automation server disconnected")
        }
        let value: Value = serde_json::from_str(line.trim_end())?;
        if value.get("id").and_then(Value::as_u64) == Some(id) {
            if let Some(error) = value.get("error") {
                anyhow::bail!("automation request failed: {error}")
            }
            return Ok(value.get("result").cloned().unwrap_or(Value::Null));
        }
    }
}
