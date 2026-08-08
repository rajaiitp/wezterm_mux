# pi-wezterm

Native Pi client for the WezTerm Automation API.

## Installation

Install or symlink `src/index.ts` as a trusted global Pi extension:

```sh
mkdir -p ~/.pi/agent/extensions/pi-wezterm
ln -sf "$PWD/src/index.ts" ~/.pi/agent/extensions/pi-wezterm/index.ts
ln -sf "$PWD/src/protocol.ts" ~/.pi/agent/extensions/pi-wezterm/protocol.ts
```

The extension activates only when WezTerm provides `WEZTERM_AUTOMATION_SOCKET`.
The WezTerm mux server injects that variable together with `WEZTERM_PANE` into
new panes.

## Tools

The extension exposes a small command interface: `terminal_run`,
`terminal_read`, `terminal_command`, and `terminal_input`. Pi sees only opaque
command IDs and command output; pane, tab, and window identity stays inside the
mux plugin. Each Pi pane owns a fixed pool of up to three visible command
panes: one side panel, then stacked panes within it. Commands reuse idle shell
panes instead of opening or closing panes repeatedly. Completion is an event
callback, not a polling loop; panes remain visible, readable, and interactive.
The extension does not inject a full mux/topology snapshot into Pi context. All
operations use the native typed JSON-RPC socket; no `wezterm cli` subprocesses
or screen scraping are used.
