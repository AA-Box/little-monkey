import { useEffect, useMemo, useRef, useState } from "react";
import { ChevronRight, Loader2, Square } from "lucide-react";
import ReactMarkdown from "react-markdown";

import { IconButton, StatusPill } from "../ui";
import { markdownComponents, PROSE_CLASSES } from "../Chat/MessageBubble";
import { ToolCallRow } from "../Chat/MessageList";
import type { SideTaskRecord } from "../../store/sideTaskStore";
import { cancelSideTask } from "../../lib/sideTaskRunner";
import type { ChatMessage } from "../../lib/llamaClient";
import { SideTaskDetail, sourceKindLabel, statusLabel, statusTone } from "./SideTaskDetail";

/**
 * One side task rendered as what it actually is: a conversation. The seed
 * instruction is the first user turn, and the task's own assistant replies
 * and tool calls follow. The composer that sends a follow-up into the SAME
 * run — the whole difference between this surface and the Background Tasks
 * panel, where there is nothing to talk to — is the PANE's single composer
 * (`SideTaskComposer.tsx`), pinned below every tab rather than rebuilt per
 * conversation, so switching tabs doesn't move the box the user is typing
 * into.
 *
 * The task's lifecycle controls (pause/resume/cancel/retry/promote/archive)
 * and its evidence stay in the collapsible Details section, reusing the exact
 * `SideTaskDetail` the Agent Inbox renders, so the two can't drift.
 */

type Row =
  | { kind: "user"; key: string; text: string }
  | { kind: "assistant"; key: string; text: string }
  | { kind: "tool"; key: string; name: string; args: string; result?: string };

/** Pairs `tool_calls` with their `tool` results and drops the raw tool
 * messages, so the transcript reads in the order things happened rather than
 * as a wire log. Same pairing `SubagentRow.extractChildToolCalls` does, but
 * kept inline here because this view needs the rows INTERLEAVED with the
 * assistant text instead of collected into one list. */
export function buildSideTaskRows(messages: ChatMessage[]): Row[] {
  const resultByCallId = new Map<string, string>();
  for (const message of messages) {
    if (message.role === "tool" && message.tool_call_id) {
      resultByCallId.set(message.tool_call_id, typeof message.content === "string" ? message.content : "");
    }
  }
  const rows: Row[] = [];
  messages.forEach((message, index) => {
    if (message.role === "system" || message.role === "tool") return;
    const text = typeof message.content === "string" ? message.content.trim() : "";
    if (message.role === "user") {
      if (text) rows.push({ kind: "user", key: `u-${index}`, text });
      return;
    }
    if (text) rows.push({ kind: "assistant", key: `a-${index}`, text });
    for (const toolCall of message.tool_calls ?? []) {
      rows.push({
        kind: "tool",
        key: `t-${toolCall.id}`,
        name: toolCall.function.name,
        args: toolCall.function.arguments,
        result: resultByCallId.get(toolCall.id),
      });
    }
  });
  return rows;
}

export function SideTaskConversation({ task }: { task: SideTaskRecord }) {
  const [detailsOpen, setDetailsOpen] = useState(false);
  const bodyRef = useRef<HTMLDivElement>(null);

  const active = task.status === "running" || task.status === "queued" || task.status === "paused";
  const rows = useMemo(() => buildSideTaskRows(task.messages), [task.messages]);

  // Follow the tail while the task is producing output, the same way the
  // main transcript does.
  useEffect(() => {
    bodyRef.current?.scrollTo({ top: bodyRef.current.scrollHeight });
  }, [task.messages.length, task.status]);

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="shrink-0 border-b border-border px-3 py-2">
        <div className="flex items-center gap-2">
          <span className="min-w-0 flex-1 truncate text-sm font-medium text-foreground">{task.title}</span>
          {active ? (
            <IconButton size="sm" aria-label={`Stop "${task.title}"`} onClick={() => cancelSideTask(task.id)}>
              <Square size={12} />
            </IconButton>
          ) : (
            <StatusPill tone={statusTone(task.status)}>{statusLabel(task.status)}</StatusPill>
          )}
        </div>
        <div className="mt-0.5 flex items-center gap-2 text-[11px] text-faint">
          <span>{sourceKindLabel(task.source.kind)}</span>
          <span>·</span>
          <span className="truncate">{task.modelLabel}</span>
          <span>·</span>
          <span>{task.profile === "code" ? "Can edit files" : "Read-only"}</span>
          <button
            type="button"
            onClick={() => setDetailsOpen((open) => !open)}
            className="ml-auto flex shrink-0 cursor-pointer items-center gap-0.5 text-accent hover:underline"
          >
            Details
            <ChevronRight size={11} className={`transition-transform duration-150 ${detailsOpen ? "rotate-90" : ""}`} />
          </button>
        </div>
      </div>

      {detailsOpen && (
        <div className="max-h-72 shrink-0 overflow-y-auto border-b border-border bg-surface-2">
          <SideTaskDetail task={task} />
        </div>
      )}

      <div ref={bodyRef} className="min-h-0 flex-1 overflow-y-auto p-3 [overscroll-behavior:contain]">
        <div className="flex flex-col gap-3">
          {rows.map((row) =>
            row.kind === "user" ? (
              <div key={row.key} className="self-end rounded-xl bg-surface-2 px-3 py-2 text-sm text-foreground">
                <p className="whitespace-pre-wrap">{row.text}</p>
              </div>
            ) : row.kind === "assistant" ? (
              <div key={row.key} className={PROSE_CLASSES}>
                <ReactMarkdown components={markdownComponents}>{row.text}</ReactMarkdown>
              </div>
            ) : (
              <ToolCallRow key={row.key} name={row.name} args={row.args} result={row.result} />
            ),
          )}
          {task.status === "running" && (
            <div className="flex items-center gap-1.5 text-xs text-faint">
              <Loader2 size={12} className="animate-spin text-warning" />
              Working…
            </div>
          )}
          {task.status === "paused" && <p className="text-xs text-faint">Paused — resume it from Details.</p>}
          {task.error && (
            <p className="whitespace-pre-wrap rounded-md border border-danger bg-danger-soft p-2 text-xs text-danger">{task.error}</p>
          )}
        </div>
      </div>

    </div>
  );
}

export default SideTaskConversation;
