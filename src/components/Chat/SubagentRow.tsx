import { memo, useId, useState } from "react";
import { Bot, ChevronRight } from "lucide-react";

import { textContent, type ChatMessage } from "../../lib/llamaClient";
import { CANCELLED_TOOL_RESULT } from "../../lib/turnEngine";
import { useSubagentStore, type SubagentStatus } from "../../store/subagentStore";
import { useSessionStore } from "../../store/sessionStore";
import { StatusPill, type PillTone } from "../ui";
import { useT } from "../../lib/i18n";
import { ToolCallRow, resultLooksLikeError } from "./MessageList";

/** Same `en-US` grouping-separator formatting `ContextUsageIndicator.tsx` uses for the parent session's own token count — kept local (rather than shared) since the two components have no other coupling and this is a one-line function. */
function formatTokenCount(value: number): string {
  return value.toLocaleString("en-US");
}

export interface SubagentRowProps {
  /** The session this `task` tool_call belongs to — used only to look up
   * `ChatSession.subagentRuns` as a fallback once `subagentStore`'s
   * transient copy is gone (e.g. after a restart). */
  sessionId: string;
  /** The originating `task` tool_call's own id — the `subagentStore`/
   * `ChatSession.subagentRuns` key (see `subagent.ts`'s
   * `RunSubagentTaskParams.toolCallId` doc comment for why this id, not
   * `runSubagentTask`'s Rust-facing turn id, is what correlates a
   * transcript row back to its run). */
  taskId: string;
  /** The `task` tool_call's raw JSON arguments string — parsed for the
   * `description`/`profile` the model supplied, exactly like `ToolCallRow`
   * parses any other tool's `args`. */
  args: string;
  /** The matching `tool` result's content — `undefined` while the call is
   * still in flight (mirrors `ToolCallRow`'s own `pending` convention). */
  result?: string;
}

export interface ParsedTaskArgs {
  description: string;
  profile: "explore" | "code";
}

/** Exported for `SubagentRow.test.ts` — this codebase's test setup runs
 * under vitest's `node` environment with no DOM/React-rendering harness (no
 * other component in this app has one either — see that test file's own
 * top comment for the deviation this represents from the design doc's
 * "renders correctly for each status" ask), so the row's actual JSX isn't
 * unit-tested; every piece of logic that DETERMINES what it renders is,
 * here and below. */
export function parseTaskArgs(args: string): ParsedTaskArgs {
  try {
    const parsed: unknown = args ? JSON.parse(args) : null;
    if (parsed && typeof parsed === "object") {
      const candidate = parsed as Partial<ParsedTaskArgs>;
      return {
        description: typeof candidate.description === "string" && candidate.description.trim().length > 0 ? candidate.description : "Subagent task",
        profile: candidate.profile === "code" ? "code" : "explore",
      };
    }
  } catch {
    // fall through to the default below
  }
  return { description: "Subagent task", profile: "explore" };
}

export interface ChildToolCallRow {
  key: string;
  name: string;
  args: string;
  result?: string;
}

/** Pairs up the child's own `tool_calls`/`tool` messages the same way
 * `MessageList.tsx`'s `buildTimeline` does for the parent transcript — a
 * deliberately simpler pass since a subagent's local transcript has none of
 * the parent's notices (no checkpoints/memory/plan/verify/sources rows can
 * appear inside a child run in this slice). */
export function extractChildToolCalls(messages: ChatMessage[]): ChildToolCallRow[] {
  const resultByCallId = new Map<string, string>();
  for (const message of messages) {
    if (message.role === "tool" && message.tool_call_id) {
      resultByCallId.set(message.tool_call_id, textContent(message.content));
    }
  }
  const rows: ChildToolCallRow[] = [];
  for (const message of messages) {
    if (message.role !== "assistant") continue;
    for (const toolCall of message.tool_calls ?? []) {
      rows.push({
        key: toolCall.id,
        name: toolCall.function.name,
        args: toolCall.function.arguments,
        result: resultByCallId.get(toolCall.id),
      });
    }
  }
  return rows;
}

export function statusLabelKey(status: SubagentStatus): string {
  switch (status) {
    case "running":
      return "SubagentRow.statusRunning";
    case "done":
      return "SubagentRow.statusDone";
    case "error":
      return "SubagentRow.statusFailed";
    case "cancelled":
      return "SubagentRow.statusCancelled";
    default:
      return "SubagentRow.statusDone";
  }
}

/** Resolves the status this row shows: the live `subagentStore` entry's own
 * status while one exists, otherwise inferred from the `task` tool result
 * string alone (a persisted-only row after a restart, where `subagentStore`
 * is empty) — `undefined` result means still running, an exact
 * `CANCELLED_TOOL_RESULT` match means cancelled, any other error-shaped JSON
 * means error, anything else means done. Exported standalone so every
 * outcome `SubagentRow` can render is covered by a pure, DOM-free test —
 * see this module's top comment. */
export function resolveSubagentStatus(liveStatus: SubagentStatus | undefined, result: string | undefined): SubagentStatus {
  if (liveStatus) return liveStatus;
  if (result === undefined) return "running";
  if (result === CANCELLED_TOOL_RESULT) return "cancelled";
  if (resultLooksLikeError(result)) return "error";
  return "done";
}

