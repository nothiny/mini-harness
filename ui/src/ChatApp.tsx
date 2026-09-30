import React, { useCallback, useEffect, useRef, useState } from "react";
import { Box, Static, Text, useApp, useInput } from "ink";
import type { EventNotification, HarnessClientLike, SessionContext } from "./client.js";
import { ApprovalPanel } from "./ApprovalPanel.js";
import { Composer } from "./Composer.js";
import { Markdown } from "./Markdown.js";
import { ToolRow, type ToolDetails, type ToolDisplay } from "./ToolRow.js";
import { useTerminalSize, wrapText } from "./layout.js";
import { addHistory } from "./editor.js";

type Stage = "idle" | "submitting" | "waiting" | "running" | "approval" | "cancelling" | "cancelled";

interface ActiveTool extends ToolDisplay { turnId: string; }
interface HistoryEntry { id: string; role: "user" | "assistant" | "error" | "tool"; text?: string; tool?: ToolDisplay; }
interface PendingApproval { turnId: string; callId: string; toolName: string; inputSummary: string; details?: ToolDetails; }
interface PendingTurn { turnId: string; text: string; wasQueued: boolean; }

interface ChatAppProps { client: HarnessClientLike; sessionId: string; context?: SessionContext; }

function contextLabel(context?: SessionContext): string {
  if (!context) return "执行环境信息未获取";
  const environment = context.execution_environment;
  return [context.provider, context.model ?? "model 未获取", environment?.filesystem, environment?.network, environment?.environment].filter(Boolean).join(" · ");
}

function historyView(entry: HistoryEntry, columns: number) {
  if (entry.role === "user") {
    const lines = wrapText(entry.text ?? "", Math.max(1, columns - 4));
    return <Box key={entry.id} flexDirection="column" marginBottom={1}><Text bold color="cyan">你</Text>{lines.map((line, index) => <Text key={index}>  {line}</Text>)}</Box>;
  }
  if (entry.role === "tool" && entry.tool) return <Box key={entry.id} marginBottom={0}><ToolRow tool={entry.tool} columns={Math.max(16, columns - 2)} /></Box>;
  if (entry.role === "error") return <Box key={entry.id} flexDirection="column" marginBottom={1}><Text bold color="red">错误</Text><Text color="red">  {entry.text}</Text></Box>;
  return <Box key={entry.id} flexDirection="column" marginBottom={1}><Text bold color="green">助手</Text><Markdown text={entry.text ?? ""} columns={Math.max(12, columns - 2)} /></Box>;
}

function terminalEvent(type: string): boolean {
  return type === "turn.completed" || type === "turn.failed" || type === "turn.cancelled" || type === "turn.timed_out";
}

function projectedDetails(event: EventNotification["event"]): ToolDetails | undefined {
  const detail = event.input_detail;
  if (!detail) return undefined;
  return {
    kind: detail.kind ?? "tool",
    text: detail.command ?? detail.path ?? detail.old_preview?.text ?? detail.new_preview?.text ?? "",
    truncated: Boolean(detail.truncated || event.input_summary_truncated),
    redacted: Boolean(detail.redacted || event.input_summary_redacted),
  };
}

