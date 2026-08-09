//! Native, line-delimited JSON-RPC automation API for trusted local clients.
//!
//! The endpoint is deliberately separate from the binary mux protocol.  It is
//! intended for typed automation clients (such as the Pi extension), while the
//! existing mux socket remains responsible for terminal rendering and client
//! synchronization.

use anyhow::{anyhow, Context};
use config::UnixDomain;
use mux::domain::SplitSource;
use mux::pane::{CachePolicy, PaneId};
use mux::tab::{SplitDirection, SplitRequest, SplitSize, TabId};
use mux::window::WindowId;
use mux::{Mux, MuxNotification};
use portable_pty::CommandBuilder;
use promise::spawn::spawn_into_main_thread;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString, OsString};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, RawFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;
use wezterm_term::StableRowIndex;
use wezterm_uds::UnixStream;

pub const PROTOCOL_VERSION: u32 = 1;
const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_TEXT_BYTES: usize = 256 * 1024;
const MAIN_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_MANAGED_PANES: usize = 3;
const MAX_COMMAND_OUTPUT_LINES: usize = 4096;
const PROMPT_MARKER_PREFIX: &str = "PI_WEZTERM_PROMPT_";

#[derive(Clone, Debug)]
struct ManagedCommand {
    pane_id: PaneId,
    owner_client_id: Option<String>,
    running: bool,
    exit_code: Option<i32>,
    signal: Option<String>,
    success: Option<bool>,
    reason: Option<String>,
    output_start_y: StableRowIndex,
    output_start_x: usize,
    captured_output: Option<String>,
    output_truncated: bool,
}

struct ManagedShellSetup {
    command: CommandBuilder,
    callback_fifo: PathBuf,
    cleanup_dir: PathBuf,
}

#[derive(Debug, Deserialize)]
struct RpcRequest {
    #[serde(default)]
    jsonrpc: Option<String>,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Option<Value>,
}

#[derive(Debug, Serialize)]
struct RpcResponse {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

#[derive(Debug, Serialize)]
struct RpcError {
    code: i32,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

#[derive(Debug, Serialize)]
struct EventNotification {
    jsonrpc: &'static str,
    method: &'static str,
    params: Value,
}

#[derive(Debug, Clone)]
struct ConnectionState {
    authenticated: bool,
    client_id: Option<String>,
    origin_pane: Option<PaneId>,
    subscribed: bool,
    subscription_sender: Option<Arc<mpsc::SyncSender<String>>>,
}

impl Default for ConnectionState {
    fn default() -> Self {
        Self {
            authenticated: false,
            client_id: None,
            origin_pane: None,
            subscribed: false,
            subscription_sender: None,
        }
    }
}

lazy_static::lazy_static! {
    static ref REVISION: AtomicU64 = AtomicU64::new(1);
    static ref INSTANCE_ID: String = format!(
        "{}-{}",
        hostname::get().ok().and_then(|h| h.into_string().ok()).unwrap_or_else(|| "localhost".to_string()),
        std::process::id()
    );
    static ref CLIENTS: Mutex<HashMap<String, mpsc::SyncSender<String>>> = Mutex::new(HashMap::new());
    static ref CLIENT_STATES: Mutex<HashMap<String, Value>> = Mutex::new(HashMap::new());
    static ref CLIENT_ORIGINS: Mutex<HashMap<String, Option<PaneId>>> = Mutex::new(HashMap::new());
    static ref MANAGED_PANES: Mutex<HashMap<PaneId, Vec<PaneId>>> = Mutex::new(HashMap::new());
    static ref MANAGED_COMMANDS: Mutex<HashMap<String, ManagedCommand>> = Mutex::new(HashMap::new());
    // A physical pane can be reused, but only its latest command ID may
    // control it. Older IDs retain immutable output and status records.
    static ref MANAGED_PANE_OWNERS: Mutex<HashMap<PaneId, String>> = Mutex::new(HashMap::new());
    static ref PROMPT_MARKERS: (Mutex<HashMap<PaneId, u64>>, Condvar) =
        (Mutex::new(HashMap::new()), Condvar::new());
    static ref SHELL_READY: (Mutex<HashSet<PaneId>>, Condvar) = (Mutex::new(HashSet::new()), Condvar::new());
    static ref SHELL_SEQUENCE: AtomicU64 = AtomicU64::new(1);
}

/// Derive a sibling endpoint from the configured mux socket.
pub fn socket_path(unix_domain: &UnixDomain) -> PathBuf {
    let mux_socket = unix_domain.socket_path();
    let file_name = mux_socket
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("wezterm-mux.sock");
    mux_socket.with_file_name(format!("{file_name}.automation"))
}

/// Bind and serve an automation endpoint for one mux domain.
pub fn spawn_listener(unix_domain: &UnixDomain) -> anyhow::Result<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let path = socket_path(unix_domain);
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("automation socket has no parent"))?;
        config::create_user_owned_dirs(parent)?;

        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err).with_context(|| format!("remove {}", path.display())),
        }

        let listener = std::os::unix::net::UnixListener::bind(&path)
            .with_context(|| format!("bind automation socket {}", path.display()))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;

        thread::Builder::new()
            .name("wezterm-automation-listener".to_string())
            .spawn(move || {
                for stream in listener.incoming() {
                    match stream {
                        Ok(stream) => {
                            thread::Builder::new()
                                .name("wezterm-automation-client".to_string())
                                .spawn(move || {
                                    let stream = UnixStream::from_std(stream);
                                    if let Err(err) = handle_connection(stream) {
                                        log::debug!("automation client ended: {err:#}");
                                    }
                                })
                                .ok();
                        }
                        Err(err) => {
                            log::error!("automation accept failed: {err}");
                            break;
                        }
                    }
                }
            })?;

        log::info!("automation API listening on {}", path.display());
        Ok(path)
    }

    #[cfg(not(unix))]
    {
        let _ = unix_domain;
        anyhow::bail!("the native automation endpoint is not implemented on this platform")
    }
}

#[cfg(unix)]
trait UnixStreamExt {
    fn from_std(stream: std::os::unix::net::UnixStream) -> UnixStream;
}

#[cfg(unix)]
impl UnixStreamExt for UnixStream {
    fn from_std(stream: std::os::unix::net::UnixStream) -> UnixStream {
        use std::os::fd::FromRawFd;
        use std::os::fd::IntoRawFd;
        // The wrapper intentionally has no public constructor because it is
        // normally created by wezterm-uds::UnixStream::connect.  Reconstitute
        // it through its platform-neutral raw-fd implementation.
        unsafe { UnixStream::from_raw_fd(stream.into_raw_fd()) }
    }
}

#[cfg(unix)]
fn handle_connection(stream: UnixStream) -> anyhow::Result<()> {
    use std::os::unix::net::UnixStream as StdUnixStream;

    let writer_stream: StdUnixStream = stream.try_clone()?;
    let (out_tx, out_rx) = mpsc::sync_channel::<String>(256);
    let writer = thread::Builder::new()
        .name("wezterm-automation-writer".to_string())
        .spawn(move || {
            let mut writer = writer_stream;
            while let Ok(line) = out_rx.recv() {
                if writer.write_all(line.as_bytes()).is_err() {
                    break;
                }
                if writer.write_all(b"\n").is_err() || writer.flush().is_err() {
                    break;
                }
            }
        })?;

    let mut reader = BufReader::new(stream);
    let mut state = ConnectionState::default();
    let mut line = Vec::new();

    loop {
        line.clear();
        let bytes = reader
            .by_ref()
            .take((MAX_REQUEST_BYTES + 1) as u64)
            .read_until(b'\n', &mut line)?;
        if bytes == 0 {
            break;
        }
        if bytes > MAX_REQUEST_BYTES || !line.ends_with(b"\n") {
            send_error(&out_tx, Value::Null, -32600, "request is too large", None);
            break;
        }

        let line = std::str::from_utf8(&line).context("request is not UTF-8")?;
        let request: RpcRequest = match serde_json::from_str(line.trim_end()) {
            Ok(request) => request,
            Err(err) => {
                send_error(
                    &out_tx,
                    Value::Null,
                    -32700,
                    "invalid JSON request",
                    Some(json!({ "detail": err.to_string() })),
                );
                continue;
            }
        };
        let id = request.id.clone().unwrap_or(Value::Null);
        if let Err(err) = dispatch_request(request, &mut state, &out_tx) {
            send_error(
                &out_tx,
                id,
                -32000,
                &err.to_string(),
                Some(json!({ "kind": "server_error" })),
            );
        }
    }

    if let Some(client_id) = state.client_id {
        CLIENTS.lock().unwrap().remove(&client_id);
        CLIENT_STATES.lock().unwrap().remove(&client_id);
        CLIENT_ORIGINS.lock().unwrap().remove(&client_id);
    }
    drop(out_tx);
    writer.join().ok();
    Ok(())
}

