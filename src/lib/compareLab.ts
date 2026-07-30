/**
 * Model Compare Lab (ROADMAP.md Phase 2): shared types and pure logic for
 * batch-running a saved prompt suite across a saved model set, scoring the
 * results, and building an exportable report.
 *
 * This module deliberately owns no execution or persistence — see
 * `compareLabRunner.ts` for the fan-out engine (which reuses
 * `compareRunner.ts`'s `resolveTarget`/`preflightTarget` and
 * `turnEngine.ts`'s `attemptStream`/`executeToolCall`) and
 * `store/compareLabStore.ts` for the saved suites/model sets/run history.
 * Keeping verifier evaluation, cost math, and report aggregation here (pure,
 * no Zustand/Tauri imports) makes them trivially unit-testable and reusable
 * from both the runner and the UI.
 */
import type { ModelTargetSnapshot } from "./modelTargets";

export type BenchmarkCategory = "coding" | "writing" | "rag" | "browser_qa" | "connector" | "custom";

export const BENCHMARK_CATEGORIES: readonly BenchmarkCategory[] = [
  "coding",
  "writing",
  "rag",
  "browser_qa",
  "connector",
  "custom",
];

export type LabVerifierKind = "contains" | "not_contains" | "regex" | "json_valid" | "min_length";

/** A lightweight, deterministic, client-side checker run against a branch's
 * final response text. This is a Compare-Lab-scoped "verifier" concept —
 * NOT the same thing as `agentLoop.ts`'s `runVerificationPhase` (which shells
 * out to a user-configured build/test command against files an agent turn
 * actually mutated on disk). That machinery assumes a single live session,
 * a workspace, and — critically — tool-driven mutations; Compare Lab runs
 * are batched, tool-off-by-default, and often have no workspace mutation at
 * all, so reusing it directly isn't meaningful. This is the equivalent
 * concept scaled to fit: a pass/fail check plus a human-readable reason,
 * mirroring `VerifyNotice`'s `ok`/label shape closely enough that the UI and
 * report reuse the same mental model. */
export interface LabVerifier {
  kind: LabVerifierKind;
  /** Needle/pattern/threshold. Ignored for `json_valid`. */
  value?: string;
  /** Regex flags; `regex` kind only. */
  flags?: string;
  /** Short human-readable description shown next to the pass/fail badge. */
  label: string;
}

export interface LabVerifierOutcome {
  ok: boolean;
  message: string;
}

/** One prompt inside a saved suite. Tools are OFF for this prompt unless
 * `toolsEnabled` is explicitly `true` — see `compareLabRunner.ts`'s
 * `runLabPair`, the single place that reads this flag to decide whether any
 * tool schema is ever offered to the model. */
export interface LabPrompt {
  id: string;
  text: string;
  toolsEnabled: boolean;
  verifier: LabVerifier | null;
  /** Free-text rubric dimensions the user can score this prompt's answers
   * against (e.g. ["Correctness", "Clarity"]). Advisory labels only — the
   * actual score is a single 1-5 integer per (prompt, model) result. */
  rubricCriteria: string[];
}

export interface BenchmarkSuite {
  id: string;
  name: string;
  description: string;
  category: BenchmarkCategory;
  prompts: LabPrompt[];
  /** Marks a seeded starter suite (see `STARTER_SUITES`) vs. one the user
   * created or cloned. Purely informational — built-ins are editable and
   * deletable like any other saved suite. */
  builtIn: boolean;
  createdAt: number;
  updatedAt: number;
}

/** A saved, named set of model targets to batch a suite against. Targets are
 * frozen `ModelTargetSnapshot`s, same identity contract `compareRunner.ts`
 * relies on for its own comparisons. */
export interface ModelSet {
  id: string;
  name: string;
  targets: ModelTargetSnapshot[];
  createdAt: number;
  updatedAt: number;
}

