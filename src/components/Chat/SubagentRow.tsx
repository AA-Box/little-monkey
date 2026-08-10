import { memo, useEffect, useId, useState } from "react";
import { Asterisk, ChevronRight } from "lucide-react";

import { textContent, type ChatMessage } from "../../lib/llamaClient";
import { CANCELLED_TOOL_RESULT } from "../../lib/turnEngine";
import { unwrapUntrustedContent } from "../../lib/untrustedContent";
import { useSubagentStore, type SubagentStatus } from "../../store/subagentStore";
import { useSessionStore } from "../../store/sessionStore";
import { formatCompactTokens, formatElapsed } from "../../lib/taskFormat";
import { useT } from "../../lib/i18n";
import {
  activityCallCommandLine,
  activityCallCopyText,
  formatActivityResult,
  resultLooksLikeError,
} from "./activityTimeline";
import { StepCopyButton, ToolStepRow, TOOL_STEP_LIST_CLASSES } from "./ToolStepRow";

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
  /** Built-in profile or a custom agent name (`customAgents.ts`). */
  profile: string;
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
        profile: typeof candidate.profile === "string" && candidate.profile.trim().length > 0 ? candidate.profile.trim() : "explore",
      };
    }
  } catch {
    // fall through to the default below
  }
  return { description: "Subagent task", profile: "explore" };
}

/** What the header's agent badge shows: the translated built-in profile
 * label, or a custom agent's own name verbatim (a user-authored identifier,
 * not translatable copy). Exported for `SubagentRow.test.ts`, same
 * logic-not-JSX convention as `parseTaskArgs`. */
export function profileBadge(profile: string): { i18nKey: string } | { raw: string } {
  if (profile === "explore") return { i18nKey: "SubagentRow.profileExplore" };
  if (profile === "code") return { i18nKey: "SubagentRow.profileCode" };
  return { raw: profile };
}

export interface ChildToolCallRow {
  key: string;
  name: string;
  args: string;
  result?: string;
}

/** One round of the child's work: the calls a single assistant message made,
 * under the narration that came with (or immediately before) them —
 * Claude-Code-desktop-style step grouping instead of one flat row per call. */
export interface ChildToolGroup {
  key: string;
  /** The child's own narration for this round; `null` when it said nothing. */
  title: string | null;
  calls: ChildToolCallRow[];
}

const GROUP_TITLE_MAX = 100;

/** First non-blank line of an assistant message, capped — a group header is
 * one line, and a child's narration is often a paragraph. */
function groupTitle(text: string): string | null {
  const line = text
    .split("\n")
    .map((part) => part.trim())
    .find((part) => part.length > 0);
  if (!line) return null;
  return line.length > GROUP_TITLE_MAX ? `${line.slice(0, GROUP_TITLE_MAX - 1)}…` : line;
}

/** Groups the child's own `tool_calls`/`tool` messages by the assistant round
 * that issued them, pairing each call with its result the same way
 * `MessageList.tsx`'s `buildTimeline` does for the parent transcript — a
 * deliberately simpler pass since a subagent's local transcript has none of
 * the parent's notices (no checkpoints/memory/plan/verify/sources rows can
 * appear inside a child run in this slice).
 *
 * A text-only assistant message carries its narration forward to the next
 * round that actually calls tools (models commonly say what they're about to
 * do in one message and do it in the next), so the group reads as a titled
 * step rather than an anonymous batch. */
export function groupChildToolCalls(messages: ChatMessage[]): ChildToolGroup[] {
  const resultByCallId = new Map<string, string>();
  for (const message of messages) {
    if (message.role === "tool" && message.tool_call_id) {
      resultByCallId.set(message.tool_call_id, textContent(message.content));
    }
  }
  const groups: ChildToolGroup[] = [];
  let pendingTitle: string | null = null;
  for (const message of messages) {
    if (message.role !== "assistant") continue;
    const title = groupTitle(textContent(message.content));
    const toolCalls = message.tool_calls ?? [];
    if (toolCalls.length === 0) {
      pendingTitle = title ?? pendingTitle;
      continue;
    }
    groups.push({
      key: toolCalls[0].id,
      title: title ?? pendingTitle,
      calls: toolCalls.map((toolCall) => ({
        key: toolCall.id,
        name: toolCall.function.name,
        args: toolCall.function.arguments,
        result: resultByCallId.get(toolCall.id),
      })),
    });
    pendingTitle = null;
  }
  return groups;
}

/** Flat, ordered view of every child call — the count/"no activity" source,
 * and what a single untitled group renders as. */
export function extractChildToolCalls(messages: ChatMessage[]): ChildToolCallRow[] {
  return groupChildToolCalls(messages).flatMap((group) => group.calls);
}

/** A child call in the shape `activityTimeline`'s per-tool formatters take
 * (`key` here is the same tool_call id their `id` means). */
function asActivityCall(call: ChildToolCallRow) {
  return { id: call.key, name: call.name, args: call.args, result: call.result };
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
  if (unwrapUntrustedContent(result) === CANCELLED_TOOL_RESULT) return "cancelled";
  if (resultLooksLikeError(result)) return "error";
  return "done";
}

