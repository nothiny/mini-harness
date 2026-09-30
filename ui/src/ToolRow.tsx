import React from "react";
import { Text } from "ink";
import { displayWidth, truncateText } from "./layout.js";

export interface ToolDetails {
  kind: string;
  text: string;
  truncated: boolean;
  redacted?: boolean;
}

export interface Tool {
  callId: string;
  name: string;
  summary: string;
  status:
    | "requested"
    | "running"
    | "approval"
    | "completed"
    | "failed"
    | "denied"
    | "unknown"
    | "deferred"
    | "cancelled";
  startedAt?: number;
  finishedAt?: number;
  details?: ToolDetails;
  resultSummary?: string;
}

/** Backwards-compatible descriptive alias for callers that prefer it. */
export type ToolDisplay = Tool;

export interface ToolRowProps {
  tool: ToolDisplay;
  columns: number;
  now?: number;
}

const STATUS: Record<ToolDisplay["status"], { icon: string; color?: "gray" | "yellow" | "green" | "red" | "cyan" }> = {
  requested: { icon: "…", color: "yellow" },
  running: { icon: "…", color: "yellow" },
  approval: { icon: "?", color: "yellow" },
  completed: { icon: "✓", color: "green" },
  failed: { icon: "✗", color: "red" },
  denied: { icon: "×", color: "red" },
  unknown: { icon: "?", color: "gray" },
  deferred: { icon: "↷", color: "gray" },
  cancelled: { icon: "■", color: "gray" },
};

function duration(tool: ToolDisplay, now: number): string {
  if (tool.startedAt === undefined) return "";
  const end = tool.finishedAt ?? now;
  const elapsed = Math.max(0, end - tool.startedAt) / 1000;
  if (elapsed < 1) return `${elapsed.toFixed(2)}s`;
  if (elapsed < 10) return `${elapsed.toFixed(1)}s`;
  return `${Math.round(elapsed)}s`;
}

/** Compact, stable representation of one tool call. */
export function ToolRow({ tool, columns, now = Date.now() }: ToolRowProps) {
  const status = STATUS[tool.status] ?? STATUS.unknown;
  const elapsed = duration(tool, now);
  const prefix = `  ${status.icon} ${tool.name}`;
  const suffix = elapsed ? `  ${elapsed}` : "";
  const summaryWidth = Math.max(1, columns - displayWidth(prefix) - displayWidth(suffix) - 2);
  const summary = truncateText(tool.summary || tool.resultSummary || tool.details?.text || "", summaryWidth);
  return (
    <Text color={status.color}>
      {prefix}{summary ? `  ${summary}` : ""}{suffix}
    </Text>
  );
}

export default ToolRow;
