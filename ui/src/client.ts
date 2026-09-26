/**
 * JSONL protocol client for mini-harness.
 *
 * Spawns `mini-harness serve --stdio` as a child process and speaks the
 * line-delimited JSON-RPC style protocol (day 9 of the tutorial).
 *
 * The client owns zero session state: it forwards commands, correlates
 * responses by id, and surfaces event notifications. Persistence, tool
 * execution, and recovery all live in the Rust process.
 */

import { spawn, type ChildProcess } from "node:child_process";
import { createInterface } from "node:readline";
import { EventEmitter } from "node:events";
import { existsSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

export interface JsonRpcResponse {
  jsonrpc: "2.0";
  id?: number;
  method?: string;
  result?: unknown;
  error?: { code: string; message: string };
}

export interface EventNotification {
  seq: number;
  session_id: string;
  turn_id: string | null;
  event: {
    type: string;
    tool_call_id?: string;
    execution_id?: string;
    tool?: string;
    approved?: boolean;
    reason?: string;
    outcome?: string;
    /** Safe one-line summary of the tool input (command, path, etc). */
    input_summary?: string;
  };
}

/** Resolves this source file's directory (the project is ESM-only). */
function thisDir(): string {
  return dirname(fileURLToPath(import.meta.url));
}

/** Finds the mini-harness binary relative to this project. */
export function findBinary(explicit?: string): string {
  if (explicit) return explicit;
  // ui/src/client.ts → ui/ → repo root → target/{debug,release}
  const repoRoot = join(thisDir(), "..", "..");
  for (const profile of ["debug", "release"]) {
    const candidate = join(repoRoot, "target", profile, "mini-harness");
    if (existsSync(candidate)) return candidate;
  }
  return "mini-harness"; // hope it's on PATH
}

export class HarnessClient {
  private proc: ChildProcess;
  private nextId = 1;
  private pending = new Map<number, {
    resolve: (value: JsonRpcResponse) => void;
    reject: (error: Error) => void;
  }>();
  private emitter = new EventEmitter();

  constructor(binaryPath: string) {
    this.proc = spawn(binaryPath, ["serve", "--stdio"], {
      stdio: ["pipe", "pipe", "inherit"], // stderr goes to our stderr
    });

    this.proc.on("error", (error) => {
      this.rejectAllPending(new Error(`failed to start server: ${error.message}`));
    });
    this.proc.on("exit", (code) => {
      this.rejectAllPending(new Error(`server exited with code ${code}`));
    });

    // Route each stdout line: responses by id, event notifications to the
    // emitter. This is the TS mirror of the Rust event pump's job.
    const rl = createInterface({ input: this.proc.stdout! });
    rl.on("line", (line) => {
      if (!line.trim()) return;
      let message: JsonRpcResponse;
      try {
        message = JSON.parse(line);
      } catch {
        console.error(`[tui] non-JSON on protocol stdout: ${line.slice(0, 80)}`);
        return;
      }
      if (message.id !== undefined && this.pending.has(message.id)) {
        const { resolve } = this.pending.get(message.id)!;
        this.pending.delete(message.id);
        resolve(message);
      } else if (message.method === "event") {
        this.emitter.emit("notification", (message as any).params);
      }
    });
  }

  /** Sends a request and awaits exactly its response. */
  request(method: string, params: Record<string, unknown> = {}): Promise<JsonRpcResponse> {
    return new Promise((resolve, reject) => {
      const id = this.nextId++;
      this.pending.set(id, { resolve, reject });
      const message = JSON.stringify({
        jsonrpc: "2.0",
        protocol_version: 2,
        id,
        method,
        params,
      });
      this.proc.stdin!.write(message + "\n", (error) => {
        if (error) {
          this.pending.delete(id);
          reject(new Error(`failed to write request: ${error.message}`));
        }
      });
    });
  }

  /** Subscribes to event notifications (v2 safe projection). */
  onEvent(listener: (notification: EventNotification) => void): () => void {
    this.emitter.on("notification", listener);
    return () => this.emitter.off("notification", listener);
  }

  /** Negotiates protocol v2; throws if the server doesn't support events. */
  async negotiate(): Promise<void> {
    const response = await this.request("protocol.negotiate", { version: 2 });
    if (response.error) {
      throw new Error(`protocol negotiation failed: ${response.error.message}`);
    }
    const capabilities = (response.result as any)?.capabilities;
    if (!capabilities?.event_notifications) {
      throw new Error("server does not advertise event notifications");
    }
  }

  /**
   * Sends session.shutdown and waits for the server to exit cleanly.
   * Call this on exit to avoid orphaning the Rust process.
   */
  async shutdown(): Promise<void> {
    try {
      await this.request("session.shutdown");
    } catch {
      // Server may have already exited; that's fine.
    }
    this.proc.stdin?.end();
    // Give the server a moment to flush, then force-kill if needed.
    await new Promise<void>((resolve) => {
      const timer = setTimeout(() => {
        this.proc.kill("SIGKILL");
        resolve();
      }, 2000);
      this.proc.on("exit", () => {
        clearTimeout(timer);
        resolve();
      });
    });
  }

  private rejectAllPending(error: Error): void {
    for (const { reject } of this.pending.values()) {
      reject(error);
    }
    this.pending.clear();
  }
}
