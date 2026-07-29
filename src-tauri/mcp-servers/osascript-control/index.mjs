#!/usr/bin/env node
// Little Monkey's bundled "macOS Control (AppleScript)" MCP server — a
// from-scratch port of k6l3/osascript-dxt's idea (a single `osascript` tool
// wrapping AppleScript execution) as a dependency-free MCP stdio server, so
// `mcp_stage_bundled_server` (src-tauri/src/bundled_mcp_servers.rs) can embed
// this file's source via `include_str!` and run it with a bare `node` — no
// `npm install`, no `@modelcontextprotocol/sdk`, nothing to publish.
//
// Protocol: MCP's stdio transport is newline-delimited JSON-RPC 2.0 (no
// Content-Length framing, unlike LSP) — one message per line on stdin/stdout.
// This implements just enough of it for one tool: `initialize`,
// `notifications/initialized`, `tools/list`, `tools/call`, `ping`.
//
// Every AppleScript this tool runs is exactly the `script` argument the
// calling agent passed — visible in Little Monkey's normal tool-call
// permission prompt before it ever executes, the same as `run_shell`'s
// command text. This file adds no gating of its own beyond that; it trusts
// the host app's existing permission system, matching every other MCP
// server's tools.
//
// AppleScript can do anything a signed-in user can do through Apple Events:
// control System Events (GUI scripting), Mail, Messages, Contacts, Finder,
// or run arbitrary shell via `do shell script`. Treat approving a call to
// this tool exactly like approving a shell command — because it is one.

import { execFile } from "node:child_process";
import { createInterface } from "node:readline";

const TOOL_NAME = "run_applescript";
const OSASCRIPT_TIMEOUT_MS = 30_000;
const OSASCRIPT_MAX_BUFFER_BYTES = 1024 * 1024;

function send(message) {
  process.stdout.write(`${JSON.stringify(message)}\n`);
}

function replyResult(id, result) {
  if (id === undefined || id === null) return; // notification — no reply
  send({ jsonrpc: "2.0", id, result });
}

function replyError(id, code, message) {
  if (id === undefined || id === null) return;
  send({ jsonrpc: "2.0", id, error: { code, message } });
}

function toolDescriptor() {
  return {
    name: TOOL_NAME,
    description:
      "Runs an AppleScript on macOS via `osascript` and returns its stdout. " +
      "Can control any scriptable application (Finder, Mail, System Events " +
      "GUI scripting, etc.) or run a shell command through `do shell script` " +
      "— treat the script text as fully trusted, exactly like a shell command.",
    inputSchema: {
      type: "object",
      properties: {
        script: {
          type: "string",
          description: "AppleScript source to execute with `osascript -e`.",
        },
      },
      required: ["script"],
      additionalProperties: false,
    },
  };
}

function runAppleScript(script) {
  return new Promise((resolve) => {
    if (process.platform !== "darwin") {
      resolve({ ok: false, output: "osascript is only available on macOS." });
      return;
    }
    execFile(
      "osascript",
      ["-e", script],
      { timeout: OSASCRIPT_TIMEOUT_MS, maxBuffer: OSASCRIPT_MAX_BUFFER_BYTES },
      (error, stdout, stderr) => {
        if (error) {
          const detail = stderr?.trim() || error.message;
          resolve({ ok: false, output: detail });
          return;
        }
        resolve({ ok: true, output: stdout });
      },
    );
  });
}

async function handleToolsCall(id, params) {
  const name = params?.name;
  const args = params?.arguments ?? {};
  if (name !== TOOL_NAME) {
    replyError(id, -32602, `Unknown tool: ${name}`);
    return;
  }
  const script = args.script;
  if (typeof script !== "string" || script.length === 0) {
    replyError(id, -32602, "Missing required string argument 'script'.");
    return;
  }
  const { ok, output } = await runAppleScript(script);
  replyResult(id, {
    content: [{ type: "text", text: output }],
    isError: !ok,
  });
}

async function handleLine(line) {
  const trimmed = line.trim();
  if (trimmed.length === 0) return;

  let message;
  try {
    message = JSON.parse(trimmed);
  } catch {
    return; // not a valid JSON-RPC frame — nothing sane to reply with
  }

  const { id, method, params } = message;
  switch (method) {
    case "initialize":
      replyResult(id, {
        protocolVersion: params?.protocolVersion ?? "2024-11-05",
        capabilities: { tools: {} },
        serverInfo: { name: "little-monkey-osascript-control", version: "1.0.0" },
      });
      break;
    case "notifications/initialized":
      break; // notification — nothing to do
    case "ping":
      replyResult(id, {});
      break;
    case "tools/list":
      replyResult(id, { tools: [toolDescriptor()] });
      break;
    case "tools/call":
      await handleToolsCall(id, params);
      break;
    default:
      replyError(id, -32601, `Method not found: ${method}`);
  }
}

// No explicit `rl.on("close", () => process.exit(...))`: stdin ending while
// an `osascript` child process is still running for an in-flight
// `tools/call` must not kill that pending reply. Node exits on its own once
// every handle (including that child process) has settled and nothing else
// is pending — the correct behavior here, not a timing accident.
const rl = createInterface({ input: process.stdin, terminal: false });
rl.on("line", (line) => {
  void handleLine(line);
});
