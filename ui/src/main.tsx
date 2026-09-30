/**
 * Entry point: terminal ChatGPT for mini-harness.
 *
 * Usage:
 *   npx tsx src/main.tsx [--provider deepseek] [--model deepseek-flash]
 *                        [--workspace .]
 *
 * Each launch creates a fresh session (no log conflicts).
 */

import { parseArgs } from "node:util";
import { resolve, join } from "node:path";
import { tmpdir } from "node:os";
import { isatty } from "node:tty";
import { render } from "ink";
import React from "react";
import { HarnessClient, findBinary, type HarnessClientLike, type SessionContext } from "./client.js";
import { ChatApp } from "./ChatApp.js";
import { DemoClient } from "./demoClient.js";

async function main() {
  const { values } = parseArgs({
    options: {
      provider: { type: "string", short: "p", default: "mock" },
      model: { type: "string", short: "m" },
      workspace: { type: "string", short: "w" },
      binary: { type: "string" },
      demo: { type: "boolean", default: false },
    },
  });

  if (!isatty(0)) {
    console.error(
      "error: this TUI requires an interactive terminal.\n" +
      "For non-interactive use: mini-harness run <prompt>",
    );
    process.exit(1);
  }

  const workspace = resolve(values.workspace ?? ".");

  // Unique log per launch — no "event_log_not_empty" errors.
  const eventLog = join(
    tmpdir(),
    `mini-harness-${Date.now()}-${process.pid}.jsonl`,
  );

  const client: HarnessClientLike = values.demo
    ? new DemoClient()
    : new HarnessClient(findBinary(values.binary));

  try {
    if (client instanceof HarnessClient) await client.negotiate();

    const response = await client.request("session.create", {
      event_log: eventLog,
      workspace,
      provider: values.provider,
      ...(values.model && { model: values.model }),
    });

    if (response.error) {
      throw new Error(response.error.message);
    }

    const result = response.result as { session_id: string; context?: SessionContext };
    const sessionId = result.session_id;
    // Older servers only return the session id. Keep the header truthful in
    // that case by using the trusted CLI values and marking the execution
    // details as unknown instead of pretending the provider is `mock`.
    const context: SessionContext = result.context ?? {
      workspace,
      provider: String(values.provider),
      model: values.model ?? null,
      execution_environment: {
        platform: "unknown",
        filesystem: "unknown",
        network: "unknown",
        environment: "unknown",
      },
    };

    const instance = render(
      React.createElement(ChatApp, { client, sessionId, context }),
      { exitOnCtrlC: false },
    );

    instance.waitUntilExit().then(async () => {
      await client.shutdown();
      process.exit(0);
    });
  } catch (error) {
    console.error(`error: ${(error as Error).message}`);
    await client.shutdown();
    process.exit(1);
  }
}

main();
