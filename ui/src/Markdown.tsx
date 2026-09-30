import React from "react";
import { Box, Text } from "ink";
import { truncateText, wrapText } from "./layout.js";

export interface MarkdownProps {
  text: string;
  columns: number;
}

interface RenderLine {
  kind: "heading" | "paragraph" | "list" | "code";
  text: string;
  prefix?: string;
}

function parseMarkdown(text: string): RenderLine[] {
  const lines = text.replace(/\r\n/g, "\n").split("\n");
  const result: RenderLine[] = [];
  let fenced = false;

  for (const line of lines) {
    if (/^\s*```/.test(line)) {
      fenced = !fenced;
      continue;
    }
    if (fenced) {
      result.push({ kind: "code", text: line });
      continue;
    }
    const heading = /^(#{1,6})\s+(.*)$/.exec(line);
    if (heading) {
      result.push({ kind: "heading", text: heading[2]!.trim() });
      continue;
    }
    const list = /^(\s*)([-*+]\s+|\d+[.)]\s+)(.*)$/.exec(line);
    if (list) {
      result.push({ kind: "list", text: list[3]!, prefix: `${list[1]!}${list[2]!}` });
      continue;
    }
    result.push({ kind: "paragraph", text: line });
  }
  return result;
}

function InlineText({ text }: { text: string }) {
  const parts = text.split(/(`[^`]*`)/g);
  return (
    <Text>
      {parts.map((part, index) => part.startsWith("`") && part.endsWith("`")
        ? <Text key={index} color="cyan">{part.slice(1, -1)}</Text>
        : <React.Fragment key={index}>{part}</React.Fragment>)}
    </Text>
  );
}

/** A deliberately small Markdown renderer for terminal responses. */
export function Markdown({ text, columns }: MarkdownProps) {
  const width = Math.max(1, columns);
  const rows: React.ReactElement[] = [];
  let previousKind: RenderLine["kind"] | undefined;

  parseMarkdown(text).forEach((line, lineIndex) => {
    if (line.kind === "code") {
      rows.push(<Text key={lineIndex} color="gray">{line.text || " "}</Text>);
      previousKind = line.kind;
      return;
    }

    const prefix = line.prefix ?? "";
    const available = Math.max(1, width - prefix.length);
    const wrapped = wrapText(line.text, available);
    if (line.kind === "paragraph" && line.text === "") {
      rows.push(<Text key={lineIndex}> </Text>);
      previousKind = line.kind;
      return;
    }
    wrapped.forEach((part, wrappedIndex) => {
      const marker = wrappedIndex === 0 ? prefix : prefix.replace(/\S/g, " ");
      if (line.kind === "heading") {
        rows.push(<Text key={`${lineIndex}-${wrappedIndex}`} bold color="green">{marker}{truncateText(part, available)}</Text>);
      } else if (line.kind === "list") {
        rows.push(<Text key={`${lineIndex}-${wrappedIndex}`}>{marker}<InlineText text={part} /></Text>);
      } else {
        rows.push(<Text key={`${lineIndex}-${wrappedIndex}`}><InlineText text={part} /></Text>);
      }
    });
    // Blank separation in source Markdown is meaningful to a reader. Ink
    // renders each Text as one terminal row, so this explicit row preserves it.
    if (previousKind && previousKind !== "code" && line.text === "") {
      rows.push(<Text key={`${lineIndex}-blank`}> </Text>);
    }
    previousKind = line.kind;
  });

  return <Box flexDirection="column">{rows}</Box>;
}

export default Markdown;
