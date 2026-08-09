// Small, visible Pi ↔ WezTerm command integration.
// @ts-nocheck

import { Type } from "typebox";
import { AutomationClient } from "./protocol.ts";

const SOCKET = process.env.WEZTERM_AUTOMATION_SOCKET;
const PANE_ID = Number.parseInt(process.env.WEZTERM_PANE ?? "", 10);
const EXTENSION_ID = "pi-wezterm";

export default function (pi) {
  if (!SOCKET || !Number.isFinite(PANE_ID)) return;

  let client;
    let ctxRef;

    const notify = (message, level = "info") => {
      try {
        ctxRef?.ui.notify(message, level);
      } catch {
        // Pi may have replaced this extension context during a reload.
      }
    };

    const connect = async (ctx) => {
      ctxRef = ctx;
      if (client?.connected) return client;

      let nextClient;
      nextClient = new AutomationClient({
        socketPath: SOCKET,
        paneId: PANE_ID,
        clientName: "pi",
        clientVersion: "0.1.0",
        onConnectionChange: (connected, reason) => {
          if (client !== nextClient) return;
          try {
            ctxRef?.ui.setStatus(
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

          const data = event.data ?? {};
          const commandId = data.commandId ?? "unknown-command";
          const success = data.success === true;
          const status = success
            ? "succeeded"
            : `exited (${data.exitCode ?? data.signal ?? "unknown"})`;
          const output = typeof data.output === "string" ? data.output : "";
          const trigger = `[terminal command finished] ${commandId} ${status}\n${output}`;

          notify(`Terminal ${commandId} ${status}`, success ? "info" : "warning");
          // The command ID is the only orchestration handle exposed to Pi;
          // pane/window/tab identity stays inside the mux plugin.
          pi.sendUserMessage(trigger, { deliverAs: "followUp" });
        },
      });

      client = nextClient;
      try {
        await nextClient.connect();
        return nextClient;
      } catch (error) {
        notify(`WezTerm unavailable: ${error instanceof Error ? error.message : String(error)}`, "warning");
        if (client === nextClient) {
          nextClient.disconnect();
          client = undefined;
        }
        return undefined;
      }
    };

    const call = async (method, params, signal, ctx) => {
      if (signal?.aborted) throw new Error("Operation cancelled");
      const connected = await connect(ctx);
      if (!connected) throw new Error("WezTerm Automation API is unavailable");
      return connected.request(method, params);
    };

    const result = (value) => ({
      content: [{ type: "text", text: typeof value === "string" ? value : JSON.stringify(value, null, 2) }],
      details: value,
    });

    pi.registerTool({
      name: "terminal_run",
      label: "Run Terminal Command",
      description:
        "Start a visible command asynchronously in Pi's managed terminal pool and return its opaque command ID. Pass argv directly; use [\"bash\", \"-lc\", SCRIPT] for shell syntax. Never sleep or poll for completion: a command.finished follow-up arrives automatically with status and output.",
      parameters: Type.Object({
        command: Type.Array(Type.String()),
        cwd: Type.Optional(Type.String()),
      }),
      async execute(_toolCallId, params, signal, _onUpdate, ctx) {
        const command = await call("command.run", {
          command: params.command,
          paneId: PANE_ID,
          cwd: params.cwd,
        }, signal, ctx);
        return result({ commandId: command.commandId, running: true });
      },
    });

    pi.registerTool({
      name: "terminal_read",
      label: "Read Terminal Output",
      description:
        "Read a command's retained output buffer, optionally by range. Use this for output needed while a command is still running or for later inspection; do not repeatedly call it to detect completion because command.finished is delivered automatically.",
      parameters: Type.Object({
        commandId: Type.String(),
        start: Type.Optional(Type.Integer()),
        end: Type.Optional(Type.Integer()),
      }),
      async execute(_toolCallId, params, signal, _onUpdate, ctx) {
        const response = await call("command.read", {
          commandId: params.commandId,
          start: params.start,
          end: params.end,
        }, signal, ctx);
        return result(response.output ?? "");
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
      async execute(_toolCallId, params, signal, _onUpdate, ctx) {
        const method = params.operation === "cancel"
          ? "command.cancel"
          : params.operation === "close"
            ? "command.close"
            : "command.getResult";
        const response = await call(method, { commandId: params.commandId }, signal, ctx);
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
      async execute(_toolCallId, params, signal, _onUpdate, ctx) {
        const response = await call("command.input", {
          commandId: params.commandId,
          text: params.text,
        }, signal, ctx);
        return result(response);
      },
    });

    pi.on("session_start", async (_event, ctx) => {
      ctxRef = ctx;
      await connect(ctx);
    });

  pi.on("session_shutdown", () => {
    ctxRef = undefined;
    client?.disconnect();
    client = undefined;
  });
}