#[cfg(not(unix))]
fn handle_connection(_stream: UnixStream) -> anyhow::Result<()> {
    anyhow::bail!("the native automation endpoint is not implemented on this platform")
}

fn dispatch_request(
    request: RpcRequest,
    state: &mut ConnectionState,
    out: &mpsc::SyncSender<String>,
) -> anyhow::Result<()> {
    let id = request.id.clone().unwrap_or(Value::Null);
    if request.jsonrpc.as_deref().is_some_and(|v| v != "2.0") {
        send_error(out, id, -32600, "jsonrpc must be 2.0", None);
        return Ok(());
    }

    if request.method == "automation.hello" {
        let params = object_params(request.params)?;
        let requested_version = params
            .get("protocolVersion")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;
        if requested_version != PROTOCOL_VERSION {
            send_error(
                out,
                id,
                -32001,
                "unsupported automation protocol version",
                Some(json!({ "supportedVersions": [PROTOCOL_VERSION] })),
            );
            return Ok(());
        }

        let client = params
            .get("client")
            .and_then(Value::as_object)
            .and_then(|client| client.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let client_id = format!(
            "{}:{}:{}",
            client,
            std::process::id(),
            REVISION.fetch_add(1, Ordering::Relaxed)
        );
        let origin_pane = params
            .get("origin")
            .and_then(Value::as_object)
            .and_then(|origin| origin.get("paneId"))
            .and_then(Value::as_u64)
            .map(|id| id as PaneId);

        state.authenticated = true;
        state.client_id = Some(client_id.clone());
        state.origin_pane = origin_pane;
        CLIENTS
            .lock()
            .unwrap()
            .insert(client_id.clone(), out.clone());
        CLIENT_STATES
            .lock()
            .unwrap()
            .insert(client_id.clone(), json!({ "phase": "connected" }));
        CLIENT_ORIGINS
            .lock()
            .unwrap()
            .insert(client_id.clone(), origin_pane);

        // Panel creation is intentionally demand-driven: hello only binds Pi
        // to its origin pane. `command.run` creates the first side panel when
        // Pi actually needs to run something.
        let managed_panel = Value::Null;

        send_result(
            out,
            id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "connectionId": client_id,
                "muxInstanceId": &*INSTANCE_ID,
                "epoch": std::process::id(),
                "originPaneId": origin_pane,
                "managedPanel": managed_panel,
                "grantedCapabilities": capabilities(),
                "features": {
                    "topologySnapshot": true,
                    "topologySubscription": true,
                    "paneText": true,
                    "semanticZones": true,
                    "paneControl": true,
                    "managedCommands": true,
                    "managedPanel": true,
                    "managedPaneLimit": MAX_MANAGED_PANES,
                    "clientCallbacks": true,
                    "sshDomains": true,
                    "sshRelay": "native-mux-domain"
                },
                "revision": REVISION.load(Ordering::Relaxed)
            }),
        );
        return Ok(());
    }

    if !state.authenticated {
        send_error(out, id, -32002, "automation.hello is required", None);
        return Ok(());
    }

    match request.method.as_str() {
        "context.get" => send_result(out, id, context_snapshot(state.origin_pane)?),
        "topology.snapshot" => send_result(out, id, topology_snapshot()?),
        "topology.subscribe" => {
            subscribe_topology(out.clone(), state)?;
            send_result(
                out,
                id,
                json!({ "subscribed": true, "revision": current_revision() }),
            );
        }
        "pane.get" => {
            let params = object_params(request.params)?;
            let pane_id = pane_id(&params)?;
            send_result(out, id, pane_snapshot(pane_id)?);
        }
        "pane.readText" => {
            let params = object_params(request.params)?;
            let pane_id = pane_id(&params)?;
            let start = params.get("start").and_then(Value::as_i64);
            let end = params.get("end").and_then(Value::as_i64);
            send_result(out, id, pane_text(pane_id, start, end)?)
        }
        "pane.getSemanticZones" => {
            let params = object_params(request.params)?;
            let pane_id = pane_id(&params)?;
            send_result(out, id, semantic_zones(pane_id)?)
        }
        "client.list" => {
            let clients = CLIENTS
                .lock()
                .unwrap()
                .keys()
                .cloned()
                .map(|client_id| {
                    let state = CLIENT_STATES
                        .lock()
                        .unwrap()
                        .get(&client_id)
                        .cloned()
                        .unwrap_or(Value::Null);
                    json!({ "clientId": client_id, "state": state })
                })
                .collect::<Vec<_>>();
            send_result(out, id, json!({ "clients": clients }))
        }
        "client.setState" => {
            let params = request.params.unwrap_or(Value::Null);
            let client_id = state
                .client_id
                .clone()
                .ok_or_else(|| anyhow!("client is not registered"))?;
            CLIENT_STATES.lock().unwrap().insert(client_id, params);
            send_result(out, id, json!({ "updated": true }))
        }
        "client.call" => {
            let params = object_params(request.params)?;
            let target = params
                .get("targetClientId")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("targetClientId is required"))?;
            let target = if target == "origin-pane" {
                let origin_pane = params
                    .get("originPaneId")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| anyhow!("originPaneId is required for origin-pane"))?
                    as PaneId;
                CLIENT_ORIGINS
                    .lock()
                    .unwrap()
                    .iter()
                    .find_map(|(client_id, origin)| {
                        (*origin == Some(origin_pane)).then_some(client_id.clone())
                    })
                    .ok_or_else(|| {
                        anyhow!("no automation client is attached to pane {origin_pane}")
                    })?
            } else {
                target.to_string()
            };
            let method = params
                .get("method")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("method is required"))?;
            let callback_id = format!("callback:{}", REVISION.fetch_add(1, Ordering::Relaxed));
            let target_tx = CLIENTS
                .lock()
                .unwrap()
                .get(&target)
                .cloned()
                .ok_or_else(|| anyhow!("client {target} is not connected"))?;
            let notification = EventNotification {
                jsonrpc: "2.0",
                method: "client.call",
                params: json!({
                    "callbackId": callback_id,
                    "method": method,
                    "params": params.get("params").cloned().unwrap_or(Value::Null),
                    "sourceClientId": state.client_id
                }),
            };
            let line = serde_json::to_string(&notification)?;
            target_tx
                .try_send(line)
                .map_err(|_| anyhow!("target client output queue is full"))?;
            send_result(
                out,
                id,
                json!({ "accepted": true, "callbackId": callback_id }),
            )
        }
        "pane.sendText" | "pane.focus" | "pane.close" | "pane.setZoomed" | "pane.split"
        | "command.run" | "command.cancel" | "command.input" | "command.close" | "panel.ensure"
        | "workspace.rename" | "tab.focus" => {
            let method = request.method;
            let mut params = request.params.unwrap_or_else(|| json!({}));
            if method == "command.run" {
                if let (Some(client_id), Some(object)) =
                    (state.client_id.clone(), params.as_object_mut())
                {
                    // Completion callbacks are delivered only to the client
                    // that launched the command, not every Pi pane.
                    object.insert("ownerClientId".to_string(), json!(client_id));
                }
            }
            let result = run_mutation_on_main(method, params);
            match result {
                Ok(value) => send_result(out, id, value),
                Err(err) => send_error(out, id, -32010, &err.to_string(), None),
            }
        }
        "command.read" => {
            let params = object_params(request.params)?;
            let command_id = managed_command_id(&params)?;
            let record = MANAGED_COMMANDS
                .lock()
                .unwrap()
                .get(&command_id)
                .cloned()
                .ok_or_else(|| anyhow!("command {command_id} not found"))?;
            let start = params.get("start").and_then(Value::as_i64);
            let end = params.get("end").and_then(Value::as_i64);
            let (output, range_start, range_end, truncated) =
                read_managed_command_output(&record, start, end)?;
            send_result(
                out,
                id,
                json!({
                    "commandId": command_id,
                    "output": output,
                    "start": range_start,
                    "end": range_end,
                    "running": record.running,
                    "truncated": truncated
                }),
            )
        }
        "command.getResult" => {
            let params = object_params(request.params)?;
            let command_id = managed_command_id(&params)?;
            let record = MANAGED_COMMANDS
                .lock()
                .unwrap()
                .get(&command_id)
                .cloned()
                .ok_or_else(|| anyhow!("command {command_id} not found"))?;
            send_result(out, id, managed_command_result(&command_id, &record))
        }
        "client.callback_result" => {}
        "automation.ping" => send_result(
            out,
            id,
            json!({ "ok": true, "revision": current_revision() }),
        ),
        _ => send_error(
            out,
            id,
            -32601,
            "method not found",
            Some(json!({ "method": request.method })),
        ),
    }

    Ok(())
}

