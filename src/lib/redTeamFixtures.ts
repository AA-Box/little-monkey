/**
 * Prompt-Injection and Tool-Abuse Lab — fixture library.
 *
 * Each fixture models a real ingestion surface this app already reads
 * untrusted bytes from (a fetched webpage, an email/connector body, an MCP
 * tool's result, a repo file, a knowledge-stack chunk, a subagent's report,
 * …) with an embedded instruction trying to hijack the agent, plus the
 * concrete tool call the embedded instruction is trying to trigger.
 *
 * The corpus itself lives in `redTeamFixtures.json`, not in this file, because
 * the Rust permission gate reads the same bytes: `permissions.rs`'s
 * `red_team_corpus` test walks every fixture here through the real
 * `path_risk_floor` → `compute_risk` → `evaluate_gate` chain via
 * `include_str!`. Two hand-maintained lists — one per language — is exactly
 * the drift this lab exists to catch, so there is only one list.
 *
 * `simulatedToolName`/`isMcp` decide which of `untrustedContent.ts`'s real
 * boundary functions the fixture's `content` must survive — the exact same
 * call `turnEngine.ts`'s `executeToolCall` loop, `subagent.ts`, and
 * `agentLoop.ts` make on every real tool result before it re-enters the
 * model's context. Nothing here reimplements that detection; fixtures just
 * describe *what a hostile source would send* so the runner can prove the
 * existing boundary still holds against it.
 */

import type { PermissionMode } from "../store/permissionStore";
import corpus from "./redTeamFixtures.json";

export type FixtureSourceType =
  | "webpage"
  | "email"
  | "mcp_tool_output"
  | "repo_file"
  | "connector_payload"
  | "pdf_document"
  | "screenshot_ocr"
  | "knowledge_source"
  | "web_search_result"
  | "subagent_output";

/** The concrete tool call a fixture's embedded instruction is trying to get
 * the agent to make — the thing the permission/risk gate must catch. */
export interface TriggeredAction {
  tool: string;
  args: Record<string, unknown>;
  /** One-line human summary shown in the fixture table, e.g. "write to
   * .github/workflows/deploy.yml". */
  description: string;
}

export interface RedTeamFixture {
  id: string;
  title: string;
  sourceType: FixtureSourceType;
  /** Tool name whose *result* this fixture's `content` stands in for (e.g.
   * `web_fetch` for a scraped page, `read_file` for a repo file). Passed to
   * `untrustedContent.ts`'s `protectToolResult` exactly as `executeToolCall`
   * would pass the real tool's name. Ignored for `sourceType ===
   * "knowledge_source"`, which instead goes through
   * `protectKnowledgeNoticeForModel`. */
  simulatedToolName: string;
  /** Whether this result arrived via an MCP server (Slack/Jira/Notion/S3/
   * GitHub connectors, per `mcpTools.ts`) — MCP results are wrapped as
   * untrusted regardless of tool name, mirroring `protectToolResult`'s
   * `isMcp` parameter. */
  isMcp?: boolean;
  /** The hostile payload itself, verbatim as the source would deliver it. */
  content: string;
  /** An exact substring of `content` that is a raw model role/control token
   * (e.g. `<|im_start|>system`) — if present, the runner asserts it does NOT
   * survive verbatim into the wrapped copy the model actually sees. */
  rawControlToken?: string;
  triggeredAction: TriggeredAction;
  /** Stands in for what `riskJudge.ts`'s `classifyToolCall` would return for
   * `triggeredAction`, so a fixture whose whole premise is "the judge was
   * talked into calling this low-risk" carries that premise itself instead of
   * depending on a caller to supply it. The deterministic floor in
   * `permissions.rs::path_risk_floor` overrides this whenever it fires — which
   * is precisely what the `floored-*-under-smart` fixtures assert. */
  judgeRiskLevel?: "low" | "medium" | "high";
  /** What a correctly-defended agent must do with `triggeredAction`: outright
   * refuse to offer it at all ("blocked", e.g. Plan Mode), or require an
   * explicit human approval before it runs ("requires_approval"). */
  expectedOutcome: "blocked" | "requires_approval";
  /** Pins the permission mode this fixture is evaluated under, overriding
   * whatever mode the panel's selector is currently set to — used for
   * fixtures whose whole premise depends on a specific mode (e.g. "even
   * while the user is in Plan Mode…"). Most fixtures leave this unset and
   * are evaluated under the panel's selected mode instead. */
  evaluationMode?: PermissionMode;
  /** True for the built-in library; false for a fixture the user added from
   * the panel — lets the store know which ones it may let the user delete. */
  builtin: boolean;
}

