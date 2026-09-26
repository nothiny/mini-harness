/**
 * Terminal ChatGPT — a minimal chat UI for mini-harness.
 *
 * You: prompt
 *   ⚙ tool indicator
 * Assistant: response
 * You: _
 *
 * Keys: Enter send (queues while a turn runs) · y/n approve · Ctrl-C ×2 exit · Ctrl-D exit
 */

import React, {
  useCallback,
  useEffect,
  useRef,
  useState,
} from "react";
import { Box, Text, useApp, useInput } from "ink";
import type { HarnessClient, EventNotification } from "./client.js";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

interface ChatEntry {
  role: "user" | "assistant" | "tool" | "error";
  text: string;
}

interface PendingApproval {
  turnId: string;
  callId: string;
  toolName: string;
  inputSummary: string;
}

/** Submitted to the server, accepted, but not started yet. */
interface QueuedTurn {
  turnId: string;
  text: string;
}

/** Local FIFO entry for the turn pump. */
interface PendingTurn {
  turnId: string;
  text: string;
  wasQueued: boolean;
}

// ---------------------------------------------------------------------------
// CJK-aware text wrapping
// ---------------------------------------------------------------------------

function charWidth(char: string): number {
  const code = char.codePointAt(0) ?? 0;
  if (
    (code >= 0x4e00 && code <= 0x9fff) ||
    (code >= 0x3400 && code <= 0x4dbf) ||
    (code >= 0x3000 && code <= 0x30ff) ||
    (code >= 0xff00 && code <= 0xffef) ||
    (code >= 0xf900 && code <= 0xfaff)
  ) return 2;
  return 1;
}

function textWidth(text: string): number {
  let width = 0;
  for (const char of text) width += charWidth(char);
  return width;
}

function wrapText(text: string, maxWidth: number): string[] {
  const lines: string[] = [];
  for (const paragraph of text.split("\n")) {
    if (!paragraph) { lines.push(""); continue; }
    // Word-aware: break at the last space before the limit. If no space
    // exists (long paths/URLs), break at the character limit.
    let current = "";
    let width = 0;
    let lastSpace = -1; // width position of the last space in `current`

    for (const char of paragraph) {
      const w = charWidth(char);
      if (width + w > maxWidth) {
        if (lastSpace > 0 && lastSpace > maxWidth * 0.3) {
          // Break at the last word boundary.
          lines.push(current.slice(0, lastSpace).trimEnd());
          current = current.slice(lastSpace + 1) + char;
          width = textWidth(current);
        } else {
          // No good break point (long token) — hard break.
          lines.push(current);
          current = char;
          width = w;
        }
        lastSpace = -1;
        // Recompute lastSpace for the new `current`.
        for (let i = 0; i < current.length; i++) {
          if (current[i] === " ") lastSpace = i;
        }
      } else {
        current += char;
        width += w;
        if (char === " ") lastSpace = current.length - 1;
      }
    }
    if (current) lines.push(current);
  }
  return lines;
}

// ---------------------------------------------------------------------------
// Message components
// ---------------------------------------------------------------------------

/** Wraps to the actual terminal width (minus margin), falling back to 80. */
function terminalWidth(): number {
  const width = process.stdout.columns ?? 80;
  // Leave room for the "  " indent on each message line.
  return Math.max(40, width - 4);
}

function ChatEntryView({ entry }: { entry: ChatEntry }) {
  const lines = wrapText(entry.text, terminalWidth());
  switch (entry.role) {
    case "user":
      if (!entry.text.trim()) return null;
      return (
        <Box flexDirection="column" marginBottom={1}>
          <Text bold color="cyan">You:</Text>
          {lines.map((line, i) => (
            <Text key={i} color="cyan">  {line}</Text>
          ))}
        </Box>
      );
    case "assistant":
      return (
        <Box flexDirection="column" marginBottom={1}>
          <Text bold color="green">  </Text>
          {lines.map((line, i) => (
            <Text key={i}>  {line}</Text>
          ))}
        </Box>
      );
    case "tool":
      return (
        <Box marginBottom={1}>
          <Text dimColor>  ⚙ {entry.text}</Text>
        </Box>
      );
    case "error":
      return (
        <Box marginBottom={1}>
          <Text bold color="red">  ✗ {entry.text}</Text>
        </Box>
      );
  }
}

// ---------------------------------------------------------------------------
// Main chat app
// ---------------------------------------------------------------------------

interface ChatAppProps {
  client: HarnessClient;
  sessionId: string;
}