fn capabilities() -> Vec<&'static str> {
    vec![
        "topology.read",
        "pane.read",
        "pane.output.subscribe",
        "pane.focus",
        "pane.input.text",
        "pane.create",
        "pane.layout",
        "pane.close",
        "panel.manage",
        "tab.control",
        "workspace.control",
        "command.run",
        "command.cancel",
        "command.input",
        "command.close",
        "client.callback",
    ]
}

fn object_params(params: Option<Value>) -> anyhow::Result<Map<String, Value>> {
    match params.unwrap_or_else(|| json!({})) {
        Value::Object(value) => Ok(value),
        _ => anyhow::bail!("params must be an object"),
    }
}

fn pane_id(params: &Map<String, Value>) -> anyhow::Result<PaneId> {
    params
        .get("paneId")
        .and_then(Value::as_u64)
        .map(|id| id as PaneId)
        .ok_or_else(|| anyhow!("paneId is required"))
}

fn managed_command_id(params: &Map<String, Value>) -> anyhow::Result<String> {
    if let Some(command_id) = params.get("commandId").and_then(Value::as_str) {
        return Ok(command_id.to_string());
    }
    let pane_id = params
        .get("paneId")
        .and_then(Value::as_u64)
        .map(|id| id as PaneId)
        .ok_or_else(|| anyhow!("commandId or paneId is required"))?;
    MANAGED_COMMANDS
        .lock()
        .unwrap()
        .iter()
        .find(|(_, command)| command.pane_id == pane_id && command.running)
        .map(|(command_id, _)| command_id.clone())
        .ok_or_else(|| anyhow!("no running command is attached to pane {pane_id}"))
}

fn managed_command_result(command_id: &str, command: &ManagedCommand) -> Value {
    json!({
        "commandId": command_id,
        "running": command.running,
        "exitCode": command.exit_code,
        "signal": command.signal,
        "success": command.success,
        "reason": command.reason,
    })
}

fn ensure_command_owns_pane(command_id: &str, command: &ManagedCommand) -> anyhow::Result<()> {
    let owners = MANAGED_PANE_OWNERS.lock().unwrap();
    anyhow::ensure!(
        owners
            .get(&command.pane_id)
            .is_some_and(|owner| owner == command_id),
        "command {command_id} no longer owns its managed pane"
    );
    Ok(())
}

fn clear_command_pane_owner(command_id: &str, pane_id: PaneId) {
    let mut owners = MANAGED_PANE_OWNERS.lock().unwrap();
    if owners
        .get(&pane_id)
        .is_some_and(|owner| owner == command_id)
    {
        owners.remove(&pane_id);
    }
}

fn send_result(out: &mpsc::SyncSender<String>, id: Value, result: Value) {
    let response = RpcResponse {
        jsonrpc: "2.0",
        id,
        result: Some(result),
        error: None,
    };
    if let Ok(line) = serde_json::to_string(&response) {
        out.send(line).ok();
    }
}

fn send_error(
    out: &mpsc::SyncSender<String>,
    id: Value,
    code: i32,
    message: &str,
    data: Option<Value>,
) {
    let response = RpcResponse {
        jsonrpc: "2.0",
        id,
        result: None,
        error: Some(RpcError {
            code,
            message: message.to_string(),
            data,
        }),
    };
    if let Ok(line) = serde_json::to_string(&response) {
        out.send(line).ok();
    }
}

fn current_revision() -> u64 {
    REVISION.load(Ordering::Relaxed)
}

fn subscribe_topology(
    out: mpsc::SyncSender<String>,
    state: &mut ConnectionState,
) -> anyhow::Result<()> {
    if state.subscribed {
        return Ok(());
    }
    state.subscribed = true;
    // The mux subscriber keeps a weak sender so disconnecting a client can
    // release its writer thread immediately, even if the mux is idle and no
    // later notification arrives to prune the subscriber.
    let sender = Arc::new(out);
    let weak_sender = Arc::downgrade(&sender);
    state.subscription_sender = Some(sender);
    Mux::get().subscribe(move |notification| {
        let Some(sender) = weak_sender.upgrade() else {
            return false;
        };
        let revision = REVISION.fetch_add(1, Ordering::Relaxed) + 1;
        let event = notification_to_event(notification, revision);
        let Ok(line) = serde_json::to_string(&event) else {
            return false;
        };
        sender.try_send(line).is_ok()
    });
    Ok(())
}

fn notification_to_event(notification: MuxNotification, revision: u64) -> EventNotification {
    let (kind, data) = match notification {
        MuxNotification::PaneOutput(pane_id) => ("pane.output", json!({ "paneId": pane_id })),
        MuxNotification::PaneExited(pane_id, status) => (
            "pane.exited",
            json!({
                "paneId": pane_id,
                "exitCode": status.exit_code(),
                "signal": status.signal(),
                "success": status.success()
            }),
        ),
        MuxNotification::PaneAdded(pane_id) => ("pane.added", json!({ "paneId": pane_id })),
        MuxNotification::PaneRemoved(pane_id) => ("pane.removed", json!({ "paneId": pane_id })),
        MuxNotification::PaneFocused(pane_id) => ("pane.focused", json!({ "paneId": pane_id })),
        MuxNotification::TabAddedToWindow { tab_id, window_id } => (
            "tab.added",
            json!({ "tabId": tab_id, "windowId": window_id }),
        ),
        MuxNotification::TabResized(tab_id) => ("tab.resized", json!({ "tabId": tab_id })),
        MuxNotification::TabTitleChanged { tab_id, title } => (
            "tab.titleChanged",
            json!({ "tabId": tab_id, "title": title }),
        ),
        MuxNotification::WindowCreated(window_id) => {
            ("window.added", json!({ "windowId": window_id }))
        }
        MuxNotification::WindowRemoved(window_id) => {
            ("window.removed", json!({ "windowId": window_id }))
        }
        MuxNotification::WindowInvalidated(window_id) => {
            ("window.changed", json!({ "windowId": window_id }))
        }
        MuxNotification::WindowWorkspaceChanged(window_id) => {
            ("window.workspaceChanged", json!({ "windowId": window_id }))
        }
        MuxNotification::WindowTitleChanged { window_id, title } => (
            "window.titleChanged",
            json!({ "windowId": window_id, "title": title }),
        ),
        MuxNotification::WorkspaceRenamed {
            old_workspace,
            new_workspace,
        } => (
            "workspace.renamed",
            json!({ "old": old_workspace, "new": new_workspace }),
        ),
        MuxNotification::ActiveWorkspaceChanged(_) => ("workspace.focused", json!({})),
        MuxNotification::Alert { pane_id, .. } => ("pane.alert", json!({ "paneId": pane_id })),
        MuxNotification::AssignClipboard { pane_id, .. } => {
            ("clipboard.changed", json!({ "paneId": pane_id }))
        }
        MuxNotification::SaveToDownloads { .. } | MuxNotification::Empty => {
            ("mux.changed", json!({}))
        }
    };
    EventNotification {
        jsonrpc: "2.0",
        method: "automation.event",
        params: json!({ "event": kind, "revision": revision, "data": data }),
    }
}

fn context_snapshot(origin_pane: Option<PaneId>) -> anyhow::Result<Value> {
    let snapshot = topology_snapshot()?;
    let focused = origin_pane.and_then(|pane_id| pane_snapshot(pane_id).ok());
    Ok(json!({
        "originPaneId": origin_pane,
        "focusedPane": focused,
        "topology": snapshot
    }))
}

