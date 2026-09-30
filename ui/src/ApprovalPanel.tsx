import React, { useEffect, useState } from "react";
import { Box, Text, useStdin } from "ink";

export interface ApprovalDetails { kind: string; text: string; truncated: boolean; redacted?: boolean; }
export interface ApprovalPanelProps {
  toolName: string;
  summary: string;
  workspace: string;
  environment: string;
  onDecision: (approved: boolean) => void;
  pending?: boolean;
  columns: number;
  details?: ApprovalDetails;
}

export function ApprovalPanel({ toolName, summary, workspace, environment, onDecision, pending = false, columns, details }: ApprovalPanelProps) {
  const { stdin, setRawMode } = useStdin();
  const [selected, setSelected] = useState<"deny" | "allow">("deny");
  const selectedRef = React.useRef<"deny" | "allow">("deny");
  const parserRef = React.useRef("");
  useEffect(() => { selectedRef.current = "deny"; setSelected("deny"); }, [toolName, summary]);
  useEffect(() => {
    setRawMode?.(true);
    const onData = (data: Buffer | string) => {
      if (pending) return;
      parserRef.current += data.toString();
      let input = parserRef.current;
      while (input) {
        if (input.startsWith("\u001b[C") || input.startsWith("\u001b[D")) {
          setSelected((value) => { const next = value === "deny" ? "allow" : "deny"; selectedRef.current = next; return next; }); input = input.slice(3); continue;
        }
        if (input.startsWith("\u001b[")) {
          if (input.length < 3) break;
          input = input.slice(1); continue;
        }
        if (input[0] === "\u001b") { input = input.slice(1); continue; }
        const char = input[0]; input = input.slice(1);
        if (char === "\r" || char === "\n") { onDecision(selectedRef.current === "allow"); continue; }
        if (char === "\t") { setSelected((value) => { const next = value === "deny" ? "allow" : "deny"; selectedRef.current = next; return next; }); continue; }
        // y/n are intentionally ignored. Approval must be an explicit focused action.
      }
      parserRef.current = input;
    };
    stdin.on("data", onData);
    return () => { stdin.off("data", onData); };
  }, [stdin, setRawMode, pending, onDecision]);
  return (
    <Box flexDirection="column" borderStyle="round" borderColor="yellow" paddingX={1} width={Math.max(24, columns)}>
      <Text bold color="yellow">需要批准：{toolName}</Text>
      {summary && <Text wrap="truncate">{summary}</Text>}
      <Text dimColor>工作目录：{workspace}</Text>
      <Text dimColor>执行环境：{environment}</Text>
      {details && <Text dimColor wrap="truncate">{details.kind}：{details.text}{details.truncated ? " …" : ""}{details.redacted ? " [已脱敏]" : ""}</Text>}
      <Box marginTop={1}>
        <Text backgroundColor={selected === "allow" ? "green" : undefined} color={selected === "allow" ? "black" : "green"}> {selected === "allow" ? "› " : "  "}允许本次 </Text>
        <Text> </Text>
        <Text backgroundColor={selected === "deny" ? "red" : undefined} color={selected === "deny" ? "black" : "red"}> {selected === "deny" ? "› " : "  "}拒绝 </Text>
      </Box>
      <Text dimColor>{pending ? "正在提交……" : `←/→ 或 Tab 选择 · Enter 确认`}</Text>
    </Box>
  );
}