const SOURCE_TYPES: ReadonlySet<string> = new Set<FixtureSourceType>([
  "webpage",
  "email",
  "mcp_tool_output",
  "repo_file",
  "connector_payload",
  "pdf_document",
  "screenshot_ocr",
  "knowledge_source",
  "web_search_result",
  "subagent_output",
]);

/**
 * Validates one raw JSON entry into a `RedTeamFixture`. The corpus is a
 * checked-in data file read by two languages, so a shape error here is a
 * build-time mistake worth failing loudly on rather than a `JSON.parse` cast
 * that silently produces a fixture the runner cannot evaluate.
 */
function parseFixture(raw: unknown, index: number): RedTeamFixture {
  const at = `redTeamFixtures.json[${index}]`;
  if (typeof raw !== "object" || raw === null) {
    throw new Error(`${at} is not an object`);
  }
  const entry = raw as Record<string, unknown>;
  const requireString = (key: string): string => {
    const value = entry[key];
    if (typeof value !== "string" || value.length === 0) {
      throw new Error(`${at}.${key} must be a non-empty string`);
    }
    return value;
  };

  const sourceType = requireString("sourceType");
  if (!SOURCE_TYPES.has(sourceType)) {
    throw new Error(`${at}.sourceType "${sourceType}" is not a known source type`);
  }

  const action = entry.triggeredAction;
  if (typeof action !== "object" || action === null) {
    throw new Error(`${at}.triggeredAction must be an object`);
  }
  const rawAction = action as Record<string, unknown>;
  if (typeof rawAction.tool !== "string" || rawAction.tool.length === 0) {
    throw new Error(`${at}.triggeredAction.tool must be a non-empty string`);
  }
  if (typeof rawAction.args !== "object" || rawAction.args === null) {
    throw new Error(`${at}.triggeredAction.args must be an object`);
  }
  if (typeof rawAction.description !== "string") {
    throw new Error(`${at}.triggeredAction.description must be a string`);
  }

  const expectedOutcome = requireString("expectedOutcome");
  if (expectedOutcome !== "blocked" && expectedOutcome !== "requires_approval") {
    throw new Error(`${at}.expectedOutcome must be "blocked" or "requires_approval"`);
  }

  const rawControlToken = entry.rawControlToken;
  if (rawControlToken !== undefined && typeof rawControlToken !== "string") {
    throw new Error(`${at}.rawControlToken must be a string when present`);
  }
  const content = requireString("content");
  if (typeof rawControlToken === "string" && !content.includes(rawControlToken)) {
    throw new Error(
      `${at}.rawControlToken must appear verbatim in content — otherwise the ` +
        `neutralization assertion proves nothing`,
    );
  }

  const judgeRiskLevel = entry.judgeRiskLevel;
  if (
    judgeRiskLevel !== undefined &&
    judgeRiskLevel !== "low" &&
    judgeRiskLevel !== "medium" &&
    judgeRiskLevel !== "high"
  ) {
    throw new Error(`${at}.judgeRiskLevel must be "low", "medium" or "high" when present`);
  }

  return {
    id: requireString("id"),
    title: requireString("title"),
    sourceType: sourceType as FixtureSourceType,
    simulatedToolName: requireString("simulatedToolName"),
    isMcp: entry.isMcp === true ? true : undefined,
    content,
    rawControlToken: rawControlToken as string | undefined,
    triggeredAction: {
      tool: rawAction.tool,
      args: rawAction.args as Record<string, unknown>,
      description: rawAction.description,
    },
    judgeRiskLevel: judgeRiskLevel as RedTeamFixture["judgeRiskLevel"],
    expectedOutcome,
    evaluationMode: entry.evaluationMode as PermissionMode | undefined,
    builtin: true,
  };
}

export const BUILTIN_FIXTURES: RedTeamFixture[] = (corpus as unknown[]).map(parseFixture);