fn topology_snapshot() -> anyhow::Result<Value> {
    let mux = Mux::get();
    let domains = mux
        .iter_domains()
        .into_iter()
        .map(|domain| {
            json!({
                "domainId": domain.domain_id(),
                "name": domain.domain_name(),
                "label": domain.domain_name(),
                "state": format!("{:?}", domain.state()),
                "spawnable": domain.spawnable(),
                "detachable": domain.detachable()
            })
        })
        .collect::<Vec<_>>();

    let mut windows = Vec::new();
    for window_id in mux.iter_windows() {
        let Some(window) = mux.get_window(window_id) else {
            continue;
        };
        let tabs = window
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                let panes = tab
                    .iter_panes_ignoring_zoom()
                    .into_iter()
                    .map(|position| pane_position_snapshot(window_id, tab.tab_id(), position))
                    .collect::<Vec<_>>();
                json!({
                    "tabId": tab.tab_id(),
                    "index": index,
                    "active": index == window.get_active_idx(),
                    "title": tab.get_title(),
                    "size": terminal_size(tab.get_size()),
                    "panes": panes
                })
            })
            .collect::<Vec<_>>();
        windows.push(json!({
            "windowId": window_id,
            "workspace": window.get_workspace(),
            "title": window.get_title(),
            "activeTabIndex": window.get_active_idx(),
            "tabs": tabs
        }));
    }

    Ok(json!({
        "muxInstanceId": &*INSTANCE_ID,
        "epoch": std::process::id(),
        "revision": current_revision(),
        "activeWorkspace": mux.active_workspace(),
        "workspaces": mux.iter_workspaces(),
        "domains": domains,
        "windows": windows
    }))
}

fn pane_position_snapshot(
    window_id: WindowId,
    tab_id: TabId,
    position: mux::tab::PositionedPane,
) -> Value {
    let pane = position.pane;
    json!({
        "paneId": pane.pane_id(),
        "windowId": window_id,
        "tabId": tab_id,
        "domainId": pane.domain_id(),
        "active": position.is_active,
        "zoomed": position.is_zoomed,
        "left": position.left,
        "top": position.top,
        "width": position.width,
        "height": position.height,
        "title": pane.get_title(),
        "cwd": pane.get_current_working_dir(CachePolicy::AllowStale).map(|url| url.to_string()),
        "foregroundProcess": pane.get_foreground_process_name(CachePolicy::AllowStale),
        "dead": pane.is_dead()
    })
}

fn pane_snapshot(pane_id: PaneId) -> anyhow::Result<Value> {
    let mux = Mux::get();
    let pane = mux
        .get_pane(pane_id)
        .ok_or_else(|| anyhow!("pane {pane_id} not found"))?;
    let (domain_id, window_id, tab_id) = mux
        .resolve_pane_id(pane_id)
        .ok_or_else(|| anyhow!("pane {pane_id} has no topology entry"))?;
    let dims = pane.get_dimensions();
    Ok(json!({
        "paneId": pane_id,
        "windowId": window_id,
        "tabId": tab_id,
        "domainId": domain_id,
        "title": pane.get_title(),
        "cwd": pane.get_current_working_dir(CachePolicy::AllowStale).map(|url| url.to_string()),
        "foregroundProcess": pane.get_foreground_process_name(CachePolicy::AllowStale),
        "dimensions": {
            "cols": dims.cols,
            "rows": dims.viewport_rows,
            "scrollbackRows": dims.scrollback_rows,
            "scrollbackTop": dims.scrollback_top,
            "physicalTop": dims.physical_top,
            "dpi": dims.dpi
        },
        "cursor": pane.get_cursor_position(),
        "seqno": pane.get_current_seqno(),
        "altScreen": pane.is_alt_screen_active(),
        "dead": pane.is_dead(),
        "metadata": format!("{:?}", pane.get_metadata())
    }))
}

fn pane_text(pane_id: PaneId, start: Option<i64>, end: Option<i64>) -> anyhow::Result<Value> {
    let pane = Mux::get()
        .get_pane(pane_id)
        .ok_or_else(|| anyhow!("pane {pane_id} not found"))?;
    let dims = pane.get_dimensions();
    // A command's response buffer is the recent retained scrollback, not
    // merely whatever happens to be visible in the pane. Explicit ranges can
    // still be used for older or very large output.
    let default_end = dims.physical_top + dims.viewport_rows as StableRowIndex;
    let end = end.unwrap_or(default_end as i64) as StableRowIndex;
    let start = start
        .map(|value| value as StableRowIndex)
        .unwrap_or_else(|| end.saturating_sub(4096).max(dims.scrollback_top));
    let end = end.max(start);
    let span = (end - start).max(0) as usize;
    anyhow::ensure!(span <= 4096, "requested text range is too large");
    let (first, lines) = pane.get_lines(start..end);
    let mut text = String::new();
    for (index, line) in lines.into_iter().enumerate() {
        if index > 0 {
            text.push('\n');
        }
        for cell in line.visible_cells() {
            text.push_str(cell.str());
        }
        if text.len() > MAX_TEXT_BYTES {
            text.truncate(MAX_TEXT_BYTES);
            break;
        }
    }
    Ok(json!({ "paneId": pane_id, "start": first, "end": end, "text": text }))
}

fn capture_managed_command_output(command: &ManagedCommand) -> anyhow::Result<(String, bool)> {
    if let Some(output) = &command.captured_output {
        return Ok((output.clone(), command.output_truncated));
    }

    let pane = Mux::get()
        .get_pane(command.pane_id)
        .ok_or_else(|| anyhow!("command pane is no longer available"))?;
    let dims = pane.get_dimensions();
    let cursor = pane.get_cursor_position();
    let mut start = command.output_start_y.max(dims.scrollback_top);
    let mut truncated = start > command.output_start_y;
    let end = cursor.y.saturating_add(1).max(start.saturating_add(1));
    let earliest_retained = end.saturating_sub(MAX_COMMAND_OUTPUT_LINES as StableRowIndex);
    if start < earliest_retained {
        start = earliest_retained;
        truncated = true;
    }

    let (first, lines) = pane.get_lines(start..end);
    if first > start {
        truncated = true;
    }
    let mut text = String::new();
    for (index, line) in lines.into_iter().enumerate() {
        if index > 0 {
            text.push('\n');
        }
        let stable_row = first + index as StableRowIndex;
        let first_column = if stable_row == command.output_start_y {
            command.output_start_x
        } else {
            0
        };
        let mut rendered = String::new();
        for cell in line
            .visible_cells()
            .skip_while(|cell| cell.cell_index() < first_column)
        {
            rendered.push_str(cell.str());
        }
        text.push_str(rendered.trim_end());
        if text.len() > MAX_TEXT_BYTES {
            let mut boundary = MAX_TEXT_BYTES;
            while !text.is_char_boundary(boundary) {
                boundary -= 1;
            }
            text.truncate(boundary);
            truncated = true;
            break;
        }
    }

    Ok((text.trim_end_matches('\n').to_string(), truncated))
}

fn slice_command_output(
    output: &str,
    start: Option<i64>,
    end: Option<i64>,
) -> anyhow::Result<(String, usize, usize)> {
    anyhow::ensure!(
        start.is_none_or(|value| value >= 0),
        "start must be non-negative"
    );
    anyhow::ensure!(
        end.is_none_or(|value| value >= 0),
        "end must be non-negative"
    );
    let lines = if output.is_empty() {
        Vec::new()
    } else {
        output.split('\n').collect::<Vec<_>>()
    };
    let end = end.map(|value| value as usize).unwrap_or(lines.len());
    let start = start
        .map(|value| value as usize)
        .unwrap_or_else(|| end.saturating_sub(MAX_COMMAND_OUTPUT_LINES));
    anyhow::ensure!(start <= end, "start must not exceed end");
    anyhow::ensure!(
        end.saturating_sub(start) <= MAX_COMMAND_OUTPUT_LINES,
        "requested output range is too large"
    );
    let bounded_start = start.min(lines.len());
    let bounded_end = end.min(lines.len()).max(bounded_start);
    Ok((
        lines[bounded_start..bounded_end].join("\n"),
        bounded_start,
        bounded_end,
    ))
}

fn read_managed_command_output(
    command: &ManagedCommand,
    start: Option<i64>,
    end: Option<i64>,
) -> anyhow::Result<(String, usize, usize, bool)> {
    let (output, truncated) = capture_managed_command_output(command)?;
    let (output, start, end) = slice_command_output(&output, start, end)?;
    Ok((output, start, end, truncated))
}

