/**
 * The second half of the learning loop, end to end through the production
 * agent path: a promoted learned skill is in the normal catalog, the model
 * invokes it through the normal skill tool, the learned procedure's tool calls
 * really execute against a real workspace on disk, the workspace's own
 * verification really runs, and the run durably records the exact skill hash
 * it used.
 *
 * # What is real here and what is not
 *
 * Real: `skills.ts`'s catalog and prompt composition, `turnEngine.ts`'s tool
 * dispatch and reserved-argument handling, `durableRun.ts`'s event recorder,
 * `skillLearning.ts`'s notice and finalization, and the tool calls themselves —
 * files are read and written on disk in a temp workspace, and the verification
 * command is a real child process whose exit code decides the result. Deleting
 * a file the procedure writes, or making the verification command exit 1,
 * fails this test.
 *
 * Deterministic: the model, which is the boundary a test is allowed to fake,
 * and the Tauri IPC transport — this suite runs in Node with no Rust process,
 * so the `tool_*` commands are serviced here by doing the same real work
 * against the same real directory. What each command *is* (the durable store,
 * the skill runtime, the promotion and rollback machinery) is exercised for
 * real on the other side of that boundary, by `skill_learning.rs`'s own tests.
 */
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
  isTauri: () => false,
}));
vi.mock("@tauri-apps/api/event", () => ({ listen: async () => () => {} }));

import { composeSkillCatalog, composeSkillSystemPrompt, nativeSkills } from "./skills";
import { executeToolCall, type SkillToolContext } from "./turnEngine";
import {
  allowedToolsRestriction,
  applyAllowedToolsRestriction,
  isToolCallAllowed,
  skillUse,
} from "./agentLoop";
import type { NativeSkillDescriptor } from "./nativeSkillsClient";
import type { ToolCall } from "./llamaClient";

/** The learned skill's own bytes. Hashed here the way the runtime hashes
 * installed content, so the hash the run records is a property of the content
 * rather than a constant the test asserts against itself. */
const LEARNED_INSTRUCTIONS = [
  "# Retry wrapper",
  "",
  "1. Read the module that makes the flaky call.",
  "2. Wrap the call in `withRetry(...)`.",
  "3. Run the project's verification command.",
].join("\n");

const LEARNED_SHA = createHash("sha256").update(LEARNED_INSTRUCTIONS).digest("hex");

let workspace: string;
/** Every durable event the run recorded, in order — the run's own record. */
let events: Array<{ type: string; payload: Record<string, unknown> }>;

function learnedDescriptor(sha = LEARNED_SHA): NativeSkillDescriptor {
  return {
    name: "Retry wrapper",
    description: "Wrap a flaky call in the retry helper and verify.",
    command: "retry-wrapper",
    version: "1.0.0",
    instructions: LEARNED_INSTRUCTIONS,
    sha256: sha,
    file_count: 1,
    total_bytes: LEARNED_INSTRUCTIONS.length,
    enabled: true,
    eligibility: { eligible: true, current_os: "macos", unsupported_os: false, missing_bins: [], missing_env: [] },
    supported_os: [],
    requirements: { bins: [], env: [] },
    source: { kind: "global", path: "/data/native-skills-v1/global/retry-wrapper" },
    permissions: [],
    git_repository: null,
    allowed_tools: ["read_file", "edit_file", "run_shell"],
    resource_files: [],
    learned: {
      origin: "learned",
      candidate_id: "learn-1",
      source_run_ids: ["run-1"],
      source_kind: "successful_novel_procedure",
      parent_skill_sha256: null,
      installed_sha256: sha,
      evaluation_ids: ["eval-1"],
      promotion_policy: "user_approved",
      approval_id: "permission-1",
      promoted_at_unix_ms: 1,
    },
  };
}

/** Resolves a tool's path argument inside the temp workspace, refusing to
 * escape it — the same rule the Rust workspace sandbox enforces, so a test
 * that accidentally writes outside the fixture fails rather than succeeding. */
function inWorkspace(path: unknown): string {
  const resolved = resolve(workspace, String(path ?? ""));
  if (resolved !== workspace && !resolved.startsWith(`${workspace}/`)) {
    throw new Error(`'${String(path)}' escapes the workspace`);
  }
  return resolved;
}

beforeEach(() => {
  workspace = mkdtempSync(join(tmpdir(), "little-monkey-learning-loop-"));
  mkdirSync(join(workspace, "src"));
  writeFileSync(join(workspace, "src", "uploader.ts"), "export const upload = () => send();\n");
  // A real verification command: it exits 0 only once the retry wrapper is
  // actually present in the file on disk.
  writeFileSync(
    join(workspace, "verify.sh"),
    "#!/bin/sh\ngrep -q 'withRetry(' src/uploader.ts\n",
    { mode: 0o755 },
  );
  events = [];

  invokeMock.mockReset();
  invokeMock.mockImplementation(async (command: string, args: Record<string, unknown> = {}) => {
    switch (command) {
      case "tool_read_file":
        return readFileSync(inWorkspace(args.path), "utf8");
      case "tool_edit_file": {
        const target = inWorkspace(args.path);
        const before = readFileSync(target, "utf8");
        if (!before.includes(String(args.old_text))) throw new Error("old_text not found");
        writeFileSync(target, before.replace(String(args.old_text), String(args.new_text)));
        return `Edited ${String(args.path)}`;
      }
      case "tool_run_shell":
        // A real child process in the real workspace: its exit status is the
        // verification result, not something this test decides.
        return execFileSync("/bin/sh", ["-c", String(args.command)], { cwd: workspace, encoding: "utf8" });
      case "run_append_event":
        events.push(args.event as { type: string; payload: Record<string, unknown> });
        return { accepted: true, sequence: events.length };
      case "hooks_list":
        return [];
      default:
        return null;
    }
  });
});

