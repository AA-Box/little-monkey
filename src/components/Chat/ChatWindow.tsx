import { useCallback, useEffect, useRef, useState } from "react";
import type { FormEvent, KeyboardEvent } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { CornerDownLeft, Square } from "lucide-react";

import { runAgentTurn, stopTurn } from "../../lib/agentLoop";
import type { AttachmentRef } from "../../lib/agentLoop";
import { isImagePath, readImageAsDataUrl } from "../../lib/imageAttachment";
import { textContent } from "../../lib/llamaClient";
import { selectSessionMessages, selectTurnRunning, sessionMessages, useSessionStore } from "../../store/sessionStore";
import { useWorkspaceStore } from "../../store/workspaceStore";
import { usePromptStore } from "../../store/promptStore";
import { useShortcutStore } from "../../store/shortcutStore";
import type { PromptEntry } from "../../store/promptStore";
import MessageList from "./MessageList";
import { MentionAutocomplete } from "./MentionAutocomplete";
import type { MentionEntry } from "./MentionAutocomplete";
import { SlashCommandAutocomplete } from "./SlashCommandAutocomplete";
import { ModeSelector } from "./ModeSelector";
import { EffortSelector } from "./EffortSelector";
import { PersonaSelector } from "./PersonaSelector";
import { StackPicker } from "./StackPicker";
import { ModelSwitcher } from "./ModelSwitcher";
import { ContextUsageIndicator } from "./ContextUsageIndicator";
import { CheckpointTimeline } from "./CheckpointTimeline";
import { AttachMenu } from "./AttachMenu";
import { AttachmentChip } from "./AttachmentChip";
import { WorkspaceBar } from "../Workspace";
import { useT } from "../../lib/i18n";
import { shortcutIdForEvent, usesMacShortcuts } from "../../lib/shortcuts";

const MAX_TEXTAREA_HEIGHT_PX = 192;

/** Shape returned by the Rust `list_workspace_paths` command. */
interface WorkspacePathsResult {
  entries: MentionEntry[];
  truncated: boolean;
}

/** Cap on how many filtered rows are handed to <MentionAutocomplete>. */
const MAX_MENTION_RESULTS = 50;

/**
 * Looks backward from `cursor` in `text` for an active "@"-mention trigger:
 * the nearest "@" that is either at the start of the text or preceded by
 * whitespace, with no whitespace between that "@" and the cursor. Returns
 * the trigger's start index (the position of "@" itself) and the query text
 * typed after it, or `null` if the cursor isn't inside a mention trigger.
 */
function findMentionRange(text: string, cursor: number): { start: number; query: string } | null {
  const upToCursor = text.slice(0, cursor);
  const at = upToCursor.lastIndexOf("@");
  if (at === -1) return null;

  const query = upToCursor.slice(at + 1);
  if (/\s/.test(query)) return null; // whitespace between "@" and cursor — not an active trigger

  const before = at === 0 ? "" : upToCursor[at - 1];
  if (at !== 0 && !/\s/.test(before)) return null; // "@" isn't at start-of-text or after whitespace

  return { start: at, query };
}

/**
 * Filters/ranks the cached workspace path list against `query`: a
 * case-insensitive substring match on the full path, with basename-starts-
 * with ranked above basename-contains, ranked above full-path-contains, then
 * capped to MAX_MENTION_RESULTS.
 */
function filterMentionEntries(all: MentionEntry[], query: string): MentionEntry[] {
  const needle = query.toLowerCase();

  const ranked: { entry: MentionEntry; rank: number }[] = [];
  for (const entry of all) {
    const path = entry.path.toLowerCase();
    const lastSlash = path.lastIndexOf("/");
    const basename = lastSlash >= 0 ? path.slice(lastSlash + 1) : path;

    let rank: number;
    if (needle === "" || basename.startsWith(needle)) {
      rank = 0;
    } else if (basename.includes(needle)) {
      rank = 1;
    } else if (path.includes(needle)) {
      rank = 2;
    } else {
      continue;
    }
    ranked.push({ entry, rank });
  }

  ranked.sort((a, b) => a.rank - b.rank || a.entry.path.length - b.entry.path.length);
  return ranked.slice(0, MAX_MENTION_RESULTS).map((r) => r.entry);
}

