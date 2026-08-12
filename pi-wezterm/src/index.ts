// Small, visible Pi ↔ WezTerm command integration.

import { Type } from "typebox";
import { AutomationClient } from "./protocol.ts";

const TERM_PROGRAM = process.env.TERM_PROGRAM ?? "";
// TERM_PROGRAM is authoritative here. Kitty variables can be inherited by a
// WezTerm pane, so do not reject WezTerm merely because KITTY_* is present.
const IS_WEZTERM = TERM_PROGRAM === "WezTerm";
const MUX_SOCKET = process.env.WEZTERM_UNIX_SOCKET;
const SOCKET = process.env.WEZTERM_AUTOMATION_SOCKET
  ?? (MUX_SOCKET ? `${MUX_SOCKET}.automation` : undefined);
const PANE_ID = Number.parseInt(process.env.WEZTERM_PANE ?? "", 10);
const EXTENSION_ID = "pi-wezterm";

type PiContext = {
  ui?: {
    notify?: (message: string, level?: string) => void;
    setStatus?: (id: string, status: string) => void;
  };
};

type PiHost = {
  registerTool: (tool: unknown) => void;
  on: (event: string, handler: (...args: unknown[]) => void) => void;
  sendUserMessage: (message: string, options?: { deliverAs?: string }) => void;
};

type CommandResponse = {
  commandId: string;
  paneId?: number;
  output?: string;
  running?: boolean;
  cancelled?: boolean;
  closed?: boolean;
  exitCode?: number | null;
  signal?: string | null;
  success?: boolean | null;
  reason?: string | null;
};

type ToolContext = PiContext;
type JsonRecord = Record<string, unknown>;

function asRecord(value: unknown): JsonRecord {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? value as JsonRecord
    : {};
}

function asCommandResponse(value: unknown): CommandResponse {
  const record = asRecord(value);
  return { commandId: String(record.commandId ?? "unknown-command"), ...record } as CommandResponse;
}