/** User-entered $-per-million-token rates for one model target, used only to
 * compute an estimated cost — never fetched or assumed. Absent/zero rates
 * simply leave `costUsd` as `null` on every result for that target. */
export interface LabCostRate {
  inputPerMillionUsd: number;
  outputPerMillionUsd: number;
}

export type LabResultStatus = "pending" | "running" | "completed" | "failed" | "cancelled";

export interface LabUsage {
  promptTokens: number;
  completionTokens: number;
  totalTokens: number;
}

/** One tool call the model attempted during a lab run. `offered` records
 * whether tools were even available this run (always `false` unless the
 * prompt opted in); `allowed` records whether THIS specific call named a
 * tool that was actually offered (mirrors `turnEngine.ts`'s
 * `isToolCallAllowed` enforcement — a model can hallucinate a tool name
 * outside its own offered schema even when tools are on). */
export interface LabToolAttempt {
  name: string;
  argumentsJson: string;
  offered: boolean;
  allowed: boolean;
  executed: boolean;
  resultSummary: string;
}

export interface LabRubric {
  score: number | null;
  notes: string;
}

/** One (prompt, model) cell of a suite run's result matrix. */
export interface LabResult {
  promptId: string;
  targetKey: string;
  status: LabResultStatus;
  content: string;
  startedAt: number | null;
  completedAt: number | null;
  latencyMs: number | null;
  usage: LabUsage | null;
  costUsd: number | null;
  /** Whether this specific prompt/run had any tool schema offered to the
   * model at all — the load-bearing field the tool-default-off tests assert
   * on, and what the report/UI badge as "Tools: off" vs "Tools: on". */
  toolsOffered: boolean;
  toolAttempts: LabToolAttempt[];
  /** `null` when no tool call was attempted (nothing to grade); otherwise
   * whether every attempted call was both allowed and executed without
   * error. */
  toolUseSuccess: boolean | null;
  verifierOutcome: LabVerifierOutcome | null;
  rubric: LabRubric;
  error: string | null;
}

export type LabRunStatus = "running" | "completed" | "cancelled";

/** A full suite-run report: frozen suite/model-set snapshots (so a report
 * remains meaningful even if the originating suite or model set is later
 * edited or deleted) plus the full result matrix. */
export interface LabRun {
  id: string;
  suiteId: string;
  suiteName: string;
  suiteCategory: BenchmarkCategory;
  modelSetId: string;
  modelSetName: string;
  prompts: LabPrompt[];
  targets: ModelTargetSnapshot[];
  createdAt: number;
  completedAt: number | null;
  status: LabRunStatus;
  results: LabResult[];
}

export function createLabPromptId(): string {
  return crypto.randomUUID();
}

export function createLabPrompt(text: string, overrides: Partial<Omit<LabPrompt, "id" | "text">> = {}): LabPrompt {
  return {
    id: createLabPromptId(),
    text,
    toolsEnabled: false,
    verifier: null,
    rubricCriteria: [],
    ...overrides,
  };
}

export function emptyResult(promptId: string, targetKey: string, toolsOffered: boolean): LabResult {
  return {
    promptId,
    targetKey,
    status: "pending",
    content: "",
    startedAt: null,
    completedAt: null,
    latencyMs: null,
    usage: null,
    costUsd: null,
    toolsOffered,
    toolAttempts: [],
    toolUseSuccess: null,
    verifierOutcome: null,
    rubric: { score: null, notes: "" },
    error: null,
  };
}

/** Evaluates a prompt's saved verifier against one branch's final response
 * text. Pure and synchronous — the runner calls this once a result's content
 * is final; the UI/report never need to re-derive it independently. */