fn semantic_zones(pane_id: PaneId) -> anyhow::Result<Value> {
    let pane = Mux::get()
        .get_pane(pane_id)
        .ok_or_else(|| anyhow!("pane {pane_id} not found"))?;
    let zones = pane
        .get_semantic_zones()?
        .into_iter()
        .map(|zone| {
            json!({
                "startY": zone.start_y,
                "startX": zone.start_x,
                "endY": zone.end_y,
                "endX": zone.end_x,
                "type": format!("{:?}", zone.semantic_type)
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({ "paneId": pane_id, "zones": zones }))
}

fn terminal_size(size: wezterm_term::TerminalSize) -> Value {
    json!({
        "cols": size.cols,
        "rows": size.rows,
        "pixelWidth": size.pixel_width,
        "pixelHeight": size.pixel_height,
        "dpi": size.dpi
    })
}

fn run_mutation_on_main(method: String, params: Value) -> anyhow::Result<Value> {
    let (tx, rx) = mpsc::sync_channel(1);
    spawn_into_main_thread(async move {
        // Domain spawning is deliberately `?Send` in mux because some
        // backends own main-thread state. Create that future only after this
        // outer, scheduler-safe task has reached the main thread.
        promise::spawn::spawn(async move {
            let result = handle_mutation(&method, params).await;
            tx.send(result).ok();
        })
        .detach();
    })
    .detach();
    rx.recv_timeout(MAIN_REQUEST_TIMEOUT)
        .map_err(|err| anyhow!("main-thread request failed: {err}"))?
}

fn send_user_input(pane: &dyn mux::pane::Pane, text: &str) -> anyhow::Result<()> {
    // Automation input represents terminal keystrokes, not a clipboard paste.
    // Pane::send_paste enables bracketed-paste mode, causing shells to insert
    // newlines and control bytes into their edit buffer instead of acting on
    // them. Write directly to the PTY so Enter, Ctrl-C, and prompt responses
    // have the same semantics as user input.
    let mut writer = pane.writer();
    writer.write_all(text.as_bytes())?;
    writer.flush()?;
    Ok(())
}

async fn handle_mutation(method: &str, params: Value) -> anyhow::Result<Value> {
    let params = object_params(Some(params))?;
    let mux = Mux::get();
    match method {
        "pane.sendText" => {
            let pane_id = pane_id(&params)?;
            let text = params
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("text is required"))?;
            anyhow::ensure!(text.len() <= MAX_TEXT_BYTES, "text is too large");
            let pane = mux
                .get_pane(pane_id)
                .ok_or_else(|| anyhow!("pane {pane_id} not found"))?;
            pane.send_paste(text)?;
            Ok(json!({ "paneId": pane_id, "sent": text.len() }))
        }
        "pane.focus" => {
            let pane_id = pane_id(&params)?;
            let (_domain, window_id, tab_id) = mux
                .resolve_pane_id(pane_id)
                .ok_or_else(|| anyhow!("pane {pane_id} not found"))?;
            let mut window = mux
                .get_window_mut(window_id)
                .ok_or_else(|| anyhow!("window {window_id} not found"))?;
            let index = window
                .idx_by_id(tab_id)
                .ok_or_else(|| anyhow!("tab {tab_id} not found in window"))?;
            window.save_and_then_set_active(index);
            drop(window);
            let pane = mux
                .get_pane(pane_id)
                .ok_or_else(|| anyhow!("pane {pane_id} not found"))?;
            mux.get_tab(tab_id)
                .ok_or_else(|| anyhow!("tab {tab_id} not found"))?
                .set_active_pane(&pane);
            mux.notify(MuxNotification::PaneFocused(pane_id));
            Ok(json!({ "focused": true, "paneId": pane_id }))
        }
        "pane.close" => {
            let pane_id = pane_id(&params)?;
            anyhow::ensure!(mux.get_pane(pane_id).is_some(), "pane {pane_id} not found");
            mux.remove_pane(pane_id);
            Ok(json!({ "closed": true, "paneId": pane_id }))
        }
        "command.cancel" => {
            let command_id = managed_command_id(&params)?;
            let record = MANAGED_COMMANDS
                .lock()
                .unwrap()
                .get(&command_id)
                .cloned()
                .ok_or_else(|| anyhow!("command {command_id} not found"))?;
            ensure_command_owns_pane(&command_id, &record)?;
            // Interrupt the command inside its persistent shell. Do not kill
            // the shell or close the pane; it remains available for reuse and
            // for direct user interaction.
            if record.running {
                let pane = mux
                    .get_pane(record.pane_id)
                    .ok_or_else(|| anyhow!("pane {} not found", record.pane_id))?;
                // The managed shell's prompt hook reports the resulting 130
                // status through the callback FIFO once Ctrl-C returns it to
                // the prompt. The shell and pane remain reusable.
                send_user_input(pane.as_ref(), "\u{3}")?;
            }
            Ok(json!({
                "cancelled": record.running,
                "commandId": command_id,
                "paneId": record.pane_id
            }))
        }
        "command.input" => {
            let command_id = managed_command_id(&params)?;
            let text = params
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("text is required"))?;
            anyhow::ensure!(text.len() <= MAX_TEXT_BYTES, "input is too large");
            let record = MANAGED_COMMANDS
                .lock()
                .unwrap()
                .get(&command_id)
                .cloned()
                .ok_or_else(|| anyhow!("command {command_id} not found"))?;
            ensure_command_owns_pane(&command_id, &record)?;
            anyhow::ensure!(record.running, "command {command_id} has already finished");
            let pane = mux
                .get_pane(record.pane_id)
                .ok_or_else(|| anyhow!("command pane is no longer available"))?;
            send_user_input(pane.as_ref(), text)?;
            Ok(json!({ "commandId": command_id, "sent": text.len() }))
        }
        "command.close" => {
            let command_id = managed_command_id(&params)?;
            let record = MANAGED_COMMANDS
                .lock()
                .unwrap()
                .get(&command_id)
                .cloned()
                .ok_or_else(|| anyhow!("command {command_id} not found"))?;
            ensure_command_owns_pane(&command_id, &record)?;
            anyhow::ensure!(
                mux.get_pane(record.pane_id).is_some(),
                "command pane is already closed"
            );
            if record.running {
                finish_command(&command_id, None, None, Some(false), Some("pane-closed"));
            }
            clear_command_pane_owner(&command_id, record.pane_id);
            mux.remove_pane(record.pane_id);
            Ok(json!({ "commandId": command_id, "closed": true }))
        }
        "pane.setZoomed" => {
            let pane_id = pane_id(&params)?;
            let zoomed = params
                .get("zoomed")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let (_, _, tab_id) = mux
                .resolve_pane_id(pane_id)
                .ok_or_else(|| anyhow!("pane {pane_id} not found"))?;
            let tab = mux
                .get_tab(tab_id)
                .ok_or_else(|| anyhow!("tab {tab_id} not found"))?;
            let changed = tab.set_zoomed(zoomed);
            Ok(json!({ "paneId": pane_id, "zoomed": zoomed, "changed": changed }))
        }
        "workspace.rename" => {
            let old = params
                .get("old")
                .or_else(|| params.get("oldWorkspace"))
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("old workspace is required"))?;
            let new = params
                .get("new")
                .or_else(|| params.get("newWorkspace"))
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("new workspace is required"))?;
            mux.rename_workspace(old, new)?;
            Ok(json!({ "old": old, "new": new }))
        }
        "tab.focus" => {
            let tab_id = params
                .get("tabId")
                .and_then(Value::as_u64)
                .ok_or_else(|| anyhow!("tabId is required"))? as TabId;
            let window_id = mux
                .window_containing_tab(tab_id)
                .ok_or_else(|| anyhow!("tab {tab_id} not found"))?;
            let mut window = mux
                .get_window_mut(window_id)
                .ok_or_else(|| anyhow!("window {window_id} not found"))?;
            let index = window
                .idx_by_id(tab_id)
                .ok_or_else(|| anyhow!("tab {tab_id} not found in window"))?;
            window.save_and_then_set_active(index);
            Ok(json!({ "focused": true, "tabId": tab_id, "windowId": window_id }))
        }
        "pane.split" => {
            let source_pane = pane_id(&params)?;
            let direction = match params
                .get("direction")
                .and_then(Value::as_str)
                .unwrap_or("horizontal")
            {
                "vertical" | "down" => SplitDirection::Vertical,
                _ => SplitDirection::Horizontal,
            };
            let size = params
                .get("percent")
                .and_then(Value::as_u64)
                .map(|percent| SplitSize::Percent(percent.clamp(1, 99) as u8))
                .unwrap_or_default();
            let command = command_builder(params.get("command"))?;
            let cwd = params
                .get("cwd")
                .and_then(Value::as_str)
                .map(str::to_string);
            let request = SplitRequest {
                direction,
                target_is_second: true,
                top_level: false,
                size,
            };
            let (pane, _size) = mux
                .split_pane(
                    source_pane,
                    request,
                    SplitSource::Spawn {
                        command,
                        command_dir: cwd,
                    },
                    config::keyassignment::SpawnTabDomain::CurrentPaneDomain,
                )
                .await?;
            Ok(json!({ "paneId": pane.pane_id() }))
        }
        "panel.ensure" => {
            let origin_pane_id = params
                .get("paneId")
                .and_then(Value::as_u64)
                .map(|id| id as PaneId)
                .ok_or_else(|| anyhow!("paneId is required for a managed panel"))?;
            let cwd = params.get("cwd").and_then(Value::as_str);
            let (pane, reused, pool_size) = ensure_managed_pane(origin_pane_id, cwd).await?;
            let (window_id, tab_id) = mux
                .resolve_pane_id(pane.pane_id())
                .map(|(_, window, tab)| (window, tab))
                .ok_or_else(|| anyhow!("managed pane is not attached to a tab"))?;
            Ok(json!({
                "paneId": pane.pane_id(),
                "originPaneId": origin_pane_id,
                "windowId": window_id,
                "tabId": tab_id,
                "poolSize": pool_size,
                "reused": reused,
                "limit": MAX_MANAGED_PANES,
            }))
        }
        "command.run" => {
            let command = params
                .get("command")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow!("command must be an array of strings"))?;
            anyhow::ensure!(!command.is_empty(), "command must not be empty");
            let cwd = params
                .get("cwd")
                .and_then(Value::as_str)
                .map(str::to_string);
            let origin_pane_id = params
                .get("paneId")
                .and_then(Value::as_u64)
                .map(|id| id as PaneId)
                .ok_or_else(|| anyhow!("paneId is required for a managed command"))?;

            let (pane, reused, pool_size) =
                ensure_managed_pane(origin_pane_id, cwd.as_deref()).await?;

            let sequence = REVISION.fetch_add(1, Ordering::Relaxed);
            let command_id = format!("command-{sequence}");
            let script = managed_command_script(command, cwd.as_deref())?;
            let output_start = pane.get_cursor_position();
            let record = ManagedCommand {
                pane_id: pane.pane_id(),
                owner_client_id: params
                    .get("ownerClientId")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                running: true,
                exit_code: None,
                signal: None,
                success: None,
                reason: None,
                output_start_y: output_start.y,
                output_start_x: output_start.x,
                captured_output: None,
                output_truncated: false,
            };
            MANAGED_COMMANDS
                .lock()
                .unwrap()
                .insert(command_id.clone(), record);
            MANAGED_PANE_OWNERS
                .lock()
                .unwrap()
                .insert(pane.pane_id(), command_id.clone());
            watch_command(command_id.clone(), pane.pane_id());
            if let Err(error) = send_user_input(pane.as_ref(), &script) {
                MANAGED_COMMANDS.lock().unwrap().remove(&command_id);
                clear_command_pane_owner(&command_id, pane.pane_id());
                return Err(error.into());
            }

            let (window_id, tab_id) = mux
                .resolve_pane_id(pane.pane_id())
                .map(|(_, window, tab)| (window, tab))
                .ok_or_else(|| anyhow!("managed pane is not attached to a tab"))?;
            Ok(json!({
                "commandId": command_id,
                "paneId": pane.pane_id(),
                "originPaneId": origin_pane_id,
                "windowId": window_id,
                "tabId": tab_id,
                "poolSize": pool_size,
                "reused": reused,
                "persistent": true,
                "completion": "automation.event command.finished"
            }))
        }
        _ => anyhow::bail!("unsupported mutation {method}"),
    }
}