export function ChatApp({ client, sessionId }: ChatAppProps) {
  const { exit } = useApp();

  const [chat, setChat] = useState<ChatEntry[]>([]);
  const [input, setInput] = useState("");
  const [inflight, setInflight] = useState(0);
  const [queued, setQueued] = useState<QueuedTurn[]>([]);
  const [toolStatus, setToolStatus] = useState<string | null>(null);
  const [approval, setApproval] = useState<PendingApproval | null>(null);
  const [lastToolName, setLastToolName] = useState<string>("");
  const [lastInputSummary, setLastInputSummary] = useState<string>("");
  const [ctrlCCount, setCtrlCCount] = useState(0);

  const busy = inflight > 0;

  // Refs for use inside async closures and event handlers.
  const pendingRef = useRef<PendingTurn[]>([]);
  const runningRef = useRef(false);
  const inputRef = useRef("");
  const approvalRef = useRef<PendingApproval | null>(null);
  const lastToolNameRef = useRef("");
  const lastInputSummaryRef = useRef("");
  const ctrlCTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  inputRef.current = input;
  approvalRef.current = approval;
  lastToolNameRef.current = lastToolName;
  lastInputSummaryRef.current = lastInputSummary;

  const addChat = useCallback((entry: ChatEntry) => {
    setChat((prev) => [...prev, entry]);
  }, []);

  // -- Event → chat translation ----------------------------------------------
  useEffect(() => {
    return client.onEvent((notification: EventNotification) => {
      const type = notification.event.type;
      const tool = notification.event.tool ?? "";

      if (type === "tool.requested") {
        setLastToolName(tool);
        setLastInputSummary(notification.event.input_summary ?? "");
        setToolStatus(`${tool}…`);
      } else if (type === "tool.started") {
        setToolStatus(`${tool} running…`);
      } else if (type === "tool.completed") {
        setToolStatus(null);
        addChat({ role: "tool", text: `${tool || "tool"} ✓` });
      } else if (type === "tool.failed") {
        setToolStatus(null);
        addChat({ role: "tool", text: `${tool || "tool"} ✗` });
      } else if (type === "tool.approval.requested") {
        setToolStatus(null);
        setApproval({
          turnId: notification.turn_id ?? "",
          callId: notification.event.tool_call_id ?? "",
          toolName: tool || lastToolNameRef.current || "tool",
          inputSummary: lastInputSummaryRef.current,
        });
      } else if (type === "turn.completed" || type === "turn.failed" ||
                 type === "turn.cancelled" || type === "turn.timed_out") {
        setToolStatus(null);
        setApproval(null);
      }
    });
  }, [client, addChat]);

  // -- Turn lifecycle ---------------------------------------------------------
  // Wait for a single turn to reach a terminal state, then append its text.
  const waitForTurn = useCallback(async (turnId: string) => {
    for (;;) {
      let response;
      try {
        response = await client.request("turn.wait", {
          turn_id: turnId,
          timeout_ms: 300,
        });
      } catch (error) {
        addChat({ role: "error", text: (error as Error).message });
        return;
      }

      if (!response.error) {
        const text = (response.result as any)?.text ?? "";
        if (text) addChat({ role: "assistant", text });
        return;
      }

      const code = (response.error as any).code;
      if (code === "turn_pending") continue; // still running

      if (code === "approval_pending") {
        // The approval event notification drives the y/n prompt; poll
        // slowly and let approval.respond finish the turn.
        await new Promise((resolve) => setTimeout(resolve, 500));
        continue;
      }

      if (code === "turn_cancelled") {
        addChat({ role: "assistant", text: "(cancelled)" });
        return;
      }

      addChat({ role: "error", text: (response.error as any).message });
      return;
    }
  }, [client, addChat]);

  // The server runs turns FIFO, so process our submissions in the same
  // order. Otherwise a queued "You:" line could appear before the
  // assistant reply of the turn that is still running.
  const pump = useCallback(async () => {
    if (runningRef.current) return;
    runningRef.current = true;
    try {
      while (pendingRef.current.length > 0) {
        const item = pendingRef.current.shift()!;
        if (item.wasQueued) {
          setQueued((prev) => prev.filter((q) => q.turnId !== item.turnId));
          addChat({ role: "user", text: item.text });
        }
        await waitForTurn(item.turnId);
        setInflight((n) => Math.max(0, n - 1));
      }
    } finally {
      runningRef.current = false;
    }
  }, [addChat, waitForTurn]);

  // Submit an input. If a turn is already running, the server queues it
  // durably and reports `queued: true`; the line shows under "排队中" until
  // the pump reaches it.
  const sendPrompt = useCallback(async (prompt: string) => {
    const userText = prompt.trim();
    if (!userText) return;
    setInput("");

    let startResponse;
    try {
      startResponse = await client.request("turn.start", {
        prompt: userText,
        session_id: sessionId,
      });
    } catch (error) {
      addChat({ role: "error", text: (error as Error).message });
      return;
    }

    if (startResponse.error) {
      addChat({ role: "error", text: (startResponse.error as any).message });
      return;
    }

    const result = startResponse.result as any;
    const turnId: string | undefined = result?.turn_id;
    if (!turnId) {
      addChat({ role: "error", text: "no turn id returned" });
      return;
    }

    const wasQueued = Boolean(result?.queued);
    if (wasQueued) {
      setQueued((prev) => [...prev, { turnId, text: userText }]);
    } else {
      addChat({ role: "user", text: userText });
    }

    pendingRef.current.push({ turnId, text: userText, wasQueued });
    setInflight((n) => n + 1);
    void pump();
  }, [client, sessionId, addChat, pump]);

  // -- Approval handling --------------------------------------------------------
  const handleApproval = useCallback(async (approved: boolean) => {
    const pending = approvalRef.current;
    if (!pending) return;

    setApproval(null);
    addChat({
      role: "tool",
      text: approved ? `${pending.toolName} approved ✓` : `${pending.toolName} denied ✗`,
    });

    try {
      const response = await client.request("approval.respond", {
        turn_id: pending.turnId,
        call_id: pending.callId,
        approved,
        session_id: sessionId,
      });

      if (response.error) {
        const code = (response.error as any).code;
        if (code === "approval_pending") {
          // Not an error: the approved tool ran and the model's next tool
          // call also needs approval. The event notification will set up
          // the next approval prompt — nothing to do here.
          return;
        }
        addChat({ role: "error", text: (response.error as any).message });
        return;
      }

      // If approved and completed, the turn pump's wait loop adds the
      // assistant text; adding it here too would duplicate it. A denial
      // cancels the turn, which the same loop reports as "(cancelled)".
    } catch (error) {
      addChat({ role: "error", text: (error as Error).message });
    }
  }, [client, sessionId, addChat]);

  // -- Input handling -----------------------------------------------------------
  useInput((char, key) => {
    // Ctrl-C: exit on double press.
    if (key.ctrl && char === "c") {
      const count = ctrlCCount + 1;
      setCtrlCCount(count);

      if (count >= 2) {
        exit();
        return;
      }

      if (ctrlCTimerRef.current) clearTimeout(ctrlCTimerRef.current);
      ctrlCTimerRef.current = setTimeout(() => setCtrlCCount(0), 2000);
      return;
    }

    if (key.ctrl && char === "d") {
      exit();
      return;
    }

    // Approval mode: intercept y/n.
    if (approvalRef.current) {
      if (char === "y" || char === "Y") { handleApproval(true); return; }
      if (char === "n" || char === "N") { handleApproval(false); return; }
      return; // ignore other keys
    }

    // Normal input.
    if (key.return) {
      sendPrompt(inputRef.current);
      return;
    }
    if (key.delete || key.backspace) {
      setInput((prev) => prev.slice(0, -1));
      return;
    }
    if (key.ctrl || key.meta || key.escape) return;
    if (char) setInput((prev) => prev + char);
  });

  // -- Render ---------------------------------------------------------------------
  const visibleChat = chat.slice(-20);

  return (
    <Box flexDirection="column">
      {/* Chat history */}
      <Box flexDirection="column">
        {visibleChat.map((entry, i) => (
          <ChatEntryView key={i} entry={entry} />
        ))}

        {/* Active tool indicator */}
        {toolStatus && (
          <Box marginBottom={1}>
            <Text dimColor>  ⚙ {toolStatus}</Text>
          </Box>
        )}

        {/* Approval prompt */}
        {approval && (
          <Box flexDirection="column" marginBottom={1}>
            <Text bold color="yellow">
              ︙ {approval.toolName} — approve? [y/n]
            </Text>
            {approval.inputSummary && (
              <Text color="yellow">  {approval.inputSummary}</Text>
            )}
          </Box>
        )}

        {/* Typing indicator */}
        {busy && !toolStatus && !approval && (
          <Box marginBottom={1}>
            <Text dimColor>  thinking…</Text>
          </Box>
        )}
      </Box>

      {/* Queued inputs: accepted by the server, not started yet */}
      {queued.map((q) => (
        <Box key={q.turnId} marginBottom={0}>
          <Text dimColor>  ⏳ 排队中：{q.text}</Text>
        </Box>
      ))}

      {/* Input box */}
      <Box borderStyle="round" borderColor={busy ? "yellow" : "blue"} paddingX={1} marginTop={1}>
        <Text bold color={busy ? "yellow" : "blue"}>{busy ? "…" : "❯"}</Text>
        <Text> {input}</Text>
        {!approval && <Text color="blue">█</Text>}
      </Box>

      {/* Hints */}
      <Box marginTop={1}>
        <Text dimColor>
          {"  "}
          {approval
            ? "y approve · n deny"
            : busy
              ? "Enter 排队 · Ctrl-C ×2 exit"
              : "Enter send · Ctrl-D exit"}
        </Text>
      </Box>
    </Box>
  );
}