/**
 * Looks for an active "/"-command trigger: unlike `findMentionRange`, this
 * only fires when "/" is the FIRST non-whitespace character of the whole
 * input and the cursor hasn't moved past that leading token — "/" mid-text
 * is almost always a path (e.g. "src/lib"), so no popup there. Returns the
 * trigger's start index (the position of "/" itself) and the query text
 * typed after it, or `null` if the cursor isn't inside an active trigger.
 */
function findSlashRange(text: string, cursor: number): { start: number; query: string } | null {
  const start = text.search(/\S/);
  if (start === -1 || text[start] !== "/") return null;
  if (cursor <= start) return null; // cursor is at or before the "/" itself — not an active trigger yet

  const query = text.slice(start + 1, cursor);
  if (/\s/.test(query)) return null; // whitespace between "/" and cursor — the leading token ended

  return { start, query };
}

/**
 * Filters/ranks prompt-library entries against `query`: command-starts-with
 * ranked above name-starts-with, ranked above either containing the query —
 * fully client-side against `promptStore` entries, unlike mentions there's
 * no Rust round trip since the whole library is already in memory.
 */
function filterSlashEntries(all: PromptEntry[], query: string): PromptEntry[] {
  const needle = query.toLowerCase();

  const ranked: { entry: PromptEntry; rank: number }[] = [];
  for (const entry of all) {
    const command = entry.command.toLowerCase();
    const name = entry.name.toLowerCase();

    let rank: number;
    if (needle === "" || command.startsWith(needle)) {
      rank = 0;
    } else if (name.startsWith(needle)) {
      rank = 1;
    } else if (command.includes(needle) || name.includes(needle)) {
      rank = 2;
    } else {
      continue;
    }
    ranked.push({ entry, rank });
  }

  ranked.sort((a, b) => a.rank - b.rank || a.entry.command.length - b.entry.command.length);
  return ranked.map((r) => r.entry);
}

interface ChatWindowProps {
  /** The session this pane renders and sends turns into. Each pane (primary
   * and split — see App.tsx) owns one; they operate independently. */
  sessionId: string;
  /** Opens Settings on the Prompts tab — passed down to `PersonaSelector`'s
   * "Manage prompts…" row (see App.tsx's deep-link hook). */
  onManagePrompts: () => void;
}

