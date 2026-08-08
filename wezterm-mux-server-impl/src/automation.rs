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
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;
use wezterm_term::StableRowIndex;
use wezterm_uds::UnixStream;

pub const PROTOCOL_VERSION: u32 = 1;
const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_TEXT_BYTES: usize = 256 * 1024;
const MAIN_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

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

        send_result(
            out,
            id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "connectionId": client_id,
                "muxInstanceId": &*INSTANCE_ID,
                "epoch": std::process::id(),
                "originPaneId": origin_pane,
                "grantedCapabilities": capabilities(),
                "features": {
                    "topologySnapshot": true,
                    "topologySubscription": true,
                    "paneText": true,
                    "semanticZones": true,
                    "paneControl": true,
                    "managedCommands": true,
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
        | "command.run" | "command.cancel" | "workspace.rename" | "tab.focus" => {
            let method = request.method;
            let params = request.params.unwrap_or_else(|| json!({}));
            let result = run_mutation_on_main(method, params);
            match result {
                Ok(value) => send_result(out, id, value),
                Err(err) => send_error(out, id, -32010, &err.to_string(), None),
            }
        }
        "command.getResult" => {
            let params = object_params(request.params)?;
            let pane_id = command_pane_id(&params)?;
            let pane = Mux::get()
                .get_pane(pane_id)
                .ok_or_else(|| anyhow!("pane {pane_id} not found"))?;
            let status = pane.get_exit_status();
            send_result(
                out,
                id,
                json!({
                    "commandId": format!("command-{pane_id}"),
                    "paneId": pane_id,
                    "running": status.is_none() && !pane.is_dead(),
                    "exitCode": status.as_ref().map(|status| status.exit_code()),
                    "signal": status.as_ref().and_then(|status| status.signal()),
                    "success": status.as_ref().map(|status| status.success())
                }),
            )
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
        "tab.control",
        "workspace.control",
        "command.run",
        "command.cancel",
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

fn command_pane_id(params: &Map<String, Value>) -> anyhow::Result<PaneId> {
    if let Some(pane_id) = params.get("paneId").and_then(Value::as_u64) {
        return Ok(pane_id as PaneId);
    }
    let command_id = params
        .get("commandId")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("paneId or commandId is required"))?;
    command_id
        .strip_prefix("command-")
        .ok_or_else(|| anyhow!("invalid commandId"))?
        .parse::<PaneId>()
        .map_err(|_| anyhow!("invalid commandId"))
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
    let start = start.unwrap_or(dims.physical_top as i64) as StableRowIndex;
    let end = end
        .unwrap_or(start as i64 + dims.viewport_rows as i64)
        .max(start as i64) as StableRowIndex;
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
            let pane_id = command_pane_id(&params)?;
            let pane = mux
                .get_pane(pane_id)
                .ok_or_else(|| anyhow!("pane {pane_id} not found"))?;
            pane.kill();
            Ok(
                json!({ "cancelled": true, "commandId": format!("command-{pane_id}"), "paneId": pane_id }),
            )
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
            mux.rename_workspace(old, new);
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
        "command.run" => {
            let command = command_builder(params.get("command"))?;
            let cwd = params
                .get("cwd")
                .and_then(Value::as_str)
                .map(str::to_string);
            let origin = params
                .get("paneId")
                .and_then(Value::as_u64)
                .map(|id| id as PaneId);
            let requested_workspace = params.get("workspace").and_then(Value::as_str);
            let origin_window_id =
                origin.and_then(|pane| mux.resolve_pane_id(pane).map(|(_, window, _)| window));
            // An explicit workspace is an orchestration boundary: keep using
            // the origin window for same-workspace runs, but create a new mux
            // window when the requested workspace differs. Without this,
            // `terminal_run({workspace = ...})` silently opened another tab in
            // the current workspace and agents could not arrange workspaces.
            let window_id = match (origin_window_id, requested_workspace) {
                (Some(window_id), Some(workspace))
                    if mux
                        .get_window(window_id)
                        .is_some_and(|window| window.get_workspace() == workspace) =>
                {
                    Some(window_id)
                }
                (Some(window_id), None) => Some(window_id),
                (None, None) => mux.iter_windows().into_iter().next(),
                _ => None,
            };
            let domain = if origin.is_some() {
                config::keyassignment::SpawnTabDomain::CurrentPaneDomain
            } else {
                config::keyassignment::SpawnTabDomain::DefaultDomain
            };
            let workspace = params
                .get("workspace")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| mux.active_workspace());
            let size = config::configuration().initial_size(0, None);
            let (_tab, pane, window_id) = mux
                .spawn_tab_or_window(
                    window_id, domain, command, cwd, size, origin, workspace, None,
                )
                .await?;
            Ok(json!({
                "commandId": format!("command-{}", pane.pane_id()),
                "paneId": pane.pane_id(),
                "windowId": window_id
            }))
        }
        _ => anyhow::bail!("unsupported mutation {method}"),
    }
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
}
