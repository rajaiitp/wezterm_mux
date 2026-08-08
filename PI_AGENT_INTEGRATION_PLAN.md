# Pi Agent ↔ WezTerm Native Integration Plan

## Status

Implementation is in progress in this fork. The first native vertical slice is
implemented:

- trusted local JSON-RPC automation endpoint;
- versioned handshake and capability advertisement;
- topology/context/pane metadata/text/semantic-zone reads;
- topology notifications with bounded per-client queues;
- native pane focus, split, close, zoom, text input, workspace/tab control, and
  command spawning;
- generic client state and callback notifications;
- Pi TypeScript client, tools, lifecycle state publishing, context injection,
  reconnect handling, and WezTerm callback handlers;
- native GUI `CallAutomationClient` key assignment;
- `wezterm cli automation-call` for diagnostics and callback orchestration;
- SSH panes through the existing native `ClientDomain` mux path;
- persistent-mux environment propagation through `WEZTERM_AUTOMATION_SOCKET`.

The remaining phased work is tracked below. In particular, binary SSH relay is
not duplicated: SSH uses native mux-domain routing, while remote Pi sessions
connect to the remote mux endpoint directly. Native managed-command wait/event
retention, full GUI modal request routing, and cross-platform endpoint support
remain future phases.

### Validation of the first vertical slice

The current implementation has been validated with:

- `cargo check -p wezterm-mux-server-impl -p wezterm-mux-server -p wezterm -p wezterm-gui`;
- release builds of the mux server, GUI, and CLI;
- `cargo test -p wezterm-mux-server-impl automation --lib`;
- live JSON-RPC calls against the persistent mux for handshake, context,
  topology, ping, subscriptions, pane reads, managed command lifecycle, and
  topology events;
- a live `wezterm cli automation-call` callback round trip;
- the TypeScript client connecting, reading context, and subscribing; and
- Pi extension loading/registration with all six terminal tools and lifecycle
  handlers.

The remaining items above are intentionally documented as future phases rather
than being implied by this validation.

## Goal

Build a first-class, bidirectional integration between Pi and WezTerm that lets
Pi understand and control terminal topology while WezTerm can observe and invoke
Pi through explicit callbacks.

The integration must be:

- native to WezTerm's mux architecture;
- exposed to Pi through a typed extension API;
- event-driven rather than polling-driven;
- usable with local, persistent, and SSH mux domains;
- generic enough to propose upstream without coupling WezTerm core to Pi;
- full-authority for trusted local Pi sessions by default;
- versioned, testable, observable, and recoverable.

## Explicit non-goals

The final implementation must not depend on:

- synthetic keyboard input;
- compositor automation;
- screen scraping or image recognition;
- launching `wezterm cli` once per operation;
- parsing human-oriented CLI output;
- shell scripts or temporary files as an IPC protocol;
- OSC escape sequences as the primary control transport;
- embedding Node.js or the Pi SDK inside the WezTerm Rust process;
- Pi-specific concepts in WezTerm's core protocol.

The existing CLI remains useful for humans and diagnostics, but it is not the
agent integration boundary.

---

## 1. Architectural decision

### 1.1 Chosen architecture

Use a **generic native WezTerm automation control plane** and a **Pi extension
client**.

```text
┌─────────────────────────────────────────────────────────────┐
│ Pi interactive process                                      │
│                                                             │
│  Pi lifecycle events ──► pi-wezterm extension               │
│  Pi custom tools      ◄── typed TypeScript client            │
└──────────────────────────────┬──────────────────────────────┘
                               │ versioned JSON-RPC
                               │ WEZTERM_AUTOMATION_SOCKET
                               ▼
┌─────────────────────────────────────────────────────────────┐
│ WezTerm mux automation service                              │
│                                                             │
│ auth · capabilities · subscriptions · request routing       │
│ topology · pane control · process lifecycle · semantic data │
└──────────────┬─────────────────────────────┬────────────────┘
               │ native mux calls            │ typed mux PDUs
               ▼                             ▼
      local/persistent mux             GUI / SSH client domain
               │                             │
               ▼                             ▼
        panes/tabs/windows          focus · modal · status · UI
```

