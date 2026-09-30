/** Small, dependency-free line editor used by the Ink composer. */

export interface EditorState {
  text: string;
  /** Cursor is a grapheme offset, rather than a UTF-16 offset. */
  cursor: number;
  historyIndex: number | null;
  draft: string;
}

export function graphemes(text: string): string[] {
  const Segmenter = (Intl as unknown as { Segmenter?: new (locale?: string, options?: { granularity: string }) => { segment(value: string): Iterable<{ segment: string }> } }).Segmenter;
  if (Segmenter) {
    return Array.from(new Segmenter(undefined, { granularity: "grapheme" }).segment(text), (part) => part.segment);
  }
  // Array.from at least keeps surrogate pairs together on older runtimes.
  return Array.from(text);
}

export function editorState(text = ""): EditorState {
  return { text, cursor: graphemes(text).length, historyIndex: null, draft: text };
}

function withText(state: EditorState, text: string, cursor = graphemes(text).length): EditorState {
  return { ...state, text, cursor, historyIndex: null, draft: text };
}

export function setEditorText(state: EditorState, text: string, cursor?: number): EditorState {
  const units = graphemes(text);
  return { ...state, text, cursor: Math.max(0, Math.min(cursor ?? units.length, units.length)), historyIndex: null, draft: text };
}

export function insertText(state: EditorState, inserted: string): EditorState {
  if (!inserted) return state;
  const units = graphemes(state.text);
  const add = graphemes(inserted);
  const next = [...units.slice(0, state.cursor), ...add, ...units.slice(state.cursor)];
  const text = next.join("");
  // A combining mark can join the grapheme immediately before the insertion.
  // Re-segment the prefix instead of adding code-point counts blindly.
  const cursor = graphemes(units.slice(0, state.cursor).join("") + inserted).length;
  return { ...state, text, cursor, historyIndex: null, draft: text };
}

export function backspace(state: EditorState): EditorState {
  if (state.cursor === 0) return state;
  const units = graphemes(state.text);
  units.splice(state.cursor - 1, 1);
  const text = units.join("");
  return { ...state, text, cursor: state.cursor - 1, historyIndex: null, draft: text };
}

export function deleteForward(state: EditorState): EditorState {
  const units = graphemes(state.text);
  if (state.cursor >= units.length) return state;
  units.splice(state.cursor, 1);
  const text = units.join("");
  return { ...state, text, historyIndex: null, draft: text };
}

export function moveCursor(state: EditorState, direction: "left" | "right" | "home" | "end"): EditorState {
  const units = graphemes(state.text);
  if (direction === "left") return { ...state, cursor: Math.max(0, state.cursor - 1) };
  if (direction === "right") return { ...state, cursor: Math.min(units.length, state.cursor + 1) };
  const before = units.slice(0, state.cursor).join("");
  const lineStart = before.lastIndexOf("\n") + 1;
  if (direction === "home") return { ...state, cursor: graphemes(before.slice(0, lineStart)).length };
  const remainder = state.text.slice(before.length);
  const newline = remainder.indexOf("\n");
  return {
    ...state,
    cursor: newline < 0 ? units.length : state.cursor + graphemes(remainder.slice(0, newline)).length,
  };
}

export function navigateHistory(state: EditorState, history: string[], direction: "up" | "down"): EditorState {
  if (!history.length) return state;
  if (direction === "up") {
    const index = state.historyIndex === null ? history.length - 1 : Math.max(0, state.historyIndex - 1);
    const text = history[index] ?? "";
    return { ...state, text, cursor: graphemes(text).length, historyIndex: index };
  }
  if (state.historyIndex === null) return state;
  if (state.historyIndex < history.length - 1) {
    const index = state.historyIndex + 1;
    const text = history[index] ?? "";
    return { ...state, text, cursor: graphemes(text).length, historyIndex: index };
  }
  return { ...state, text: state.draft, cursor: graphemes(state.draft).length, historyIndex: null };
}

export function addHistory(history: string[], text: string, limit = 100): string[] {
  if (!text.trim()) return history;
  const withoutDuplicate = history.filter((item) => item !== text);
  return [...withoutDuplicate, text].slice(-limit);
}

export function currentLineStart(state: EditorState): number {
  const before = graphemes(state.text).slice(0, state.cursor).join("");
  return graphemes(before.slice(before.lastIndexOf("\n") + 1)).length;
}

// Kept exported for consumers that need to reset the editor after acceptance.
export { withText };
