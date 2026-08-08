import { createConnection, type Socket } from "node:net";

type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

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
  timer: NodeJS.Timeout;
};

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
  onConnectionChange?: (connected: boolean, reason?: string) => void;
};

export class AutomationClient {
  private socket?: Socket;
  private buffer = "";
  private nextId = 1;
  private pending = new Map<string, Pending>();
  private connectPromise?: Promise<void>;
  private closed = false;

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
              "pane.read",
              "pane.input.text",
              "pane.create",
              "pane.layout",
              "panel.manage",
              "command.run",
              "command.cancel",
            ],
          });
          this.options.onConnectionChange?.(true);
          this.connectPromise = undefined;
          resolve();
        } catch (error) {
          fail(error instanceof Error ? error : new Error(String(error)));
          socket.destroy();
        }
      });

      socket.on("data", (chunk: string) => this.consume(chunk));
      socket.on("error", (error) => {
        this.options.onConnectionChange?.(false, error.message);
        fail(error);
      });
      socket.on("close", () => {
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
      this.socket!.write(`${request}\n`, (error) => {
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
    this.socket?.end();
    this.socket?.destroy();
    this.socket = undefined;
    this.failPending(new Error("WezTerm automation disconnected"));
  }

  private consume(chunk: string): void {
    this.buffer += chunk;
    while (true) {
      const newline = this.buffer.indexOf("\n");
      if (newline < 0) return;
      const line = this.buffer.slice(0, newline);
      this.buffer = this.buffer.slice(newline + 1);
      if (!line.trim()) continue;
      let message: RpcResponse | RpcNotification;
      try {
        message = JSON.parse(line) as RpcResponse | RpcNotification;
      } catch {
        continue;
      }

      if ("method" in message && message.method === "automation.event") {
        this.options.onEvent?.(message.params as AutomationEvent);
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