export function evaluateVerifier(verifier: LabVerifier | null, content: string): LabVerifierOutcome | null {
  if (!verifier) return null;
  const text = content ?? "";
  switch (verifier.kind) {
    case "contains": {
      const needle = (verifier.value ?? "").trim();
      if (!needle) return { ok: false, message: "Verifier has no expected text configured." };
      const ok = text.toLowerCase().includes(needle.toLowerCase());
      return { ok, message: ok ? `Contains "${needle}".` : `Missing expected text "${needle}".` };
    }
    case "not_contains": {
      const needle = (verifier.value ?? "").trim();
      if (!needle) return { ok: true, message: "No forbidden text configured." };
      const ok = !text.toLowerCase().includes(needle.toLowerCase());
      return { ok, message: ok ? `Does not contain "${needle}".` : `Unexpectedly contains "${needle}".` };
    }
    case "regex": {
      const pattern = verifier.value ?? "";
      try {
        const re = new RegExp(pattern, verifier.flags ?? "");
        const ok = re.test(text);
        const description = `/${pattern}/${verifier.flags ?? ""}`;
        return { ok, message: ok ? `Matches ${description}.` : `Does not match ${description}.` };
      } catch (error) {
        return { ok: false, message: `Invalid verifier regex: ${error instanceof Error ? error.message : String(error)}` };
      }
    }
    case "json_valid": {
      try {
        JSON.parse(text);
        return { ok: true, message: "Response parses as valid JSON." };
      } catch {
        return { ok: false, message: "Response is not valid JSON." };
      }
    }
    case "min_length": {
      const min = Number.parseInt(verifier.value ?? "0", 10) || 0;
      const length = text.trim().length;
      const ok = length >= min;
      return { ok, message: ok ? `Meets minimum length of ${min}.` : `Below minimum length of ${min} (got ${length}).` };
    }
    default:
      return null;
  }
}

/** `null` when the rate isn't configured (unknown cost, never assumed) or
 * there's no usage to price yet. */
export function computeCostUsd(rate: LabCostRate | null | undefined, usage: LabUsage | null): number | null {
  if (!rate || !usage) return null;
  if (!Number.isFinite(rate.inputPerMillionUsd) || !Number.isFinite(rate.outputPerMillionUsd)) return null;
  if (rate.inputPerMillionUsd < 0 || rate.outputPerMillionUsd < 0) return null;
  const cost =
    (usage.promptTokens / 1_000_000) * rate.inputPerMillionUsd +
    (usage.completionTokens / 1_000_000) * rate.outputPerMillionUsd;
  return Number.isFinite(cost) ? cost : null;
}

/** Whether every tool call attempted in `attempts` was both allowed (named a
 * tool actually offered this run) and executed without the runner recording
 * an error. `null` when nothing was attempted — there is nothing to grade,
 * distinct from a graded failure. */
export function toolUseSuccessFor(attempts: readonly LabToolAttempt[]): boolean | null {
  if (attempts.length === 0) return null;
  return attempts.every((attempt) => attempt.allowed && attempt.executed);
}

export interface LabModelSummary {
  targetKey: string;
  label: string;
  totalPrompts: number;
  completed: number;
  failed: number;
  cancelled: number;
  avgLatencyMs: number | null;
  totalPromptTokens: number;
  totalCompletionTokens: number;
  totalTokens: number;
  /** `null` only when NOT ONE result for this model has a known cost —
   * otherwise sums whatever is known (see `costKnownForAll` for whether that
   * sum is exhaustive or partial). */
  totalCostUsd: number | null;
  /** `false` when at least one completed result for this model has no known
   * cost (rate not configured) — the report/UI should label the total as an
   * estimate/partial in that case rather than implying completeness. */
  costKnownForAll: boolean;
  verifierPassRate: number | null;
  toolUseSuccessRate: number | null;
  avgRubricScore: number | null;
  ratedCount: number;
}

function average(values: readonly number[]): number | null {
  if (values.length === 0) return null;
  return values.reduce((sum, value) => sum + value, 0) / values.length;
}

