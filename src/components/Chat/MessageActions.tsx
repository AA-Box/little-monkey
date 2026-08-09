import { useEffect, useState, type ReactNode } from "react";
import { Check, Copy, Pin, PinOff, Square, Volume2 } from "lucide-react";

import { useT } from "../../lib/i18n";
import { chapterTitle, formatMessageTime, formatMessageTimestamp } from "./messageChapters";

/** How often the relative timestamp re-derives itself, so an answer left on
 * screen doesn't keep claiming it arrived "just now". A minute is the
 * smallest unit the label distinguishes. */
const CLOCK_TICK_MS = 60_000;

const ACTION_CLASSES =
  "flex h-7 w-7 cursor-pointer items-center justify-center rounded-md text-faint transition-colors duration-150 hover:bg-surface-2 hover:text-foreground focus-visible:text-foreground";

/**
 * The label that names what an action does, shown above it on hover or
 * keyboard focus. CSS-only (a named `group/action` toggling `hidden`), the
 * same technique `EffortSelector.tsx` uses for its hover card — no state, no
 * timers, and no dependency on the browser's own slow, unstyled `title`
 * bubble, which these buttons deliberately no longer set (two tooltips for
 * one icon reads as a bug).
 */
function Tooltip({ text }: { text: string }) {
  return (
    <span
      role="tooltip"
      className="pointer-events-none absolute bottom-full left-1/2 z-30 mb-1 hidden -translate-x-1/2 whitespace-nowrap rounded-md border border-border bg-background px-2 py-1 text-[11px] text-foreground shadow-lg group-hover/action:block group-focus-within/action:block"
    >
      {text}
    </span>
  );
}

function ActionButton({
  label,
  pressed,
  onClick,
  children,
}: {
  /** Names the action — both the tooltip's text and the button's a11y name. */
  label: string;
  /** Toggle state for the actions that have one (pinned, speaking); omitted
   * for the plain ones, which then expose no `aria-pressed` at all. */
  pressed?: boolean;
  onClick: () => void;
  children: ReactNode;
}) {
  return (
    <span className="group/action relative">
      <button
        type="button"
        onClick={onClick}
        aria-label={label}
        aria-pressed={pressed}
        className={`${ACTION_CLASSES} ${pressed ? "text-accent hover:text-accent" : ""}`}
      >
        {children}
      </button>
      <Tooltip text={label} />
    </span>
  );
}

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
      <ActionButton
        label={copied ? t("MessageBubble.copiedLabel") : t("MessageBubble.copyMessage")}
        onClick={() => void handleCopy()}
      >
        {copied ? <Check size={13} /> : <Copy size={13} />}
      </ActionButton>

      {onToggleChapter && (
        <ActionButton
          label={pinLabel}
          pressed={pinned}
          onClick={() => onToggleChapter(pinned ? undefined : chapterTitle(text))}
        >
          {pinned ? <PinOff size={13} /> : <Pin size={13} />}
        </ActionButton>
      )}

      {speech && (
        <ActionButton label={speakLabel} pressed={speaking} onClick={handleSpeak}>
          {speaking ? <Square size={13} /> : <Volume2 size={13} />}
        </ActionButton>
      )}

      {at !== undefined && (
        <span className="group/action relative">
          <time dateTime={new Date(at).toISOString()} className="px-1 text-[11px] text-faint">
            {formatMessageTime(at, now)}
          </time>
          <Tooltip text={formatMessageTimestamp(at)} />
        </span>
      )}
    </>
  );
}