/**
 * Renders a `task` tool_call as a dedicated timeline row (see
 * `MessageList.tsx`'s `buildTimeline`, which special-cases `name === 'task'`
 * to produce this instead of a plain `ToolCallRow`): the model-supplied
 * description, a profile badge, a spinner + `lastActivity` while the
 * subagent is still running (subscribed live from `subagentStore`), and an
 * expandable mini-transcript of the child's own tool calls (reusing
 * `ToolCallRow` for each one) once it settles.
 *
 * Falls back to `ChatSession.subagentRuns[taskId]` (persisted alongside the
 * session) for the mini-transcript when `subagentStore` has no live entry —
 * e.g. a transcript reloaded after an app restart, where the transient store
 * is empty but the run itself already finished and was persisted by
 * `runSubagentTask`'s `finish` helper.
 */
const SubagentRow = memo(function SubagentRow({ sessionId, taskId, args, result }: SubagentRowProps) {
  const { t } = useT();
  const [open, setOpen] = useState(false);
  const detailsId = useId();
  const live = useSubagentStore((state) => state.runs[taskId]);
  const persisted = useSessionStore((state) => state.sessions.find((s) => s.id === sessionId)?.subagentRuns?.[taskId]);
  // Stats snapshotted at finish time — the tokens/count source once the
  // transient store is gone (post-restart), see ChatSession.subagentRunMeta.
  const persistedMeta = useSessionStore((state) => state.sessions.find((s) => s.id === sessionId)?.subagentRunMeta?.[taskId]);

  const { description, profile } = parseTaskArgs(args);
  const status: SubagentStatus = resolveSubagentStatus(live?.status, result);
  const running = status === "running";
  const transcript = live?.liveMessages ?? persisted ?? [];
  const childToolCalls = extractChildToolCalls(transcript);
  const toolCallCount = live?.toolCallCount ?? persistedMeta?.toolCallCount ?? childToolCalls.length;
  const usage = live?.usage ?? persistedMeta?.usage;

  const tone: PillTone = status === "running" ? "warning" : status === "error" ? "danger" : status === "cancelled" ? "neutral" : "success";

  return (
    <div className="flex justify-start">
      <div className="max-w-[85%] min-w-0 overflow-hidden rounded-md border border-border bg-surface-2">
        <button
          type="button"
          aria-expanded={open}
          aria-controls={detailsId}
          onClick={() => setOpen((prev) => !prev)}
          className="flex min-h-11 w-full cursor-pointer items-center gap-2 px-3 py-1.5 text-left text-xs text-muted transition-colors duration-150 hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-inset motion-reduce:transition-none"
        >
          <ChevronRight
            size={12}
            className={`shrink-0 text-faint transition-transform duration-150 motion-reduce:transition-none ${open ? "rotate-90" : ""}`}
          />
          <Bot size={13} className="shrink-0 text-faint" />
          <span className="truncate font-medium text-foreground">{description}</span>
          <span className="shrink-0 rounded-full bg-surface-2 px-1.5 py-0.5 text-[10px] font-medium text-faint">
            {t(profile === "code" ? "SubagentRow.profileCode" : "SubagentRow.profileExplore")}
          </span>
          <StatusPill tone={tone}>{t(statusLabelKey(status))}</StatusPill>
          {usage && (
            <span className="shrink-0 font-mono text-[10px] text-faint">
              {t("SubagentRow.tokenUsage", { count: formatTokenCount(usage.totalTokens) })}
            </span>
          )}
          {running && live?.lastActivity && (
            <span className="ml-auto flex min-w-0 shrink items-center gap-1 truncate text-faint">
              <span className="flex items-center gap-1">
                <span className="h-1 w-1 animate-bounce rounded-full bg-faint [animation-delay:-0.3s]" />
                <span className="h-1 w-1 animate-bounce rounded-full bg-faint [animation-delay:-0.15s]" />
                <span className="h-1 w-1 animate-bounce rounded-full bg-faint" />
              </span>
              <span className="truncate font-mono">{live.lastActivity}</span>
            </span>
          )}
        </button>
        {open && (
          <div id={detailsId} className="space-y-2 border-t border-border bg-background px-3 py-2 font-mono text-[11px] text-muted">
            <div className="flex items-center gap-2 text-faint">
              <span>{t("SubagentRow.toolCallCount", { count: toolCallCount })}</span>
              {usage && (
                <>
                  <span>·</span>
                  <span>{t("SubagentRow.tokenUsage", { count: formatTokenCount(usage.totalTokens) })}</span>
                </>
              )}
            </div>
            {childToolCalls.length === 0 ? (
              <div className="text-faint">{t("SubagentRow.noActivity")}</div>
            ) : (
              <div className="space-y-1.5">
                {childToolCalls.map((row) => (
                  <ToolCallRow key={row.key} name={row.name} args={row.args} result={row.result} />
                ))}
              </div>
            )}
            {result !== undefined && (
              <div>
                <div className="mb-1 text-faint">{t("SubagentRow.reportLabel")}</div>
                <pre className="max-h-56 overflow-auto whitespace-pre-wrap break-all">{result}</pre>
              </div>
            )}
          </div>
        )}
      </div>
    </div>
  );
});

export default SubagentRow;