/** Builds one model's summary row across every prompt in `run`. Exported
 * (rather than folded only into `buildLabReport`) so the UI can recompute a
 * single model's row live while a run is still in progress, without
 * recomputing every other model's row too. */
export function summarizeModel(run: LabRun, targetKey: string): LabModelSummary {
  const target = run.targets.find((candidate) => candidate.key === targetKey);
  const rows = run.results.filter((result) => result.targetKey === targetKey);
  const completed = rows.filter((row) => row.status === "completed");
  const failed = rows.filter((row) => row.status === "failed");
  const cancelled = rows.filter((row) => row.status === "cancelled");
  const latencies = completed.flatMap((row) => (row.latencyMs !== null ? [row.latencyMs] : []));
  const costs = rows.flatMap((row) => (row.costUsd !== null ? [row.costUsd] : []));
  const costEligible = rows.filter((row) => row.usage !== null);
  const verifierRows = rows.filter((row) => row.verifierOutcome !== null);
  const toolRows = rows.filter((row) => row.toolUseSuccess !== null);
  const rubricScores = rows.flatMap((row) => (row.rubric.score !== null ? [row.rubric.score] : []));

  return {
    targetKey,
    label: target ? `${target.label} · ${target.displayName}` : targetKey,
    totalPrompts: rows.length,
    completed: completed.length,
    failed: failed.length,
    cancelled: cancelled.length,
    avgLatencyMs: average(latencies),
    totalPromptTokens: rows.reduce((sum, row) => sum + (row.usage?.promptTokens ?? 0), 0),
    totalCompletionTokens: rows.reduce((sum, row) => sum + (row.usage?.completionTokens ?? 0), 0),
    totalTokens: rows.reduce((sum, row) => sum + (row.usage?.totalTokens ?? 0), 0),
    totalCostUsd: costs.length > 0 ? costs.reduce((sum, value) => sum + value, 0) : null,
    costKnownForAll: costEligible.length > 0 && costEligible.every((row) => row.costUsd !== null),
    verifierPassRate:
      verifierRows.length > 0
        ? verifierRows.filter((row) => row.verifierOutcome?.ok).length / verifierRows.length
        : null,
    toolUseSuccessRate:
      toolRows.length > 0 ? toolRows.filter((row) => row.toolUseSuccess).length / toolRows.length : null,
    avgRubricScore: average(rubricScores),
    ratedCount: rubricScores.length,
  };
}

export interface LabReport {
  run: LabRun;
  models: LabModelSummary[];
}

/** Full report for one run: every participating model's summary row, in the
 * same order the run's own `targets` snapshot lists them. */
export function buildLabReport(run: LabRun): LabReport {
  return { run, models: run.targets.map((target) => summarizeModel(run, target.key)) };
}

function escapeMarkdownCell(value: string): string {
  // Escape backslashes in the same pass as pipes. Escaping pipes alone leaves
  // `a\|b` as `a\\|b`, which markdown renders as a literal backslash followed
  // by an unescaped pipe — the cell still breaks out of the table.
  return value.replace(/[\\|]/g, "\\$&").replace(/\r?\n/g, " ").trim();
}

function formatMsForReport(value: number | null): string {
  if (value === null) return "—";
  return value < 1000 ? `${Math.round(value)} ms` : `${(value / 1000).toFixed(1)} s`;
}

function formatCostForReport(value: number | null, known: boolean): string {
  if (value === null) return "—";
  const formatted = `$${value.toFixed(4)}`;
  return known ? formatted : `~${formatted}`;
}

function formatRateForReport(value: number | null): string {
  if (value === null) return "—";
  return `${Math.round(value * 100)}%`;
}

/** Renders `buildLabReport`'s output as a standalone Markdown report for
 * sharing with a team — a per-model summary table followed by one section
 * per prompt with each model's answer, verifier outcome, and rubric score. */