export function ChatApp({ client, sessionId, context }: ChatAppProps) {
  const { exit } = useApp();
  const { columns, rows } = useTerminalSize();
  const width = Math.max(24, columns);
  const [history, setHistory] = useState<HistoryEntry[]>([]);
  const [input, setInput] = useState("");
  const [inputHistory, setInputHistory] = useState<string[]>([]);
  const [activeTools, setActiveTools] = useState<Map<string, ActiveTool>>(new Map());
  const [approval, setApproval] = useState<PendingApproval | null>(null);
  const [queued, setQueued] = useState<PendingTurn[]>([]);
  const [inflight, setInflight] = useState(0);
  const [stage, setStage] = useState<Stage>("idle");
  const [approvalPending, setApprovalPending] = useState(false);

  const approvalRef = useRef<PendingApproval | null>(approval);
  const toolsRef = useRef(activeTools);
  const pendingRef = useRef<PendingTurn[]>([]);
  const currentTurnRef = useRef<string | null>(null);
  const runningRef = useRef(false);
  const cancellingRef = useRef(false);
  const historyIdRef = useRef(0);
  const inputRef = useRef(input);
  approvalRef.current = approval;
  toolsRef.current = activeTools;
  inputRef.current = input;

  const addEntry = useCallback((entry: Omit<HistoryEntry, "id">) => {
    setHistory((previous) => [...previous, { ...entry, id: `entry-${++historyIdRef.current}` }]);
  }, []);

  const updateTools = useCallback((callId: string, update: (previous?: ActiveTool) => ActiveTool) => {
    setActiveTools((previous) => {
      const next = new Map(previous);
      next.set(callId, update(previous.get(callId)));
      toolsRef.current = next;
      return next;
    });
  }, []);

  const finishTools = useCallback((turnId: string) => {
    const finished = [...toolsRef.current.values()].filter((tool) => tool.turnId === turnId);
    if (!finished.length) return;
    setHistory((previous) => [...previous, ...finished.map((tool) => ({ id: `entry-${++historyIdRef.current}`, role: "tool" as const, tool }))]);
    setActiveTools((previous) => {
      const next = new Map(previous);
      for (const tool of finished) next.delete(tool.callId);
      toolsRef.current = next;
      return next;
    });
  }, []);

  useEffect(() => client.onEvent((notification) => {
    const event = notification.event;
    const type = event.type;
    const callId = event.tool_call_id ?? `event-${notification.seq}`;
    const turnId = notification.turn_id ?? currentTurnRef.current ?? "";
    if (type === "tool.requested") {
      updateTools(callId, (previous) => ({ callId, turnId, name: event.tool ?? previous?.name ?? "tool", summary: event.input_summary ?? previous?.summary ?? "", status: "requested", startedAt: previous?.startedAt, details: projectedDetails(event) ?? previous?.details }));
      setStage("running");
      return;
    }
    if (type === "tool.started") {
      updateTools(callId, (previous) => ({ callId, turnId: previous?.turnId ?? turnId, name: previous?.name ?? "tool", summary: previous?.summary ?? "", status: "running", startedAt: previous?.startedAt ?? Date.now(), details: previous?.details }));
      setStage("running");
      return;
    }
    if (type === "tool.approval.requested") {
      const tool = toolsRef.current.get(callId);
      updateTools(callId, (previous) => ({ ...(previous ?? { callId, turnId, name: "tool", summary: "", status: "approval" as const }), status: "approval" }));
      setApproval({ turnId, callId, toolName: tool?.name ?? "tool", inputSummary: tool?.summary ?? "", details: tool?.details });
      setStage("approval");
      return;
    }
    if (type === "tool.completed" || type === "tool.failed" || type === "tool.outcome_unknown" || type === "tool.policy_denied") {
      const status: ToolDisplay["status"] = type === "tool.completed" ? "completed" : type === "tool.policy_denied" ? "denied" : type === "tool.outcome_unknown" ? "unknown" : "failed";
      updateTools(callId, (previous) => ({ callId, turnId: previous?.turnId ?? turnId, name: previous?.name ?? "tool", summary: previous?.summary ?? "", status, startedAt: previous?.startedAt, finishedAt: Date.now(), details: previous?.details, resultSummary: event.result_summary ? String(event.result_summary.kind ?? "结果已记录") : event.error_summary?.text ?? event.reason }));
      if (approvalRef.current?.callId === callId) setApproval(null);
      return;
    }
    if (type === "tool.approval.responded" && !event.approved) {
      updateTools(callId, (previous) => ({ ...(previous ?? { callId, turnId, name: "tool", summary: "", status: "denied" as const }), status: "denied", finishedAt: Date.now() }));
      return;
    }
    if (terminalEvent(type) && approvalRef.current?.turnId === turnId) setApproval(null);
  }), [client, updateTools]);

  const waitForTurn = useCallback(async (turnId: string) => {
    for (;;) {
      let response;
      try { response = await client.request("turn.wait", { turn_id: turnId, session_id: sessionId, timeout_ms: 300 }); }
      catch (error) { finishTools(turnId); addEntry({ role: "error", text: (error as Error).message }); setStage("idle"); return; }
      if (!response.error) {
        finishTools(turnId);
        const text = (response.result as { text?: string } | undefined)?.text ?? "";
        if (text) addEntry({ role: "assistant", text });
        setStage("idle");
        return;
      }
      const code = response.error.code;
      if (code === "turn_pending") {
        setStage(toolsRef.current.size ? "running" : "waiting");
        // The real server holds turn.wait open for the requested timeout;
        // demo clients may return immediately, so keep polling bounded.
        await new Promise((resolve) => setTimeout(resolve, 25));
        continue;
      }
      if (code === "approval_pending") {
        setStage("approval");
        // The event stream and turn.wait are independent protocol surfaces.
        // If the approval notification was delayed or dropped, wait() still
        // gives us a trustworthy indication that input must be paused. Build
        // the panel from the correlated active tool instead of leaving an
        // empty composer on screen.
        if (!approvalRef.current) {
          const pendingTool = [...toolsRef.current.values()].find((tool) =>
            tool.turnId === turnId && (tool.status === "requested" || tool.status === "running" || tool.status === "approval"),
          );
          if (pendingTool) {
            setApproval({
              turnId,
              callId: pendingTool.callId,
              toolName: pendingTool.name,
              inputSummary: pendingTool.summary,
              details: pendingTool.details,
            });
          }
        }
        await new Promise((resolve) => setTimeout(resolve, 80));
        continue;
      }
      finishTools(turnId);
      if (code === "turn_cancelled") { addEntry({ role: "error", text: "本次执行已取消" }); setStage("cancelled"); }
      else { addEntry({ role: "error", text: response.error.message }); setStage("idle"); }
      return;
    }
  }, [addEntry, client, finishTools, sessionId]);

  const pump = useCallback(async () => {
    if (runningRef.current) return;
    runningRef.current = true;
    try {
      while (pendingRef.current.length) {
        const item = pendingRef.current.shift()!;
        currentTurnRef.current = item.turnId;
        if (item.wasQueued) { setQueued((previous) => previous.filter((turn) => turn.turnId !== item.turnId)); addEntry({ role: "user", text: item.text }); }
        await waitForTurn(item.turnId);
        currentTurnRef.current = null;
        cancellingRef.current = false;
        setInflight((count) => Math.max(0, count - 1));
      }
    } finally { runningRef.current = false; currentTurnRef.current = null; }
  }, [addEntry, waitForTurn]);

  const sendPrompt = useCallback(async (prompt: string) => {
    if (!prompt.trim()) return false;
    setStage("submitting");
    let response;
    try { response = await client.request("turn.start", { prompt, session_id: sessionId }); }
    catch (error) { addEntry({ role: "error", text: (error as Error).message }); setStage(inflight ? "waiting" : "idle"); return false; }
    if (response.error) { addEntry({ role: "error", text: response.error.message }); setStage(inflight ? "waiting" : "idle"); return false; }
    const result = response.result as { turn_id?: string; queued?: boolean } | undefined;
    if (!result?.turn_id) { addEntry({ role: "error", text: "服务端没有返回 turn id" }); setStage("idle"); return false; }
    setInputHistory((previous) => addHistory(previous, prompt));
    // Clear only the submitted draft. If the user started editing while the
    // request was being accepted, preserve that newer draft in the composer.
    if (inputRef.current === prompt) setInput("");
    const item: PendingTurn = { turnId: result.turn_id, text: prompt, wasQueued: Boolean(result.queued) };
    if (item.wasQueued) setQueued((previous) => [...previous, item]); else addEntry({ role: "user", text: prompt });
    pendingRef.current.push(item);
    setInflight((count) => count + 1);
    void pump();
    return true;
  }, [addEntry, client, inflight, pump, sessionId]);

  const handleApproval = useCallback(async (approved: boolean) => {
    const pending = approvalRef.current;
    if (!pending) return;
    setApprovalPending(true);
    try {
      const response = await client.request("approval.respond", { session_id: sessionId, turn_id: pending.turnId, call_id: pending.callId, approved });
      if (!response.error || response.error.code === "approval_pending") setApproval(null);
      else addEntry({ role: "error", text: response.error.message });
    } catch (error) { addEntry({ role: "error", text: (error as Error).message }); }
    finally { setApprovalPending(false); }
  }, [addEntry, client, sessionId]);

  useInput((char, key) => {
    if (key.ctrl && char === "d") { exit(); return; }
    if (key.ctrl && char === "c") {
      const turnId = currentTurnRef.current;
      if (!turnId || cancellingRef.current) { setStage("idle"); return; }
      cancellingRef.current = true;
      setStage("cancelling");
      void client.request("turn.cancel", { session_id: sessionId, turn_id: turnId }).catch((error) => { cancellingRef.current = false; addEntry({ role: "error", text: (error as Error).message }); setStage("waiting"); });
    }
  });

  const active = [...activeTools.values()];
  const busy = inflight > 0;
  const status = stage === "submitting" ? "正在提交……" : stage === "waiting" ? "等待模型响应" : stage === "running" ? `正在执行 ${active.at(-1)?.name ?? "工具"}` : stage === "approval" ? "等待你批准" : stage === "cancelling" ? "正在取消……" : stage === "cancelled" ? "已取消" : "就绪";
  const environment = contextLabel(context);

  return <Box flexDirection="column" width={width}>
    <Text bold>mini-harness · {context?.provider ?? "mock"}</Text>
    <Text dimColor>{context?.workspace ?? "workspace 未获取"} · session {sessionId.slice(0, 8)}</Text>
    <Text dimColor>{status} · {environment}</Text>
    <Static items={history}>{(entry) => historyView(entry, width)}</Static>
    {active.length > 0 && <Box flexDirection="column" marginTop={1} marginBottom={1}><Text bold>执行过程</Text>{active.map((tool) => <ToolRow key={tool.callId} tool={tool} columns={Math.max(16, width - 2)} />)}</Box>}
    {approval && <ApprovalPanel key={approval.callId} toolName={approval.toolName} summary={approval.inputSummary} workspace={context?.workspace ?? "未获取"} environment={environment} details={approval.details} onDecision={handleApproval} pending={approvalPending} columns={width} />}
    {queued.map((turn) => <Text key={turn.turnId} dimColor>⏳ 排队中：{turn.text}</Text>)}
    <Composer value={input} onChange={setInput} onSubmit={sendPrompt} history={inputHistory} disabled={Boolean(approval) || stage === "approval"} columns={width} rows={Math.min(6, Math.max(2, rows - 8))} />
    <Text dimColor>Enter 发送 · Alt-Enter 换行 · ↑/↓ 历史 · Ctrl-C 取消当前执行 · Ctrl-D 退出{busy && stage !== "approval" ? " · 当前执行中可继续排队" : ""}</Text>
  </Box>;
}
