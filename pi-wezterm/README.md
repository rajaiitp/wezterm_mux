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

The extension exposes `terminal_context`, `terminal_read`, `terminal_control`,
`terminal_run`, `terminal_command`, and `terminal_input`. All operations use
the native typed JSON-RPC socket; no `wezterm cli` subprocesses or screen
scraping are used.