export default function (pi: PiHost) {
  // Pi discovers extensions globally. Do not register any tools or lifecycle
  // handlers in Kitty (or another terminal), even if stale WEZTERM_* variables
  // happen to be inherited by the process.
  if (!IS_WEZTERM || !SOCKET || !Number.isFinite(PANE_ID)) return;

  let client: AutomationClient | undefined;
    let ctxRef: PiContext | undefined;
    const activeCommands = new Map<string, string>();

    const shellQuote = (value: string): string => {
      const text = String(value);
      return text === "" ? "''" : `'${text.replace(/'/g, "'\\''")}'`;
    };

    const displayCommand = (argv: string[], cwd?: string): string => {
      const command = argv.map(shellQuote).join(" ");
      return cwd ? `cd -- ${shellQuote(cwd)} && ${command}` : command;
    };

    const notify = (message: string, level = "info"): void => {
      try {
        ctxRef?.ui?.notify?.(message, level);
      } catch {
        // Pi may have replaced this extension context during a reload.
      }
    };

    const connect = async (ctx: PiContext): Promise<AutomationClient | undefined> => {
      ctxRef = ctx;
      if (client?.connected) return client;

      let nextClient: AutomationClient;
      nextClient = new AutomationClient({
        socketPath: SOCKET,
        paneId: PANE_ID,
        clientName: "pi",
        clientVersion: "0.1.0",
        onConnectionChange: (connected, reason) => {
          if (client !== nextClient) return;
          try {
            ctxRef?.ui?.setStatus?.(
              EXTENSION_ID,
              connected
                ? "wezterm: connected"
                : reason
                  ? `wezterm: ${reason}`
                  : "wezterm: disconnected",
            );
          } catch {
            // Ignore callbacks for a retired Pi context.
          }
        },
        onEvent: (event) => {
          if (client !== nextClient || event?.event !== "command.finished") return;

          const data = asRecord(event.data);
          const commandId = String(data.commandId ?? "unknown-command");
          const success = data.success === true;
          const status = success
            ? "succeeded"
            : `exited (${data.exitCode ?? data.signal ?? "unknown"})`;
          const commandText = activeCommands.get(commandId);
          const trigger = commandText
            ? `[terminal command finished] ${commandId} ${status}\n$ ${commandText}`
            : `[terminal command finished] ${commandId} ${status}`;

          activeCommands.delete(commandId);
          notify(`Terminal ${commandId} ${status}`, success ? "info" : "warning");
          // The command ID is the only orchestration handle exposed to Pi;
          // pane/window/tab identity stays inside the mux plugin.
          pi.sendUserMessage(trigger, { deliverAs: "followUp" });
        },
      });

      client = nextClient;
      let lastError: unknown;
      // The mux starts the automation listener asynchronously. Retry transient
      // startup/restart races here instead of surfacing a misleading
      // "connection refused" error on the first Pi tool call.
      for (let attempt = 0; attempt < 8; attempt += 1) {
        client = nextClient;
        try {
          await nextClient.connect();
          return nextClient;
        } catch (error) {
          lastError = error;
          if (client === nextClient) {
            nextClient.disconnect();
            client = undefined;
          }
          const message = error instanceof Error ? error.message : String(error);
          if (!/connection refused|connect|unavailable|closed/i.test(message)) break;
          await new Promise((resolve) => setTimeout(resolve, 100 * (attempt + 1)));
        }
      }
      notify(
        `WezTerm unavailable after retrying: ${lastError instanceof Error ? lastError.message : String(lastError)}`,
        "warning",
      );
      return undefined;
    };

    const call = async (
      method: string,
      params: Record<string, unknown>,
      signal: AbortSignal | undefined,
      ctx: PiContext,
    ): Promise<unknown> => {
      if (signal?.aborted) throw new Error("Operation cancelled");
      const connected = await connect(ctx);
      if (!connected) throw new Error("WezTerm Automation API is unavailable");
      return connected.request(method, params as never);
    };

    const result = (value: unknown) => ({
      content: [{ type: "text", text: typeof value === "string" ? value : JSON.stringify(value, null, 2) }],
      details: value,
    });

    pi.registerTool({
      name: "terminal_run",
      label: "Run Terminal Command",
      description:
        "Start a visible command asynchronously and show the exact command plus its opaque command ID. Pass argv directly; use [\"bash\", \"-lc\", SCRIPT] for shell syntax. Never sleep or poll for completion: command.finished arrives automatically.",
      parameters: Type.Object({
        command: Type.Array(Type.String()),
        cwd: Type.Optional(Type.String()),
      }),
      async execute(_toolCallId: string, params: JsonRecord, signal: AbortSignal | undefined, _onUpdate: unknown, ctx: ToolContext) {
        const command = asCommandResponse(await call("command.run", {
          command: params.command,
          paneId: PANE_ID,
          cwd: params.cwd,
        }, signal, ctx));
        const commandText = displayCommand(params.command as string[], params.cwd as string | undefined);
        activeCommands.set(command.commandId, commandText);
        return {
          content: [{
            type: "text",
            text: `[terminal command started] ${command.commandId}\n$ ${commandText}`,
          }],
          details: {
            commandId: command.commandId,
            command: params.command,
            cwd: params.cwd,
            running: true,
          },
        };
      },
    });

    pi.registerTool({
      name: "terminal_read",
      label: "Read Terminal Output",
      description:
        "Read output captured only for the specified command, even after its pane is reused. Optional start/end values are zero-based line offsets within that command's output. Read only when the output is relevant; command.finished reports completion automatically.",
      parameters: Type.Object({
        commandId: Type.String(),
        start: Type.Optional(Type.Integer({ description: "First command-output line to include." })),
        end: Type.Optional(Type.Integer({ description: "Exclusive command-output line boundary." })),
      }),
      async execute(_toolCallId: string, params: JsonRecord, signal: AbortSignal | undefined, _onUpdate: unknown, ctx: ToolContext) {
        const response = asCommandResponse(await call("command.read", {
          commandId: params.commandId,
          start: params.start,
          end: params.end,
        }, signal, ctx));
        return {
          content: [{ type: "text", text: response.output ?? "" }],
          details: response,
        };
      },
    });

    pi.registerTool({
      name: "terminal_command",
      label: "Cancel, Inspect, or Close Command",
      description: "Perform a one-shot result inspection, send Ctrl-C with cancel, or explicitly close a managed command pane. Do not poll result for completion; wait for the automatic command.finished follow-up.",
      parameters: Type.Object({
        operation: Type.String({ description: "result, cancel, or close" }),
        commandId: Type.String(),
      }),
      async execute(_toolCallId: string, params: JsonRecord, signal: AbortSignal | undefined, _onUpdate: unknown, ctx: ToolContext) {
        const method = params.operation === "cancel"
          ? "command.cancel"
          : params.operation === "close"
            ? "command.close"
            : "command.getResult";
        const response = asCommandResponse(await call(method, { commandId: params.commandId }, signal, ctx));
        return result({
          commandId: response.commandId ?? params.commandId,
          running: response.running,
          cancelled: response.cancelled,
          closed: response.closed,
          exitCode: response.exitCode,
          signal: response.signal,
          success: response.success,
          reason: response.reason,
        });
      },
    });

    pi.registerTool({
      name: "terminal_input",
      label: "Interact with Terminal",
      description: "Send exact keyboard-style input to a running managed command. End each submitted line with \\n, and send multiple responses as separate calls when interaction requires it. Use \\u0003 for Ctrl-C. Completion still arrives through the automatic command.finished follow-up; do not poll afterward.",
      parameters: Type.Object({
        commandId: Type.String(),
        text: Type.String({
          description: "Exact terminal input. Include a trailing \\n to submit a line.",
        }),
      }),
      async execute(_toolCallId: string, params: JsonRecord, signal: AbortSignal | undefined, _onUpdate: unknown, ctx: ToolContext) {
        const response = asCommandResponse(await call("command.input", {
          commandId: params.commandId,
          text: params.text,
        }, signal, ctx));
        return result(response);
      },
    });

  pi.on("session_start", (_event: unknown, ctx: unknown) => {
    // Connect lazily on the first tool call. The persistent mux/automation
    // listener may still be starting when Pi loads its extensions.
    ctxRef = ctx as PiContext;
  });

  pi.on("session_shutdown", () => {
    ctxRef = undefined;
    client?.disconnect();
    client = undefined;
  });
}