export default function ChatWindow({ sessionId, onManagePrompts }: ChatWindowProps) {
  const messages = useSessionStore(selectSessionMessages(sessionId));
  const persistError = useSessionStore((state) => state.persistError);
  const roots = useWorkspaceStore((state) => state.roots);
  const { t } = useT();

  const [input, setInput] = useState("");
  // Whether THIS pane's session has a turn in flight — session-scoped store
  // state, not component state: the turn survives pane switches (keeping
  // its Stop affordance wherever its session is shown), and a session
  // already running a turn stays locked in whichever pane displays it.
  const sending = useSessionStore(selectTurnRunning(sessionId));
  const [error, setError] = useState<string | null>(null);
  const [attachments, setAttachments] = useState<AttachmentRef[]>([]);
  const textareaRef = useRef<HTMLTextAreaElement>(null);

  // "@"-mention autocomplete state. `mentionQuery` being non-null is what
  // controls whether the popup is rendered at all.
  const [mentionQuery, setMentionQuery] = useState<string | null>(null);
  const [mentionEntries, setMentionEntries] = useState<MentionEntry[]>([]);
  const [mentionActiveIndex, setMentionActiveIndex] = useState(0);

  // Index of the "@" that opened the current mention trigger, so selection
  // knows exactly what span of the textarea to replace.
  const mentionStartRef = useRef<number | null>(null);
  // Cache of the full workspace path list, fetched at most once per mount
  // (or re-fetched after a prior failure, e.g. no workspace was open yet).
  const workspacePathsRef = useRef<MentionEntry[] | null>(null);
  const workspacePathsPromiseRef = useRef<Promise<MentionEntry[]> | null>(null);
  // Guards against a slow/late `list_workspace_paths` response clobbering
  // the results of a more recent keystroke.
  const mentionRequestIdRef = useRef(0);

  // Invalidate the cached mention path list whenever the attached folders
  // change (primary swapped, or a secondary attached/removed) — otherwise a
  // newly attached folder's files would never show up in "@"-mentions until
  // ChatWindow happens to remount.
  const rootsKey = roots.map((r) => r.id).join("|");
  useEffect(() => {
    workspacePathsRef.current = null;
    workspacePathsPromiseRef.current = null;
  }, [rootsKey]);

  // "/"-command autocomplete state — mirrors the "@"-mention state above.
  // `slashQuery` being non-null is what controls whether the popup renders.
  const promptEntries = usePromptStore((state) => state.entries);
  const [slashQuery, setSlashQuery] = useState<string | null>(null);
  const [slashEntries, setSlashEntries] = useState<PromptEntry[]>([]);
  const [slashActiveIndex, setSlashActiveIndex] = useState(0);
  // Index of the "/" that opened the current slash trigger, so selection
  // knows exactly what span of the textarea to replace.
  const slashStartRef = useRef<number | null>(null);

  const resizeTextarea = useCallback(() => {
    const el = textareaRef.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, MAX_TEXTAREA_HEIGHT_PX)}px`;
  }, []);

  // Composer state (draft text, attachments, error banner, @/ popups) is
  // pane-local, not session-local — nothing else keys it to `sessionId`. A
  // brand new `sessionId` (switching panes, or `newSession` handing this
  // pane a freshly reset session) means a blank compose slate.
  useEffect(() => {
    setInput("");
    setError(null);
    setAttachments([]);
    setMentionQuery(null);
    mentionStartRef.current = null;
    setSlashQuery(null);
    slashStartRef.current = null;
    requestAnimationFrame(resizeTextarea);
  }, [sessionId, resizeTextarea]);

  const loadWorkspacePaths = useCallback((): Promise<MentionEntry[]> => {
    if (workspacePathsRef.current) return Promise.resolve(workspacePathsRef.current);
    if (!workspacePathsPromiseRef.current) {
      workspacePathsPromiseRef.current = invoke<WorkspacePathsResult>("list_workspace_paths")
        .then((result) => {
          workspacePathsRef.current = result.entries;
          return result.entries;
        })
        .catch(() => {
          // No workspace open (or some other failure) — never let this throw
          // into the chat's error banner. Reset so a later "@" can retry,
          // e.g. once a workspace has been opened.
          workspacePathsPromiseRef.current = null;
          return [];
        });
    }
    return workspacePathsPromiseRef.current;
  }, []);

  const handleAddFiles = useCallback(async () => {
    try {
      const selected = await open({ multiple: true, directory: false });
      if (!selected) return; // user cancelled
      const paths = Array.isArray(selected) ? selected : [selected];
      if (paths.length === 0) return;
      setAttachments((prev) => {
        const existing = new Set(prev.map((a) => a.path));
        const additions = paths.filter((path) => !existing.has(path)).map((path) => ({ path, isDir: false }));
        return additions.length > 0 ? [...prev, ...additions] : prev;
      });

      // Image files get their bytes read + base64-encoded once, right now,
      // rather than on send — the resulting data URL doubles as both the
      // chip's thumbnail preview and the content actually wired to the
      // model later (see `agentLoop.ts`'s `resolveReferences`), so the file
      // is never read twice. Chips still appear immediately above; each
      // image one just upgrades in place once its read finishes.
      for (const path of paths) {
        if (!isImagePath(path)) continue;
        try {
          const dataUrl = await readImageAsDataUrl(path);
          setAttachments((prev) => prev.map((a) => (a.path === path ? { ...a, kind: "image", dataUrl } : a)));
        } catch (err) {
          console.error(`Failed to read image "${path}"`, err);
        }
      }
    } catch (err) {
      console.error("Failed to open file picker", err);
    }
  }, []);

  const handleAddFolder = useCallback(async () => {
    try {
      const selected = await open({ multiple: true, directory: true });
      if (!selected) return; // user cancelled
      const paths = Array.isArray(selected) ? selected : [selected];
      if (paths.length === 0) return;
      setAttachments((prev) => {
        const existing = new Set(prev.map((a) => a.path));
        const additions = paths.filter((path) => !existing.has(path)).map((path) => ({ path, isDir: true }));
        return additions.length > 0 ? [...prev, ...additions] : prev;
      });
    } catch (err) {
      console.error("Failed to open folder picker", err);
    }
  }, []);

  const handleRemoveAttachment = useCallback((path: string) => {
    setAttachments((prev) => prev.filter((a) => a.path !== path));
  }, []);

  const sendTurn = useCallback((text: string, pendingAttachments: AttachmentRef[]) => {
    setError(null);

    // The agent loop owns the turn's abort handle (keyed by session — see
    // stopTurn) and flips the per-session running flag `sending` above
    // subscribes to, so nothing pane-local tracks the turn.
    void runAgentTurn(sessionId, text, pendingAttachments)
      .catch((err: unknown) => {
        setError(err instanceof Error ? err.message : String(err));
      })
      .finally(() => {
        textareaRef.current?.focus();
      });
  }, [sessionId]);

  const handleSend = useCallback(() => {
    const text = input.trim();
    if (!text || sending) return;

    const pendingAttachments = attachments;
    setInput("");
    setAttachments([]);
    requestAnimationFrame(resizeTextarea);
    sendTurn(text, pendingAttachments);
  }, [input, sending, attachments, resizeTextarea, sendTurn]);

  const handleStop = useCallback(() => {
    stopTurn(sessionId);
  }, [sessionId]);

  const handleEditMessage = useCallback(
    (index: number, newText: string) => {
      if (sending) return;
      useSessionStore.getState().truncateFromIndex(sessionId, index);
      sendTurn(newText, []);
    },
    [sending, sendTurn, sessionId]
  );

  // Regenerate the last turn: drop everything from the last user message
  // onward (its whole downstream reply included) and resubmit that message —
  // the same mechanics as editing a past message, just without changing the
  // text. Image attachments are rebuilt from the stored message's content
  // parts (they carry the already-encoded data URL), so a retried turn keeps
  // its images.
  const handleRetry = useCallback(() => {
    if (sending) return;
    const currentMessages = sessionMessages(sessionId);
    let lastUserIndex = -1;
    for (let i = currentMessages.length - 1; i >= 0; i--) {
      if (currentMessages[i].role === "user") {
        lastUserIndex = i;
        break;
      }
    }
    if (lastUserIndex === -1) return;

    const userMessage = currentMessages[lastUserIndex];
    const text = textContent(userMessage.content);
    const imageAttachments: AttachmentRef[] =
      typeof userMessage.content === "string"
        ? []
        : userMessage.content
            .filter((part) => part.type === "image_url")
            .map((part, i) => ({ path: `retried-image-${i}`, isDir: false, kind: "image" as const, dataUrl: part.image_url.url }));
    if (!text.trim() && imageAttachments.length === 0) return;

    useSessionStore.getState().truncateFromIndex(sessionId, lastUserIndex);
    sendTurn(text, imageAttachments);
  }, [sending, sendTurn, sessionId]);

  const closeMentionPopup = useCallback(() => {
    setMentionQuery(null);
    mentionStartRef.current = null;
  }, []);

  const selectMentionEntry = useCallback(
    (entry: MentionEntry) => {
      const start = mentionStartRef.current;
      const el = textareaRef.current;
      if (start === null) {
        closeMentionPopup();
        return;
      }

      const currentValue = el ? el.value : input;
      const end = el?.selectionStart ?? currentValue.length;
      const insertion = `@${entry.path} `;
      const nextValue = currentValue.slice(0, start) + insertion + currentValue.slice(end);
      const cursorPos = start + insertion.length;

      setInput(nextValue);
      closeMentionPopup();

      requestAnimationFrame(() => {
        const node = textareaRef.current;
        if (node) {
          node.focus();
          node.setSelectionRange(cursorPos, cursorPos);
        }
        resizeTextarea();
      });
    },
    [input, closeMentionPopup, resizeTextarea]
  );

  const closeSlashPopup = useCallback(() => {
    setSlashQuery(null);
    slashStartRef.current = null;
  }, []);

  const selectSlashEntry = useCallback(
    (entry: PromptEntry) => {
      const start = slashStartRef.current;
      const el = textareaRef.current;
      if (start === null) {
        closeSlashPopup();
        return;
      }

      const currentValue = el ? el.value : input;
      const end = el?.selectionStart ?? currentValue.length;

      // Picking a persona sets it as this session's active persona (composed
      // into the system prompt every turn — see systemPrompt.ts) and clears
      // the "/command" the user typed, the Cherry-Studio-style "switch
      // assistant from the composer" gesture; it never inserts text into the
      // composer the way a snippet does.
      if (entry.kind === "persona") {
        useSessionStore.getState().setSessionPersona(sessionId, entry.id);
        const nextValue = currentValue.slice(0, start) + currentValue.slice(end);
        setInput(nextValue);
        closeSlashPopup();

        requestAnimationFrame(() => {
          const node = textareaRef.current;
          if (node) {
            node.focus();
            node.setSelectionRange(start, start);
          }
          resizeTextarea();
        });
        return;
      }

      const insertion = entry.content;
      const nextValue = currentValue.slice(0, start) + insertion + currentValue.slice(end);
      const cursorPos = start + insertion.length;

      setInput(nextValue);
      closeSlashPopup();

      requestAnimationFrame(() => {
        const node = textareaRef.current;
        if (node) {
          node.focus();
          node.setSelectionRange(cursorPos, cursorPos);
        }
        resizeTextarea();
      });
    },
    [input, closeSlashPopup, resizeTextarea, sessionId]
  );

  const handleInput = (event: FormEvent<HTMLTextAreaElement>) => {
    const value = event.currentTarget.value;
    const cursor = event.currentTarget.selectionStart;
    setInput(value);
    resizeTextarea();

    // Mention and slash triggers are mutually exclusive: one anchors at an
    // "@" that can appear anywhere, the other only at index 0 — but both
    // popups are still separate pieces of state, so an inactive trigger's
    // popup must be explicitly closed rather than left stale.
    const mentionRange = findMentionRange(value, cursor);
    if (mentionRange) {
      if (slashQuery !== null) closeSlashPopup();

      mentionStartRef.current = mentionRange.start;
      setMentionQuery(mentionRange.query);
      setMentionActiveIndex(0);

      const requestId = ++mentionRequestIdRef.current;
      void loadWorkspacePaths().then((all) => {
        if (mentionRequestIdRef.current !== requestId) return; // a newer keystroke superseded this fetch
        setMentionEntries(filterMentionEntries(all, mentionRange.query));
      });
      return;
    }
    if (mentionQuery !== null) closeMentionPopup();

    const slashRange = findSlashRange(value, cursor);
    if (slashRange) {
      slashStartRef.current = slashRange.start;
      setSlashQuery(slashRange.query);
      setSlashActiveIndex(0);
      setSlashEntries(filterSlashEntries(promptEntries, slashRange.query));
      return;
    }
    if (slashQuery !== null) closeSlashPopup();
  };

  const handleKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    // Enter confirms an active IME composition on CJK and other input
    // methods; it must not choose a suggestion or send the message too.
    if (event.nativeEvent.isComposing) return;
    // Resolve from the store on every event so an edit in Settings is live
    // without remounting the composer (including in a secondary window).
    const { overrides } = useShortcutStore.getState();
    const isMac = usesMacShortcuts();
    const suggestionShortcut = shortcutIdForEvent(event, "suggestions", isMac, overrides);
    if (mentionQuery !== null) {
      if (suggestionShortcut === "nextSuggestion") {
        event.preventDefault();
        setMentionActiveIndex((prev) => (mentionEntries.length === 0 ? 0 : (prev + 1) % mentionEntries.length));
        return;
      }
      if (suggestionShortcut === "previousSuggestion") {
        event.preventDefault();
        setMentionActiveIndex((prev) =>
          mentionEntries.length === 0 ? 0 : (prev - 1 + mentionEntries.length) % mentionEntries.length
        );
        return;
      }
      if (suggestionShortcut === "chooseSuggestion") {
        event.preventDefault();
        const entry = mentionEntries[mentionActiveIndex];
        if (entry) {
          selectMentionEntry(entry);
        } else {
          closeMentionPopup();
        }
        return;
      }
      if (suggestionShortcut === "closeSuggestions") {
        event.preventDefault();
        closeMentionPopup();
        return;
      }
    }

    if (slashQuery !== null) {
      if (suggestionShortcut === "nextSuggestion") {
        event.preventDefault();
        setSlashActiveIndex((prev) => (slashEntries.length === 0 ? 0 : (prev + 1) % slashEntries.length));
        return;
      }
      if (suggestionShortcut === "previousSuggestion") {
        event.preventDefault();
        setSlashActiveIndex((prev) => (slashEntries.length === 0 ? 0 : (prev - 1 + slashEntries.length) % slashEntries.length));
        return;
      }
      if (suggestionShortcut === "chooseSuggestion") {
        event.preventDefault();
        const entry = slashEntries[slashActiveIndex];
        if (entry) {
          selectSlashEntry(entry);
        } else {
          closeSlashPopup();
        }
        return;
      }
      if (suggestionShortcut === "closeSuggestions") {
        event.preventDefault();
        closeSlashPopup();
        return;
      }
    }

    const composerShortcut = shortcutIdForEvent(event, "composer", isMac, overrides);
    if (composerShortcut === "sendMessage") {
      event.preventDefault();
      handleSend();
      return;
    }

    if (composerShortcut === "insertLineBreak") {
      event.preventDefault();

      const textarea = event.currentTarget;
      const currentValue = textarea.value;
      const selectionStart = textarea.selectionStart ?? currentValue.length;
      const selectionEnd = textarea.selectionEnd ?? selectionStart;
      const nextValue =
        currentValue.slice(0, selectionStart) + "\n" + currentValue.slice(selectionEnd);
      const nextCaret = selectionStart + 1;

      setInput(nextValue);
      closeMentionPopup();
      closeSlashPopup();
      requestAnimationFrame(() => {
        const node = textareaRef.current;
        if (!node) return;
        node.focus();
        node.setSelectionRange(nextCaret, nextCaret);
        resizeTextarea();
      });
      return;
    }

    // Once Enter/Shift+Enter is reassigned, do not let the textarea's native
    // newline behavior keep the old binding alive behind the registry's back.
    if (event.key === "Enter") event.preventDefault();
  };

  return (
    <div className="flex h-full min-h-0 min-w-0 flex-1 flex-col bg-background">
      <MessageList
        sessionId={sessionId}
        messages={messages}
        onEditUserMessage={handleEditMessage}
        editingDisabled={sending}
        onRetry={handleRetry}
      />

      {error && (
        <div className="mx-4 mb-2">
          <div className="mx-auto flex max-w-3xl items-center justify-between gap-3 rounded-lg border border-danger bg-danger-soft px-3 py-2 text-sm text-danger">
            <span className="min-w-0 break-words">{error}</span>
            <button
              type="button"
              onClick={handleRetry}
              disabled={sending}
              className="shrink-0 cursor-pointer rounded-md border border-danger px-2 py-0.5 text-xs transition-colors hover:bg-danger hover:text-danger-foreground disabled:cursor-not-allowed disabled:opacity-50"
            >
              {t("ChatWindow.retryButton")}
            </button>
          </div>
        </div>
      )}

      {persistError && (
        <div className="mx-4 mb-2">
          <div className="mx-auto max-w-3xl rounded-lg border border-warning bg-warning-soft px-3 py-2 text-sm text-warning">
            {t("ChatWindow.persistErrorBanner", { error: persistError })}
          </div>
        </div>
      )}

      <div className="shrink-0 border-t border-border bg-background px-4 py-3">
        <WorkspaceBar sessionId={sessionId} />
        <div className="relative mx-auto max-w-3xl">
          {mentionQuery !== null && (
            <MentionAutocomplete
              query={mentionQuery}
              entries={mentionEntries}
              activeIndex={mentionActiveIndex}
              onSelect={selectMentionEntry}
              onHoverIndex={setMentionActiveIndex}
            />
          )}
          {slashQuery !== null && (
            <SlashCommandAutocomplete
              query={slashQuery}
              entries={slashEntries}
              activeIndex={slashActiveIndex}
              onSelect={selectSlashEntry}
              onHoverIndex={setSlashActiveIndex}
            />
          )}
          <div className="flex flex-col rounded-3xl border border-border bg-surface px-4 py-2.5 transition-colors focus-within:border-accent focus-within:ring-1 focus-within:ring-accent">
            {attachments.length > 0 && (
              <div className="mb-1.5 flex flex-wrap gap-1.5">
                {attachments.map((attachment) => {
                  const segments = attachment.path.split(/[\\/]/).filter(Boolean);
                  const name = segments[segments.length - 1] ?? attachment.path;
                  return (
                    <AttachmentChip
                      key={attachment.path}
                      name={name}
                      isDir={attachment.isDir}
                      previewUrl={attachment.kind === "image" ? attachment.dataUrl : undefined}
                      onRemove={() => handleRemoveAttachment(attachment.path)}
                    />
                  );
                })}
              </div>
            )}
            <div className="flex items-end gap-2">
              <textarea
                ref={textareaRef}
                value={input}
                onChange={handleInput}
                onKeyDown={handleKeyDown}
                placeholder={t("ChatWindow.inputPlaceholder")}
                rows={1}
                className="max-h-48 min-h-[2.25rem] flex-1 resize-none bg-transparent py-1.5 text-[15px] leading-relaxed text-foreground outline-none placeholder:text-faint"
              />
              <button
                type="button"
                onClick={sending ? handleStop : handleSend}
                disabled={!sending && !input.trim()}
                aria-label={sending ? t("ChatWindow.stopResponseAriaLabel") : t("ChatWindow.sendMessageAriaLabel")}
                className="flex h-7 w-7 shrink-0 cursor-pointer items-center justify-center rounded-full text-faint transition-colors duration-150 hover:bg-surface-2 hover:text-foreground disabled:cursor-not-allowed disabled:opacity-40 disabled:hover:bg-transparent focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2 focus-visible:ring-offset-background"
              >
                {sending ? <Square size={13} className="fill-current" /> : <CornerDownLeft size={16} />}
              </button>
            </div>
          </div>
        </div>
        <div className="mx-auto mt-1.5 flex max-w-3xl items-center justify-between">
          <div className="flex items-center gap-1.5">
            <ModeSelector />
            <PersonaSelector sessionId={sessionId} onManagePrompts={onManagePrompts} />
            <AttachMenu onAddFiles={() => void handleAddFiles()} onAddFolder={() => void handleAddFolder()} />
          </div>
          <div className="flex items-center gap-3">
            <ModelSwitcher />
            <StackPicker sessionId={sessionId} />
            <EffortSelector />
            <CheckpointTimeline sessionId={sessionId} />
            <ContextUsageIndicator sessionId={sessionId} />
          </div>
        </div>
      </div>
    </div>
  );
}
