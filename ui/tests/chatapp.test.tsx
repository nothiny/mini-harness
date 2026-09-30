import { describe, expect, it } from "vitest";
import React from "react";
import { render } from "ink-testing-library";
import { ChatApp } from "../src/ChatApp.js";
import { DemoClient } from "../src/demoClient.js";

const delay = (milliseconds: number) => new Promise((resolve) => setTimeout(resolve, milliseconds));

class RecordingClient extends DemoClient {
  readonly prompts: string[] = [];

  override request(method: string, params: Record<string, unknown> = {}) {
    if (method === "turn.start") this.prompts.push(String(params.prompt ?? ""));
    return super.request(method, params);
  }
}

class ApprovalEventDroppingClient extends DemoClient {
  override onEvent(listener: Parameters<DemoClient["onEvent"]>[0]) {
    return super.onEvent((notification) => {
      if (notification.event.type === "tool.approval.requested") return;
      listener(notification);
    });
  }
}

describe("ChatApp", () => {
  it("keeps the requested tool name when later events omit it", async () => {
    const client = new DemoClient();
    const view = render(<ChatApp client={client} sessionId="demo-session" />);
    await delay(0);
    view.stdin.write("inspect\r");
    await delay(220);
    const frame = view.lastFrame() ?? "";
    expect(frame).toMatch(/read/);
    expect(frame).toMatch(/助手/);
    view.unmount();
    await client.shutdown();
  });

  it("starts consecutive prompts with separate drafts", async () => {
    const client = new RecordingClient();
    const view = render(<ChatApp client={client} sessionId="demo-session" />);
    await delay(0);

    // The first turn is still running when the second prompt is entered.
    // The composer must clear the submitted draft before accepting new text.
    view.stdin.write("one\r");
    view.stdin.write("two\r");
    await delay(20);

    expect(client.prompts).toEqual(["one", "two"]);
    view.unmount();
    await client.shutdown();
  });

  it("shows the approval action and command while a tool is paused", async () => {
    const client = new DemoClient();
    const view = render(<ChatApp client={client} sessionId="demo-session" />);
    await delay(0);

    view.stdin.write("approve\r");
    await delay(180);

    const frame = view.lastFrame() ?? "";
    expect(frame).toContain("需要批准：bash");
    expect(frame).toContain("cargo test --test process_executor");
    expect(frame).toContain("允许本次");
    expect(frame).toContain("拒绝");

    view.unmount();
    await client.shutdown();
  });

  it("recovers the approval panel from wait when its event is delayed", async () => {
    const client = new ApprovalEventDroppingClient();
    const view = render(<ChatApp client={client} sessionId="demo-session" />);
    await delay(0);

    view.stdin.write("approve\r");
    await delay(220);

    const frame = view.lastFrame() ?? "";
    expect(frame).toContain("等待你批准");
    expect(frame).toContain("需要批准：bash");
    expect(frame).toContain("允许本次");
    expect(frame).toContain("拒绝");

    view.unmount();
    await client.shutdown();
  });
});
