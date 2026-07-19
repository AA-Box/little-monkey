import { useEffect, useRef, useState } from "react";
import type { FormEvent, KeyboardEvent } from "react";
import { CornerDownLeft, Square, Trash2, X } from "lucide-react";
import ReactMarkdown from "react-markdown";

import { markdownComponents, PROSE_CLASSES } from "./MessageBubble";
import { selectSessionMessages, useSessionStore } from "../../store/sessionStore";
import { selectSideChatOpen, useSideChatStore } from "../../store/sideChatStore";
import { isBtwNotice, parseBtwNotice, type BtwNotice } from "../../lib/slashCommands";
import { runSideQuestion, stopSideQuestion } from "../../lib/sideQuestion";
import { useT } from "../../lib/i18n";

/**
 * The floating `/btw` panel — a small always-on-top chat, separate from the
 * main transcript, for the quick asides `/btw` exists to support. Turns are
 * read straight out of the session's `[Btw]` notices (slashCommands.ts /
 * sideQuestion.ts) rather than a store of their own, so closing the panel
 * never loses history: reopening it (another `/btw`, or the panel itself
 * still mounted-but-hidden) just replays what's already in the transcript.
 */
export default function SideChatPanel({ sessionId }: { sessionId: string }) {
  const { t } = useT();
  const open = useSideChatStore(selectSideChatOpen(sessionId));
  const messages = useSessionStore(selectSessionMessages(sessionId));
  const [followUp, setFollowUp] = useState("");
  const [error, setError] = useState<string | null>(null);
  const bodyRef = useRef<HTMLDivElement>(null);

  const turns: BtwNotice[] = messages.filter(isBtwNotice).map((msg) => parseBtwNotice(msg)).filter((n): n is BtwNotice => n !== null);
  const running = turns.length > 0 && !turns[turns.length - 1].done;

  useEffect(() => {
    if (!open) return;
    bodyRef.current?.scrollTo({ top: bodyRef.current.scrollHeight });
  }, [open, messages]);

  if (!open) return null;

  const handleClose = () => useSideChatStore.getState().close(sessionId);

  const handleClear = () => {
    useSessionStore.getState().replaceMessages(sessionId, messages.filter((msg) => !isBtwNotice(msg)));
    setError(null);
  };

  const handleFollowUp = async (e: FormEvent) => {
    e.preventDefault();
    const question = followUp.trim();
    if (!question || running) return;
    setFollowUp("");
    setError(null);
    try {
      await runSideQuestion(sessionId, question);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  };

  const handleKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      void handleFollowUp(e as unknown as FormEvent);
    }
  };

  return (
    <div className="absolute bottom-full right-[max(1rem,calc((100%-48rem)/2))] z-40 mb-2 flex h-[22rem] max-h-[60vh] w-80 max-w-[calc(100%-2rem)] flex-col overflow-hidden rounded-2xl border border-border bg-background shadow-xl">
      <div className="flex shrink-0 items-center justify-between gap-2 px-4 py-3">
        <span className="text-base font-medium text-foreground">{t("SideChatPanel.title")}</span>
        <div className="flex items-center gap-1">
          <button
            type="button"
            onClick={handleClear}
            disabled={running || turns.length === 0}
            aria-label={t("SideChatPanel.clearAriaLabel")}
            title={t("SideChatPanel.clearAriaLabel")}
            className="flex h-6 w-6 shrink-0 cursor-pointer items-center justify-center rounded-md text-muted transition-colors hover:bg-surface-2 hover:text-foreground disabled:cursor-not-allowed disabled:opacity-40 disabled:hover:bg-transparent"
          >
            <Trash2 size={14} />
          </button>
          <button
            type="button"
            onClick={handleClose}
            aria-label={t("SideChatPanel.closeAriaLabel")}
            title={t("SideChatPanel.closeAriaLabel")}
            className="flex h-6 w-6 shrink-0 cursor-pointer items-center justify-center rounded-md text-muted transition-colors hover:bg-surface-2 hover:text-foreground"
          >
            <X size={14} />
          </button>
        </div>
      </div>

      <div ref={bodyRef} className="min-h-0 flex-1 overflow-y-auto px-4 py-2">
        {turns.length === 0 && <p className="py-6 text-center text-xs text-faint">{t("SideChatPanel.emptyState")}</p>}
        <div className="flex flex-col gap-3">
          {turns.map((turn, index) => (
            <div key={index} className="flex flex-col gap-1.5">
              <div className="flex justify-end">
                <div className="max-w-[85%] rounded-2xl bg-surface-2 px-4 py-2 text-[15px] text-foreground">{turn.question}</div>
              </div>
              {!turn.done ? (
                <span className="animate-pulse text-accent" aria-hidden>
                  ✳
                </span>
              ) : !turn.ok ? (
                <p className="whitespace-pre-wrap break-words text-sm text-danger">{turn.answer}</p>
              ) : turn.answer ? (
                <div className={`${PROSE_CLASSES} text-sm`}>
                  <ReactMarkdown components={markdownComponents}>{turn.answer}</ReactMarkdown>
                </div>
              ) : null}
            </div>
          ))}
        </div>
      </div>

      {error && <p className="shrink-0 px-4 pb-1 text-sm text-danger">{error}</p>}

      <form onSubmit={(e) => void handleFollowUp(e)} className="shrink-0 p-3">
        <div className="flex items-end gap-1 rounded-2xl bg-surface-2 px-3 py-2">
          <textarea
            value={followUp}
            onChange={(e) => setFollowUp(e.target.value)}
            onKeyDown={handleKeyDown}
            placeholder={t("SideChatPanel.followUpPlaceholder")}
            rows={1}
            className="max-h-32 min-h-[1.25rem] flex-1 resize-none bg-transparent text-sm text-foreground outline-none placeholder:text-faint"
          />
          <button
            type={running ? "button" : "submit"}
            onClick={running ? () => stopSideQuestion(sessionId) : undefined}
            disabled={!running && !followUp.trim()}
            aria-label={running ? t("ChatWindow.stopResponseAriaLabel") : t("ChatWindow.sendMessageAriaLabel")}
            className="flex h-6 w-6 shrink-0 cursor-pointer items-center justify-center rounded-full text-faint transition-colors hover:bg-background hover:text-foreground disabled:cursor-not-allowed disabled:opacity-40"
          >
            {running ? <Square size={12} className="fill-current" /> : <CornerDownLeft size={14} />}
          </button>
        </div>
      </form>
    </div>
  );
}
