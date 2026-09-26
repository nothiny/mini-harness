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
import { HarnessClient, findBinary } from "./client.js";
import { ChatApp } from "./ChatApp.js";

async function main() {
  const { values } = parseArgs({
    options: {
      provider: { type: "string", short: "p", default: "mock" },
      model: { type: "string", short: "m" },
      workspace: { type: "string", short: "w" },
      binary: { type: "string" },
    },
  });

  if (!isatty(0)) {
    console.error(
      "error: this TUI requires an interactive terminal.\n" +
      "For non-interactive use: mini-harness run <prompt>",
    );
    process.exit(1);
  }

  const binary = findBinary(values.binary);
  const workspace = resolve(values.workspace ?? ".");

  // Unique log per launch — no "event_log_not_empty" errors.
  const eventLog = join(
    tmpdir(),
    `mini-harness-${Date.now()}-${process.pid}.jsonl`,
  );

  const client = new HarnessClient(binary);

  try {
    await client.negotiate();

    const response = await client.request("session.create", {
      event_log: eventLog,
      workspace,
      provider: values.provider,
      ...(values.model && { model: values.model }),
    });

    if (response.error) {
      throw new Error(response.error.message);
    }

    const sessionId = (response.result as any).session_id;

    const instance = render(
      React.createElement(ChatApp, { client, sessionId }),
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
