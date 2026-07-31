/**
 * Model Compare Lab execution engine (ROADMAP.md Phase 2): batches a saved
 * suite's prompts across a saved model set's targets, reusing
 * `compareRunner.ts`'s target resolution/availability checks and
 * `turnEngine.ts`'s single-attempt streaming primitive — the exact same
 * building blocks the existing one-shot Compare flow uses, just looped
 * across many (prompt, model) pairs instead of one prompt across 2-4
 * branches of a single chat session.
 *
 * Tool-default-off guarantee: a pair's tool schema is empty unless
 * `prompt.toolsEnabled` is `true` (see `toolsFor` below) — Compare Lab never
 * looks at any global/session tool settings for this decision. Even when a
 * prompt opts in, only the read-only `explore` tool profile
 * (`read_file`/`list_dir`/`glob`/`grep`) is ever offered — no mutating tool,
 * `task`, `skill`, or MCP tool is reachable from a lab run at all, regardless
 * of what any individual suite prompt requests. `compareLabRunner.test.ts`
 * pins both halves of this: a default-mode prompt whose model hallucinates a
 * tool call never reaches `invoke`, and an explicit opt-in prompt's tool call
 * does reach it (proving the gate is a real switch, not dead code).
 */
import { resolveTarget, preflightTarget } from "./compareRunner";
import {
  attemptStream,
  executeToolCall,
  isToolCallAllowed,
  stringifyToolError,
  type ResolvedTarget,
} from "./turnEngine";
import type { ChatMessage } from "./llamaClient";
import { currentSystemPrompt } from "./systemPrompt";
import { toolsForProfile } from "./tools";
import { isLocalExecutionTarget } from "./comparisonPlan";
import type { McpToolRegistry } from "./mcpTools";
import {
  computeCostUsd,
  emptyResult,
  evaluateVerifier,
  toolUseSuccessFor,
  type BenchmarkSuite,
  type LabCostRate,
  type LabPrompt,
  type LabResult,
  type LabToolAttempt,
  type LabUsage,
  type ModelSet,
} from "./compareLab";
import type { ModelTargetSnapshot } from "./modelTargets";
import { useCompareLabStore } from "../store/compareLabStore";
import { errorMessage } from "./errors";

/** Synthetic session id passed to `attemptStream` purely so provider
 * rate-limit tracking has something to key on — Lab runs never write into
 * `useUsageStore`/`useUsageHistoryStore` (see `recordUsage: false` below),
 * so nothing user-visible is keyed by this id. */
const LAB_SESSION_ID = "compare-lab";

/** Hard cap on tool-calling rounds for a tools-enabled prompt, mirroring the
 * bounded-loop shape `agentLoop.ts`'s own turn loop uses (a different,
 * larger cap there) — prevents a runaway model from looping forever inside
 * one batched pair. */
const MAX_TOOL_ROUNDS = 4;

/** No MCP server is ever reachable from a lab run — this empty registry is
 * threaded through so `executeToolCall`'s `mcp__`-prefixed dispatch branch is
 * simply unreachable (no name in `EXPLORE_PROFILE_TOOL_NAMES` starts with
 * `mcp__`), never because of any live connection state. */
const EMPTY_MCP_REGISTRY: McpToolRegistry = new Map();

export interface LabRunHandle {
  runId: string;
  done: Promise<void>;
}

const runControllers = new Map<string, AbortController>();

function toolsFor(prompt: LabPrompt) {
  // The ONLY place `compareLabRunner.ts` decides whether any tool schema is
  // offered at all. Everything else in this module treats `tools` as an
  // opaque list handed to `attemptStream`/`executeToolCall` — there is no
  // fallback path anywhere below that widens this beyond the read-only
  // `explore` profile, even for an opted-in prompt.
  return prompt.toolsEnabled ? toolsForProfile("explore") : [];
}

function labSystemPrompt(toolsEnabled: boolean): string {
  const base = currentSystemPrompt(null, [], false);
  const suffix = toolsEnabled
    ? [
        "",
        "## Compare Lab run — read-only tools enabled for this prompt",
        "You are one independently evaluated branch of a Model Compare Lab suite run. This prompt has explicitly opted into a small set of READ-ONLY tools (read_file, list_dir, glob, grep). No other tool is available, and none of these can modify anything. Use them only if the prompt actually requires it.",
      ].join("\n")
    : [
        "",
        "## Compare Lab run — read-only, no tools",
        "You are one independently evaluated branch of a Model Compare Lab suite run. No tools are available in this run. Answer the prompt directly; do not claim to have read, changed, executed, or verified anything that is not already present in the prompt itself.",
      ].join("\n");
  return `${base}${suffix}`;
}