### 1.2 Why this boundary

Pi already provides:

- extension lifecycle callbacks;
- typed custom tools;
- extension commands and shortcuts;
- session identity and persistence;
- tool-call interception and permission gates;
- UI requests and status widgets;
- SDK and RPC event streams.

WezTerm already provides:

- stable mux pane, tab, window, workspace, domain, and client concepts;
- a native notification bus;
- versioned typed PDUs between mux servers and clients;
- local and SSH mux transport;
- pane output, metadata, process information, semantic zones, and OSC 133
  parsing;
- GUI actions, Lua events, native selectors/prompts/confirmations;
- persistent pane exit status, `run-wait`, `watch`, and text retrieval in this
  fork.

A native control plane connects those existing systems without embedding either
runtime into the other.

### 1.3 Core remains agent-agnostic

Name the WezTerm feature **Automation API**, not **Pi API**.

WezTerm core should understand generic automation clients, methods,
capabilities, and events. Pi-specific session and model behavior belongs in the
Pi extension.

---

## 2. Process and transport model

### 2.1 Automation endpoint

Each mux server exposes a separate automation socket:

```text
WEZTERM_AUTOMATION_SOCKET=/run/user/$UID/wezterm/<mux-id>.automation.sock
```

For this fork's persistent mux it should be derived from the configured mux
socket, for example:

```text
~/.local/share/wezterm/persistent-mux.automation.sock
```

Properties:

- Unix domain socket on Unix systems;
- owner-only permissions (`0600`);
- peer UID verification using `SO_PEERCRED` where available;
- named-pipe equivalent with user ACLs on Windows;
- created and removed with the mux server lifecycle;
- exported into every spawned pane as `WEZTERM_AUTOMATION_SOCKET`;
- not overloaded onto the existing binary mux socket.

### 2.2 Protocol framing

Use JSON-RPC 2.0 with strict LF-delimited JSON records.

Reasons:

- Rust and TypeScript support it directly;
- schemas remain inspectable and testable;
- requests can be bidirectional;
- notifications naturally represent callbacks;
- no native Node addon is required;
- strict JSONL semantics are already familiar from Pi RPC mode.

Protocol requirements:

- explicit protocol version in the handshake;
- request IDs unique per connection;
- maximum record size;
- maximum nesting depth;
- bounded outbound queues;
- request cancellation;
- typed error codes;
- no unbounded pane-output buffering;
- heartbeat and graceful disconnect support.

### 2.3 Handshake

