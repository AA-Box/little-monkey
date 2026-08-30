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
 * of what any individual suite prompt requests.
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
import {
  EMPTY_FROZEN_STANDARDS_CONTEXT,
  freezeStandardsForTask,
  type FrozenStandardsContext,
} from "./standardsExecution";
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

const LAB_SESSION_ID = "compare-lab";
const MAX_TOOL_ROUNDS = 4;
const EMPTY_MCP_REGISTRY: McpToolRegistry = new Map();

export interface LabRunHandle {
  runId: string;
  done: Promise<void>;
}

const runControllers = new Map<string, AbortController>();

function toolsFor(prompt: LabPrompt) {
  return prompt.toolsEnabled ? toolsForProfile("explore") : [];
}

function labSystemPrompt(toolsEnabled: boolean, standards: FrozenStandardsContext): string {
  const base = currentSystemPrompt(
    null,
    [],
    false,
    standards.promptSection,
    standards.checkerCommandIds.length > 0,
  );
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

async function runLabPair(
  runId: string,
  prompt: LabPrompt,
  target: ModelTargetSnapshot,
  costRate: LabCostRate | undefined,
  signal: AbortSignal,
  standards: FrozenStandardsContext,
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
    { role: "system", content: labSystemPrompt(toolsOffered, standards) },
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
      // Freeze once per suite prompt before any pair starts. Every model that
      // evaluates the same prompt therefore sees the same approved versions,
      // even if Standards Studio changes while the batch is running.
      const standardsContexts = new Map<string, FrozenStandardsContext>(
        await Promise.all(
          run.prompts.map(async (prompt) => [prompt.id, await freezeStandardsForTask(prompt.text)] as const),
        ),
      );
      const standardsFor = (prompt: LabPrompt) =>
        standardsContexts.get(prompt.id) ?? EMPTY_FROZEN_STANDARDS_CONTEXT;

      const remoteWork = run.prompts.flatMap((prompt) =>
        remoteTargets.map((target) => runLabPair(
          run.id,
          prompt,
          target,
          costRates[target.key],
          controller.signal,
          standardsFor(prompt),
        )),
      );
      const remoteDone = Promise.allSettled(remoteWork);

      for (const target of localTargets) {
        for (const prompt of run.prompts) {
          if (controller.signal.aborted) break;
          await runLabPair(
            run.id,
            prompt,
            target,
            costRates[target.key],
            controller.signal,
            standardsFor(prompt),
          );
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

export function stopLabRun(runId: string): void {
  runControllers.get(runId)?.abort();
}
