import React from "react";
import { render } from "ink-testing-library";
import { describe, expect, it, vi } from "vitest";
import { Composer } from "../src/Composer.js";
import { backspace, editorState, graphemes, insertText, moveCursor, navigateHistory } from "../src/editor.js";

describe("editor", () => {
  it("deletes an emoji as one grapheme", () => {
    let state = editorState("a🙂b");
    state = moveCursor(state, "left");
    state = backspace(state);
    expect(state.text).toBe("ab");
  });

  it("keeps a combining sequence together", () => {
    const state = insertText(editorState(""), "e\u0301");
    expect(graphemes(state.text)).toEqual(["e\u0301"]);
    expect(state.cursor).toBe(1);
  });

  it("restores a draft after history navigation", () => {
    let state = editorState("draft");
    state = navigateHistory(state, ["one", "two"], "up");
    state = navigateHistory(state, ["one", "two"], "down");
    expect(state.text).toBe("draft");
  });

  it("moves End to the current line after a wide grapheme", () => {
    const state = moveCursor(editorState("🙂x\ny"), "home");
    const end = moveCursor({ ...state, cursor: 1 }, "end");
    expect(end.cursor).toBe(2);
  });
});

describe("Composer", () => {
  it("keeps bracketed multiline paste in the editor", async () => {
    const onChange = vi.fn();
    const onSubmit = vi.fn();
    const app = render(<Composer value="" onChange={onChange} onSubmit={onSubmit} history={[]} columns={40} />);
    await new Promise((resolve) => setImmediate(resolve));
    app.stdin.write("\u001b[200~first\nsecond");
    app.stdin.write("\u001b[201~");
    await new Promise((resolve) => setImmediate(resolve));
    expect(onSubmit).not.toHaveBeenCalled();
    expect(onChange.mock.lastCall?.[0]).toBe("first\nsecond");
    app.unmount();
  });

  it("clears a submitted draft immediately and restores it after rejection", async () => {
    const onChange = vi.fn();
    let reject!: (accepted: boolean) => void;
    const onSubmit = vi.fn(() => new Promise<boolean>((resolve) => { reject = resolve; }));
    const app = render(<Composer value="" onChange={onChange} onSubmit={onSubmit} history={[]} columns={40} />);
    await new Promise((resolve) => setImmediate(resolve));
    app.stdin.write("hello\r");
    await new Promise((resolve) => setImmediate(resolve));
    expect(onSubmit).toHaveBeenCalledWith("hello");
    expect(onChange.mock.lastCall?.[0]).toBe("");
    app.stdin.write("next");
    await new Promise((resolve) => setImmediate(resolve));
    reject(false);
    await new Promise((resolve) => setImmediate(resolve));
    expect(onChange.mock.lastCall?.[0]).toBe("next");
    app.unmount();
  });
});