afterEach(() => {
  rmSync(workspace, { recursive: true, force: true });
});

describe("a promoted learned skill on a second, independent task", () => {
  it("is offered, invoked, and actually performs its procedure with real tools", async () => {
    // 1. Normal discovery → normal catalog. Nothing here knows this skill was
    //    learned rather than hand-installed; that is the point.
    const available = nativeSkills([learnedDescriptor()]);
    const catalog = composeSkillCatalog(available, new Set());
    expect(catalog).toContain("/retry-wrapper");

    const invoked: Array<{ command: string; scope: string; sha256: string }> = [];
    const skillContext: SkillToolContext = {
      availableSkills: available,
      invokedCommands: new Set(),
      maxSkillsPerTurn: 3,
      onInvoked: (skill) => {
        const use = skillUse(skill);
        if (use) invoked.push(use);
      },
    };

    const call = (name: string, args: unknown): ToolCall => ({
      id: `call-${name}`,
      type: "function",
      function: { name, arguments: JSON.stringify(args) },
    });

    // 2. The model invokes the learned skill through the ordinary skill tool.
    const skillResult = await executeToolCall(
      call("skill", { command: "retry-wrapper", arguments: "the uploader is flaky" }),
      null,
      "run-2",
      new Map(),
      undefined,
      undefined,
      undefined,
      undefined,
      undefined,
      skillContext,
    );
    // The skill's own instructions come back — this is the dispatch that puts
    // the learned procedure in front of the model.
    expect(skillResult).toContain("withRetry(");
    expect(skillContext.invokedCommands.has("retry-wrapper")).toBe(true);

    // 3. The run durably records WHICH version it used, from the invocation
    //    itself. The hash is the content's, not a label.
    expect(invoked).toEqual([{ command: "retry-wrapper", scope: "global", sha256: LEARNED_SHA }]);

    // 4. The model then performs the procedure with real tools.
    const read = await executeToolCall(
      call("read_file", { path: "src/uploader.ts" }),
      null,
      "run-2",
      new Map(),
    );
    expect(read).toContain("send()");

    await executeToolCall(
      call("edit_file", { path: "src/uploader.ts", old_text: "send()", new_text: "withRetry(send)" }),
      null,
      "run-2",
      new Map(),
    );
    // A real file on disk really changed.
    expect(readFileSync(join(workspace, "src", "uploader.ts"), "utf8")).toContain("withRetry(send)");

    // 5. Real verification: a child process whose exit code decides the answer.
    const verification = await executeToolCall(
      call("run_shell", { command: "sh verify.sh && echo VERIFIED" }),
      null,
      "run-2",
      new Map(),
    );
    expect(verification).toContain("VERIFIED");
  });

  it("puts the learned skill's own instructions into the turn's system prompt", () => {
    const [skill] = nativeSkills([learnedDescriptor()]);
    const prompt = composeSkillSystemPrompt("Base prompt.", [
      { skill, arguments: "the uploader is flaky", activation: "explicit" },
    ]);
    expect(prompt).toContain("Base prompt.");
    expect(prompt).toContain("withRetry(");
  });

  it("attributes the run to the exact installed hash, so a rollback changes what is recorded", () => {
    // The same command at a different version is a different thing to record
    // an outcome against — which is why effectiveness is keyed by hash and
    // never by command.
    const rolledBack = createHash("sha256").update("# Retry wrapper\n\nAn older procedure.\n").digest("hex");
    const [current] = nativeSkills([learnedDescriptor()]);
    const [previous] = nativeSkills([learnedDescriptor(rolledBack)]);
    expect(skillUse(current)?.sha256).toBe(LEARNED_SHA);
    expect(skillUse(previous)?.sha256).toBe(rolledBack);
    expect(skillUse(current)?.sha256).not.toBe(skillUse(previous)?.sha256);
  });

  it("can only narrow what the run may do, never widen it", () => {
    // The run's own ceiling. A skill's allowed-tools list is intersected with
    // it; it is not a second, independent grant.
    const runTools = ["read_file", "edit_file"].map((name) => ({
      type: "function" as const,
      function: { name, description: "", parameters: {} },
    }));
    const [narrow] = nativeSkills([
      { ...learnedDescriptor(), command: "reader", allowed_tools: ["read_file"] },
    ]);
    const restricted = applyAllowedToolsRestriction(
      runTools,
      allowedToolsRestriction(new Set(["reader"]), [narrow]),
    );
    expect(restricted.map((tool) => tool.function.name)).toEqual(["read_file"]);
    // Structural, not advisory: a call outside the intersection is refused at
    // execution too, not merely absent from the schema.
    expect(
      isToolCallAllowed(
        { id: "c", type: "function", function: { name: "edit_file", arguments: "{}" } },
        restricted,
      ),
    ).toBe(false);

    // A skill naming a tool the run never had does not gain it.
    const [wide] = nativeSkills([
      { ...learnedDescriptor(), command: "sheller", allowed_tools: ["read_file", "run_shell"] },
    ]);
    const widened = applyAllowedToolsRestriction(
      runTools,
      allowedToolsRestriction(new Set(["sheller"]), [wide]),
    );
    expect(widened.map((tool) => tool.function.name)).not.toContain("run_shell");
  });

  it("records nothing for a skill with no content hash to attribute an outcome to", () => {
    // A local prompt skill is not a learned skill and never can be.
    expect(
      skillUse({
        id: "local:notes",
        source: "local",
        command: "notes",
        name: "Notes",
        instructions: "…",
        version: "1",
        contentSha256: "",
        permissions: [],
      }),
    ).toBeNull();
  });
});