The first request must be `automation.hello`:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "automation.hello",
  "params": {
    "protocolVersion": 1,
    "client": {
      "name": "pi",
      "version": "1.0.0",
      "pid": 1234
    },
    "origin": {
      "paneId": 42
    },
    "requestedCapabilities": ["*"]
  }
}
```

The response includes:

- negotiated version;
- connection/session ID;
- mux instance ID and epoch;
- origin pane reference;
- granted capabilities;
- server feature flags;
- event replay support and current sequence number.

### 2.4 Stable object references

Raw numeric IDs are insufficient across reconnects and SSH mappings. Public
methods should use opaque references:

```ts
interface PaneRef {
  muxInstanceId: string;
  epoch: number;
  domainName: string;
  paneId: number;
  generation: number;
}
```

Equivalent refs are required for tabs, windows, workspaces, domains, clients,
and managed commands.

Every mutating call validates the reference and returns `STALE_OBJECT` rather
than accidentally targeting a newly allocated object with the same numeric ID.

---

## 3. Security and authority

### 3.1 Selected policy

Trusted Pi processes running as the same local user receive full terminal
control by default.

The generic upstream API must still implement capabilities so other deployments
can choose a narrower policy.

### 3.2 Capability groups

- `topology.read`
- `pane.read`
- `pane.output.subscribe`
- `pane.focus`
- `pane.input.text`
- `pane.input.raw`
- `pane.create`
- `pane.layout`
- `pane.close`
- `tab.control`
- `workspace.control`
- `domain.control`
- `command.run`
- `command.cancel`
- `ui.notify`
- `ui.interact`
- `clipboard.read`
- `clipboard.write`
- `client.callback`

Support `*` for trusted clients.

### 3.3 Policy hooks

Configuration should support rules by client name, executable, peer UID, pane,
domain, and workspace.

```lua
config.automation_rules = {
  {
    client = "pi",
    same_user = true,
    capabilities = { "*" },
  },
}
```

Add an optional Lua callback for policy augmentation:

```lua
wezterm.on("automation-client-authenticate", function(client, request)
  return { allow = true, capabilities = { "*" } }
end)
```

Core authorization must not depend on Lua being configured correctly.

### 3.4 Audit and redaction

Record structured audit events for mutating operations:

- automation connection ID;
- Pi session ID if supplied;
- method name;
- target refs;
- timestamp and result;
- confirmation/policy decision.

Never log raw pasted text, credentials, clipboard contents, environment secrets,
or full pane output by default.

---

## 4. WezTerm Automation API

### 4.1 Context and topology

Methods:

- `context.get`
  - origin pane;
  - focused pane/tab/window/workspace;
  - current domain;
  - neighboring pane refs;
  - attached automation/Pi client metadata.
- `topology.snapshot`
  - domains, workspaces, windows, tabs, panes, layout tree;
  - titles, cwd, foreground process, size, focus, zoom state;
  - monotonic topology revision.
- `topology.subscribe`
  - filtered event stream;
  - optional initial snapshot;
  - replay from sequence number where available.

### 4.2 Pane inspection

Methods:

- `pane.get`
- `pane.readText`
  - visible viewport, scrollback range, or semantic zone;
  - plain text or attributed cells;
  - bounded result size and pagination.
- `pane.search`
- `pane.getSemanticZones`
- `pane.getSelection`
- `pane.getMetadata`
  - cwd, title, process info, cursor, dimensions, user vars, progress;
- `pane.subscribeOutput`
  - render deltas or normalized text deltas;
  - bounded buffering and gap notifications.

No image capture is part of the default agent API. Text and structured terminal
state are the primary context source.

### 4.3 Pane control

Methods:

- `pane.focus`
- `pane.split`
- `pane.spawn`
- `pane.resize`
- `pane.zoom`
- `pane.moveToTab`
- `pane.moveToNewTab`
- `pane.close`
- `pane.sendText`
  - explicit bracketed-paste mode;
- `pane.sendKey`
  - structured key and modifiers;
- `pane.sendRaw`
  - separate high-authority capability;
- `pane.waitForExit`
- `pane.waitForText`
- `pane.waitForPrompt`

The agent should prefer semantic spawn/control APIs over input injection.

### 4.4 Tabs, windows, workspaces, and domains

Methods:

- `tab.create`, `tab.focus`, `tab.rename`, `tab.move`, `tab.close`
- `window.create`, `window.focus`, `window.rename`, `window.close`
- `workspace.create`, `workspace.focus`, `workspace.rename`,
  `workspace.close`
- `domain.list`, `domain.attach`, `domain.detach`

All create operations return stable refs and a topology revision.

### 4.5 Managed commands

Provide a first-class command lifecycle rather than asking Pi to spawn and then
poll a pane.

Methods:

- `command.run`
  - command, cwd, env, domain, workspace, layout target;
  - visible pane, scratch overlay, hidden capture pane, or existing pane;
  - returns `CommandRef` and optional `PaneRef`.
- `command.wait`
- `command.cancel`
- `command.getResult`
- `command.subscribe`

Events:

- `command.started`
- `command.output`
- `command.prompted`
- `command.exited`
- `command.cancelled`

Build this on the fork's native exit-status cache and pane lifecycle rather than
polling process lists.

### 4.6 GUI interaction

Methods routed to an attached GUI client:

- `ui.notify`
- `ui.showSelector`
- `ui.showPrompt`
- `ui.showConfirmation`
- `ui.showScratchPane`
- `ui.highlightPane`
- `ui.setPaneBadge`
- `ui.setTabStatus`
- `ui.clearStatus`
- `ui.focusWindow`

Interactive methods return typed user responses and support cancellation and
timeouts.

If no GUI is attached, return `NO_GUI_CLIENT` rather than silently doing
nothing.

### 4.7 Generic client callbacks

Automation clients may register callback methods during `automation.hello`:

```json
{
  "callbacks": [
    "agent.prompt",
    "agent.steer",
    "agent.abort",
    "agent.getState"
  ]
}
```

WezTerm can call them with `client.call`.

Expose a generic Lua action/API:

```lua
wezterm.automation.call_client {
  target = "focused-pane",
  method = "agent.prompt",
  params = { text = "Explain the latest command failure" },
}
```

The fork additionally exposes a native `CallAutomationClient` key
assignment. It accepts a method and JSON params and defaults to
`client_id = "origin-pane"`, which resolves the Pi client whose hello handshake
identified the active pane:

```lua
wezterm.action.CallAutomationClient {
  method = "agent.prompt",
  params = '{"text":"Explain the selected command output"}',
}
```

Also expose generic Lua events in the next API layer:

- `automation-client-connected`
- `automation-client-disconnected`
- `automation-client-state-changed`
- `automation-request-audit`

Lua is an extension surface, not the transport implementation.

---

## 5. Native event model

### 5.1 Topology events

- `pane.added`, `pane.removed`, `pane.focused`, `pane.resized`
- `tab.added`, `tab.removed`, `tab.focused`, `tab.renamed`
- `window.added`, `window.removed`, `window.focused`, `window.renamed`
- `workspace.added`, `workspace.removed`, `workspace.focused`,
  `workspace.renamed`
- `domain.attached`, `domain.detached`

### 5.2 Pane metadata events

- `pane.cwdChanged`
- `pane.titleChanged`
- `pane.foregroundProcessChanged`
- `pane.progressChanged`
- `pane.userVarChanged`
- `pane.bell`
- `pane.paletteChanged`
- `pane.selectionChanged`

### 5.3 Semantic shell events

Promote OSC 133/semantic-zone transitions to mux notifications:

- `shell.promptStarted`
- `shell.commandInput`
- `shell.commandStarted`
- `shell.commandOutput`
- `shell.commandFinished`

Include a command correlation ID, pane ref, command text when available, output
range, cwd, timestamps, and exit status.

Do not infer command completion by scanning prompts when explicit shell
integration data exists.

### 5.4 Delivery and backpressure

Each event has:

- monotonic sequence number;
- topology revision;
- timestamp;
- mux epoch;
- source object ref.

Pane output may be coalesced. When a slow client falls behind, emit
`stream.gap` with the lost range and require the client to call `pane.readText`
to resynchronize.

---

## 6. SSH and persistent mux behavior

### 6.1 Pi running in a remote SSH pane

The remote mux server exports its local automation socket into the remote Pi
process. Pi connects to the remote mux directly.

Mux-owned operations execute remotely. GUI-owned operations cross the existing
SSH mux connection using new typed automation PDUs.

### 6.2 Remote/local ID translation

`ClientDomain` must translate object refs between remote mux IDs and local GUI
IDs. Never expose the current implementation's incidental local remapping to the
Pi extension.

### 6.3 Disconnected GUI

When the GUI disconnects:

- remote/local Pi remains connected to its mux;
- managed commands continue;
- topology and output subscriptions continue where possible;
- GUI requests fail clearly or remain pending only when explicitly requested;
- Pi client state is replayed to the GUI after reattachment.

### 6.4 Version compatibility

Automation protocol compatibility is negotiated separately from the binary mux
codec version.

When relaying through SSH, every hop advertises supported automation features.
Unsupported GUI methods return `UNSUPPORTED_FEATURE` with the missing hop.

---

## 7. Pi extension design

### 7.1 Package layout

Create a standalone Pi package, provisionally:

```text
pi-wezterm/
├── package.json
├── src/
│   ├── extension.ts
│   ├── client.ts
│   ├── protocol.generated.ts
│   ├── tools/
│   ├── callbacks/
│   ├── context.ts
│   ├── status.ts
│   └── reconnect.ts
└── tests/
```

Install it as a global Pi extension or package. Project repositories must not
need to carry integration code.

### 7.2 Extension lifecycle mapping

On `session_start`:

- read `WEZTERM_AUTOMATION_SOCKET` and `WEZTERM_PANE`;
- connect and negotiate capabilities;
- register Pi callbacks;
- publish Pi session identity, cwd, session file/name, model, and thinking
  level;
- restore the pane/session binding after `/new`, `/resume`, `/fork`, or reload.

On Pi events publish compact state updates:

- `session_info_changed`
- `model_select`
- `thinking_level_select`
- `agent_start`
- `turn_start`
- `tool_execution_start/update/end`
- `turn_end`
- `agent_settled`
- `session_shutdown`

On `session_shutdown`:

- mark the session detached;
- clear transient pane badges/status;
- close subscriptions cleanly.

### 7.3 Pi client state

Publish a generic state object through `client.setState`:

```ts
interface PiTerminalState {
  sessionId: string;
  sessionName?: string;
  sessionFile?: string;
  cwd: string;
  model?: string;
  thinkingLevel?: string;
  phase: "idle" | "thinking" | "tool" | "waiting" | "error";
  activeTool?: string;
  turnIndex?: number;
  updatedAt: string;
}
```

WezTerm may use this for tab/pane badges and status rendering without knowing Pi
internals.

### 7.4 Pi callbacks callable by WezTerm

Register:

- `agent.getState`
- `agent.prompt`
- `agent.steer`
- `agent.followUp`
- `agent.abort`
- `agent.compact`
- `agent.newSession`
- `agent.setModel`
- `agent.setThinkingLevel`
- `agent.focus`
- `agent.describeSelection`

Map these to supported Pi extension APIs such as `sendUserMessage`, session
methods, model/thinking setters, and UI notifications. Return typed errors when
an operation is unavailable during the current streaming state.

### 7.5 Tools exposed to the model

Avoid one tool per low-level operation. Register a small coherent set with
strict discriminated schemas:

1. `terminal_run`
   - start a command in an idle pane from a fixed three-pane side-panel pool;
   - return only an opaque command ref.
2. `terminal_read`
   - read the retained response buffer by command ref.
3. `terminal_command`
   - inspect, interrupt, or explicitly close by command ref.
4. `terminal_input`
   - interact with the managed shell by command ref.

Pane, tab, and window refs never enter the Pi tool schema or command results;
the mux plugin owns that mapping.

The pool is finite and user-visible: the mux plugin creates the first side
panel during Pi's automation handshake in the origin tab, then later commands
create at most two stacked siblings inside it. Completed commands leave their
shells and output in place; later commands reuse idle panes. A pane moved out of
the origin tab is never reused for another Pi command, and Pi may explicitly
close managed panes.

### 7.6 Context and callbacks

Do not inject mux topology into every Pi turn. The integration starts with only
its own pane binding and connection status. Pi calls `terminal_read` for a
specific pane only when it needs output or metadata.

Managed commands run in a persistent shell pane. The managed shell installs a
prompt hook that reports each command's exit status through a private callback
FIFO; WezTerm then emits `command.finished` with the command ref, exit code, and
success flag. Nothing is printed into the terminal for protocol purposes. This
is event-driven—there is no command polling loop. The pane and its response
buffer remain available for inspection or interaction until explicitly closed.

### 7.7 Extension UI

Expose:

- `/terminal` command for connection and capability status;
- `/terminal-bind` to bind the current Pi session to another pane;
- `/terminal-clients` to inspect active automation clients;
- footer status showing connected/disconnected/degraded state;
- clear notifications when a GUI-only request cannot be fulfilled.

---

## 8. Useful higher-level integrations

### 8.1 Task workspace orchestration

Pi can create a dedicated workspace and layout for a task:

- editor pane;
- build/test pane;
- development server pane;
- log/watch pane;
- optional SSH domain panes.

It can preserve and restore that layout using stable refs and workspace
metadata.

### 8.2 Managed build and test feedback

Pi can:

- start a build in a visible pane;
- subscribe to command lifecycle events;
- parse failures from semantic output;
- focus/highlight the failing pane;
- retain the pane for user inspection;
- wait on the native exit status without polling.

### 8.3 Foreground-process-aware control

Before sending input, Pi can inspect the foreground process and alternate-screen
state. It can choose to:

- spawn a new pane instead of disrupting an editor;
- use bracketed paste for a shell;
- request confirmation before sending to an interactive TUI;
- focus the target without writing input.

### 8.4 Interactive escalation

When a task requires a true TTY, Pi can open a managed scratch pane for:

- password entry;
- REPLs;
- debuggers;
- database consoles;
- interactive rebases;
- remote SSH prompts.

Pi waits on pane/process lifecycle while the user interacts directly. No
pseudo-interactive subprocess needs to be hidden inside the agent tool runner.

### 8.5 Selection and semantic-zone handoff

Add terminal actions such as:

- **Ask Pi about selection**
- **Explain latest command output**
- **Fix command failure**
- **Send semantic zone to Pi**
- **Open this path in the Pi task workspace**

These actions call the registered Pi client callback with structured text and
origin metadata.

### 8.6 Pi activity visualization

Use client state callbacks to show:

- idle/running/waiting/error pane badges;
- active tool in the tab status;
- attention highlight when Pi requests input;
- completion notification when the Pi pane is unfocused;
- one-click focus on the active Pi pane.

### 8.7 Multi-agent coordination

With multiple Pi panes, WezTerm can expose all registered sessions. Pi can:

- assign subtasks to specific panes/workspaces;
- focus another agent;
- send a structured handoff callback;
- observe whether another agent is idle or running;
- collect results without parsing that agent's screen.

### 8.8 SSH-domain orchestration

Pi can create an `ssh-<host>` workspace, attach a domain, run remote commands,
monitor output, and detach while preserving remote sessions. The API keeps
remote and local refs explicit so commands cannot accidentally cross domains.

### 8.9 Service and port awareness

Managed commands may publish declared ports or parsed shell-integration
metadata. WezTerm can offer actions to:

- open a local URL;
- create an SSH tunnel;
- focus the server pane;
- notify Pi when a service exits;
- restart a service through an explicit managed-command call.

### 8.10 Session recovery

After GUI restart or SSH reattachment:

- the mux retains panes and managed commands;
- Pi reconnects through the exported socket;
- session-to-pane bindings are restored;
- WezTerm receives the latest Pi state snapshot;
- event subscribers resume from sequence numbers or receive a gap/resync event.

---

## 9. WezTerm implementation map

### 9.1 New modules/crates

Proposed structure:

```text
automation-protocol/
  src/lib.rs                 # serde request/response/event contracts
  schema/automation-v1.json  # generated canonical schema