async fn ensure_managed_pane(
    origin_pane_id: PaneId,
    cwd: Option<&str>,
) -> anyhow::Result<(Arc<dyn mux::pane::Pane>, bool, usize)> {
    let mux = Mux::get();
    let (_, origin_window_id, origin_tab_id) = mux
        .resolve_pane_id(origin_pane_id)
        .ok_or_else(|| anyhow!("origin pane {origin_pane_id} not found"))?;
    let mut pools = MANAGED_PANES.lock().unwrap();
    let pool = pools.entry(origin_pane_id).or_default();
    // A managed pane may only be reused or extended while it remains in the
    // origin pane's tab. Moving it elsewhere removes it from this pool but
    // never closes the user's pane.
    pool.retain(|pane_id| {
        mux.get_pane(*pane_id).is_some_and(|pane| !pane.is_dead())
            && mux
                .resolve_pane_id(*pane_id)
                .is_some_and(|(_, window_id, tab_id)| {
                    window_id == origin_window_id && tab_id == origin_tab_id
                })
    });

    let reusable = pool.iter().copied().find(|pane_id| {
        mux.get_pane(*pane_id)
            .map(|pane| !pane.is_dead() && !pane_is_busy(*pane_id))
            .unwrap_or(false)
    });
    if let Some(pane_id) = reusable {
        let pane = mux
            .get_pane(pane_id)
            .ok_or_else(|| anyhow!("managed pane {pane_id} disappeared"))?;
        return Ok((pane, true, pool.len()));
    }

    anyhow::ensure!(
        pool.len() < MAX_MANAGED_PANES,
        "all {MAX_MANAGED_PANES} managed terminal panes are busy; cancel one or wait for completion"
    );
    let source_pane = pool.last().copied().unwrap_or(origin_pane_id);
    let first_pane = pool.is_empty();
    let shell_setup = prepare_managed_shell()?;
    let ManagedShellSetup {
        command: shell_command,
        callback_fifo,
        cleanup_dir,
    } = shell_setup;
    let request = SplitRequest {
        // First split: a right-hand side panel. Further splits stack panes
        // vertically inside that side panel.
        direction: if first_pane {
            SplitDirection::Horizontal
        } else {
            SplitDirection::Vertical
        },
        target_is_second: true,
        top_level: false,
        size: SplitSize::Percent(50),
    };
    let (pane, _size) = mux
        .split_pane(
            source_pane,
            request,
            SplitSource::Spawn {
                command: Some(shell_command),
                command_dir: cwd.map(str::to_string),
            },
            config::keyassignment::SpawnTabDomain::CurrentPaneDomain,
        )
        .await?;
    let pane_id = pane.pane_id();
    if let Err(error) = start_shell_callback(pane_id, callback_fifo, cleanup_dir) {
        mux.remove_pane(pane_id);
        return Err(error);
    }
    pool.push(pane_id);
    if pool.len() == MAX_MANAGED_PANES {
        balance_managed_panel(origin_tab_id, &pool);
    }
    Ok((pane, false, pool.len()))
}

fn balance_managed_panel(tab_id: TabId, pane_ids: &[PaneId]) {
    if pane_ids.len() != MAX_MANAGED_PANES {
        return;
    }
    let Some(tab) = Mux::get().get_tab(tab_id) else {
        return;
    };
    let managed = tab
        .iter_panes()
        .into_iter()
        .filter(|pane| pane_ids.contains(&pane.pane.pane_id()))
        .collect::<Vec<_>>();
    if managed.len() != MAX_MANAGED_PANES {
        return;
    }
    let Some(panel_top) = managed.iter().map(|pane| pane.top).min() else {
        return;
    };
    let Some(panel_bottom) = managed.iter().map(|pane| pane.top + pane.height).max() else {
        return;
    };
    let panel_height = panel_bottom.saturating_sub(panel_top);
    if panel_height < MAX_MANAGED_PANES {
        return;
    }
    let Some(outer) = tab
        .iter_splits()
        .into_iter()
        .filter(|split| split.direction == SplitDirection::Vertical)
        .min_by_key(|split| split.top)
    else {
        return;
    };
    let desired = panel_top + panel_height / MAX_MANAGED_PANES;
    let delta = desired as isize - outer.top as isize;
    if delta != 0 {
        tab.resize_split_by(outer.index, delta);
    }
}

