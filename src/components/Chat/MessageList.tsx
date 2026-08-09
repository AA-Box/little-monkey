import { memo, useEffect, useId, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { reapplyCheckpoint, revertCheckpoint } from "../../lib/checkpointCompensation";
import {
  BookmarkX,
  BookOpen,
  Bot,
  Brain,
  ChevronRight,
  ClipboardCheck,
  Eye,
  FilePenLine,
  FileSearch,
  FileText,
  Folder,
  Globe,
  ListChecks,
  LoaderCircle,
  MessageSquareX,
  Pin,
  Plug,
  RefreshCw,
  Search,
  TerminalSquare,
  TriangleAlert,
  Undo2,
  Wrench,
  type LucideIcon,
} from "lucide-react";

import { textContent, type ChatMessage } from "../../lib/llamaClient";
import { StatusPill } from "../ui";
import {
  checkpointAnchorValid,
  formatCheckpointNotice,
  formatMemoryNotice,
  isCheckpointNotice,
  isMemoryNotice,
  isMentionNotice,
  isPlanNotice,
  isPrivacyNotice,
  isRecipeNotice,
  isSourcesNotice,
  isSwitchNotice,
  isVerifyFixNotice,
  isVerifyNotice,
  parseCheckpointNotice,
  parseMemoryNotice,
  parsePlanNotice,
  parseRecipeNotice,
  parseSourcesNotice,
  parseVerifyNotice,
  type CheckpointNotice,
  type MemoryNotice,
  type PlanNotice,
  type RecipeNotice,
  type SourcesNotice,
  type VerifyNotice,
} from "../../lib/agentLoop";
import { isCompactionMarker } from "../../lib/contextTrimmer";
import { isBtwNotice, isCommandNotice, parseCommandNotice, type CommandNotice } from "../../lib/slashCommands";
import { selectRunningVerifyLabel, selectTurnRunning, useSessionStore } from "../../store/sessionStore";
import { liveTurnTokens, selectTurnStatus, useTurnStatusStore, type TurnStatus } from "../../store/turnStatusStore";
import { formatCompactTokens, formatElapsed } from "../../lib/taskFormat";
import { useCheckpointStore } from "../../store/checkpointStore";
import { useRulesStore } from "../../store/rulesStore";
import { useLocalAppsStore } from "../../store/localAppsStore";
import MessageBubble from "./MessageBubble";
import GeneratedImageCard from "./GeneratedImageCard";
import PlanCard from "./PlanCard";
import SubagentRow from "./SubagentRow";
import SubagentGroupCard, { type SubagentGroupTask } from "./SubagentGroupCard";
import WorkflowChatCard from "./WorkflowChatCard";
import { CheckpointPreviewModal } from "./CheckpointPreviewModal";
import { useT } from "../../lib/i18n";
import { StepCopyButton, ToolStepRow, TOOL_STEP_LIST_CLASSES } from "./ToolStepRow";
import {
  activityCallCommandLine,
  activityCallCopyText,
  activityCallDiff,
  activityCallLabel,
  activityDiffStat,
  activityProgress,
  formatActivityResult,
  groupAssistantRound,
  liveActivityLabel,
  resultLooksLikeError,
  summarizeActivity,
  type ActivityCall,
} from "./activityTimeline";
import { errorMessage } from "../../lib/errors";

export { resultLooksLikeError } from "./activityTimeline";

export interface MessageListProps {
  /** The session whose transcript `messages` is — checkpoint notices mutate
   * their own message in place and must target the right session when two
   * panes are open. */
  sessionId: string;
  messages: ChatMessage[];
  /** Called with a past user message's index and its edited text when the
   * user saves an edit — omit to disable the edit affordance entirely. */
  onEditUserMessage?: (index: number, newText: string) => void;
  /** Disables editing while a turn is in flight. */
  editingDisabled?: boolean;
  /** Called when the user asks to regenerate the last turn — omit to hide
   * the affordance. */
  onRetry?: () => void;
  /** Real transcript index represented by `messages[0]`. Comparison cards
   * render only their branch suffix, but artifact/checkpoint actions still
   * need indices into the full persisted transcript. */
  messageIndexOffset?: number;
  /** Threaded straight through to every `MessageBubble` — see that
   * component's own `onStartSideTask` doc comment. Omitted entirely hides
   * the affordance (e.g. inside a subagent's own mini-transcript, which
   * never renders through this list at all). */
  onStartSideTask?: (index: number) => void;
  /** Prepares a generated PNG as an image attachment in the composer so the
   * user can describe an edit before sending another turn. */
  onEditGeneratedImage?: (path: string, prompt: string, artifactId?: string) => void | Promise<void>;
  /** Opens the Background-tasks drawer — threaded to `SubagentGroupCard` so
   * clicking a parallel-agents card reveals its per-agent table there (see
   * that component's `onOpenPanel`). Omitted keeps the inline expansion. */
  onOpenBackgroundTasks?: () => void;
}

type TimelineItem =
  | { kind: "bubble"; key: string; message: ChatMessage; index: number }
  | { kind: "activity"; key: string; calls: ActivityCall[]; prompt: string }
  | { kind: "subagent"; key: string; taskId: string; args: string; result?: string }
  | { kind: "subagentGroup"; key: string; tasks: SubagentGroupTask[] }
  | { kind: "workflow"; key: string; runId: string; args: string; result?: string }
  | { kind: "notice"; key: string; text: string }
  | { kind: "command"; key: string; notice: CommandNotice }
  | { kind: "checkpoint"; key: string; notice: CheckpointNotice; messageIndex: number }
  | { kind: "memory"; key: string; notice: MemoryNotice; messageIndex: number }
  | { kind: "plan"; key: string; notice: PlanNotice; messageIndex: number }
  | { kind: "verify"; key: string; notice: VerifyNotice }
  | { kind: "sources"; key: string; notice: SourcesNotice }
  | { kind: "recipe"; key: string; notice: RecipeNotice }
  | { kind: "typing"; key: string };

/**
 * Flattens the raw `ChatMessage[]` history into a render-friendly timeline:
 * - user / assistant text messages render as chat bubbles.
 * - every assistant round's non-task `tool_call`s are paired with matching
 *   `role: 'tool'` results (correlated via `tool_call_id`) and grouped into
 *   one compact, collapsible activity row.
 * - `task` calls keep their dedicated live subagent row/group.
 * - a trailing empty assistant message (no content, no tool calls yet)
 *   renders as a typing indicator.
 * - `system` messages are never shown in the transcript, except our own
 *   synthetic notices (compaction, model switch, per-turn checkpoint,
 *   remembered fact, presented plan, doc-chat sources).
 */
function buildTimeline(messages: ChatMessage[], messageIndexOffset = 0): TimelineItem[] {
  const resultByCallId = new Map<string, string>();
  for (const msg of messages) {
    if (msg.role === "tool" && msg.tool_call_id) {
      resultByCallId.set(msg.tool_call_id, textContent(msg.content));
    }
  }

  const renderedCallIds = new Set<string>();
  const items: TimelineItem[] = [];
  let latestUserPrompt = "";

  messages.forEach((msg, index) => {
    const messageIndex = index + messageIndexOffset;
    if (msg.role === "user") {
      latestUserPrompt = textContent(msg.content);
      items.push({ kind: "bubble", key: `msg-${messageIndex}`, message: msg, index: messageIndex });
      return;
    }

    if (msg.role === "assistant") {
      const hasContent = textContent(msg.content).trim().length > 0;
      const toolCalls = msg.tool_calls ?? [];

      if (hasContent) {
        items.push({ kind: "bubble", key: `msg-${messageIndex}`, message: msg, index: messageIndex });
      } else if (toolCalls.length === 0 && index === messages.length - 1) {
        items.push({ kind: "typing", key: `typing-${index}` });
      }

      for (const toolCall of toolCalls) {
        renderedCallIds.add(toolCall.id);
      }

      // One assistant round becomes at most one normal activity row and one
      // task row/group. `groupAssistantRound` positions each at the first
      // call of its kind and preserves the calls' original order inside it.
      for (const entry of groupAssistantRound(toolCalls, resultByCallId)) {
        if (entry.kind === "activity") {
          items.push({
            kind: "activity",
            key: `activity-${entry.calls[0].id}`,
            calls: entry.calls,
            prompt: latestUserPrompt,
          });
          continue;
        }

        if (entry.kind === "workflows") {
          // One card per `workflow` call — a round issuing several distinct
          // named workflows should read as several named cards, unlike the
          // interchangeable agents of a task group.
          for (const call of entry.calls) {
            items.push({ kind: "workflow", key: `workflow-${call.id}`, runId: call.id, args: call.args, result: call.result });
          }
          continue;
        }

        if (entry.calls.length >= 2) {
          items.push({
            kind: "subagentGroup",
            key: `subagent-group-${entry.calls[0].id}`,
            tasks: entry.calls.map((call) => ({
              taskId: call.id,
              args: call.args,
              result: call.result,
            })),
          });
        } else {
          const [call] = entry.calls;
          items.push({
            kind: "subagent",
            key: `subagent-${call.id}`,
            taskId: call.id,
            args: call.args,
            result: call.result,
          });
        }
      }
      return;
    }

    if (msg.role === "tool") {
      // Already rendered alongside its originating assistant tool_call.
      if (msg.tool_call_id && renderedCallIds.has(msg.tool_call_id)) return;
      // Orphaned result (e.g. history was truncated) — still show something.
      items.push({
        kind: "activity",
        key: `activity-orphan-${index}`,
        calls: [{
          id: msg.tool_call_id ?? `orphan-${index}`,
          name: "tool",
          args: "",
          result: textContent(msg.content),
        }],
        prompt: latestUserPrompt,
      });
    }

    if (msg.role === "system") {
      // Only our own synthetic notices (context compaction, model
      // auto-switch, unresolved mentions, per-turn checkpoint, remembered
      // fact) are ever rendered — any other system message stays hidden,
      // same as before this app ever produced these.
      if (isCheckpointNotice(msg)) {
        const notice = parseCheckpointNotice(msg);
        if (notice) {
          items.push({ kind: "checkpoint", key: `checkpoint-${notice.id}`, notice, messageIndex });
        }
        return;
      }
      if (isMemoryNotice(msg)) {
        const notice = parseMemoryNotice(msg);
        if (notice) {
          items.push({ kind: "memory", key: `memory-${notice.id}`, notice, messageIndex });
        }
        return;
      }
      if (isPlanNotice(msg)) {
        const notice = parsePlanNotice(msg);
        if (notice) {
          items.push({ kind: "plan", key: `plan-${notice.id}`, notice, messageIndex });
        }
        return;
      }
      if (isVerifyNotice(msg)) {
        const notice = parseVerifyNotice(msg);
        if (notice) {
          items.push({ kind: "verify", key: `verify-${index}`, notice });
        }
        return;
      }
      if (isSourcesNotice(msg)) {
        const notice = parseSourcesNotice(msg);
        if (notice) {
          items.push({ kind: "sources", key: `sources-${index}`, notice });
        }
        return;
      }
      if (isRecipeNotice(msg)) {
        const notice = parseRecipeNotice(msg);
        if (notice) {
          items.push({ kind: "recipe", key: `recipe-${index}`, notice });
        }
        return;
      }
      if (isBtwNotice(msg)) return; // rendered in the floating Side Chat panel instead (SideChatPanel.tsx)
      if (isCommandNotice(msg)) {
        const notice = parseCommandNotice(msg);
        if (notice) items.push({ kind: "command", key: `command-${index}`, notice });
        return;
      }
      if (
        isCompactionMarker(msg) ||
        isSwitchNotice(msg) ||
        isMentionNotice(msg) ||
        isVerifyFixNotice(msg) ||
        isPrivacyNotice(msg)
      ) {
        items.push({ kind: "notice", key: `notice-${index}`, text: textContent(msg.content) });
      }
    }
  });

  return items;
}

function formatJson(raw: string): string {
  try {
    return JSON.stringify(JSON.parse(raw), null, 2);
  } catch {
    return raw;
  }
}

const TOOL_ICONS: Record<string, LucideIcon> = {
  run_shell: TerminalSquare,
  write_file: FilePenLine,
  edit_file: FilePenLine,
  read_file: FileText,
  list_dir: Folder,
  grep: Search,
  glob: FileSearch,
  remember: Brain,
  web_fetch: Globe,
  web_search: Search,
  search_docs: BookOpen,
  // `task` (subagent delegation) is special-cased in `buildTimeline` to
  // render as a dedicated `SubagentRow` instead of a plain `ToolCallRow` —
  // kept here anyway as the icon for the "orphaned tool result" fallback
  // path (a persisted transcript with a `task` result but no matching
  // `tool_calls` entry, e.g. after history truncation).
  task: Bot,
};

function toolIcon(name: string): LucideIcon {
  // An MCP tool call's name is `mcp__<serverId>__<toolName>` (see
  // `mcpTools.ts::mcpToolDefs`) — never a fixed key in `TOOL_ICONS` above,
  // since the server/tool half varies, so it's matched by prefix instead.
  if (name.startsWith("mcp__")) return Plug;
  return TOOL_ICONS[name] ?? Wrench;
}

/** Extracts a tool call's suggested filename for the `generate_image`
 * inline preview — same defensive shape as `agentLoop.ts`'s
 * `toolCallPathArg`, kept as its own tiny copy for the same reason
 * `resultLooksLikeError` is (a one-line structural check isn't worth
 * coupling the modules over). */
function argsImageFilename(args: string): string | null {
  try {
    const parsed: unknown = JSON.parse(args || "{}");
    const value = parsed as { filename?: unknown; path?: unknown } | null;
    const filename = typeof value?.filename === "string" ? value.filename : value?.path;
    return typeof filename === "string" && filename.trim() ? filename : null;
  } catch {
    return null;
  }
}

function diffBlock(text: string, marker: "-" | "+"): string {
  const value = text.length > 0 ? text : "(empty)";
  return value
    .split("\n")
    .map((line) => `${marker} ${line}`)
    .join("\n");
}

/** One tool call as a step of `ActivityRow`'s list: its command line and copy
 * button, then any diff it produced and whatever it returned. Wrapped in the
 * `ToolStepRow` chrome a subagent's own steps use, so the parent transcript
 * and a subagent card read identically. */
const ActivityCallDetail = memo(function ActivityCallDetail({ call }: { call: ActivityCall }) {
  const pending = call.result === undefined;
  const failed = call.result !== undefined && resultLooksLikeError(call.result);
  const diff = activityCallDiff(call);
  const result = call.result === undefined ? null : formatActivityResult(call.result);

  return (
    <ToolStepRow title={activityCallCommandLine(call)} failed={failed}>
      <div className="flex items-start gap-2">
        <pre className="min-w-0 flex-1 whitespace-pre-wrap break-all font-mono text-[11px] text-foreground">
          {activityCallCommandLine(call)}
        </pre>
        <StepCopyButton text={activityCallCopyText(call)} />
      </div>

      {diff && (
        <div
          className="mt-2 space-y-2"
          aria-label={`${activityCallLabel(call.name)} ${diff.state} preview`}
        >
          {diff.state !== "applied" && (
            <div
              className={`text-[10px] font-medium uppercase tracking-wide ${
                diff.state === "attempted" ? "text-danger" : "text-warning"
              }`}
            >
              {diff.state === "attempted" ? "Attempted change (failed)" : "Proposed change"}
            </div>
          )}
          {diff.before && (
            <div>
              <div className="mb-1 text-[10px] font-medium uppercase tracking-wide text-faint">
                {diff.state === "applied" ? "Before" : "Expected before"}
              </div>
              <pre className="max-h-52 overflow-auto whitespace-pre-wrap break-all border-l-2 border-danger bg-danger-soft px-2 py-1.5 font-mono text-[11px] text-danger">
                {diffBlock(diff.before.text, "-")}
              </pre>
            </div>
          )}
          <div>
            <div className="mb-1 text-[10px] font-medium uppercase tracking-wide text-faint">
              {diff.state === "applied"
                ? diff.kind === "write" ? "Added content" : "After"
                : diff.state === "attempted" ? "Attempted content" : "Proposed content"}
            </div>
            <pre className="max-h-52 overflow-auto whitespace-pre-wrap break-all border-l-2 border-success bg-success-soft px-2 py-1.5 font-mono text-[11px] text-success">
              {diffBlock(diff.after.text, "+")}
            </pre>
          </div>
        </div>
      )}

      {result ? (
        <pre
          className={`mt-1.5 max-h-56 overflow-auto whitespace-pre-wrap break-all font-mono text-[11px] ${
            failed ? "text-danger" : "text-muted"
          }`}
        >
          {result.text || "(no output)"}
        </pre>
      ) : (
        <div className="mt-1.5 flex items-center gap-1.5 font-mono text-[11px] text-muted">
          <LoaderCircle size={12} className="animate-spin motion-reduce:animate-none" aria-hidden />
          {pending ? "Waiting for result…" : ""}
        </div>
      )}
    </ToolStepRow>
  );
});

/** Codex-style operational timeline for one assistant round. It deliberately
 * reports only observable tool activity; model reasoning remains represented
 * by the generic live "Thinking" status below. */
const ActivityRow = memo(function ActivityRow({ calls }: { calls: ActivityCall[] }) {
  const [open, setOpen] = useState(false);
  const detailsId = useId();
  const summary = summarizeActivity(calls);
  const progress = activityProgress(calls);
  const { added, removed } = activityDiffStat(calls);

  return (
    <div className="flex justify-start">
      <div className="w-full min-w-0">
        <button
          type="button"
          aria-expanded={open}
          aria-controls={detailsId}
          onClick={() => setOpen((prev) => !prev)}
          className="flex w-full cursor-pointer items-center gap-1.5 py-0.5 text-left text-[13px] text-muted transition-colors duration-150 hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent motion-reduce:transition-none"
        >
          <span className="min-w-0 truncate">{summary}</span>
          {added > 0 && <span className="shrink-0 text-success">+{added}</span>}
          {removed > 0 && <span className="shrink-0 text-danger">-{removed}</span>}
          {progress.status === "running" && (
            <LoaderCircle size={12} className="shrink-0 animate-spin text-faint motion-reduce:animate-none" aria-hidden />
          )}
          {progress.status === "failed" && (
            <span className="shrink-0 text-danger">{progress.label}</span>
          )}
          <ChevronRight
            size={13}
            className={`shrink-0 text-faint transition-transform duration-150 motion-reduce:transition-none ${open ? "rotate-90" : ""}`}
            aria-hidden
          />
        </button>
        {open && (
          <div id={detailsId} className={`mt-1.5 max-w-[85%] ${TOOL_STEP_LIST_CLASSES}`}>
            {calls.map((call) => (
              <ActivityCallDetail key={call.id} call={call} />
            ))}
          </div>
        )}
      </div>
    </div>
  );
});

/** Memoized like `MessageBubble`: props are plain strings (stable for every
 * settled call), so streaming deltas to the transcript's last message don't
 * re-render the (potentially long) tool-call history. */
export const ToolCallRow = memo(function ToolCallRow({ name, args, result }: { name: string; args: string; result?: string }) {
  const { t } = useT();
  const [open, setOpen] = useState(false);
  const detailsId = useId();
  const pending = result === undefined;
  const failed = !pending && resultLooksLikeError(result);
  const Icon = toolIcon(name);
  const preview = args ? formatJson(args).replace(/\s+/g, " ").trim() : "";

  return (
    <div className="flex justify-start">
      <div className="max-w-[85%] min-w-0 overflow-hidden rounded-md border border-border bg-surface-2">
        <button
          type="button"
          aria-expanded={open}
          aria-controls={detailsId}
          onClick={() => setOpen((prev) => !prev)}
          className="flex min-h-11 w-full cursor-pointer items-center gap-2 px-3 py-1.5 text-left font-mono text-xs text-muted transition-colors duration-150 hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-inset motion-reduce:transition-none"
        >
          <ChevronRight
            size={12}
            className={`shrink-0 text-faint transition-transform duration-150 motion-reduce:transition-none ${open ? "rotate-90" : ""}`}
          />
          <Icon size={13} className="shrink-0 text-faint" />
          <span
            className={`h-1.5 w-1.5 shrink-0 rounded-full ${
              pending ? "animate-pulse bg-warning" : failed ? "bg-danger" : "bg-success"
            }`}
          />
          <span className="truncate">
            {name}
            {preview ? `(${preview})` : "()"}
          </span>
          <StatusPill tone={pending ? "warning" : failed ? "danger" : "success"}>
            {t(pending ? "SubagentRow.statusRunning" : failed ? "SubagentRow.statusFailed" : "SubagentRow.statusDone")}
          </StatusPill>
        </button>
        {open && (
          <div id={detailsId} className="space-y-2 border-t border-border bg-background px-3 py-2 font-mono text-[11px] text-muted">
            {args && (
              <div>
                <div className="mb-1 text-faint">{t("MessageList.argumentsLabel")}</div>
                <pre className="max-h-56 overflow-auto whitespace-pre-wrap break-all">{formatJson(args)}</pre>
              </div>
            )}
            {result !== undefined && (
              <div>
                <div className="mb-1 text-faint">{t("MessageList.resultLabel")}</div>
                <pre className="max-h-56 overflow-auto whitespace-pre-wrap break-all">{formatJson(result)}</pre>
              </div>
            )}
          </div>
        )}
      </div>
    </div>
  );
});

/** Renders a compaction/model-switch notice: a small, centered, muted line — visually distinct from a chat bubble, since it's app-generated commentary rather than something either party "said". */
const NoticeRow = memo(function NoticeRow({ text }: { text: string }) {
  return (
    <div className="flex justify-center">
      <div className="max-w-[85%] rounded-md bg-surface-2 px-3 py-1 text-center text-xs text-faint">{text}</div>
    </div>
  );
});

const CommandRow = memo(function CommandRow({ notice }: { notice: CommandNotice }) {
  return (
    <div className="flex justify-start">
      <div className={`max-w-[85%] overflow-hidden rounded-md border px-3 py-2 ${
        notice.ok ? "border-border bg-surface-2" : "border-danger bg-danger-soft"
      }`}>
        <div className="mb-1 font-mono text-[11px] font-semibold text-faint">/{notice.command}</div>
        <pre className="whitespace-pre-wrap break-words font-sans text-xs text-muted">{notice.text}</pre>
      </div>
    </div>
  );
});

/** The three restore scopes a checkpoint notice offers — Claude Code
 * /rewind semantics: code only / conversation only / both. */
type RestoreScope = "files" | "conversation" | "both";

/**
 * Renders a per-turn checkpoint notice: how many files the turn changed,
 * with a Restore menu offering three scopes (see src-tauri/src/checkpoints.rs):
 * - "Restore files" copies every touched file back to its pre-turn state and
 *   rewrites the notice in place with `reverted: true`, so the state survives
 *   re-renders and app restarts.
 * - "Rewind conversation" truncates the transcript back to just before the
 *   turn's user message (`anchorIndex`). Offered only while `anchorValid`
 *   (the anchored message still matches the turn's prompt — compaction and
 *   edit-resubmit shift indices) and hard-blocked while a turn is running in
 *   this session, whose `addMessage` calls would resurrect truncated state.
 * - "Restore both" does both, files first (a failed file restore must not
 *   still rewind the conversation).
 */
const CheckpointRow = memo(function CheckpointRow({
  sessionId,
  notice,
  messageIndex,
  anchorValid,
}: {
  sessionId: string;
  notice: CheckpointNotice;
  messageIndex: number;
  anchorValid: boolean;
}) {
  const { t } = useT();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [menuOpen, setMenuOpen] = useState(false);
  const [previewOpen, setPreviewOpen] = useState(false);
  const menuRef = useRef<HTMLDivElement>(null);
  const turnRunning = useSessionStore(selectTurnRunning(sessionId));

  // Notices recorded before manifest v2 (see `CheckpointNotice`'s doc
  // comment) never got an `anchorIndex`/`label` at all — the preview modal
  // needs both (it identifies the turn's own message range from them), so
  // there's nothing to preview for those, same as they already can't offer
  // conversation rewind.
  const previewSubject =
    typeof notice.anchorIndex === "number" && typeof notice.label === "string"
      ? { id: notice.id, anchorIndex: notice.anchorIndex, label: notice.label, shellRan: Boolean(notice.shellRan), reverted: Boolean(notice.reverted) }
      : null;

  useEffect(() => {
    if (!menuOpen) return;
    function handlePointerDown(event: PointerEvent) {
      if (menuRef.current && !menuRef.current.contains(event.target as Node)) {
        setMenuOpen(false);
      }
    }
    window.addEventListener("pointerdown", handlePointerDown);
    return () => window.removeEventListener("pointerdown", handlePointerDown);
  }, [menuOpen]);

  const fileNames = notice.files.map((path) => path.split(/[\\/]/).filter(Boolean).pop() ?? path);
  const canRewind = anchorValid && !turnRunning;
  const rewindBlockedReason = turnRunning
    ? t("MessageList.checkpointRewindBlockedTurnRunning")
    : !anchorValid
      ? t("MessageList.checkpointRewindUnavailable")
      : undefined;

  const restoreFiles = async (): Promise<boolean> => {
    try {
      await revertCheckpoint(notice.id);
      useSessionStore.getState().updateMessageAt(sessionId, messageIndex, {
        content: formatCheckpointNotice({ ...notice, reverted: true }),
      });
      void useCheckpointStore.getState().refresh(sessionId);
      return true;
    } catch (err) {
      const message = errorMessage(err);
      setError(t("MessageList.checkpointRevertFailed", { error: message }));
      return false;
    }
  };

  /** Undoes a previous "Restore files": plays the checkpoint's redo backups
   * back over the files revert touched (see `checkpoint_reapply` in
   * checkpoints.rs) and flips the notice back to not-reverted. */
  const reapplyFiles = async (): Promise<void> => {
    setBusy(true);
    setError(null);
    try {
      await reapplyCheckpoint(notice.id);
      useSessionStore.getState().updateMessageAt(sessionId, messageIndex, {
        content: formatCheckpointNotice({ ...notice, reverted: false }),
      });
      void useCheckpointStore.getState().refresh(sessionId);
    } catch (err) {
      const message = errorMessage(err);
      setError(t("MessageList.checkpointReapplyFailed", { error: message }));
    } finally {
      setBusy(false);
    }
  };

  /** Truncating at the anchor drops the turn's user message and everything
   * after it — including this notice itself, so no in-place rewrite needed. */
  const rewindConversation = () => {
    if (!canRewind || typeof notice.anchorIndex !== "number") return;
    useSessionStore.getState().truncateFromIndex(sessionId, notice.anchorIndex);
  };

  const handleRestore = async (scope: RestoreScope) => {
    setMenuOpen(false);
    setBusy(true);
    setError(null);
    try {
      if (scope === "files") {
        await restoreFiles();
      } else if (scope === "conversation") {
        rewindConversation();
      } else if (await restoreFiles()) {
        rewindConversation();
      }
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex justify-center">
      <div className="flex max-w-[85%] flex-col items-center gap-1 rounded-md bg-surface-2 px-3 py-1.5 text-center text-xs text-faint">
        <div className="flex items-center gap-2">
          <FilePenLine size={12} className="shrink-0" />
          <span title={notice.files.join("\n")}>
            {t("MessageList.checkpointFilesChanged", { count: notice.files.length, files: fileNames.join(", ") })}
          </span>
          {previewSubject && (
            <button
              type="button"
              onClick={() => setPreviewOpen(true)}
              className="flex cursor-pointer items-center gap-1 rounded-md border border-border px-1.5 py-0.5 text-muted transition-colors hover:text-foreground"
            >
              <Eye size={11} />
              {t("CheckpointTimeline.previewButton")}
            </button>
          )}
          {notice.reverted ? (
            <>
              <span className="font-medium text-muted">{t("MessageList.checkpointRevertedLabel")}</span>
              <button
                type="button"
                onClick={() => void reapplyFiles()}
                disabled={busy}
                className="flex cursor-pointer items-center gap-1 rounded-md border border-border px-1.5 py-0.5 text-muted transition-colors hover:text-foreground disabled:cursor-not-allowed disabled:opacity-50"
              >
                <RefreshCw size={11} />
                {busy ? t("MessageList.checkpointReapplying") : t("MessageList.checkpointReapplyButton")}
              </button>
            </>
          ) : (
            <div ref={menuRef} className="relative inline-block">
              <button
                type="button"
                onClick={() => setMenuOpen((prev) => !prev)}
                disabled={busy}
                aria-haspopup="true"
                aria-expanded={menuOpen}
                className="flex cursor-pointer items-center gap-1 rounded-md border border-border px-1.5 py-0.5 text-muted transition-colors hover:text-foreground disabled:cursor-not-allowed disabled:opacity-50"
              >
                <Undo2 size={11} />
                {busy ? t("MessageList.checkpointReverting") : t("MessageList.checkpointRestoreButton")}
              </button>
              {menuOpen && (
                <div className="absolute left-1/2 top-full z-20 mt-1 w-52 -translate-x-1/2 rounded-lg border border-border bg-background py-1 shadow-lg">
                  <button
                    type="button"
                    onClick={() => void handleRestore("files")}
                    className="flex w-full cursor-pointer items-center gap-2 px-3 py-2 text-left text-sm hover:bg-surface-2"
                  >
                    <FilePenLine size={14} className="shrink-0 text-faint" />
                    {t("MessageList.checkpointRestoreFiles")}
                  </button>
                  <button
                    type="button"
                    onClick={() => void handleRestore("conversation")}
                    disabled={!canRewind}
                    title={rewindBlockedReason}
                    className="flex w-full cursor-pointer items-center gap-2 px-3 py-2 text-left text-sm hover:bg-surface-2 disabled:cursor-not-allowed disabled:opacity-50 disabled:hover:bg-transparent"
                  >
                    <MessageSquareX size={14} className="shrink-0 text-faint" />
                    {t("MessageList.checkpointRewindConversation")}
                  </button>
                  <button
                    type="button"
                    onClick={() => void handleRestore("both")}
                    disabled={!canRewind}
                    title={rewindBlockedReason}
                    className="flex w-full cursor-pointer items-center gap-2 px-3 py-2 text-left text-sm hover:bg-surface-2 disabled:cursor-not-allowed disabled:opacity-50 disabled:hover:bg-transparent"
                  >
                    <Undo2 size={14} className="shrink-0 text-faint" />
                    {t("MessageList.checkpointRestoreBoth")}
                  </button>
                </div>
              )}
            </div>
          )}
        </div>
        {notice.shellRan && (
          <div className="flex items-center gap-1 text-warning">
            <TriangleAlert size={11} className="shrink-0" />
            <span>{t("MessageList.checkpointShellRanCaveat")}</span>
          </div>
        )}
        {error && <div className="text-danger">{error}</div>}
      </div>
      {previewOpen && previewSubject && (
        <CheckpointPreviewModal
          sessionId={sessionId}
          checkpoint={previewSubject}
          onClose={() => setPreviewOpen(false)}
          onChanged={() => void useCheckpointStore.getState().refresh(sessionId)}
        />
      )}
    </div>
  );
});

/**
 * Renders a "remembered fact" notice inserted right after a successful
 * `remember` tool call: the fact's text plus a one-click Forget button that
 * calls `memory_delete` and rewrites the notice in place with
 * `forgotten: true` (exact `CheckpointRow` "Restore files" pattern, minus the
 * restore-scope menu — there's only one thing to undo here).
 */
const MemoryRow = memo(function MemoryRow({
  sessionId,
  notice,
  messageIndex,
}: {
  sessionId: string;
  notice: MemoryNotice;
  messageIndex: number;
}) {
  const { t } = useT();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const forget = async () => {
    setBusy(true);
    setError(null);
    try {
      await invoke("memory_delete", { id: notice.id });
      useSessionStore.getState().updateMessageAt(sessionId, messageIndex, {
        content: formatMemoryNotice({ ...notice, forgotten: true }),
      });
      void useRulesStore.getState().refresh();
    } catch (err) {
      const message = errorMessage(err);
      setError(t("MessageList.memoryForgetFailed", { error: message }));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex justify-center">
      <div className="flex max-w-[85%] flex-col items-center gap-1 rounded-md bg-surface-2 px-3 py-1.5 text-center text-xs text-faint">
        <div className="flex items-center gap-2">
          <Brain size={12} className="shrink-0" />
          <span>{t("MessageList.memoryRemembered", { text: notice.text })}</span>
          {notice.forgotten ? (
            <span className="font-medium text-muted">{t("MessageList.memoryForgottenLabel")}</span>
          ) : (
            <button
              type="button"
              onClick={() => void forget()}
              disabled={busy}
              className="flex cursor-pointer items-center gap-1 rounded-md border border-border px-1.5 py-0.5 text-muted transition-colors hover:text-foreground disabled:cursor-not-allowed disabled:opacity-50"
            >
              <BookmarkX size={11} />
              {busy ? t("MessageList.memoryForgetting") : t("MessageList.memoryForgetButton")}
            </button>
          )}
        </div>
        {error && <div className="text-danger">{error}</div>}
      </div>
    </div>
  );
});

/** Renders one `[Recipe]` notice — purely informational, no action to take
 * (unlike `MemoryRow`'s Forget button): just marks that this session was
 * started by `recipeRunner.ts`'s "Run now" (design doc slice 2), naming
 * which recipe. */
const RecipeRow = memo(function RecipeRow({ notice }: { notice: RecipeNotice }) {
  const { t } = useT();
  // Only known once `localAppsStore` has been refreshed at least once
  // (App.tsx's boot effect) — falls back to the plain notice below when the
  // app was since unpublished or the list hasn't loaded yet.
  const localApp = useLocalAppsStore((s) =>
    notice.localAppId ? s.apps.find((a) => a.id === notice.localAppId) : undefined,
  );
  const label = localApp
    ? t("MessageList.recipeStartedFromLocalApp", { name: notice.name, appName: localApp.name })
    : t("MessageList.recipeStarted", { name: notice.name });
  return (
    <div className="flex justify-center">
      <div className="flex max-w-[85%] items-center gap-2 rounded-md bg-surface-2 px-3 py-1.5 text-center text-xs text-faint">
        <ListChecks size={12} className="shrink-0" />
        <span>{label}</span>
      </div>
    </div>
  );
});

/** Renders one `[Verify]` notice: the configured command's label, a
 * pass/fail `StatusPill`, its duration, and a collapsible output block —
 * reuses `ToolCallRow`'s collapse affordance rather than introducing a new
 * one. Report-only in this slice: there is nothing to act on here yet (no
 * "run again"/"fix it" affordance), just the result. */
const VerifyRow = memo(function VerifyRow({ notice }: { notice: VerifyNotice }) {
  const { t } = useT();
  const [open, setOpen] = useState(false);
  const seconds = (notice.durationMs / 1000).toFixed(1);

  return (
    <div className="flex justify-center">
      <div className="max-w-[85%] min-w-0 overflow-hidden rounded-md border border-border bg-surface-2">
        <button
          type="button"
          onClick={() => setOpen((prev) => !prev)}
          className="flex w-full cursor-pointer items-center gap-2 px-3 py-1.5 text-left text-xs text-muted transition-colors duration-150 hover:text-foreground"
        >
          <ChevronRight
            size={12}
            className={`shrink-0 text-faint transition-transform duration-150 ${open ? "rotate-90" : ""}`}
          />
          <ClipboardCheck size={13} className="shrink-0 text-faint" />
          <span className="truncate font-medium text-foreground">{notice.label}</span>
          <StatusPill tone={notice.ok ? "success" : "danger"}>
            {notice.ok ? t("MessageList.verifyPassedBadge") : t("MessageList.verifyFailedBadge")}
          </StatusPill>
          <span className="ml-auto shrink-0 whitespace-nowrap text-faint">
            {t("MessageList.verifyDuration", { seconds })}
          </span>
        </button>
        {open && (
          <div className="border-t border-border bg-background px-3 py-2 font-mono text-[11px] text-muted">
            {notice.output ? (
              <pre className="max-h-56 overflow-auto whitespace-pre-wrap break-all">{notice.output}</pre>
            ) : (
              <span className="text-faint">{t("MessageList.verifyNoOutput")}</span>
            )}
          </div>
        )}
      </div>
    </div>
  );
});

/**
 * Renders a doc-chat `[Sources]` notice (see `SOURCES_NOTE_PREFIX`): the
 * retrieved passages as collapsible chips, each showing the source file name
 * and its stack badge collapsed, expanding to the full snippet on click —
 * same collapse affordance as `ToolCallRow`/`VerifyRow`, just one toggle per
 * chip instead of one for the whole row, since a doc-chat turn typically
 * retrieves several passages at once and showing every snippet by default
 * would dominate the transcript.
 */
const SourcesRow = memo(function SourcesRow({ notice }: { notice: SourcesNotice }) {
  const { t } = useT();
  const [openIndex, setOpenIndex] = useState<number | null>(null);

  if (notice.results.length === 0) return null;

  return (
    <div className="flex justify-start">
      <div className="max-w-[85%] min-w-0 overflow-hidden rounded-md border border-border bg-surface-2">
        <div className="flex items-center gap-2 px-3 py-1.5 text-xs text-muted">
          <BookOpen size={13} className="shrink-0 text-faint" />
          <span className="font-medium text-foreground">
            {t("MessageList.sourcesHeading", { count: notice.results.length })}
          </span>
        </div>
        <div className="flex flex-col gap-1 border-t border-border px-2 py-2">
          {notice.results.map((result, i) => {
            const fileName = result.path.split(/[\\/]/).filter(Boolean).pop() ?? result.path;
            const open = openIndex === i;
            return (
              <div
                key={`${result.path}-${i}`}
                className="overflow-hidden rounded-md border border-border bg-background"
              >
                <button
                  type="button"
                  onClick={() => setOpenIndex(open ? null : i)}
                  title={result.path}
                  className="flex w-full cursor-pointer items-center gap-2 px-2 py-1 text-left text-xs text-muted transition-colors duration-150 hover:text-foreground"
                >
                  <ChevronRight
                    size={11}
                    className={`shrink-0 text-faint transition-transform duration-150 ${open ? "rotate-90" : ""}`}
                  />
                  <FileText size={12} className="shrink-0 text-faint" />
                  <span className="truncate font-mono">{fileName}</span>
                  <span className="ml-auto shrink-0 rounded-full bg-surface-2 px-1.5 py-0.5 text-[10px] font-medium text-faint">
                    {result.stack}
                  </span>
                </button>
                {open && (
                  <pre className="max-h-40 overflow-auto whitespace-pre-wrap break-all border-t border-border bg-surface-2 px-2 py-1.5 font-mono text-[11px] text-muted">
                    {result.snippet}
                  </pre>
                )}
              </div>
            );
          })}
        </div>
      </div>
    </div>
  );
});

function TypingIndicator() {
  return (
    <div className="flex justify-start">
      <div className="flex items-center gap-1 rounded-2xl border border-border bg-surface px-4 py-3">
        <span className="h-1.5 w-1.5 animate-bounce rounded-full bg-faint [animation-delay:-0.3s]" />
        <span className="h-1.5 w-1.5 animate-bounce rounded-full bg-faint [animation-delay:-0.15s]" />
        <span className="h-1.5 w-1.5 animate-bounce rounded-full bg-faint" />
      </div>
    </div>
  );
}

/** How long the turn must go without a sign of life (`TurnStatus.
 * lastEventAt`) before "thinking…" escalates to "still thinking…" — purely
 * cosmetic reassurance that a long-SILENT turn hasn't hung, same motivation
 * as `VerifyRunningRow`'s existence. Keyed off silence, not total turn age:
 * a healthy multi-round turn shouldn't be permanently branded stuck. */
const STILL_THINKING_AFTER_MS = 60_000;

/**
 * The live status line for the ACTIVE turn — "✳ 1m 30s · 181.1k tokens ·
 * thinking…" — rendered at the bottom of the transcript for the whole turn
 * (replacing the bare `TypingIndicator`, which only ever showed while the
 * trailing assistant message was still empty and disappeared during tool
 * rounds). Elapsed time ticks locally once a second; tokens and the
 * activity word come live from `turnStatusStore` (fed by `agentLoop.ts` /
 * `attemptStream`).
 */
function TurnStatusLine({ status }: { status: TurnStatus }) {
  const { t } = useT();
  const [, setTick] = useState(0);
  useEffect(() => {
    const interval = setInterval(() => setTick((value) => value + 1), 1000);
    return () => clearInterval(interval);
  }, []);

  const elapsedMs = Date.now() - status.startedAt;
  const silentMs = Date.now() - status.lastEventAt;
  const { tokens, estimated } = liveTurnTokens(status);
  const word = status.activity
    ? `${liveActivityLabel(status.activity)}…`
    : t(silentMs >= STILL_THINKING_AFTER_MS ? "MessageList.turnStatusStillThinking" : "MessageList.turnStatusThinking");

  return (
    // Deliberately NOT a `role="status"` live region: the elapsed label
    // changes every second and would spam screen readers.
    <div className="flex items-center gap-2 text-xs text-faint">
      <span className="animate-pulse text-accent" aria-hidden>
        ✳
      </span>
      <span className="font-mono">{formatElapsed(elapsedMs)}</span>
      {tokens > 0 && (
        <>
          <span>·</span>
          {/* `~` while part of the number is estimated from streamed text —
              it firms up to the exact count the moment the attempt reports
              its usage. */}
          <span className="font-mono">
            {t("MessageList.turnStatusTokens", {
              count: `${estimated ? "~" : ""}${formatCompactTokens(tokens)}`,
            })}
          </span>
        </>
      )}
      <span>·</span>
      <span className="truncate font-mono">{word}</span>
    </div>
  );
}

/**
 * Shown while a configured verification command is actually executing
 * (`sessionStore.runningVerifyLabel`, set/cleared by `runVerificationPhase`
 * in agentLoop.ts) — a dedicated "running <label>…" row rather than the bare
 * `TypingIndicator`, since test suites can run for minutes (up to a
 * command's `timeout_secs`) and a bouncing-dots bubble alone would read as a
 * hang (see the design doc's "long-running test suites stall the turn"
 * risk). Same bounce animation, kept visually related to `TypingIndicator`.
 */
function VerifyRunningRow({ label }: { label: string }) {
  const { t } = useT();
  return (
    <div className="flex justify-center">
      <div className="flex items-center gap-2 rounded-md border border-border bg-surface-2 px-3 py-1.5 text-xs text-muted">
        <ClipboardCheck size={13} className="shrink-0 text-faint" />
        <span className="truncate">{t("MessageList.verifyRunning", { label })}</span>
        <span className="flex items-center gap-1">
          <span className="h-1 w-1 animate-bounce rounded-full bg-faint [animation-delay:-0.3s]" />
          <span className="h-1 w-1 animate-bounce rounded-full bg-faint [animation-delay:-0.15s]" />
          <span className="h-1 w-1 animate-bounce rounded-full bg-faint" />
        </span>
      </div>
    </div>
  );
}

function EmptyState() {
  const { t } = useT();
  return (
    <div className="flex h-full flex-col items-center justify-center gap-1.5 text-center">
      <p className="text-lg font-medium text-foreground">{t("MessageList.emptyStateTitle")}</p>
      <p className="max-w-xs text-sm text-faint">
        {t("MessageList.emptyStateDescription")}
      </p>
    </div>
  );
}

/** Whether the transcript is in a state where "regenerate the last turn"
 * makes sense: there's a user message to re-run, and the turn isn't still
 * streaming (the caller also gates on that via `editingDisabled`). */
function canRetry(messages: ChatMessage[]): boolean {
  return messages.some((m) => m.role === "user");
}

export default function MessageList({
  sessionId,
  messages,
  onEditUserMessage,
  editingDisabled,
  onRetry,
  messageIndexOffset = 0,
  onStartSideTask,
  onEditGeneratedImage,
  onOpenBackgroundTasks,
}: MessageListProps) {
  const { t } = useT();
  const containerRef = useRef<HTMLDivElement>(null);
  const stickToBottomRef = useRef(true);

  useEffect(() => {
    const el = containerRef.current;
    if (el && stickToBottomRef.current) {
      el.scrollTop = el.scrollHeight;
    }
  }, [messages]);

  const handleScroll = () => {
    const el = containerRef.current;
    if (!el) return;
    const distanceFromBottom = el.scrollHeight - el.scrollTop - el.clientHeight;
    stickToBottomRef.current = distanceFromBottom < 96;
  };

  const showRetry = Boolean(onRetry) && !editingDisabled && canRetry(messages);
  const runningVerifyLabel = useSessionStore(selectRunningVerifyLabel(sessionId));
  const turnStatus = useTurnStatusStore(selectTurnStatus(sessionId));
  // While the status line is live it replaces the bouncing-dots bubble
  // entirely — the typing item only survives for MessageList hosts whose
  // turns never register with `turnStatusStore` (e.g. crew actor panes).
  const items = buildTimeline(messages, messageIndexOffset).filter((item) => !(turnStatus && item.kind === "typing"));
  const session = useSessionStore((state) => state.sessions.find((candidate) => candidate.id === sessionId));
  const messageTranslations = session?.messageTranslations ?? [];
  const preferredTranslationLocale = session?.displayTranslationLocale ?? null;

  return (
    <div
      ref={containerRef}
      onScroll={handleScroll}
      className="min-h-0 flex-1 overflow-y-auto bg-background px-4 py-6 [overscroll-behavior:contain]"
    >
      {items.length === 0 ? (
        <EmptyState />
      ) : (
        <div className="mx-auto flex max-w-3xl flex-col gap-6">
          {items.map((item) => {
            if (item.kind === "bubble") {
              const editable = item.message.role === "user" && onEditUserMessage;
              const bubble = (
                <MessageBubble
                  key={item.key}
                  message={item.message}
                  index={item.index}
                  sessionId={sessionId}
                  onEditMessage={editable ? onEditUserMessage : undefined}
                  editDisabled={editingDisabled}
                  translations={messageTranslations}
                  preferredTranslationLocale={preferredTranslationLocale}
                  onStartSideTask={onStartSideTask}
                />
              );
              // A message pinned as a chapter (see `MessageActions.tsx`)
              // carries its own divider, so a long transcript reads as
              // labelled sections instead of one undifferentiated scroll.
              if (item.message.chapter === undefined) return bubble;
              return (
                <div key={item.key} className="flex flex-col gap-6">
                  <div className="flex items-center gap-2 text-faint">
                    <span className="h-px flex-1 bg-border" />
                    <span className="flex items-center gap-1.5 text-[11px] font-medium uppercase tracking-wide">
                      <Pin size={11} />
                      {item.message.chapter}
                    </span>
                    <span className="h-px flex-1 bg-border" />
                  </div>
                  {bubble}
                </div>
              );
            }
            if (item.kind === "activity") {
              // An activity line belongs to the answer around it, so it sits
              // 8px from both neighbours rather than the list's own 24px.
              return (
                <div key={item.key} className="-mt-4 -mb-4 space-y-3">
                  <ActivityRow calls={item.calls} />
                  {item.calls.map((call) => {
                    if (call.name !== "generate_image") return null;
                    const path = argsImageFilename(call.args);
                    if (!path) return null;
                    return (
                      <GeneratedImageCard
                        key={`generated-image-${call.id}`}
                        path={path}
                        prompt={item.prompt}
                        result={call.result}
                        failed={call.result !== undefined && resultLooksLikeError(call.result)}
                        onEdit={onEditGeneratedImage}
                      />
                    );
                  })}
                </div>
              );
            }
            if (item.kind === "subagent") {
              return (
                <SubagentRow key={item.key} sessionId={sessionId} taskId={item.taskId} args={item.args} result={item.result} />
              );
            }
            if (item.kind === "workflow") {
              return (
                <WorkflowChatCard
                  key={item.key}
                  sessionId={sessionId}
                  runId={item.runId}
                  args={item.args}
                  result={item.result}
                  onOpenPanel={onOpenBackgroundTasks}
                />
              );
            }
            if (item.kind === "subagentGroup") {
              return <SubagentGroupCard key={item.key} sessionId={sessionId} tasks={item.tasks} onOpenPanel={onOpenBackgroundTasks} />;
            }
            if (item.kind === "notice") {
              return <NoticeRow key={item.key} text={item.text} />;
            }
            if (item.kind === "command") {
              return <CommandRow key={item.key} notice={item.notice} />;
            }
            if (item.kind === "checkpoint") {
              return (
                <CheckpointRow
                  key={item.key}
                  sessionId={sessionId}
                  notice={item.notice}
                  messageIndex={item.messageIndex}
                  anchorValid={checkpointAnchorValid(messages, item.notice)}
                />
              );
            }
            if (item.kind === "memory") {
              return (
                <MemoryRow key={item.key} sessionId={sessionId} notice={item.notice} messageIndex={item.messageIndex} />
              );
            }
            if (item.kind === "plan") {
              return (
                <PlanCard key={item.key} sessionId={sessionId} notice={item.notice} messageIndex={item.messageIndex} />
              );
            }
            if (item.kind === "verify") {
              return <VerifyRow key={item.key} notice={item.notice} />;
            }
            if (item.kind === "sources") {
              return <SourcesRow key={item.key} notice={item.notice} />;
            }
            if (item.kind === "recipe") {
              return <RecipeRow key={item.key} notice={item.notice} />;
            }
            return <TypingIndicator key={item.key} />;
          })}
          {runningVerifyLabel && <VerifyRunningRow label={runningVerifyLabel} />}
          {/* Hidden while the verify row shows — one live row at a time,
              and "thinking…" would be wrong during a verify command. */}
          {turnStatus && !runningVerifyLabel && <TurnStatusLine status={turnStatus} />}
          {showRetry && (
            <div className="-mt-4 flex justify-start">
              <button
                type="button"
                onClick={onRetry}
                className="flex cursor-pointer items-center gap-1.5 rounded-md px-2 py-1 text-xs text-faint transition-colors duration-150 hover:bg-surface-2 hover:text-foreground"
              >
                <RefreshCw size={12} />
                {t("MessageList.regenerateButton")}
              </button>
            </div>
          )}
        </div>
      )}
    </div>
  );
}
