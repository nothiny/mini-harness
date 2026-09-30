import React, { useEffect, useMemo, useRef, useState } from "react";
import { Box, Text, useStdin } from "ink";
import {
  addHistory,
  backspace,
  currentLineStart,
  deleteForward,
  editorState,
  graphemes,
  insertText,
  moveCursor,
  navigateHistory,
  setEditorText,
  type EditorState,
} from "./editor.js";
import { displayWidth } from "./layout.js";

export interface ComposerProps {
  value: string;
  onChange: (value: string) => void;
  /** Resolving `false` restores the submitted text when no newer draft exists. */
  onSubmit: (value: string) => boolean | void | Promise<boolean | void>;
  history: string[];
  disabled?: boolean;
  columns: number;
  rows?: number;
}

const PASTE_START = "\u001b[200~";
const PASTE_END = "\u001b[201~";

function widthOf(grapheme: string): number {
  return Math.max(1, displayWidth(grapheme));
}

interface VisualRow { units: string[]; cursor: boolean; }

function visualRows(state: EditorState, columns: number): VisualRow[] {
  const width = Math.max(4, columns);
  const units = graphemes(state.text);
  const rows: VisualRow[] = [{ units: [], cursor: false }];
  let rowWidth = 0;
  for (let i = 0; i <= units.length; i += 1) {
    if (i === state.cursor) {
      // Keep the cursor visible on its own continuation row when the current
      // row is already full (including the cursor at end-of-input).
      if (rowWidth >= width) {
        rows.push({ units: [], cursor: false });
        rowWidth = 0;
      }
      rows[rows.length - 1].cursor = true;
    }
    if (i === units.length) break;
    const unit = units[i];
    if (unit === "\n") {
      rows.push({ units: [], cursor: false });
      rowWidth = 0;
      continue;
    }
    const unitWidth = widthOf(unit);
    if (rowWidth > 0 && rowWidth + unitWidth > width) {
      rows.push({ units: [], cursor: false });
      rowWidth = 0;
      if (i === state.cursor) rows[rows.length - 1].cursor = true;
    }
    rows[rows.length - 1].units.push(unit);
    rowWidth += unitWidth;
  }
  return rows;
}

type SubmitHandler = ComposerProps["onSubmit"];

function applyKey(state: EditorState, key: string, history: string[], onSubmit: SubmitHandler): EditorState {
  switch (key) {
    case "left": return moveCursor(state, "left");
    case "right": return moveCursor(state, "right");
    case "home": return moveCursor(state, "home");
    case "end": return moveCursor(state, "end");
    case "up": return navigateHistory(state, history, "up");
    case "down": return navigateHistory(state, history, "down");
    case "backspace": return backspace(state);
    case "delete": return deleteForward(state);
    case "newline": return insertText(state, "\n");
    case "submit": onSubmit(state.text); return state;
    default: return insertText(state, key);
  }
}