function addUsage(a: LabUsage | null, b: LabUsage | undefined): LabUsage | null {
  if (!b) return a;
  return {
    promptTokens: (a?.promptTokens ?? 0) + b.promptTokens,
    completionTokens: (a?.completionTokens ?? 0) + b.completionTokens,
    totalTokens: (a?.totalTokens ?? 0) + b.totalTokens,
  };
}

/** Runs exactly one (prompt, target) pair to completion and writes every
 * intermediate/final state into `compareLabStore` as it goes, so the UI can
 * render partial progress live instead of only after the whole suite
 * finishes. Never throws — every failure mode (unavailable target, stream
 * error, aborted) is recorded as a terminal `LabResult` instead. */
async function runLabPair(
  runId: string,
  prompt: LabPrompt,
  target: ModelTargetSnapshot,
  costRate: LabCostRate | undefined,
  signal: AbortSignal,
): Promise<void> {
  const store = useCompareLabStore.getState();
  const toolsOffered = prompt.toolsEnabled === true;
  const startedAt = Date.now();
  store.updateResult(runId, prompt.id, target.key, {
    ...emptyResult(prompt.id, target.key, toolsOffered),
    status: "running",
    startedAt,
  });

  const finalize = (patch: Partial<LabResult>) => {
    const completedAt = Date.now();
    const usage = patch.usage ?? null;
    store.updateResult(runId, prompt.id, target.key, {
      completedAt,
      latencyMs: completedAt - startedAt,
      costUsd: computeCostUsd(costRate, usage),
      ...patch,
    });
  };

  try {
    preflightTarget(target);
  } catch (error) {
    finalize({ status: "failed", error: errorMessage(error) });
    return;
  }

  if (signal.aborted) {
    finalize({ status: "cancelled" });
    return;
  }

  let resolved: ResolvedTarget;
  try {
    resolved = await resolveTarget(target);
  } catch (error) {
    finalize({ status: "failed", error: errorMessage(error) });
    return;
  }

  const tools = toolsFor(prompt);
  const wireHistory: ChatMessage[] = [
    { role: "system", content: labSystemPrompt(toolsOffered) },
    { role: "user", content: prompt.text },
  ];
  const toolAttempts: LabToolAttempt[] = [];
  let usage: LabUsage | null = null;
  let content = "";

  for (let round = 0; round < Math.max(1, MAX_TOOL_ROUNDS); round++) {
    if (signal.aborted) {
      finalize({ status: "cancelled", usage, toolAttempts, content });
      return;
    }
    const result = await attemptStream(
      resolved,
      wireHistory,
      tools,
      signal,
      target.effort,
      LAB_SESSION_ID,
      (delta) => {
        content = delta;
        store.updateResult(runId, prompt.id, target.key, { content: delta, status: "running" });
      },
      false,
    );
    usage = addUsage(usage, result.usage);
    content = result.content;

    if (signal.aborted) {
      finalize({ status: "cancelled", usage, toolAttempts, content });
      return;
    }
    if (result.streamError) {
      finalize({ status: "failed", error: result.streamError, usage, toolAttempts, content });
      return;
    }
    if (result.toolCalls.length === 0) {
      const verifierOutcome = evaluateVerifier(prompt.verifier, content);
      finalize({
        status: "completed",
        usage,
        toolAttempts,
        content,
        verifierOutcome,
        toolUseSuccess: toolUseSuccessFor(toolAttempts),
        error: null,
      });
      return;
    }

    // The model asked for a tool. Every attempt is recorded regardless of
    // whether it was actually offered — the whole point of `allowed` below
    // is to make a hallucinated call outside the offered schema (or ANY call
    // at all under a default no-tools prompt, since `tools` is `[]` there
    // and nothing can ever be `allowed`) visible in the report rather than
    // silently dropped.
    wireHistory.push({ role: "assistant", content: result.content, tool_calls: result.toolCalls });
    for (const toolCall of result.toolCalls) {
      const allowed = isToolCallAllowed(toolCall, tools);
      let resultContent: string;
      if (!allowed) {
        resultContent = stringifyToolError(
          new Error(
            toolsOffered
              ? `Tool "${toolCall.function.name}" was not offered this run and was not executed.`
              : "No tools are available in this default Compare Lab run; the request was not executed.",
          ),
        );
      } else {
        resultContent = await executeToolCall(toolCall, null, `compare-lab:${runId}`, EMPTY_MCP_REGISTRY, signal);
      }
      toolAttempts.push({
        name: toolCall.function.name,
        argumentsJson: toolCall.function.arguments ?? "{}",
        offered: toolsOffered,
        allowed,
        executed: allowed,
        resultSummary: resultContent.length > 2000 ? `${resultContent.slice(0, 2000)}…` : resultContent,
      });
      wireHistory.push({ role: "tool", tool_call_id: toolCall.id, content: resultContent });
    }

    if (!toolsOffered) {
      // A default (tools-off) prompt whose model still emitted a tool call:
      // every attempt above was rejected without execution. Stop here rather
      // than looping the rejection back for more rounds — this is reported
      // as a failed pair (blocked tool use), consistent with how the
      // existing single-shot Compare flow treats the same situation in
      // `compareRunner.ts`'s `runBranch`.
      finalize({
        status: "failed",
        error: "The model requested a tool in a default (tools-off) Compare Lab run; no tool was executed.",
        usage,
        toolAttempts,
        content,
        toolUseSuccess: toolUseSuccessFor(toolAttempts),
      });
      return;
    }
  }

  // Exhausted MAX_TOOL_ROUNDS while tools stayed in play the whole time.
  const verifierOutcome = evaluateVerifier(prompt.verifier, content);
  finalize({
    status: "completed",
    usage,
    toolAttempts,
    content,
    verifierOutcome,
    toolUseSuccess: toolUseSuccessFor(toolAttempts),
    error: null,
  });
}

