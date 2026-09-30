import type { EventNotification, HarnessClientLike, JsonRpcResponse, SessionContext } from "./client.js";

interface DemoTurn {
  turnId: string;
  prompt: string;
  approval: boolean;
  approved: boolean | null;
  cancelled: boolean;
  done: boolean;
}

/**
 * Deterministic protocol double for UI development. It deliberately speaks
 * the same small request/event surface as HarnessClient, so the demo can
 * exercise correlation, approval, cancellation and error states offline.
 */
export class DemoClient implements HarnessClientLike {
  private listener: ((notification: EventNotification) => void) | undefined;
  private nextTurn = 1;
  private nextCall = 1;
  private turns = new Map<string, DemoTurn>();
  private timers = new Set<ReturnType<typeof setTimeout>>();

  private later(callback: () => void, delay = 40): void {
    const timer = setTimeout(() => {
      this.timers.delete(timer);
      callback();
    }, delay);
    this.timers.add(timer);
  }

  private emit(turnId: string, event: EventNotification["event"]): void {
    this.listener?.({
      seq: Date.now(),
      session_id: "demo-session",
      turn_id: turnId,
      event,
    });
  }

  request(method: string, params: Record<string, unknown> = {}): Promise<JsonRpcResponse> {
    if (method === "protocol.negotiate") {
      return Promise.resolve({
        jsonrpc: "2.0",
        result: { protocol_version: 2, capabilities: { event_notifications: true } },
      });
    }
    if (method === "session.create") {
      const context: SessionContext = {
        workspace: String(params.workspace ?? "~/demo-workspace"),
        provider: String(params.provider ?? "mock"),
        model: String(params.model ?? "demo"),
        execution_environment: {
          platform: "demo",
          filesystem: "demo-sandbox",
          network: "offline",
          environment: "scrubbed",
        },
      };
      return Promise.resolve({ jsonrpc: "2.0", result: { session_id: "demo-session", context } });
    }
    if (method === "turn.start") {
      const turnId = `demo-turn-${this.nextTurn++}`;
      const prompt = String(params.prompt ?? "");
      const turn: DemoTurn = {
        turnId,
        prompt,
        approval: /approve|bash|command/i.test(prompt),
        approved: null,
        cancelled: false,
        done: false,
      };
      this.turns.set(turnId, turn);
      this.scheduleTurn(turn);
      return Promise.resolve({ jsonrpc: "2.0", result: { turn_id: turnId, accepted: true, queued: false } });
    }
    if (method === "approval.respond") {
      const turn = this.turns.get(String(params.turn_id));
      if (!turn) return Promise.resolve({ jsonrpc: "2.0", error: { code: "not_found", message: "demo turn not found" } });
      turn.approved = Boolean(params.approved);
      if (!turn.approved) {
        turn.done = true;
        this.emit(turn.turnId, { type: "tool.approval.responded", tool_call_id: String(params.call_id), approved: false });
        this.emit(turn.turnId, { type: "tool.failed", tool_call_id: String(params.call_id), outcome: "denied" });
        this.emit(turn.turnId, { type: "turn.cancelled" });
      } else {
        this.emit(turn.turnId, { type: "tool.approval.responded", tool_call_id: String(params.call_id), approved: true });
        this.later(() => {
          if (turn.cancelled) return;
          turn.done = true;
          this.emit(turn.turnId, { type: "tool.completed", tool_call_id: String(params.call_id), result_summary: { kind: "demo", text: "command finished" } });
          this.emit(turn.turnId, { type: "turn.completed" });
        });
      }
      return Promise.resolve({ jsonrpc: "2.0", result: { accepted: true } });
    }
    if (method === "turn.cancel") {
      const turn = this.turns.get(String(params.turn_id));
      if (turn) {
        turn.cancelled = true;
        turn.done = true;
        this.emit(turn.turnId, { type: "turn.cancelled" });
      }
      return Promise.resolve({ jsonrpc: "2.0", result: { turn_id: params.turn_id, cancelled: true } });
    }
    if (method === "turn.wait") {
      const turn = this.turns.get(String(params.turn_id));
      if (!turn) return Promise.resolve({ jsonrpc: "2.0", error: { code: "not_found", message: "demo turn not found" } });
      if (!turn.done) {
        return Promise.resolve({ jsonrpc: "2.0", error: { code: turn.approval && turn.approved === null ? "approval_pending" : "turn_pending", message: "demo turn pending" } });
      }
      if (turn.cancelled || turn.approved === false) {
        return Promise.resolve({ jsonrpc: "2.0", error: { code: "turn_cancelled", message: "demo turn cancelled" } });
      }
      return Promise.resolve({
        jsonrpc: "2.0",
        result: { completed: true, text: `Demo response for: ${turn.prompt}\n\nThe UI kept this tool call correlated by its call id.` },
      });
    }
    if (method === "session.shutdown") return Promise.resolve({ jsonrpc: "2.0", result: {} });
    return Promise.resolve({ jsonrpc: "2.0", error: { code: "method_not_found", message: method } });
  }

  onEvent(listener: (notification: EventNotification) => void): () => void {
    this.listener = listener;
    return () => {
      if (this.listener === listener) this.listener = undefined;
    };
  }

  async shutdown(): Promise<void> {
    for (const timer of this.timers) clearTimeout(timer);
    this.timers.clear();
    this.listener = undefined;
  }

  private scheduleTurn(turn: DemoTurn): void {
    const callId = `demo-call-${this.nextCall++}`;
    const tool = turn.approval ? "bash" : "read";
    const summary = turn.approval ? "cargo test --test process_executor" : "src/executor/process.rs";
    this.later(() => {
      if (turn.cancelled) return;
      this.emit(turn.turnId, { type: "tool.requested", tool_call_id: callId, tool, input_summary: summary, input_detail: { kind: tool, command: summary } });
      this.later(() => {
        if (turn.cancelled) return;
        this.emit(turn.turnId, { type: "tool.started", tool_call_id: callId, execution_id: `${turn.turnId}-exec` });
        if (turn.approval) {
          this.later(() => {
            if (turn.cancelled) return;
            this.emit(turn.turnId, { type: "tool.approval.requested", tool_call_id: callId });
          });
        } else {
          this.later(() => {
            if (turn.cancelled) return;
            turn.done = true;
            this.emit(turn.turnId, { type: "tool.completed", tool_call_id: callId, result_summary: { kind: "demo", text: "file read" } });
            this.emit(turn.turnId, { type: "turn.completed" });
          });
        }
      });
    });
  }
}
