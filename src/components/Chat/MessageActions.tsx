import { useEffect, useState } from "react";
import { Bookmark, BookmarkCheck, Check, Copy, Square, Volume2 } from "lucide-react";

import { useT } from "../../lib/i18n";
import { chapterTitle, formatMessageTime, formatMessageTimestamp } from "./messageChapters";

/** How often the relative timestamp re-derives itself, so an answer left on
 * screen doesn't keep claiming it arrived "just now". A minute is the
 * smallest unit the label distinguishes. */
const CLOCK_TICK_MS = 60_000;

const ACTION_CLASSES =
  "flex h-7 w-7 cursor-pointer items-center justify-center rounded-md text-faint transition-colors duration-150 hover:bg-surface-2 hover:text-foreground focus-visible:text-foreground";

/** Whether this WebView exposes the Web Speech API. Checked once at module
 * load — the capability never appears mid-session, and a build target
 * without it should simply not render the button. */
const speech = typeof window !== "undefined" && "speechSynthesis" in window ? window.speechSynthesis : null;

/**
 * The row of actions under a finished assistant answer: copy its text, pin
 * it as a chapter of the conversation, have it read aloud, and see when it
 * arrived. Rendered by `MessageBubble`'s `AssistantMessage` alongside the
 * translation and side-task controls, which share the same hover-reveal.
 */
export default function MessageActions({
  text,
  at,
  chapter,
  onToggleChapter,
}: {
  /** The answer's plain text — what gets copied and spoken. */
  text: string;
  /** When the message entered the transcript; absent for messages that
   * predate timestamping, which simply show no time. */
  at?: number;
  /** This message's chapter title, or undefined when it isn't pinned. */
  chapter?: string;
  /** Pins the message under `title`, or unpins it when passed undefined. */
  onToggleChapter?: (title: string | undefined) => void;
}) {
  const { t } = useT();
  const [copied, setCopied] = useState(false);
  const [speaking, setSpeaking] = useState(false);
  const [now, setNow] = useState(() => Date.now());

  useEffect(() => {
    if (at === undefined) return;
    const timer = setInterval(() => setNow(Date.now()), CLOCK_TICK_MS);
    return () => clearInterval(timer);
  }, [at]);

  // Speech outlives this component (unmounting a bubble — a session switch,
  // a re-render past the virtualization window — doesn't stop the browser
  // from talking), so cancel on the way out.
  useEffect(() => () => {
    if (speech && speaking) speech.cancel();
  }, [speaking]);

  const handleCopy = async () => {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      // Clipboard permission denied/unavailable — the same silent no-op
      // `CodeBlock` and `ToolStepRow` take; nothing destructive to fall
      // back to.
    }
  };

  const handleSpeak = () => {
    if (!speech) return;
    // One utterance at a time across the whole transcript: `cancel()` first
    // so starting a second answer replaces the first rather than queueing
    // behind it.
    speech.cancel();
    if (speaking) {
      setSpeaking(false);
      return;
    }
    const utterance = new SpeechSynthesisUtterance(text);
    utterance.onend = () => setSpeaking(false);
    utterance.onerror = () => setSpeaking(false);
    setSpeaking(true);
    speech.speak(utterance);
  };

  const pinned = chapter !== undefined;
  const pinLabel = pinned ? t("MessageBubble.unpinChapter") : t("MessageBubble.pinChapter");
  const speakLabel = speaking ? t("MessageBubble.stopReading") : t("MessageBubble.readAloud");

  return (
    <>
      <button
        type="button"
        onClick={() => void handleCopy()}
        aria-label={copied ? t("MessageBubble.copiedLabel") : t("MessageBubble.copyMessage")}
        title={copied ? t("MessageBubble.copiedLabel") : t("MessageBubble.copyMessage")}
        className={ACTION_CLASSES}
      >
        {copied ? <Check size={13} /> : <Copy size={13} />}
      </button>

      {onToggleChapter && (
        <button
          type="button"
          onClick={() => onToggleChapter(pinned ? undefined : chapterTitle(text))}
          aria-label={pinLabel}
          aria-pressed={pinned}
          title={pinLabel}
          className={`${ACTION_CLASSES} ${pinned ? "text-accent hover:text-accent" : ""}`}
        >
          {pinned ? <BookmarkCheck size={13} /> : <Bookmark size={13} />}
        </button>
      )}

      {speech && (
        <button
          type="button"
          onClick={handleSpeak}
          aria-label={speakLabel}
          aria-pressed={speaking}
          title={speakLabel}
          className={`${ACTION_CLASSES} ${speaking ? "text-accent hover:text-accent" : ""}`}
        >
          {speaking ? <Square size={13} /> : <Volume2 size={13} />}
        </button>
      )}

      {at !== undefined && (
        <time
          dateTime={new Date(at).toISOString()}
          title={formatMessageTimestamp(at)}
          className="px-1 text-[11px] text-faint"
        >
          {formatMessageTime(at, now)}
        </time>
      )}
    </>
  );
}
