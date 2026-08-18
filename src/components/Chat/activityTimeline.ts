import type { ToolCall } from "../../lib/llamaClient";
import { unwrapUntrustedContent } from "../../lib/untrustedContent";
import { parseProgrammaticExecutionResult } from "../../lib/programmaticExecution";

/** A concrete, transcript-backed tool invocation rendered in an activity row. */
export interface ActivityCall {
  id: string;
  name: string;
  args: string;
  result?: string;
}

/** At most two entries are produced for one assistant round: the round's
 * normal activity and its delegated tasks. Their position is determined by
 * whichever kind appeared first, while calls within each entry retain their
 * original order. */
export type AssistantRoundEntry =
  | { kind: "activity"; calls: ActivityCall[] }
  | { kind: "tasks"; calls: ActivityCall[] }
  | { kind: "workflows"; calls: ActivityCall[] };

export function groupAssistantRound(
  toolCalls: ToolCall[],
  resultByCallId: ReadonlyMap<string, string>,
): AssistantRoundEntry[] {
  const activityCalls = toolCalls.filter((call) => call.function.name !== "task" && call.function.name !== "workflow");
  const taskCalls = toolCalls.filter((call) => call.function.name === "task");
  const workflowCalls = toolCalls.filter((call) => call.function.name === "workflow");
  const entries: AssistantRoundEntry[] = [];
  let emittedActivity = false;
  let emittedTasks = false;
  let emittedWorkflows = false;

  const toActivityCall = (call: ToolCall): ActivityCall => ({
    id: call.id,
    name: call.function.name,
    args: call.function.arguments,
    result: resultByCallId.get(call.id),
  });

  for (const call of toolCalls) {
    if (call.function.name === "task") {
      if (!emittedTasks) {
        emittedTasks = true;
        entries.push({ kind: "tasks", calls: taskCalls.map(toActivityCall) });
      }
    } else if (call.function.name === "workflow") {
      if (!emittedWorkflows) {
        emittedWorkflows = true;
        entries.push({ kind: "workflows", calls: workflowCalls.map(toActivityCall) });
      }
    } else if (!emittedActivity) {
      emittedActivity = true;
      entries.push({ kind: "activity", calls: activityCalls.map(toActivityCall) });
    }
  }

  return entries;
}

export function resultLooksLikeError(raw: string): boolean {
  try {
    const parsed: unknown = JSON.parse(unwrapUntrustedContent(raw));
    const program = parseProgrammaticExecutionResult(unwrapUntrustedContent(raw));
    if (program) return program.status !== "succeeded";
    return typeof parsed === "object" && parsed !== null && "error" in parsed;
  } catch {
    return false;
  }
}

type SummaryKind =
  | "read-file"
  | "edit-file"
  | "attempted-file-edit"
  | "proposed-file-edit"
  | "run-command"
  | "list-folder"
  | "search-files"
  | "search-web"
  | "read-web"
  | "search-docs"
  | "save-memory"
  | "load-skill"
  | "read-skill-file"
  | "generate-image"
  | "prepare-plan"
  | "run-program"
  | `tool:${string}`;

function summaryKind(call: ActivityCall): SummaryKind {
  const { name } = call;
  switch (name) {
    case "read_file":
      return "read-file";
    case "write_file":
    case "edit_file":
      if (call.result === undefined) return "proposed-file-edit";
      if (resultLooksLikeError(call.result)) return "attempted-file-edit";
      return "edit-file";
    case "run_shell":
      return "run-command";
    case "list_dir":
      return "list-folder";
    case "glob":
    case "grep":
      return "search-files";
    case "web_search":
      return "search-web";
    case "web_fetch":
      return "read-web";
    case "search_docs":
      return "search-docs";
    case "remember":
      return "save-memory";
    case "skill":
      return "load-skill";
    case "read_skill_resource":
      return "read-skill-file";
    case "generate_image":
      return "generate-image";
    case "present_plan":
      return "prepare-plan";
    case "run_program":
      return "run-program";
    default:
      return `tool:${name}`;
  }
}

function humanizeToolName(name: string): string {
  if (name.startsWith("mcp__")) {
    const [, server = "MCP", ...toolParts] = name.split("__");
    const tool = toolParts.join(" ");
    return `${server.split("_").join(" ")} ${tool.split("_").join(" ")}`.trim();
  }
  return name.split("_").join(" ");
}

