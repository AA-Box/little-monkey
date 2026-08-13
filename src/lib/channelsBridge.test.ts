/**
 * The Channels bridge contract: what the panel sends is what the daemon's
 * commands take.
 *
 * These calls are fixed-argument Tauri commands, so a mismatch is not a type
 * error anywhere — `invoke` takes `unknown`, and a renamed field simply
 * arrives as `undefined`. That failure is silent and looks like a routing bug
 * days later: a thread-scoped route quietly stored as an account-scoped one.
 * So the field names are compared against the Rust source that receives them,
 * the same way `contractDrift.test.ts` compares the published tool contract.
 *
 * Only names and registration are checked here. Whether a scope is legal, and
 * whether two routes tie, stays the daemon's answer — the frontend must not
 * hold a second copy of the routing rules.
 */
import { readFileSync } from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

import type { RouteOptions } from "./channelsClient";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = path.resolve(__dirname, "../../");

const daemonCommands = readFileSync(
  path.join(REPO_ROOT, "src-tauri/src/daemon_commands.rs"),
  "utf8",
);
const libRs = readFileSync(path.join(REPO_ROOT, "src-tauri/src/lib.rs"), "utf8");
const channelsClient = readFileSync(path.join(REPO_ROOT, "src/lib/channelsClient.ts"), "utf8");

/** Every field of one `pub struct` in the Rust source, in declaration order. */
function rustStructFields(source: string, name: string): string[] {
  const start = source.indexOf(`pub struct ${name} {`);
  expect(start, `${name} not found in daemon_commands.rs`).toBeGreaterThan(-1);
  const body = source.slice(start, source.indexOf("\n}", start));
  return [...body.matchAll(/^\s{4}pub (\w+):/gm)].map((match) => match[1]);
}

describe("the channels bridge", () => {
  it("registers every command the panel invokes", () => {
    const invoked = [
      ...new Set([...channelsClient.matchAll(/invoke<[^>]*>\(\s*"(channels_\w+)"/g)].map((m) => m[1])),
    ];
    // A guard against the regex silently matching nothing and the rest of
    // this test passing vacuously.
    expect(invoked.length).toBeGreaterThan(10);
    for (const command of invoked) {
      expect(daemonCommands, `${command} has no #[tauri::command]`).toContain(
        `pub async fn ${command}(`,
      );
      expect(libRs, `${command} is not in generate_handler!`).toContain(
        `daemon_commands::${command},`,
      );
    }
  });

  it("sends exactly the route fields the daemon accepts", () => {
    // Typed as Required<RouteOptions>, so a field added to the TypeScript
    // side without a Rust counterpart fails to compile here, and one added in
    // Rust without a TypeScript counterpart fails the comparison below.
    const everyField: Required<RouteOptions> = {
      account_id: null,
      conversation_id: null,
      thread_id: null,
      sender_id: null,
      kind: null,
      repository: null,
      params: [],
      session_scope: null,
      priority: null,
      reply: null,
      enabled: null,
    };
    expect(rustStructFields(daemonCommands, "RouteOptionArgs").sort()).toEqual(
      Object.keys(everyField).sort(),
    );
  });

  it("covers the whole routing ladder from the bridge", () => {
    // The rungs the daemon resolves in order. Missing one here means a level
    // that can be reached by a message but not configured by an operator,
    // which is the gap this work exists to close.
    for (const field of ["account_id", "conversation_id", "thread_id", "sender_id", "kind"]) {
      expect(rustStructFields(daemonCommands, "RouteOptionArgs")).toContain(field);
    }
  });

  it("never routes a provider request through the frontend", () => {
    // Every channel call is a daemon command. A request issued here would be
    // the app talking to Telegram or Slack directly, outside the daemon's
    // egress policy, its credential handling and its durable event log. The
    // provider URLs this file does hold are documentation links, opened in a
    // browser rather than called.
    for (const source of [
      channelsClient,
      readFileSync(path.join(REPO_ROOT, "src/components/Settings/ChannelsPanel.tsx"), "utf8"),
      readFileSync(path.join(REPO_ROOT, "src/components/Settings/ChannelRoutesSection.tsx"), "utf8"),
    ]) {
      expect(source).not.toMatch(/\bfetch\(/);
      expect(source).not.toMatch(/XMLHttpRequest/);
    }
  });
});
