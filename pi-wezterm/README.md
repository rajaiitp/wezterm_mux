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
`terminal_read`, `terminal_command`, and `terminal_input`. Pi receives an
opaque command ID plus a concise `$ ...` display line showing the exact
argv/cwd that was started; pane, tab, window, and pool identity stay inside the
mux plugin.
Each Pi pane owns a fixed pool of up to three visible command panes: one side
panel, then stacked panes within it. Commands reuse idle shells instead of
opening or closing panes repeatedly, while each command ID retains its own
immutable output and status. A stale command ID can still read its captured
output but cannot input to, cancel, or close a pane that has since been reused.

Completion is a compact `command.finished` callback, not a polling loop. The
callback reports status only; Pi calls `terminal_read` when that command's
output is relevant. A long-lived interactive program such as SSH keeps its pane
ownership until it exits, so repeated `terminal_input` calls continue to reach
the same session. `terminal_input` writes keyboard-style bytes directly rather
than using bracketed paste: a trailing `\n` submits a line and control bytes
such as Ctrl-C take effect.

The extension does not inject a mux/topology snapshot into Pi context. All
operations use the native typed JSON-RPC socket; no `wezterm cli` subprocesses
or screen scraping are used.