export function renderLabReportMarkdown(report: LabReport): string {
  const { run, models } = report;
  const lines: string[] = [];
  lines.push(`# Model Compare Lab report`);
  lines.push("");
  lines.push(`- Suite: **${run.suiteName}** (${run.suiteCategory})`);
  lines.push(`- Model set: **${run.modelSetName}**`);
  lines.push(`- Status: ${run.status}`);
  lines.push(`- Created: ${new Date(run.createdAt).toISOString()}`);
  if (run.completedAt !== null) lines.push(`- Completed: ${new Date(run.completedAt).toISOString()}`);
  lines.push(`- Tools: only offered for prompts that explicitly opt in (${run.prompts.filter((p) => p.toolsEnabled).length}/${run.prompts.length} prompt(s) opted in)`);
  lines.push("");
  lines.push("## Model summary");
  lines.push("");
  lines.push("| Model | Completed | Failed | Avg latency | Tokens | Cost | Verifier pass | Tool success | Avg rubric |");
  lines.push("| --- | --- | --- | --- | --- | --- | --- | --- | --- |");
  for (const model of models) {
    lines.push(
      `| ${escapeMarkdownCell(model.label)} | ${model.completed}/${model.totalPrompts} | ${model.failed} | ${formatMsForReport(model.avgLatencyMs)} | ${model.totalTokens} | ${formatCostForReport(model.totalCostUsd, model.costKnownForAll)} | ${formatRateForReport(model.verifierPassRate)} | ${formatRateForReport(model.toolUseSuccessRate)} | ${model.avgRubricScore !== null ? model.avgRubricScore.toFixed(1) : "—"} |`,
    );
  }
  lines.push("");
  lines.push("## Prompts");
  run.prompts.forEach((prompt, index) => {
    lines.push("");
    lines.push(`### ${index + 1}. ${escapeMarkdownCell(prompt.text).slice(0, 120)}`);
    lines.push("");
    lines.push(`> ${escapeMarkdownCell(prompt.text)}`);
    lines.push("");
    lines.push(`Tools offered: ${prompt.toolsEnabled ? "yes (explicit opt-in)" : "no (default)"}`);
    lines.push("");
    for (const target of run.targets) {
      const result = run.results.find((row) => row.promptId === prompt.id && row.targetKey === target.key);
      if (!result) continue;
      lines.push(`#### ${escapeMarkdownCell(target.label)} · ${escapeMarkdownCell(target.displayName)}`);
      lines.push("");
      lines.push(`- Status: ${result.status}`);
      lines.push(`- Latency: ${formatMsForReport(result.latencyMs)}`);
      lines.push(`- Tokens: ${result.usage ? result.usage.totalTokens : "—"}`);
      lines.push(`- Cost: ${formatCostForReport(result.costUsd, result.costUsd !== null)}`);
      if (result.verifierOutcome) {
        lines.push(`- Verifier: ${result.verifierOutcome.ok ? "PASS" : "FAIL"} — ${escapeMarkdownCell(result.verifierOutcome.message)}`);
      }
      if (result.toolUseSuccess !== null) {
        lines.push(`- Tool use: ${result.toolUseSuccess ? "success" : "failed"} (${result.toolAttempts.length} call(s))`);
      }
      if (result.rubric.score !== null) {
        lines.push(`- Rubric score: ${result.rubric.score}/5${result.rubric.notes ? ` — ${escapeMarkdownCell(result.rubric.notes)}` : ""}`);
      }
      if (result.error) lines.push(`- Error: ${escapeMarkdownCell(result.error)}`);
      lines.push("");
      lines.push("```");
      lines.push(result.content || "(empty response)");
      lines.push("```");
      lines.push("");
    }
  });
  return lines.join("\n");
}

export function renderLabReportJson(report: LabReport): string {
  return `${JSON.stringify(report, null, 2)}\n`;
}