function formatSummaryRun(kind: SummaryKind, count: number): string {
  switch (kind) {
    case "read-file":
      return count === 1 ? "Read a file" : `Read ${count} files`;
    case "edit-file":
      return count === 1 ? "Edited a file" : `Edited ${count} files`;
    case "attempted-file-edit":
      return count === 1 ? "Attempted a file edit" : `Attempted ${count} file edits`;
    case "proposed-file-edit":
      return count === 1 ? "Proposed a file edit" : `Proposed ${count} file edits`;
    case "run-command":
      return count === 1 ? "Ran a command" : `Ran ${count} commands`;
    case "list-folder":
      return count === 1 ? "Listed a folder" : `Listed ${count} folders`;
    case "search-files":
      return count === 1 ? "Searched files" : `Searched files ${count} times`;
    case "search-web":
      return count === 1 ? "Searched the web" : `Searched the web ${count} times`;
    case "read-web":
      return count === 1 ? "Read a web page" : `Read ${count} web pages`;
    case "search-docs":
      return count === 1 ? "Searched project docs" : `Searched project docs ${count} times`;
    case "save-memory":
      return count === 1 ? "Saved a memory" : `Saved ${count} memories`;
    case "load-skill":
      return count === 1 ? "Loaded a skill" : `Loaded ${count} skills`;
    case "read-skill-file":
      return count === 1 ? "Read a skill file" : `Read ${count} skill files`;
    case "generate-image":
      return count === 1 ? "Generated an image" : `Generated ${count} images`;
    case "prepare-plan":
      return count === 1 ? "Prepared a plan" : `Prepared ${count} plans`;
    case "run-program":
      return count === 1 ? "Ran a program" : `Ran ${count} programs`;
    default: {
      const name = humanizeToolName(kind.slice("tool:".length));
      return count === 1 ? `Used ${name}` : `Used ${name} ${count} times`;
    }
  }
}

/** Verbs for the actions that name their file instead of counting files. */
const FILE_VERBS: Partial<Record<SummaryKind, string>> = {
  "read-file": "read",
  "edit-file": "edited",
  "attempted-file-edit": "tried to edit",
  "proposed-file-edit": "proposed edits to",
};

/** How many named files a summary will list before it falls back to counts —
 * past this the line is longer than the row can show anyway. */
const MAX_NAMED_FILES = 3;

function fileName(call: ActivityCall): string {
  const path = stringArg(parseArgs(call.args), "path");
  return path.split(/[\\/]/).pop() ?? "";
}

function joinVerbs(verbs: string[]): string {
  if (verbs.length < 2) return verbs[0] ?? "";
  return `${verbs.slice(0, -1).join(", ")} and ${verbs[verbs.length - 1]}`;
}

type SummaryRun =
  | { key: string; file: string; verbs: string[] }
  | { key: string; kind: SummaryKind; count: number };

function sentence(phrases: string[]): string {
  return phrases
    .map((phrase, index) => (index === 0
      ? `${phrase.charAt(0).toUpperCase()}${phrase.slice(1)}`
      : `${phrase.charAt(0).toLowerCase()}${phrase.slice(1)}`))
    .join(", ");
}

/** Folds a round into "edited and read process_table.rs, ran 3 commands":
 * calls on the same file collapse into one named phrase however far apart
 * they were, other actions collapse into counts, and groups appear in order
 * of first occurrence. */
export function summarizeActivity(calls: ActivityCall[]): string {
  if (calls.length === 0) return "Worked on the task";

  const runs: SummaryRun[] = [];
  const runByKey = new Map<string, SummaryRun>();
  const namedFiles = new Set<string>();

  for (const call of calls) {
    const kind = summaryKind(call);
    const verb = FILE_VERBS[kind];
    const file = verb ? fileName(call) : "";
    const named = verb && file ? { verb, file } : null;
    const key = named ? `file:${named.file}` : `kind:${kind}`;
    if (named) namedFiles.add(named.file);

    const existing = runByKey.get(key);
    if (!existing) {
      const run: SummaryRun = named
        ? { key, file: named.file, verbs: [named.verb] }
        : { key, kind, count: 1 };
      runByKey.set(key, run);
      runs.push(run);
    } else if ("file" in existing) {
      if (named && !existing.verbs.includes(named.verb)) existing.verbs.push(named.verb);
    } else {
      existing.count += 1;
    }
  }

  if (namedFiles.size > MAX_NAMED_FILES) return summarizeByCount(calls);

  return sentence(runs.map((run) => (
    "file" in run ? `${joinVerbs(run.verbs)} ${run.file}` : formatSummaryRun(run.kind, run.count)
  )));
}

/** Count-only fallback for rounds touching too many files to name. Adjacent
 * calls sharing an action fold together, so the order of work still shows. */
function summarizeByCount(calls: ActivityCall[]): string {
  const runs: Array<{ kind: SummaryKind; count: number }> = [];
  for (const call of calls) {
    const kind = summaryKind(call);
    const previous = runs[runs.length - 1];
    if (previous?.kind === kind) previous.count += 1;
    else runs.push({ kind, count: 1 });
  }
  return sentence(runs.map(({ kind, count }) => formatSummaryRun(kind, count)));
}