/** A small raw terminal editor. The parser deliberately handles paste markers across chunks. */
export function Composer({ value, onChange, onSubmit, history, disabled = false, columns, rows = 5 }: ComposerProps) {
  const { stdin, setRawMode } = useStdin();
  const [state, setState] = useState<EditorState>(() => editorState(value));
  const stateRef = useRef(state);
  const historyRef = useRef(history);
  const externalValueRef = useRef(value);
  const parserRef = useRef("");
  const pasteRef = useRef(false);
  const localHistoryRef = useRef<string[]>([]);
  const editRevisionRef = useRef(0);

  stateRef.current = state;
  historyRef.current = [...history, ...localHistoryRef.current.filter((item) => !history.includes(item))];

  useEffect(() => {
    if (value !== externalValueRef.current) {
      externalValueRef.current = value;
      setState((previous) => setEditorText(previous, value));
    }
  }, [value]);

  const commit = (next: EditorState) => {
    editRevisionRef.current += 1;
    stateRef.current = next;
    setState(next);
    externalValueRef.current = next.text;
    onChange(next.text);
  };

  const submit = (text: string) => {
    if (!text.trim()) return;
    const submitted = text;
    // Clear immediately so typing the next queued prompt starts a new draft;
    // the async result below restores this text only when the draft is still
    // untouched and the request was rejected.
    commit(setEditorText(stateRef.current, ""));
    const clearRevision = editRevisionRef.current;
    let result: boolean | void | Promise<boolean | void>;
    try {
      result = onSubmit(submitted);
    } catch {
      if (editRevisionRef.current === clearRevision && stateRef.current.text === "") commit(setEditorText(stateRef.current, submitted));
      return;
    }
    void Promise.resolve(result).then((accepted) => {
      if (accepted === false) {
        if (editRevisionRef.current === clearRevision && stateRef.current.text === "") commit(setEditorText(stateRef.current, submitted));
        return;
      }
      localHistoryRef.current = addHistory(localHistoryRef.current, submitted);
    }).catch(() => {
      if (editRevisionRef.current === clearRevision && stateRef.current.text === "") commit(setEditorText(stateRef.current, submitted));
    });
  };

  const consume = (chunk: string) => {
    parserRef.current += chunk;
    let input = parserRef.current;
    const consumeText = (text: string) => { if (text && !disabled) commit(insertText(stateRef.current, text)); };
    while (input) {
      if (pasteRef.current) {
        const end = input.indexOf(PASTE_END);
        if (end < 0) {
          // Keep a possible split suffix of the end marker for the next chunk.
          const keep = Math.min(PASTE_END.length - 1, input.length);
          const safe = input.slice(0, input.length - keep);
          consumeText(safe);
          input = input.slice(input.length - keep);
          break;
        }
        consumeText(input.slice(0, end));
        input = input.slice(end + PASTE_END.length);
        pasteRef.current = false;
        continue;
      }
      if (input.startsWith(PASTE_START)) { pasteRef.current = true; input = input.slice(PASTE_START.length); continue; }
      // Bracketed-paste start may itself be split across data events.
      if (PASTE_START.startsWith(input)) break;
      if (input.startsWith("\u001b[A")) { if (!disabled) commit(applyKey(stateRef.current, "up", historyRef.current, submit)); input = input.slice(3); continue; }
      if (input.startsWith("\u001b[B")) { if (!disabled) commit(applyKey(stateRef.current, "down", historyRef.current, submit)); input = input.slice(3); continue; }
      if (input.startsWith("\u001b[C")) { if (!disabled) commit(applyKey(stateRef.current, "right", historyRef.current, submit)); input = input.slice(3); continue; }
      if (input.startsWith("\u001b[D")) { if (!disabled) commit(applyKey(stateRef.current, "left", historyRef.current, submit)); input = input.slice(3); continue; }
      if (input.startsWith("\u001b[H") || input.startsWith("\u001b[1~")) { if (!disabled) commit(applyKey(stateRef.current, "home", historyRef.current, submit)); input = input.startsWith("\u001b[H") ? input.slice(3) : input.slice(4); continue; }
      if (input.startsWith("\u001b[F") || input.startsWith("\u001b[4~")) { if (!disabled) commit(applyKey(stateRef.current, "end", historyRef.current, submit)); input = input.startsWith("\u001b[F") ? input.slice(3) : input.slice(4); continue; }
      if (input.startsWith("\u001b[3~")) { if (!disabled) commit(applyKey(stateRef.current, "delete", historyRef.current, submit)); input = input.slice(4); continue; }
      if (input.startsWith("\u001b\r") || input.startsWith("\u001b\n")) { if (!disabled) commit(applyKey(stateRef.current, "newline", historyRef.current, submit)); input = input.slice(2); continue; }
      if (input[0] === "\u001b") {
        // An incomplete escape sequence may be split over data events.
        if (input.length < 3 && (input === "\u001b" || input.startsWith("\u001b["))) break;
        input = input.slice(1); continue;
      }
      const code = input.charCodeAt(0);
      if (code === 13) { if (!disabled) submit(stateRef.current.text); input = input.slice(1); continue; }
      if (code === 10) { if (!disabled) commit(applyKey(stateRef.current, "newline", historyRef.current, submit)); input = input.slice(1); continue; }
      if (code === 127 || code === 8) { if (!disabled) commit(applyKey(stateRef.current, "backspace", historyRef.current, submit)); input = input.slice(1); continue; }
      if (code === 1) { if (!disabled) commit(setEditorText(stateRef.current, stateRef.current.text, currentLineStart(stateRef.current))); input = input.slice(1); continue; }
      if (code === 5) { if (!disabled) commit(moveCursor(stateRef.current, "end")); input = input.slice(1); continue; }
      if (code === 3 || code === 4 || code === 12) { input = input.slice(1); continue; }
      // Avoid splitting a UTF-8 chunk into a surrogate pair. Node's toString normally prevents this.
      const unit = Array.from(input)[0] ?? "";
      if (!disabled) consumeText(unit);
      input = input.slice(unit.length);
    }
    parserRef.current = input;
  };

  useEffect(() => {
    setRawMode?.(true);
    const onData = (data: Buffer | string) => consume(data.toString());
    stdin.on("data", onData);
    return () => { stdin.off("data", onData); };
  });

  const rendered = useMemo(() => visualRows(state, Math.max(4, columns - 4)), [state, columns]);
  const maxRows = Math.max(1, rows);
  const first = Math.max(0, rendered.findIndex((row) => row.cursor) - maxRows + 1);
  const shown = rendered.slice(first, first + maxRows);

  return (
    <Box borderStyle="round" borderColor={disabled ? "gray" : "blue"} paddingX={1} flexDirection="column" width={Math.max(8, columns)}>
      {shown.map((row, index) => (
        <Text key={`${first}-${index}`}>
          {row.units.join("")}{row.cursor && <Text color="blue" inverse> </Text>}
        </Text>
      ))}
    </Box>
  );
}
