# Native Herdr Features for WezTerm

## Scope selected

- Native workspaces, tabs, and panes
- Persistent sessions
- Native WezTerm SSH domains for remote sessions
- General native CLI/RPC orchestration APIs
- App-aware Pi/Claude/Codex session restoration
- macOS and Linux
- Keep Herdr running until parity is proven

## Existing WezTerm capabilities to reuse

- `mux/src/tab.rs`, `mux/src/window.rs`, and `mux/src/pane.rs`: native pane trees, tabs, workspaces, focus, zoom, and movement.
- `wezterm/src/cli/`: existing list, get-text, send-text, split, activate, resize, kill, zoom, tab, and workspace commands.
- `codec/src/lib.rs`: existing mux request/response protocol for list, spawn, split, write, focus, resize, zoom, and workspace operations.
- `mux/src/ssh.rs` and `config/src/ssh.rs`: native SSH domains and remote mux transport.
- `wezterm-gui/src/termwindow/render/pane.rs`: existing pane rendering and inactive-pane HSB handling.

## Gaps to add

### Phase 1: Native orchestration API

Initial implementation:

- `wezterm cli wait` waits for a pane to disappear, supports `--timeout`, and polls the native mux.
- `wezterm cli watch` polls recent pane output for literal or regular-expression matches, with timeout and polling controls.
- `wezterm cli run-wait` spawns a command and waits for its pane to disappear.

`run-wait` now uses a versioned exit-status RPC and emits JSON with `pane_id`, `exit_code`, and `signal`. If a pane is removed before its status can be queried, `exit_code` is null.

Remaining Phase 1 work:

Add general JSON-capable CLI/RPC operations:

- `list` with stable workspace/window/tab/pane hierarchy and focus state
- `read` with visible/scrollback ranges
- `send` and key injection
- `split`, `spawn`, `focus`, `resize`, `zoom`, `move`, and `stop`
- `watch` for output/readiness matches
- cancellation and timeout handling

Prefer extending the existing codec protocol and `wezterm cli` rather than adding a second server.

### Phase 2: Native persistence

Initial implementation:

- `wezterm cli save-state` writes an atomic, versioned JSON snapshot to `$XDG_STATE_HOME/wezterm/herdr.json` on Linux or the macOS application-support directory, unless `--file` is supplied.
- `wezterm cli restore-state` recreates windows, tabs, shell panes, working directories, titles, focus, zoom state, and an approximation of the saved split topology. `--workspace` can override the saved workspace.

The snapshot now includes lightweight foreground-process metadata (`name`, executable, argv, and cwd). Native GUI startup restores from the snapshot after Lua startup hooks, only when the mux is still empty. Native GUI shutdown, last-window close, and a configurable periodic timer save the snapshot directly from the native mux.

Environment controls:

- `WEZTERM_HERDR_NATIVE_SESSION_AUTOSAVE=0|1`
- `WEZTERM_HERDR_NATIVE_SESSION_RESTORE=0|1`
- `WEZTERM_HERDR_NATIVE_SESSION_INTERVAL=<seconds>`

Remaining persistence work:

- preserve exact split ratios
- capture and restore scrollback snapshots
- persist native SSH domain identifiers
- add snapshot deletion and migration commands

### Phase 3: App-aware restore

Initial adapters:

- **nvim/vim**: restores the captured argv in the saved working directory.
- **Pi**: restores an explicit session argv when present; otherwise selects the newest JSONL session for the saved working directory and launches `pi --session <path>`, falling back to `pi -c`.

Unknown processes fall back to the configured shell and cwd. Claude and Codex adapters remain follow-up work.

### Phase 4: Focus and UI polish

Optional visual improvements from the existing Herdr workflow:

- focused-pane background/fill instead of borders
- workspace/tab/pane sidebar or overlay
- agent status labels and attention states

These should be layered on the existing WezTerm render path and remain configurable.

## Non-goals for the first native fork

- Reimplementing WezTerm's mux or SSH transport
- A separate Herdr server/client protocol
- Windows support in the first milestone
- Worktree automation and notifications until the core APIs are stable

## Expected effort

- Phase 1: 1–2 weeks
- Phase 2: 1–2 weeks
- Phase 3: 1–2 weeks
- macOS/Linux testing and stabilization: 1–2 weeks

A usable MVP is roughly 3–4 weeks; production-quality parity is more realistically 6–8 weeks of focused work.