export type ActivityProgressStatus = "running" | "failed" | "completed";

export interface ActivityProgress {
  status: ActivityProgressStatus;
  label: string;
  pendingCount: number;
  failedCount: number;
  completedCount: number;
}

export function activityProgress(calls: ActivityCall[]): ActivityProgress {
  let pendingCount = 0;
  let failedCount = 0;
  let completedCount = 0;

  for (const call of calls) {
    if (call.result === undefined) pendingCount += 1;
    else if (resultLooksLikeError(call.result)) failedCount += 1;
    else completedCount += 1;
  }

  if (pendingCount > 0) {
    return {
      status: "running",
      label: failedCount > 0 ? `Running · ${failedCount} failed` : "Running",
      pendingCount,
      failedCount,
      completedCount,
    };
  }
  if (failedCount > 0) {
    return {
      status: "failed",
      label: failedCount === 1 ? "Failed" : `${failedCount} failed`,
      pendingCount,
      failedCount,
      completedCount,
    };
  }
  return {
    status: "completed",
    label: "Completed",
    pendingCount,
    failedCount,
    completedCount,
  };
}

function parseArgs(raw: string): Record<string, unknown> {
  try {
    const parsed: unknown = JSON.parse(raw || "{}");
    return parsed && typeof parsed === "object" && !Array.isArray(parsed)
      ? (parsed as Record<string, unknown>)
      : {};
  } catch {
    return {};
  }
}

function stringArg(args: Record<string, unknown>, key: string): string {
  const value = args[key];
  return typeof value === "string" ? value : "";
}

export interface CappedActivityText {
  text: string;
  truncated: boolean;
}

export function capActivityText(
  raw: string,
  maxChars = 4_000,
  maxLines = 80,
): CappedActivityText {
  const normalized = raw.replace(/\r\n?/g, "\n");
  const lines = normalized.split("\n");
  let text = lines.slice(0, maxLines).join("\n");
  let truncated = lines.length > maxLines;
  if (text.length > maxChars) {
    text = text.slice(0, maxChars);
    truncated = true;
  }
  if (truncated) text = `${text.replace(/\s+$/u, "")}\n… truncated`;
  return { text, truncated };
}

function compactSubject(raw: string): string {
  return capActivityText(raw, 480, 8).text;
}

/** Path, command, URL, query, or other primary target shown for a concrete
 * call. Large write/edit payloads are intentionally excluded here and are
 * rendered through the separately capped diff preview instead. */
export function activityCallSubject(call: ActivityCall): string {
  const args = parseArgs(call.args);
  const path = stringArg(args, "path");
  switch (call.name) {
    case "read_file":
    case "write_file":
    case "edit_file":
      return compactSubject(path || "Path unavailable");
    case "list_dir":
      return compactSubject(path || "Workspace root");
    case "glob": {
      const pattern = stringArg(args, "pattern");
      return compactSubject(`${pattern || "Files"} in ${path || "workspace"}`);
    }
    case "grep": {
      const pattern = stringArg(args, "pattern");
      return compactSubject(`${pattern || "Text"} in ${path || "workspace"}`);
    }
    case "run_shell": {
      const command = stringArg(args, "command");
      const cwd = stringArg(args, "cwd");
      return compactSubject(cwd ? `${command || "Command"}\nWorking directory: ${cwd}` : command || "Command unavailable");
    }
    case "web_fetch":
      return compactSubject(stringArg(args, "url") || "URL unavailable");
    case "web_search":
    case "search_docs":
      return compactSubject(stringArg(args, "query") || "Query unavailable");
    case "remember":
      return compactSubject(stringArg(args, "text") || "Memory text unavailable");
    case "skill":
      return compactSubject(`/${stringArg(args, "command") || "skill"}`);
    case "read_skill_resource": {
      const command = stringArg(args, "command");
      return compactSubject([command ? `/${command}` : "", path].filter(Boolean).join(" · ") || "Skill file unavailable");
    }
    case "generate_image":
      return compactSubject(
        stringArg(args, "filename")
          || path
          || stringArg(args, "prompt")
          || "Image output",
      );
    case "run_program":
      return "Bounded tool program";
    default: {
      for (const key of ["command", "path", "file", "filename", "url", "query", "model", "repo", "title"]) {
        const value = stringArg(args, key);
        if (value) return compactSubject(value);
      }
      return "No path or command provided";
    }
  }
}

