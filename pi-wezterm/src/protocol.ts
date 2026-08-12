import { createConnection, type Socket } from "node:net";

export type Json = null | boolean | number | string | Json[] | { [key: string]: Json | undefined };

type RpcResponse = {
  jsonrpc?: string;
  id?: string | number | null;
  result?: Json;
  error?: { code: number; message: string; data?: Json };
};

type RpcNotification = {
  jsonrpc?: string;
  method: string;
  params?: Json;
};

type Pending = {
  resolve: (value: Json) => void;
  reject: (error: Error) => void;
  timer: ReturnType<typeof setTimeout>;
};

const MAX_LINE_BYTES = 1024 * 1024;
const MAX_BUFFER_BYTES = MAX_LINE_BYTES * 2;

export type AutomationEvent = {
  event: string;
  revision: number;
  data: Json;
};

export type AutomationClientOptions = {
  socketPath: string;
  paneId?: number;
  clientName?: string;
  clientVersion?: string;
  onEvent?: (event: AutomationEvent) => void;
  onResync?: (snapshot: Json) => void;
  onConnectionChange?: (connected: boolean, reason?: string) => void;
};

export class AutomationClient {
  private socket?: Socket;
  private buffer = "";
  private nextId = 1;
  private pending = new Map<string, Pending>();
  private connectPromise?: Promise<void>;
  private closed = false;
  private lastRevision = 0;
  private connectionEpoch = 0;

  constructor(private readonly options: AutomationClientOptions) {}

  get connected(): boolean {
    return !!this.socket && !this.closed;
  }

  async connect(): Promise<void> {
    if (this.connected) return;
    if (this.connectPromise) return this.connectPromise;

    this.connectPromise = new Promise<void>((resolve, reject) => {
      const socket = createConnection(this.options.socketPath);
      this.socket = socket;
      this.closed = false;
      this.buffer = "";
      const epoch = ++this.connectionEpoch;
      socket.setEncoding("utf8");

      const fail = (error: Error) => {
        if (this.connectPromise) {
          this.connectPromise = undefined;
          reject(error);
        }
        this.failPending(error);
      };

      socket.once("connect", async () => {
        try {
          await this.request("automation.hello", {
            protocolVersion: 1,
            client: {
              name: this.options.clientName ?? "pi",
              version: this.options.clientVersion ?? "0.1.0",
              pid: process.pid,
            },
            origin: this.options.paneId === undefined ? undefined : { paneId: this.options.paneId },
            requestedCapabilities: [
              "topology.read",
              "pane.read",
              "pane.focus",
              "pane.input.text",
              "pane.create",
              "pane.close",
              "workspace.control",
              "command.run",
              "command.cancel",
              "command.input",
              "command.close",
              "client.callback",
            ],
          });
          await this.request("topology.subscribe", {
            sinceRevision: this.lastRevision || undefined,
          });
          this.options.onConnectionChange?.(true);
          if (this.connectionEpoch === epoch) {
            this.connectPromise = undefined;
            resolve();
          }
        } catch (error) {
          fail(error instanceof Error ? error : new Error(String(error)));
          socket.destroy();
        }
      });

      socket.on("data", (chunk: string) => {
        if (this.connectionEpoch === epoch) this.consume(chunk);
      });
      socket.on("error", (error: Error) => {
        this.options.onConnectionChange?.(false, error.message);
        fail(error);
      });
      socket.on("close", () => {
        if (this.connectionEpoch !== epoch) return;
        this.closed = true;
        this.socket = undefined;
        this.options.onConnectionChange?.(false, "connection closed");
        this.failPending(new Error("WezTerm automation connection closed"));
      });
    });

    return this.connectPromise;
  }

  async request<T extends Json = Json>(method: string, params?: Json, timeoutMs = 30000): Promise<T> {
    if (!this.socket || this.closed) throw new Error("WezTerm automation is not connected");
    const id = String(this.nextId++);
    const request = JSON.stringify({ jsonrpc: "2.0", id, method, params });
    if (Buffer.byteLength(request) > MAX_LINE_BYTES) {
      throw new Error(`WezTerm request is too large: ${method}`);
    }

    return new Promise<T>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`WezTerm request timed out: ${method}`));
      }, timeoutMs);
      timer.unref?.();
      this.pending.set(id, {
        resolve: (value) => resolve(value as T),
        reject,
        timer,
      });
      this.socket!.write(`${request}\n`, (error?: Error | null) => {
        if (error) {
          clearTimeout(timer);
          this.pending.delete(id);
          reject(error);
        }
      });
    });
  }

  disconnect(): void {
    this.closed = true;
    this.connectionEpoch += 1;
    this.socket?.end();
    this.socket?.destroy();
    this.socket = undefined;
    this.buffer = "";
    this.connectPromise = undefined;
    this.failPending(new Error("WezTerm automation disconnected"));
  }

  private consume(chunk: string): void {
    this.buffer += chunk;
    if (Buffer.byteLength(this.buffer) > MAX_BUFFER_BYTES) {
      this.socket?.destroy(new Error("WezTerm automation receive buffer exceeded its limit"));
      return;
    }
    while (true) {
      const newline = this.buffer.indexOf("\n");
      if (newline < 0) return;
      const line = this.buffer.slice(0, newline);
      this.buffer = this.buffer.slice(newline + 1);
      if (Buffer.byteLength(line) > MAX_LINE_BYTES) {
        this.socket?.destroy(new Error("WezTerm automation response is too large"));
        return;
      }
      if (!line.trim()) continue;
      let message: RpcResponse | RpcNotification;
      try {
        message = JSON.parse(line) as RpcResponse | RpcNotification;
      } catch (error) {
        this.socket?.destroy(new Error(`Invalid WezTerm automation response: ${String(error)}`));
        return;
      }

      if ("method" in message && message.method === "automation.event") {
        const event = message.params as AutomationEvent;
        if (event && typeof event.revision === "number") {
          if (this.lastRevision && event.revision > this.lastRevision + 1) {
            void this.request<Json>("topology.resync")
              .then((snapshot) => this.options.onResync?.(snapshot))
              .catch(() => undefined);
          }
          this.lastRevision = Math.max(this.lastRevision, event.revision);
        }
        this.options.onEvent?.(event);
        continue;
      }
      if (!("id" in message) || message.id === undefined) continue;
      const id = String(message.id);
      const pending = this.pending.get(id);
      if (!pending) continue;
      this.pending.delete(id);
      clearTimeout(pending.timer);
      if (message.error) {
        pending.reject(new Error(`${message.error.message} (${message.error.code})`));
      } else {
        pending.resolve(message.result ?? null);
      }
    }
  }

  private failPending(error: Error): void {
    for (const pending of this.pending.values()) {
      clearTimeout(pending.timer);
      pending.reject(error);
    }
    this.pending.clear();
  }
}
