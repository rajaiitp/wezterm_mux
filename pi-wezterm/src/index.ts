// Native callback-driven Pi ↔ WezTerm integration.
// This extension intentionally becomes a no-op outside a WezTerm pane.
// @ts-nocheck

import { Type } from "typebox";
import { AutomationClient } from "./protocol.ts";

const SOCKET = process.env.WEZTERM_AUTOMATION_SOCKET;
const PANE_ID = Number.parseInt(process.env.WEZTERM_PANE ?? "", 10);
const EXTENSION_ID = "pi-wezterm";

function hasTerminal(): boolean {
  return !!SOCKET && Number.isFinite(PANE_ID);
}

function text(value: unknown): string {
  if (typeof value === "string") return value;
  return JSON.stringify(value, null, 2);
}

function result(value: unknown) {
  return {
    content: [{ type: "text", text: text(value) }],
    details: value,
  };
}

export default function (pi) {
  if (!hasTerminal()) return;

  let client: AutomationClient | undefined;
  let ctxRef;
  let currentState = {
    sessionId: undefined,
    sessionName: undefined,
    cwd: process.cwd(),
    phase: "idle",
    activeTool: undefined,
    updatedAt: new Date().toISOString(),
  };

  const updateStatus = (connected: boolean, reason?: string) => {
    const ctx = ctxRef;
    if (!ctx) return;

    try {
      ctx.ui.setStatus(
        EXTENSION_ID,
        connected ? "wezterm: connected" : reason ? `wezterm: ${reason}` : "wezterm: disconnected",
      );
    } catch (error) {
      // Socket callbacks can arrive after Pi replaces or reloads the session.
      // Never let a late callback use the retired extension context.
      if (error instanceof Error && error.message.includes("This extension ctx is stale")) {
        if (ctxRef === ctx) ctxRef = undefined;
        return;
      }
      throw error;
    }
  };

  const publishState = () => {
    currentState.updatedAt = new Date().toISOString();
    if (!client?.connected) return;
    void client.request("client.setState", currentState).catch(() => undefined);
  };

  const onClientCall = async (call) => {
    const params = call.params && typeof call.params === "object" ? call.params : {};
    switch (call.method) {
      case "agent.getState":
        return currentState;
      case "agent.prompt":
        pi.sendUserMessage(String(params.text ?? ""));
        return { accepted: true };
      case "agent.steer":
        pi.sendUserMessage(String(params.text ?? ""), { deliverAs: "steer" });
        return { accepted: true };
      case "agent.followUp":
        pi.sendUserMessage(String(params.text ?? ""), { deliverAs: "followUp" });
        return { accepted: true };
      case "agent.abort":
        ctxRef?.abort();
        return { accepted: true };
      case "agent.compact":
        pi.sendUserMessage("/compact", { deliverAs: "followUp" });
        return { accepted: true };
      case "agent.newSession":
        pi.sendUserMessage("/new", { deliverAs: "followUp" });
        return { accepted: true };
      case "agent.setThinkingLevel":
        pi.setThinkingLevel(String(params.level ?? "medium"));
        return { accepted: true };
      default:
        throw new Error(`Unknown Pi callback: ${call.method}`);
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
        // Ignore callbacks from a socket belonging to a replaced/reloaded session.
        if (client !== nextClient) return;
        updateStatus(connected, reason);
      },
      onClientCall,
      onEvent: (event) => {
        if (client !== nextClient) return;
        if (event?.event === "pane.removed" && event.data?.paneId === PANE_ID) {
          nextClient.disconnect();
        }
      },
    });
    client = nextClient;
    try {
      await nextClient.connect();
      await nextClient.subscribe();
      updateStatus(true);
      publishState();
      return nextClient;
    } catch (error) {
      updateStatus(false, error instanceof Error ? error.message : String(error));
      if (client === nextClient) {
        nextClient.disconnect();
        client = undefined;
      }
      return undefined;
    }
  };

  const call = async (method, params, signal) => {
    if (signal?.aborted) throw new Error("Operation cancelled");
    const connected = await connect(ctxRef);
    if (!connected) throw new Error("WezTerm Automation API is unavailable");
    return connected.request(method, params);
  };

  pi.registerTool({
    name: "terminal_context",
    label: "Terminal Context",
    description: "Inspect the native WezTerm mux topology, origin pane, workspaces, tabs, and domains.",
    parameters: Type.Object({}),
    async execute(_toolCallId, _params, signal, _onUpdate, ctx) {
      ctxRef = ctx;
      return result(await call("context.get", {}, signal));
    },
  });

  pi.registerTool({
    name: "terminal_read",
    label: "Read Terminal",
    description: "Read structured pane metadata, text, semantic zones, or the complete mux snapshot.",
    parameters: Type.Object({
      operation: Type.String({ description: "snapshot, pane, text, or semantic-zones" }),
      paneId: Type.Optional(Type.Integer()),
      start: Type.Optional(Type.Integer()),
      end: Type.Optional(Type.Integer()),
    }),
    async execute(_toolCallId, params, signal, _onUpdate, ctx) {
      ctxRef = ctx;
      const paneId = params.paneId ?? PANE_ID;
      switch (params.operation) {
        case "snapshot":
          return result(await call("topology.snapshot", {}, signal));
        case "pane":
          return result(await call("pane.get", { paneId }, signal));
        case "semantic-zones":
          return result(await call("pane.getSemanticZones", { paneId }, signal));
        case "text":
          return result(await call("pane.readText", {
            paneId,
            start: params.start,
            end: params.end,
          }, signal));
        default:
          throw new Error(`Unknown terminal_read operation: ${params.operation}`);
      }
    },
  });

  pi.registerTool({
    name: "terminal_control",
    label: "Control Terminal",
    description: "Focus, split, close, zoom, rename, or otherwise manage native WezTerm topology.",
    parameters: Type.Object({
      operation: Type.String({ description: "focus, split, close, zoom, tab-focus, or workspace-rename" }),
      paneId: Type.Optional(Type.Integer()),
      tabId: Type.Optional(Type.Integer()),
      text: Type.Optional(Type.String()),
      oldWorkspace: Type.Optional(Type.String()),
      newWorkspace: Type.Optional(Type.String()),
      zoomed: Type.Optional(Type.Boolean()),
      direction: Type.Optional(Type.String()),
      percent: Type.Optional(Type.Integer()),
      command: Type.Optional(Type.Array(Type.String())),
      cwd: Type.Optional(Type.String()),
    }),
    async execute(_toolCallId, params, signal, _onUpdate, ctx) {
      ctxRef = ctx;
      const paneId = params.paneId ?? PANE_ID;
      switch (params.operation) {
        case "focus":
          return result(await call("pane.focus", { paneId }, signal));
        case "close":
          return result(await call("pane.close", { paneId }, signal));
        case "zoom":
          return result(await call("pane.setZoomed", { paneId, zoomed: params.zoomed ?? true }, signal));
        case "split":
          return result(await call("pane.split", {
            paneId,
            direction: params.direction,
            percent: params.percent,
            command: params.command,
            cwd: params.cwd,
          }, signal));
        case "tab-focus":
          return result(await call("tab.focus", { tabId: params.tabId }, signal));
        case "workspace-rename":
          return result(await call("workspace.rename", {
            oldWorkspace: params.oldWorkspace,
            newWorkspace: params.newWorkspace,
          }, signal));
        default:
          throw new Error(`Unknown terminal_control operation: ${params.operation}`);
      }
    },
  });

  pi.registerTool({
    name: "terminal_run",
    label: "Run Terminal Command",
    description: "Run a managed command in a native WezTerm pane and return its command and pane references.",
    parameters: Type.Object({
      command: Type.Array(Type.String()),
      paneId: Type.Optional(Type.Integer()),
      cwd: Type.Optional(Type.String()),
      workspace: Type.Optional(Type.String()),
    }),
    async execute(_toolCallId, params, signal, _onUpdate, ctx) {
      ctxRef = ctx;
      currentState.phase = "tool";
      currentState.activeTool = "terminal_run";
      publishState();
      try {
        return result(await call("command.run", {
          command: params.command,
          paneId: params.paneId ?? PANE_ID,
          cwd: params.cwd,
          workspace: params.workspace,
        }, signal));
      } finally {
        currentState.phase = "idle";
        currentState.activeTool = undefined;
        publishState();
      }
    },
  });

  pi.registerTool({
    name: "terminal_command",
    label: "Manage Terminal Command",
    description: "Inspect or cancel a managed WezTerm command without polling a human-oriented CLI.",
    parameters: Type.Object({
      operation: Type.String({ description: "result or cancel" }),
      commandId: Type.Optional(Type.String()),
      paneId: Type.Optional(Type.Integer()),
    }),
    async execute(_toolCallId, params, signal, _onUpdate, ctx) {
      ctxRef = ctx;
      const method = params.operation === "cancel" ? "command.cancel" : "command.getResult";
      return result(await call(method, {
        commandId: params.commandId,
        paneId: params.paneId,
      }, signal));
    },
  });

  pi.registerTool({
    name: "terminal_input",
    label: "Send Terminal Input",
    description: "Send explicit text to a native pane using WezTerm's structured input path.",
    parameters: Type.Object({
      text: Type.String(),
      paneId: Type.Optional(Type.Integer()),
    }),
    async execute(_toolCallId, params, signal, _onUpdate, ctx) {
      ctxRef = ctx;
      return result(await call("pane.sendText", {
        paneId: params.paneId ?? PANE_ID,
        text: params.text,
      }, signal));
    },
  });

  pi.registerCommand("terminal", {
    description: "Show WezTerm Automation API connection and current terminal context",
    handler: async (_args, ctx) => {
      ctxRef = ctx;
      const connected = await connect(ctx);
      if (!connected) return;
      const context = await connected.request("context.get", {});
      ctx.ui.notify(`WezTerm connected (pane ${PANE_ID})\n${text(context)}`, "info");
    },
  });

  pi.on("session_start", async (_event, ctx) => {
    ctxRef = ctx;
    currentState.sessionId = ctx.sessionManager.getSessionId?.();
    currentState.sessionName = ctx.sessionManager.getSessionName?.();
    currentState.cwd = process.cwd();
    currentState.phase = ctx.isIdle?.() === false ? "thinking" : "idle";
    await connect(ctx);
    publishState();
  });

  pi.on("session_info_changed", (_event, ctx) => {
    ctxRef = ctx;
    currentState.sessionName = pi.getSessionName?.();
    publishState();
  });

  pi.on("agent_start", (_event, ctx) => {
    ctxRef = ctx;
    currentState.phase = "thinking";
    publishState();
  });

  pi.on("agent_settled", (_event, ctx) => {
    ctxRef = ctx;
    currentState.phase = "idle";
    currentState.activeTool = undefined;
    publishState();
  });

  pi.on("tool_execution_start", (event, ctx) => {
    ctxRef = ctx;
    currentState.phase = "tool";
    currentState.activeTool = event.toolName;
    publishState();
  });

  pi.on("tool_execution_end", (_event, ctx) => {
    ctxRef = ctx;
    currentState.activeTool = undefined;
    publishState();
  });

  pi.on("before_agent_start", async (_event, ctx) => {
    ctxRef = ctx;
    const connected = await connect(ctx);
    if (!connected) return;
    try {
      const context = await connected.request("context.get", {});
      return {
        message: {
          customType: "wezterm-terminal-context",
          content: `Current native WezTerm context:\n${text(context)}`,
          display: false,
        },
      };
    } catch {
      return undefined;
    }
  });

  pi.on("session_shutdown", () => {
    // Invalidate the context before closing the socket: its close callback may
    // run after this extension instance has been retired.
    ctxRef = undefined;
    currentState.phase = "idle";
    publishState();
    client?.disconnect();
    client = undefined;
  });
}