export function labRunFileBaseName(run: LabRun): string {
  const safeSuite = run.suiteName.replace(/[^A-Za-z0-9_.-]+/g, "-").slice(0, 60) || "suite";
  const safeId = run.id.replace(/[^A-Za-z0-9_.-]+/g, "-").slice(0, 24);
  return `compare-lab-${safeSuite}-${safeId}`;
}

// ---------------------------------------------------------------------------
// Starter benchmark suites — one real, runnable suite per ROADMAP.md category
// (coding, writing, RAG, browser QA, connector). Each prompt carries a
// concrete, checkable verifier rather than being an empty placeholder. Every
// prompt defaults to `toolsEnabled: false`; the one exception (explicitly
// commented below) demonstrates the opt-in path deliberately, so the
// tool-default-off guarantee and the opt-in path are both exercised by a
// suite a user can actually run out of the box.
// ---------------------------------------------------------------------------

function starterSuite(
  id: string,
  name: string,
  description: string,
  category: BenchmarkCategory,
  prompts: LabPrompt[],
): BenchmarkSuite {
  const now = Date.now();
  return { id, name, description, category, prompts, builtIn: true, createdAt: now, updatedAt: now };
}

export const STARTER_CODING_SUITE_ID = "starter-coding";
export const STARTER_WRITING_SUITE_ID = "starter-writing";
export const STARTER_RAG_SUITE_ID = "starter-rag";
export const STARTER_BROWSER_QA_SUITE_ID = "starter-browser-qa";
export const STARTER_CONNECTOR_SUITE_ID = "starter-connector";

/** Builds the seeded starter suites fresh (stable ids, fresh prompt ids and
 * timestamps) — called once by `compareLabStore.ts`'s hydration when no
 * suites are persisted yet, and by tests that want a clean starting set. */
