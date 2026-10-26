import stringWidth from "string-width";
import { useStdout } from "ink";
import { useEffect, useState } from "react";

/** The number of terminal cells occupied by a string. */
export function displayWidth(value: string): number {
  return stringWidth(value);
}

/**
 * Split a string into user-visible units.  Intl.Segmenter keeps a family
 * emoji or a combining sequence together; Array.from is a safe fallback for
 * runtimes without Segmenter.
 */
export function graphemes(value: string): string[] {
  const Segmenter = (Intl as typeof Intl & {
    Segmenter?: new (locale?: string, options?: { granularity: "grapheme" }) => {
      segment(input: string): Iterable<{ segment: string }>;
    };
  }).Segmenter;
  if (!Segmenter) return Array.from(value);
  return Array.from(new Segmenter(undefined, { granularity: "grapheme" }).segment(value), (part) => part.segment);
}

/** Return text shortened to a terminal-cell width, with an ellipsis. */
export function truncateText(value: string, columns: number): string {
  if (columns <= 0) return "";
  if (displayWidth(value) <= columns) return value;
  if (columns === 1) return "…";
  const target = columns - 1;
  let result = "";
  let width = 0;
  for (const unit of graphemes(value)) {
    const unitWidth = displayWidth(unit);
    if (width + unitWidth > target) break;
    result += unit;
    width += unitWidth;
  }
  return `${result}…`;
}

/**
 * Wrap prose to terminal cells while retaining explicit blank lines.  Code
 * blocks should bypass this helper because their whitespace is meaningful.
 */
export function wrapText(value: string, columns: number): string[] {
  const widthLimit = Math.max(1, columns);
  const output: string[] = [];

  for (const sourceLine of value.split("\n")) {
    if (sourceLine.length === 0) {
      output.push("");
      continue;
    }

    let line = sourceLine;
    while (line.length > 0) {
      if (displayWidth(line) <= widthLimit) {
        output.push(line);
        break;
      }

      const units = graphemes(line);
      let used = 0;
      let fitCount = 0;
      let breakAt = -1;
      for (let index = 0; index < units.length; index += 1) {
        const unitWidth = displayWidth(units[index]!);
        if (used + unitWidth > widthLimit) break;
        used += unitWidth;
        fitCount = index + 1;
        if (/\s/.test(units[index]!)) breakAt = index;
      }

      // A wide grapheme still needs to make progress on a one-column line.
      if (fitCount === 0) {
        output.push(units[0]!);
        line = units.slice(1).join("");
        continue;
      }

      // Prefer a word boundary when there is one; otherwise consume all
      // graphemes that fit this line.
      const fitting = breakAt >= 0 ? breakAt + 1 : fitCount;
      let rendered = units.slice(0, fitting).join("");
      line = units.slice(fitting).join("");
      if (breakAt >= 0) {
        rendered = rendered.trimEnd();
        line = line.trimStart();
      }
      // A boundary at the first character belongs to the previous line.
      // Dropping it avoids manufacturing an empty row for "hello world" at
      // exactly the word boundary.
      if (breakAt === 0 && rendered.length === 0) continue;
      output.push(rendered);
    }
  }

  return output;
}

export interface TerminalSize {
  columns: number;
  rows: number;
}

/** Track terminal dimensions without forcing callers to read process.stdout. */
export function useTerminalSize(): TerminalSize {
  const { stdout } = useStdout();
  // A pty can report 0x0 (CI, `script`, detached terminals); `??` would keep
  // that 0 and collapse the layout to a single column, so treat it as absent.
  const readSize = (): TerminalSize => {
    const columns = stdout.columns || process.stdout.columns || 0;
    const rows = stdout.rows || process.stdout.rows || 0;
    return {
      columns: columns > 0 ? columns : 80,
      rows: rows > 0 ? rows : 24,
    };
  };
  const [size, setSize] = useState<TerminalSize>(readSize);

  useEffect(() => {
    const update = () => setSize(readSize());
    stdout.on("resize", update);
    update();
    return () => { stdout.off("resize", update); };
  }, [stdout]);

  return size;
}