fn broadcast_event_to(target_client_id: Option<&str>, event: &str, data: Value) {
    let revision = REVISION.fetch_add(1, Ordering::Relaxed) + 1;
    let notification = EventNotification {
        jsonrpc: "2.0",
        method: "automation.event",
        params: json!({ "event": event, "revision": revision, "data": data }),
    };
    let Ok(line) = serde_json::to_string(&notification) else {
        return;
    };

    let clients = CLIENTS
        .lock()
        .unwrap()
        .iter()
        .filter(|(client_id, _)| target_client_id.is_none_or(|target| target == client_id.as_str()))
        .map(|(client_id, sender)| (client_id.clone(), sender.clone()))
        .collect::<Vec<_>>();
    let mut stale = Vec::new();
    for (client_id, sender) in clients {
        if sender.try_send(line.clone()).is_err() {
            stale.push(client_id);
        }
    }
    if !stale.is_empty() {
        let mut clients = CLIENTS.lock().unwrap();
        for client_id in stale {
            clients.remove(&client_id);
        }
    }
}

fn pane_is_busy(pane_id: PaneId) -> bool {
    let command_id = MANAGED_PANE_OWNERS.lock().unwrap().get(&pane_id).cloned();
    command_id.is_some_and(|command_id| {
        MANAGED_COMMANDS
            .lock()
            .unwrap()
            .get(&command_id)
            .is_some_and(|command| command.running)
    })
}

fn shell_quote(value: &str) -> anyhow::Result<String> {
    anyhow::ensure!(!value.contains('\0'), "command contains a NUL byte");
    Ok(format!("'{}'", value.replace('\'', "'\\''")))
}

fn managed_command_script(command: &[Value], cwd: Option<&str>) -> anyhow::Result<String> {
    let mut words = Vec::with_capacity(command.len());
    for value in command {
        let value = value
            .as_str()
            .ok_or_else(|| anyhow!("command arguments must be strings"))?;
        words.push(shell_quote(value)?);
    }
    let mut script = String::new();
    if let Some(cwd) = cwd {
        script.push_str("cd -- ");
        script.push_str(&shell_quote(cwd)?);
        script.push_str(" && ");
    }
    script.push_str(&words.join(" "));
    script.push('\n');
    Ok(script)
}

fn fifo_path_literal(path: &Path) -> anyhow::Result<String> {
    shell_quote(
        path.to_str()
            .ok_or_else(|| anyhow!("callback FIFO path is not valid UTF-8"))?,
    )
}

fn default_shell_path() -> String {
    #[cfg(unix)]
    unsafe {
        let passwd = libc::getpwuid(libc::getuid());
        if !passwd.is_null() && !(*passwd).pw_shell.is_null() {
            if let Ok(shell) = CStr::from_ptr((*passwd).pw_shell).to_str() {
                if !shell.is_empty() {
                    return shell.to_string();
                }
            }
        }
    }
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
}

fn make_callback_fifo(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        let raw = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| anyhow!("callback FIFO path contains NUL"))?;
        if unsafe { libc::mkfifo(raw.as_ptr(), 0o600) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("create callback FIFO {}", path.display()));
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        anyhow::bail!("shell callback FIFOs are only supported on Unix")
    }
}

fn prepare_managed_shell() -> anyhow::Result<ManagedShellSetup> {
    let sequence = SHELL_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let cleanup_dir = std::env::temp_dir().join(format!(
        "wezterm-automation-shell-{}-{sequence}",
        std::process::id()
    ));
    fs::create_dir(&cleanup_dir)
        .with_context(|| format!("create managed shell directory {}", cleanup_dir.display()))?;
    let callback_fifo = cleanup_dir.join("callback.fifo");
    if let Err(error) = make_callback_fifo(&callback_fifo) {
        let _ = fs::remove_dir_all(&cleanup_dir);
        return Err(error);
    }

    let configured = config::configuration().default_prog.clone();
    let argv = configured.unwrap_or_else(|| vec![default_shell_path()]);
    anyhow::ensure!(!argv.is_empty(), "managed shell command is empty");
    let name = Path::new(&argv[0])
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("sh")
        .to_ascii_lowercase();
    let fifo = fifo_path_literal(&callback_fifo)?;
    let rc_path = cleanup_dir.join("rc");
    let mut command = CommandBuilder::from_argv(argv.into_iter().map(OsString::from).collect());

    if name == "zsh" {
        // The user's normal zsh startup files install the callback hook.
        command.env("PI_WEZTERM_CALLBACK_FIFO", &callback_fifo);
    } else if name == "bash" {
        let rc = format!(
            "if [ -r \"$HOME/.bashrc\" ]; then . \"$HOME/.bashrc\"; fi\n\
__pi_wezterm_prompt() {{\n\
  local __pi_status=$?\n\
  __pi_wezterm_prompt_seq=$(( ${{__pi_wezterm_prompt_seq:-0}} + 1 ))\n\
  printf '\\033]1337;SetUserVar=PI_WEZTERM_PROMPT_%s=\\007' \"$__pi_wezterm_prompt_seq\"\n\
  printf '%s %s\\n' \"$__pi_status\" \"$__pi_wezterm_prompt_seq\" > {fifo}\n\
  return $__pi_status\n\
}}\n\
if declare -p PROMPT_COMMAND >/dev/null 2>&1 && declare -p PROMPT_COMMAND | grep -q 'declare -a'; then\n\
  PROMPT_COMMAND=(__pi_wezterm_prompt \"${{PROMPT_COMMAND[@]}}\")\n\
else\n\
  PROMPT_COMMAND=__pi_wezterm_prompt${{PROMPT_COMMAND:+;${{PROMPT_COMMAND}}}}\n\
fi\n",
        );
        fs::write(&rc_path, rc)?;
        command.arg("--rcfile");
        command.arg(&rc_path);
        command.arg("-i");
    } else {
        let rc = format!(
            "__pi_wezterm_prompt() {{\n\
  __pi_status=$?\n\
  __pi_wezterm_prompt_seq=$(( ${{__pi_wezterm_prompt_seq:-0}} + 1 ))\n\
  printf '\\033]1337;SetUserVar=PI_WEZTERM_PROMPT_%s=\\007' \"$__pi_wezterm_prompt_seq\"\n\
  printf '%s %s\\n' \"$__pi_status\" \"$__pi_wezterm_prompt_seq\" > {fifo}\n\
  return $__pi_status\n\
}}\n\n__pi_wezterm_prompt\n",
        );
        fs::write(&rc_path, rc)?;
        command.env("ENV", &rc_path);
    }

    Ok(ManagedShellSetup {
        command,
        callback_fifo,
        cleanup_dir,
    })
}

fn watch_prompt_markers(pane_id: PaneId) {
    Mux::get().subscribe(move |notification| match notification {
        MuxNotification::Alert {
            pane_id: marker_pane_id,
            alert: wezterm_term::Alert::SetUserVar { name, .. },
        } if marker_pane_id == pane_id => {
            if let Some(sequence) = name
                .strip_prefix(PROMPT_MARKER_PREFIX)
                .and_then(|value| value.parse::<u64>().ok())
            {
                let (markers, changed) = &*PROMPT_MARKERS;
                let mut markers = markers.lock().unwrap();
                markers.insert(pane_id, sequence);
                changed.notify_all();
            }
            true
        }
        MuxNotification::PaneRemoved(removed_pane_id) if removed_pane_id == pane_id => false,
        _ => true,
    });
}

fn wait_for_prompt_marker(pane_id: PaneId, sequence: u64) -> bool {
    let (markers, changed) = &*PROMPT_MARKERS;
    let markers = markers.lock().unwrap();
    let (markers, _) = changed
        .wait_timeout_while(markers, Duration::from_secs(5), |markers| {
            markers
                .get(&pane_id)
                .is_none_or(|observed| *observed < sequence)
        })
        .unwrap();
    markers
        .get(&pane_id)
        .is_some_and(|observed| *observed >= sequence)
}

fn mark_shell_ready(pane_id: PaneId) {
    let (ready, changed) = &*SHELL_READY;
    ready.lock().unwrap().insert(pane_id);
    changed.notify_all();
}

fn wait_for_shell_ready(pane_id: PaneId) -> bool {
    let (ready, changed) = &*SHELL_READY;
    let ready = ready.lock().unwrap();
    let (ready, _) = changed
        .wait_timeout_while(ready, Duration::from_secs(5), |ready| {
            !ready.contains(&pane_id)
        })
        .unwrap();
    ready.contains(&pane_id)
}