/** One step of a subagent's mini-transcript: the round's narration as the
 * step title, expanding to each call's command and output with its own copy
 * button — the same `ToolStepRow` chrome the parent transcript's
 * `ActivityRow` uses for its own steps. */
const ChildToolGroupRow = memo(function ChildToolGroupRow({ group }: { group: ChildToolGroup }) {
  const failed = group.calls.some((call) => call.result !== undefined && resultLooksLikeError(call.result));
  // Untitled round: its own command line is the only thing to name it by.
  const title = group.title ?? activityCallCommandLine(asActivityCall(group.calls[0]));

  return (
    <ToolStepRow title={title} failed={failed}>
      <div className="space-y-3">
        {group.calls.map((call) => {
          const activityCall = asActivityCall(call);
          const result = call.result === undefined ? null : formatActivityResult(call.result);
          return (
            <div key={call.key} className="min-w-0">
              <div className="flex items-start gap-2">
                <pre className="min-w-0 flex-1 whitespace-pre-wrap break-all font-mono text-[11px] text-foreground">
                  {activityCallCommandLine(activityCall)}
                </pre>
                <StepCopyButton text={activityCallCopyText(activityCall)} />
              </div>
              <pre className="mt-1.5 max-h-56 overflow-auto whitespace-pre-wrap break-all font-mono text-[11px] text-muted">
                {result ? result.text || "(no output)" : "…"}
              </pre>
            </div>
          );
        })}
      </div>
    </ToolStepRow>
  );
});

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
  // The live/persisted run knows the ACTUAL profile it ran under (a custom
  // name survives there even when the transcript args were minified away).
  const badge = profileBadge(live?.profile ?? persistedMeta?.profile ?? profile);
  const status: SubagentStatus = resolveSubagentStatus(live?.status, result);
  const running = status === "running";
  const transcript = live?.liveMessages ?? persisted ?? [];
  const childGroups = groupChildToolCalls(transcript);
  const usage = live?.usage ?? persistedMeta?.usage;
  // Elapsed comes from whichever stats source exists — the live entry while
  // the run is going, the finish-time snapshot afterwards (see
  // ChatSession.subagentRunMeta).
  const stats = live ?? persistedMeta;

  // A 1s tick, active only while the run is live, so the footer's elapsed
  // label advances without any store churn — same approach
  // `SubagentGroupCard` takes for its own ticking label.
  const [, setTick] = useState(0);
  useEffect(() => {
    if (!running) return;
    const interval = setInterval(() => setTick((value) => value + 1), 1000);
    return () => clearInterval(interval);
  }, [running]);

  // The header names what the run is doing right now while it works (the
  // child's own last tool boundary), and what it was asked to do once done.
  const headerTitle = running ? live?.lastActivity || description : description;
  const elapsed = stats ? formatElapsed((stats.finishedAt ?? Date.now()) - stats.startedAt) : null;
  const footerParts = [
    status === "error" ? t("SubagentRow.statusFailed") : status === "cancelled" ? t("SubagentRow.statusCancelled") : null,
    elapsed,
    usage ? t("SubagentRow.tokenUsage", { count: formatCompactTokens(usage.totalTokens) }) : null,
  ].filter(Boolean);

  return (
    <div className="flex justify-start">
      <div className="w-full min-w-0">
        <button
          type="button"
          aria-expanded={open}
          aria-controls={detailsId}
          onClick={() => setOpen((prev) => !prev)}
          className="flex min-w-0 max-w-full cursor-pointer items-center gap-1.5 py-0.5 text-left text-[13px] text-muted transition-colors duration-150 hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent motion-reduce:transition-none"
        >
          <span className="min-w-0 truncate">{headerTitle}</span>
          <span className="shrink-0 rounded-full border border-border px-1.5 text-[10px] font-medium text-faint">
            {"i18nKey" in badge ? t(badge.i18nKey) : badge.raw}
          </span>
          {running && (
            <span className="flex shrink-0 items-center gap-1" aria-hidden>
              <span className="h-1 w-1 animate-bounce rounded-full bg-faint [animation-delay:-0.3s]" />
              <span className="h-1 w-1 animate-bounce rounded-full bg-faint [animation-delay:-0.15s]" />
              <span className="h-1 w-1 animate-bounce rounded-full bg-faint" />
            </span>
          )}
          <ChevronRight
            size={13}
            className={`shrink-0 text-faint transition-transform duration-150 motion-reduce:transition-none ${open ? "rotate-90" : ""}`}
            aria-hidden
          />
        </button>
        {open && (
          <div id={detailsId} className="mt-1.5 max-w-[85%]">
            <div className={TOOL_STEP_LIST_CLASSES}>
              {childGroups.length === 0 ? (
                <div className="px-3 py-2.5 text-[13px] text-faint">{t("SubagentRow.noActivity")}</div>
              ) : (
                childGroups.map((group) => <ChildToolGroupRow key={group.key} group={group} />)
              )}
            </div>
            {footerParts.length > 0 && (
              <div className="mt-1.5 flex items-center gap-1.5 px-1 text-[11px] text-faint">
                <Asterisk size={12} className="shrink-0 text-accent" aria-hidden />
                <span className="truncate font-mono">{footerParts.join(" · ")}</span>
              </div>
            )}
            {result !== undefined && (
              <div className="mt-2 px-1 font-mono text-[11px] text-muted">
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
