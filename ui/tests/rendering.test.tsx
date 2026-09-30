import React from "react";
import { render } from "ink-testing-library";
import { describe, expect, it } from "vitest";
import Markdown from "../src/Markdown.js";
import ToolRow from "../src/ToolRow.js";
import { displayWidth, truncateText, wrapText } from "../src/layout.js";

describe("rendering", () => {
  it("keeps headings, lists, inline code, and fenced whitespace", () => {
  const view = render(
    <Markdown
      columns={32}
      text={'# 标题\n\n说明 `cargo test`\n\n- 第一项\n- 第二项\n\n```sh\n  echo 你好\n\n```\n结束'}
    />,
  );
  const frame = view.lastFrame() ?? "";
  expect(frame).toMatch(/标题/);
  expect(frame).toMatch(/cargo test/);
  expect(frame).toMatch(/- 第一项/);
  expect(frame).toMatch(/  echo 你好/);
  const codeTail = frame.slice(frame.indexOf("echo 你好"));
  expect(codeTail.split("\n").length).toBeGreaterThan(2);
  view.unmount();
  });

  it("wraps by terminal cells for CJK and emoji", () => {
  const lines = wrapText("你好🙂abc", 6);
  expect(lines).toEqual(["你好🙂", "abc"]);
  expect(wrapText("hello world", 5)).toEqual(["hello", "world"]);
  expect(displayWidth("你好🙂")).toBe(6);
  expect(displayWidth(truncateText("你好🙂abc", 5))).toBe(5);
  });

  it("keeps name and status while truncating the summary", () => {
  const view = render(
    <ToolRow
      columns={32}
      now={4_250}
      tool={{
        callId: "call-1",
        name: "bash",
        summary: "cargo test --test process_executor",
        status: "running",
        startedAt: 1_000,
      }}
    />,
  );
  const frame = view.lastFrame() ?? "";
  expect(frame).toMatch(/bash/);
  expect(frame).toMatch(/…/);
  expect(frame).toMatch(/3\.3s/);
  expect(displayWidth(frame.trimEnd().split("\n")[0]!)).toBeLessThanOrEqual(32);
  view.unmount();
  });
});
