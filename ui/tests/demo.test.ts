import { describe, expect, it } from "vitest";
import { DemoClient } from "../src/demoClient.js";

const delay = (milliseconds: number) => new Promise((resolve) => setTimeout(resolve, milliseconds));

describe("demo client", () => {
it("emits correlated tool lifecycle events", async () => {
  const client = new DemoClient();
  const events: string[] = [];
  client.onEvent((notification) => events.push(`${notification.event.type}:${notification.event.tool_call_id ?? ""}`));
  const started = await client.request("turn.start", { prompt: "inspect the process" });
  const turnId = (started.result as { turn_id: string }).turn_id;
  await delay(150);
  const waited = await client.request("turn.wait", { turn_id: turnId });
  expect(waited.error).toBeUndefined();
  expect(events.join("\n")).toMatch(/tool\.requested:demo-call-1/);
  expect(events.join("\n")).toMatch(/tool\.started:demo-call-1/);
  expect(events.join("\n")).toMatch(/tool\.completed:demo-call-1/);
  await client.shutdown();
});
});