fn start_shell_callback(
    pane_id: PaneId,
    callback_fifo: PathBuf,
    cleanup_dir: PathBuf,
) -> anyhow::Result<()> {
    watch_prompt_markers(pane_id);
    let (ready, _) = &*SHELL_READY;
    ready.lock().unwrap().remove(&pane_id);

    #[cfg(unix)]
    {
        let (reader, writer) = open_callback_fifo(&callback_fifo)
            .with_context(|| format!("open managed shell callback {}", callback_fifo.display()))?;
        thread::Builder::new()
            .name(format!("wezterm-shell-callback-{pane_id}"))
            .spawn(move || {
                // Keep the writer open while the blocking reader is alive;
                // shell prompt writes remain the only completion notifications.
                let _writer = writer;
                let mut initial_prompt_seen = false;
                for line in BufReader::new(reader).lines() {
                    let Ok(line) = line else { break };
                    let mut fields = line.split_whitespace();
                    let Some(exit_code) = fields.next().and_then(|value| value.parse::<i32>().ok())
                    else {
                        continue;
                    };
                    let marker_sequence = fields.next().and_then(|value| value.parse::<u64>().ok());
                    // Consume the startup prompt before command.run registers
                    // ownership. This prevents a fast command or cancellation
                    // from being mistaken for the shell's first prompt.
                    if !initial_prompt_seen {
                        initial_prompt_seen = true;
                        mark_shell_ready(pane_id);
                        continue;
                    }
                    // The FIFO can outrun the PTY parser. Wait for the hidden
                    // prompt marker so output capture and subsequent pane reuse
                    // observe every byte emitted by the completed command.
                    if marker_sequence
                        .is_some_and(|sequence| !wait_for_prompt_marker(pane_id, sequence))
                    {
                        log::warn!(
                            "timed out waiting for managed shell prompt marker in pane {pane_id}"
                        );
                    }
                    finish_running_command(pane_id, exit_code);
                }
                let (ready, _) = &*SHELL_READY;
                ready.lock().unwrap().remove(&pane_id);
                let (markers, _) = &*PROMPT_MARKERS;
                markers.lock().unwrap().remove(&pane_id);
                let _ = fs::remove_dir_all(&cleanup_dir);
            })
            .map_err(|error| anyhow!("start managed shell callback: {error}"))?;
        anyhow::ensure!(
            wait_for_shell_ready(pane_id),
            "managed shell did not reach its initial prompt"
        );
        Ok(())
    }

    #[cfg(not(unix))]
    {
        let _ = (callback_fifo, cleanup_dir);
        anyhow::bail!("managed shell callbacks are only supported on Unix");
    }
}

#[cfg(unix)]
fn open_callback_fifo(path: &Path) -> anyhow::Result<(std::fs::File, std::fs::File)> {
    let reader = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?;
    let writer = OpenOptions::new().write(true).open(path)?;
    let fd: RawFd = reader.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    anyhow::ensure!(flags >= 0, "get callback FIFO flags failed");
    let result = unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) };
    anyhow::ensure!(result == 0, "set callback FIFO blocking mode failed");
    Ok((reader, writer))
}

fn finish_running_command(pane_id: PaneId, exit_code: i32) {
    let command_id = MANAGED_PANE_OWNERS.lock().unwrap().get(&pane_id).cloned();
    if let Some(command_id) = command_id {
        finish_command(
            &command_id,
            Some(exit_code),
            None,
            Some(exit_code == 0),
            None,
        );
    }
}

fn finish_command(
    command_id: &str,
    exit_code: Option<i32>,
    signal: Option<String>,
    success: Option<bool>,
    reason: Option<&str>,
) {
    let snapshot = {
        let commands = MANAGED_COMMANDS.lock().unwrap();
        let Some(command) = commands.get(command_id) else {
            return;
        };
        if !command.running {
            return;
        }
        command.clone()
    };
    let (captured_output, output_truncated) =
        capture_managed_command_output(&snapshot).unwrap_or_else(|_| (String::new(), true));

    let (event, owner_client_id) = {
        let mut commands = MANAGED_COMMANDS.lock().unwrap();
        let Some(command) = commands.get_mut(command_id) else {
            return;
        };
        if !command.running {
            return;
        }
        command.running = false;
        command.exit_code = exit_code;
        command.signal = signal;
        command.success = success;
        command.reason = reason.map(str::to_string);
        command.captured_output = Some(captured_output);
        command.output_truncated = output_truncated;
        (
            json!({
                "commandId": command_id,
                "exitCode": command.exit_code,
                "signal": command.signal,
                "success": command.success,
                "reason": command.reason,
                "outputAvailable": command
                    .captured_output
                    .as_ref()
                    .is_some_and(|output| !output.is_empty()),
                "outputTruncated": command.output_truncated,
            }),
            command.owner_client_id.clone(),
        )
    };
    broadcast_event_to(owner_client_id.as_deref(), "command.finished", event);
}

fn watch_command(command_id: String, pane_id: PaneId) {
    // Shell prompt hooks deliver normal command completion through a side
    // channel. Mux lifecycle notifications only cover shell/pane teardown.
    Mux::get().subscribe(move |notification| {
        if !MANAGED_COMMANDS
            .lock()
            .unwrap()
            .get(&command_id)
            .is_some_and(|command| command.running)
        {
            return false;
        }
        match notification {
            MuxNotification::PaneExited(exited_pane_id, status) if exited_pane_id == pane_id => {
                finish_command(
                    &command_id,
                    Some(status.exit_code() as i32),
                    status.signal().map(str::to_string),
                    Some(status.success()),
                    Some("shell-exited"),
                );
                false
            }
            MuxNotification::PaneRemoved(removed_pane_id) if removed_pane_id == pane_id => {
                finish_command(&command_id, None, None, Some(false), Some("pane-removed"));
                false
            }
            _ => true,
        }
    });
}

fn command_builder(value: Option<&Value>) -> anyhow::Result<Option<CommandBuilder>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let args = value
        .as_array()
        .ok_or_else(|| anyhow!("command must be an array of strings"))?;
    let args = args
        .iter()
        .map(|arg| {
            arg.as_str()
                .map(std::ffi::OsString::from)
                .ok_or_else(|| anyhow!("command arguments must be strings"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    if args.is_empty() {
        return Ok(None);
    }
    Ok(Some(CommandBuilder::from_argv(args)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_automation_socket_path() {
        let domain = config::UnixDomain::default();
        let path = socket_path(&domain);
        assert!(path.to_string_lossy().ends_with(".automation"));
    }

    #[test]
    fn serializes_events_as_jsonrpc_notifications() {
        let event = notification_to_event(MuxNotification::PaneAdded(7), 9);
        let value = serde_json::to_value(event).unwrap();
        assert_eq!(value["method"], "automation.event");
        assert_eq!(value["params"]["data"]["paneId"], 7);
    }

    #[test]
    fn managed_scripts_quote_arguments_without_terminal_completion_output() {
        let command = vec![json!("printf"), json!("it's safe")];
        let script = managed_command_script(&command, Some("/tmp/work")).unwrap();
        assert!(script.contains("cd -- '/tmp/work'"));
        assert!(script.contains("'it'\\''s safe'"));
        assert!(!script.contains("PI_WEZTERM_DONE"));
        assert_eq!(script.lines().count(), 1);
    }

    fn completed_command(pane_id: PaneId, output: &str) -> ManagedCommand {
        ManagedCommand {
            pane_id,
            owner_client_id: None,
            running: false,
            exit_code: Some(0),
            signal: None,
            success: Some(true),
            reason: None,
            output_start_y: 0,
            output_start_x: 0,
            captured_output: Some(output.to_string()),
            output_truncated: false,
        }
    }

    #[test]
    fn completed_command_reads_immutable_output_without_its_pane() {
        let command = completed_command(usize::MAX - 1, "first\nsecond\nthird");
        let (output, start, end, truncated) =
            read_managed_command_output(&command, Some(1), Some(3)).unwrap();
        assert_eq!(output, "second\nthird");
        assert_eq!((start, end), (1, 3));
        assert!(!truncated);
    }

    #[test]
    fn stale_command_id_cannot_control_a_reused_pane() {
        let pane_id = usize::MAX - 2;
        let stale = completed_command(pane_id, "old output");
        let current = completed_command(pane_id, "new output");
        MANAGED_PANE_OWNERS
            .lock()
            .unwrap()
            .insert(pane_id, "command-current".to_string());

        assert!(ensure_command_owns_pane("command-stale", &stale)
            .unwrap_err()
            .to_string()
            .contains("no longer owns"));
        ensure_command_owns_pane("command-current", &current).unwrap();

        MANAGED_PANE_OWNERS.lock().unwrap().remove(&pane_id);
    }
}