mux/src/automation/
  mod.rs                     # service and routing
  auth.rs                    # peer identity and capability policy
  registry.rs                # client registration/state/callbacks
  events.rs                  # notification translation/subscriptions
  refs.rs                    # stable refs and stale-object validation

wezterm-mux-server-impl/src/automation.rs
wezterm-client/src/automation.rs
wezterm-gui/src/automation.rs
lua-api-crates/mux/src/automation.rs
```

Exact crate boundaries should follow upstream maintainer preference, but the
protocol types must not live in GUI-only code.

### 9.2 Existing files likely touched

- `mux/src/lib.rs`
  - automation notifications and registry hooks.
- `mux/src/domain.rs`
  - export automation endpoint and origin metadata to panes.
- `codec/src/lib.rs`
  - automation relay PDUs and codec-version additions.
- `wezterm-mux-server-impl/src/sessionhandler.rs`
  - relay requests/events across mux connections.
- `wezterm-client/src/domain.rs`
  - remote/local ref translation.
- `wezterm-client/src/client.rs`
  - automation PDU client methods.
- `wezterm-gui/src/termwindow/mod.rs`
  - GUI routing, focus, UI requests, status invalidation.
- `wezterm-gui/src/scripting/guiwin.rs`
  - Lua automation callbacks/actions.
- `config/src/config.rs`
  - automation endpoint and capability rules.
- terminal semantic-zone/parser modules
  - command lifecycle notifications.

### 9.3 Schema generation

Maintain one canonical schema and generate:

- Rust protocol types or schema assertions;
- TypeScript interfaces and validators;
- method/capability documentation;
- compatibility fixtures.

Do not maintain hand-written Rust and TypeScript contracts independently.

---

## 10. Delivery phases

### Phase 0 — ADR and contracts

Deliverables:

- architecture decision record;
- protocol naming and versioning rules;
- capability model;
- stable object-ref design;
- error code registry;
- canonical JSON schema;
- local and SSH sequence diagrams.

Exit criteria:

- Rust and TypeScript can validate the same hello/topology fixtures;
- maintainers agree on generic, non-Pi-specific core naming.

### Phase 1 — Read-only local control plane

Deliverables:

- automation socket lifecycle;
- peer authentication;
- handshake;
- `context.get`, `topology.snapshot`, `topology.subscribe`;
- pane metadata and bounded text reads;
- Pi extension connection/status;
- origin pane registration.

Exit criteria:

- Pi can identify its pane and describe the complete local/persistent mux layout;
- reconnect works after Pi reload and GUI restart;
- no subprocess or polling transport is used.

### Phase 2 — Native control operations

Deliverables:

- pane/tab/workspace focus and layout methods;
- spawn, split, move, resize, zoom, close;
- structured text/key input;
- stable refs and stale-object errors;
- full-authority Pi capability rule.

Exit criteria:

- Pi can create and manage a task workspace entirely through the API;
- race and stale-ID tests pass.

### Phase 3 — Managed commands and semantic shell events

Deliverables:

- `command.run/wait/cancel/getResult`;
- native command refs;
- output subscription backpressure;
- OSC 133 command lifecycle events;
- prompt waiting and semantic-zone reads.

Exit criteria:

- build/test/server workflows require no output polling;
- exit codes remain available after pane removal.

### Phase 4 — Bidirectional callbacks and GUI UI

Deliverables:

- client callback registry and `client.call`;
- Pi prompt/steer/abort/state callbacks;
- native selector/prompt/confirmation requests;
- pane badges, highlights, and top-bar status;
- Lua events and generic call-client action.

Exit criteria:

- a WezTerm action can send selection/context to the Pi session attached to a
  pane;
- Pi can request native GUI interaction and receive a typed response.

### Phase 5 — SSH mux relay

Deliverables:

- automation relay PDUs;
- remote/local object-ref translation;
- remote Pi registration;
- GUI reattach state replay;
- capability enforcement at every hop.

Exit criteria:

- Pi in an SSH mux pane can control its remote workspace and request local GUI
  focus/UI;
- sessions remain functional across SSH GUI disconnect/reattach.

### Phase 6 — Hardening and upstream preparation

Deliverables:

- protocol fuzzing;
- load and backpressure tests;
- audit/redaction review;
- Windows named-pipe implementation;
- compatibility matrix;
- public WezTerm docs and examples;
- standalone TypeScript client package;
- upstream RFC/PR split into reviewable layers.

Exit criteria:

- no unbounded queues;
- malformed clients cannot crash or stall the mux;
- old mux clients continue to negotiate or fail clearly;
- API documentation is complete enough for a non-Pi automation client.

---

## 11. Test strategy

### 11.1 Protocol tests

- golden request/response/event fixtures;
- Rust ↔ TypeScript schema conformance;
- version negotiation;
- malformed JSON and oversized record rejection;
- cancellation and duplicate request IDs;
- stable error code assertions.

### 11.2 Authorization tests

- socket permissions;
- same-UID peer acceptance;
- other-UID rejection where testable;
- capability allow/deny rules;
- callback registration restrictions;
- audit redaction.

### 11.3 Mux integration tests

- topology revisions;
- pane lifecycle events;
- stable refs and stale generation handling;
- focus/split/move/close behavior;
- output backpressure and gap recovery;
- managed command exit cache;
- GUI absent/reattached behavior.

### 11.4 Pi extension tests

Use a fake automation server to test:

- handshake and reconnect;
- Pi lifecycle-to-client-state mapping;
- tool schemas and method routing;
- context injection size and revision behavior;
- inbound prompt/steer/abort callbacks;
- session replacement and shutdown cleanup;
- typed error rendering.

### 11.5 End-to-end tests

Automate state and text assertions for:

- local GUI mux;
- standalone persistent mux;
- GUI restart;
- Pi `/reload`, `/new`, `/resume`, and `/fork`;
- multiple Pi panes;
- SSH mux disconnect/reattach;
- command workspace orchestration;
- user-driven selector/confirmation callbacks.

Tests should assert protocol state, topology, pane text, and process exit status.
They should not rely on compositor automation or visual image comparison.

---

## 12. Observability

Expose diagnostics through:

- `wezterm cli automation-clients` for human inspection only;
- debug overlay entries for connection and protocol errors;
- structured logs with connection/session IDs;
- counters for requests, errors, queue depth, dropped/coalesced events, and
  callback latency;
- Pi `/terminal` status with negotiated version and capabilities.

The diagnostic CLI is not the Pi transport.

---

## 13. Documentation deliverables

WezTerm documentation:

- Automation API overview;
- protocol and schema reference;
- security/capability guide;
- Lua automation callbacks;
- local, persistent mux, and SSH examples;
- client implementation guide.

Pi package documentation:

- installation and discovery;
- tools and commands;
- trusted/full-control behavior;
- terminal context injection;
- remote SSH behavior;
- troubleshooting and reconnect semantics.

---

## 14. Definition of done

The integration is complete when:

1. Pi discovers its pane and full mux context without subprocesses or polling.
2. Pi controls panes, tabs, workspaces, domains, and managed commands through
   typed native calls.
3. WezTerm calls registered Pi callbacks for prompt, steer, abort, state, and
   selection handoff.
4. Pi lifecycle state appears in WezTerm through generic client metadata.
5. Native selectors/prompts/confirmations support bidirectional interactions.
6. Local, persistent, and SSH mux domains share one protocol model.
7. GUI disconnects do not terminate Pi, panes, or managed commands.
8. Reconnect and event resynchronization are deterministic.
9. Full local authority is explicit, authenticated, and auditable.
10. No integration path relies on key injection, screen scraping, shell IPC,
    compositor automation, or per-operation CLI subprocesses.
11. The WezTerm core API is generic and documented for other automation
    clients.
12. Protocol, Rust, TypeScript, mux, Pi extension, and SSH end-to-end tests pass.

---

## 15. Recommended first implementation slice

Start with the smallest vertical slice that proves the architecture:

1. add `WEZTERM_AUTOMATION_SOCKET` to locally spawned panes;
2. implement `automation.hello` and same-UID authentication;
3. implement `context.get` and `topology.snapshot`;
4. create a minimal Pi extension that connects on `session_start`;
5. register `terminal_context`;
6. publish Pi idle/running state to WezTerm;
7. render that state in a generic Lua callback;
8. test persistent mux GUI restart and Pi `/reload` reconnection.

Only after this slice is reliable should control methods, output subscriptions,
and SSH relaying be added.