/** Starts a full suite run: every (prompt, target) pair in `suite.prompts` ×
 * `modelSet.targets`. Remote-execution targets (providers, cloud Ollama tags)
 * all run fully concurrently; any local-execution target (local llama.cpp,
 * non-cloud Ollama — see `comparisonPlan.ts`'s `isLocalExecutionTarget`) is
 * always serialized, one pair at a time, for the whole run — a simpler,
 * strictly more conservative rule than `comparisonPlan.ts`'s per-comparison
 * memory-budget planner (which only serializes when an estimate actually
 * exceeds available memory): a lab run can cover many more prompts than a
 * single comparison, so this never risks two local runtimes resident at
 * once, at the cost of sometimes serializing when concurrent local execution
 * would in fact have fit. */
export function startLabRun(
  suite: BenchmarkSuite,
  modelSet: ModelSet,
  costRates: Readonly<Record<string, LabCostRate>>,
): LabRunHandle {
  if (suite.prompts.length === 0) throw new Error("This suite has no prompts to run.");
  if (modelSet.targets.length === 0) throw new Error("This model set has no models to run against.");

  const store = useCompareLabStore.getState();
  const run = store.createRun(suite, modelSet);
  const controller = new AbortController();
  runControllers.set(run.id, controller);

  const localTargets = run.targets.filter(isLocalExecutionTarget);
  const remoteTargets = run.targets.filter((target) => !isLocalExecutionTarget(target));

  const done = (async () => {
    try {
      const remoteWork = run.prompts.flatMap((prompt) =>
        remoteTargets.map((target) => runLabPair(run.id, prompt, target, costRates[target.key], controller.signal)),
      );
      const remoteDone = Promise.allSettled(remoteWork);

      for (const target of localTargets) {
        for (const prompt of run.prompts) {
          if (controller.signal.aborted) break;
          await runLabPair(run.id, prompt, target, costRates[target.key], controller.signal);
        }
      }

      await remoteDone;
    } finally {
      runControllers.delete(run.id);
      useCompareLabStore.getState().completeRun(run.id, controller.signal.aborted ? "cancelled" : "completed");
    }
  })();

  return { runId: run.id, done };
}

/** Aborts every still-in-flight pair of a run. Already-finished pairs keep
 * their recorded terminal state; in-flight pairs are marked `cancelled`. */
export function stopLabRun(runId: string): void {
  runControllers.get(runId)?.abort();
}