export function buildStarterSuites(): BenchmarkSuite[] {
  return [
    starterSuite(
      STARTER_CODING_SUITE_ID,
      "Coding basics",
      "Small, checkable coding tasks — one prompt opts into read-only tools to exercise the explicit tool-use path.",
      "coding",
      [
        createLabPrompt(
          "Write a Python function `fibonacci(n)` that returns the nth Fibonacci number (0-indexed, fibonacci(0) == 0) iteratively, with a docstring and type hints. Return only the code.",
          {
            verifier: { kind: "contains", value: "def fibonacci", label: 'Defines "def fibonacci"' },
            rubricCriteria: ["Correctness", "Readability", "Handles n=0/n=1"],
          },
        ),
        createLabPrompt(
          "Write a SQL query that returns each customer's total order amount from an `orders` table with columns `customer_id` and `amount`, grouped and ordered from highest to lowest total.",
          {
            verifier: { kind: "regex", value: "group\\s+by", flags: "i", label: "Uses GROUP BY" },
            rubricCriteria: ["Correctness", "Uses appropriate aggregate"],
          },
        ),
        createLabPrompt(
          // Deliberately the one starter prompt with tools on, to exercise the
          // explicit opt-in path end to end — see this file's top doc comment.
          "Use the available tools to list the top-level entries in the current workspace root, then summarize what you found in one sentence.",
          {
            toolsEnabled: true,
            verifier: { kind: "min_length", value: "10", label: "Gives a non-trivial summary" },
            rubricCriteria: ["Actually used a tool", "Summary matches what was listed"],
          },
        ),
      ],
    ),
    starterSuite(
      STARTER_WRITING_SUITE_ID,
      "Writing quality",
      "Short-form writing tasks judged on constraint-following and tone.",
      "writing",
      [
        createLabPrompt(
          "Write a two-paragraph product announcement for a note-taking app's new offline mode. Keep it under 120 words and end with a call to action.",
          {
            verifier: { kind: "min_length", value: "80", label: "Meets minimum length" },
            rubricCriteria: ["Under 120 words", "Clear call to action", "Tone"],
          },
        ),
        createLabPrompt(
          "Rewrite the following sentence to be more concise without losing meaning: \"Due to the fact that the server was experiencing an unusually high volume of requests, the response times that our users were experiencing became significantly slower than what is typical.\"",
          {
            verifier: { kind: "not_contains", value: "due to the fact that", label: 'Drops "due to the fact that"' },
            rubricCriteria: ["Conciseness", "Meaning preserved"],
          },
        ),
      ],
    ),
    starterSuite(
      STARTER_RAG_SUITE_ID,
      "RAG grounding",
      "Answer strictly from supplied context, judged on grounding and refusal to invent facts not present in it.",
      "rag",
      [
        createLabPrompt(
          [
            "Context:",
            '"Little Monkey\'s offline mode was introduced in version 2.4 and caches the last 30 days of documents locally."',
            "",
            "Question: Based only on the context above, which version introduced offline mode, and how many days of documents does it cache?",
          ].join("\n"),
          {
            verifier: { kind: "contains", value: "2.4", label: "Cites the correct version" },
            rubricCriteria: ["Grounded in context", "No invented details"],
          },
        ),
        createLabPrompt(
          [
            "Context:",
            '"The support hours for the Basic plan are Monday to Friday, 9am to 5pm."',
            "",
            "Question: Based only on the context above, what are the support hours for the Enterprise plan?",
          ].join("\n"),
          {
            verifier: { kind: "regex", value: "(don't|do not|cannot|no information|not (mentioned|specified|provided|stated))", flags: "i", label: "Declines to invent an unsupported answer" },
            rubricCriteria: ["Refuses to hallucinate", "Explains why"],
          },
        ),
      ],
    ),
    starterSuite(
      STARTER_BROWSER_QA_SUITE_ID,
      "Browser QA reasoning",
      "Plan a browser interaction sequence in words — judged on correct ordering and referencing the right elements. Read-only; no live browser session is driven by this suite.",
      "browser_qa",
      [
        createLabPrompt(
          "A login page has an email field, a password field, and a 'Sign in' button, in that order. Write the exact ordered steps (as a numbered list) you would take to log in as test@example.com with password 'hunter2', including what to type into which field before clicking anything.",
          {
            verifier: { kind: "regex", value: "sign in", flags: "i", label: "Mentions the Sign in button" },
            rubricCriteria: ["Correct order", "References the right fields"],
          },
        ),
        createLabPrompt(
          "A page shows a cookie-consent banner with 'Accept all' and 'Decline non-essential' buttons overlapping the rest of the page. Describe the single first action you should take before doing anything else on the page, and why.",
          {
            verifier: { kind: "regex", value: "decline", flags: "i", label: "Recommends the privacy-preserving option" },
            rubricCriteria: ["Chooses the privacy-preserving option", "Explains why it must happen first"],
          },
        ),
      ],
    ),
    starterSuite(
      STARTER_CONNECTOR_SUITE_ID,
      "Connector task planning",
      "Draft the exact tool call a connector integration would need — judged on including the right parameters. No connector is actually invoked.",
      "connector",
      [
        createLabPrompt(
          "A user asks: \"Summarize unread messages in the #general channel from the last 24 hours.\" Write the exact tool call you would make to a Slack-style connector to fetch this information, as a single JSON object with the channel name and a time range.",
          {
            verifier: { kind: "json_valid", label: "Response is valid JSON" },
            rubricCriteria: ["Includes channel name", "Includes a time range", "Valid JSON"],
          },
        ),
        createLabPrompt(
          "A user asks: \"Create a follow-up task in our project tracker for the bug reported in ticket ABC-123, due next Friday.\" Write the exact tool call you would make to a Jira-style connector, as a single JSON object.",
          {
            verifier: { kind: "contains", value: "ABC-123", label: "References the source ticket" },
            rubricCriteria: ["References ABC-123", "Includes a due date", "Valid JSON"],
          },
        ),
      ],
    ),
  ];
}