export function activityCallLabel(name: string): string {
  switch (name) {
    case "read_file":
      return "Read file";
    case "write_file":
      return "Write file";
    case "edit_file":
      return "Edit file";
    case "list_dir":
      return "List folder";
    case "glob":
      return "Find files";
    case "grep":
      return "Search files";
    case "run_shell":
      return "Run command";
    case "remember":
      return "Save memory";
    case "web_fetch":
      return "Read web page";
    case "web_search":
      return "Search web";
    case "search_docs":
      return "Search project docs";
    case "skill":
      return "Load skill";
    case "read_skill_resource":
      return "Read skill file";
    case "generate_image":
      return "Generate image";
    case "present_plan":
      return "Prepare plan";
    case "run_program":
      return "Run program";
    default:
      return `Use ${humanizeToolName(name)}`;
  }
}

/** The one-line command a call is shown as inside an expanded step — a shell
 * call reads as the command itself (`$ …`), anything else as its human label
 * plus subject. */
export function activityCallCommandLine(call: ActivityCall): string {
  const subject = activityCallSubject(call);
  return call.name === "run_shell" ? `$ ${subject}` : `${activityCallLabel(call.name)} ${subject}`;
}

/** Plain text a step's copy button puts on the clipboard: the command line,
 * then whatever the call returned. */
export function activityCallCopyText(call: ActivityCall): string {
  const command = activityCallCommandLine(call);
  return call.result === undefined ? command : `${command}\n\n${formatActivityResult(call.result).text}`;
}

export interface ActivityDiff {
  kind: "edit" | "write";
  state: "applied" | "attempted" | "proposed";
  before?: CappedActivityText;
  after: CappedActivityText;
}

const DIFF_MAX_CHARS = 2_400;
const DIFF_MAX_LINES = 40;

/** Builds a bounded preview solely from the model-provided mutation args.
 * It never re-reads the workspace, so rendering old transcripts cannot touch
 * the filesystem or accidentally show unrelated current file content. */
export function activityCallDiff(call: ActivityCall): ActivityDiff | null {
  if (call.name !== "edit_file" && call.name !== "write_file") return null;
  const args = parseArgs(call.args);
  const state = call.result === undefined
    ? "proposed"
    : resultLooksLikeError(call.result)
      ? "attempted"
      : "applied";

  if (call.name === "write_file") {
    const content = args.content;
    if (typeof content !== "string") return null;
    return {
      kind: "write",
      state,
      after: capActivityText(content, DIFF_MAX_CHARS, DIFF_MAX_LINES),
    };
  }

  const oldString = args.old_string;
  const newString = args.new_string;
  if (typeof oldString !== "string" || typeof newString !== "string") return null;
  return {
    kind: "edit",
    state,
    before: capActivityText(oldString, DIFF_MAX_CHARS, DIFF_MAX_LINES),
    after: capActivityText(newString, DIFF_MAX_CHARS, DIFF_MAX_LINES),
  };
}

export interface ActivityDiffStat {
  added: number;
  removed: number;
}

function countLines(text: string): number {
  if (text === "") return 0;
  return text.replace(/\n$/, "").split("\n").length;
}

/** Added/removed line counts for a round's applied mutations, read straight
 * from the call args (never the workspace) so old transcripts stay stable.
 * Failed and still-pending calls changed nothing, so they don't count. */
export function activityDiffStat(calls: ActivityCall[]): ActivityDiffStat {
  let added = 0;
  let removed = 0;
  for (const call of calls) {
    if (call.result === undefined || resultLooksLikeError(call.result)) continue;
    const args = parseArgs(call.args);
    if (call.name === "write_file") {
      added += countLines(stringArg(args, "content"));
    } else if (call.name === "edit_file") {
      added += countLines(stringArg(args, "new_string"));
      removed += countLines(stringArg(args, "old_string"));
    }
  }
  return { added, removed };
}

export function formatActivityResult(raw: string): CappedActivityText {
  let formatted = unwrapUntrustedContent(raw);
  try {
    formatted = JSON.stringify(JSON.parse(formatted), null, 2);
  } catch {
    // Plain-text output is already presentation-ready.
  }
  return capActivityText(formatted);
}

/** A generic operational status for the live footer. This reports only the
 * visible tool boundary and never fabricates or exposes model reasoning. */
export function liveActivityLabel(name: string): string {
  switch (name) {
    case "read_file":
    case "list_dir":
      return "Reading files";
    case "glob":
    case "grep":
      return "Searching files";
    case "write_file":
    case "edit_file":
      return "Editing files";
    case "run_shell":
      return "Running a command";
    case "web_fetch":
      return "Reading a web page";
    case "web_search":
      return "Searching the web";
    case "search_docs":
      return "Searching project docs";
    case "task":
      return "Running a subagent";
    case "generate_image":
      return "Generating an image";
    case "run_program":
      return "Running a bounded program";
    default:
      return `Using ${humanizeToolName(name)}`;
  }
}
