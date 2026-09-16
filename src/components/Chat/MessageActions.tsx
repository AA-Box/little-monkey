import { useEffect, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
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
 *
 * `hint` adds a second line explaining what the action actually does — for
 * the controls whose consequence an icon and a two-word label can't carry
 * (interrupting a turn, editing a message and losing what followed it). The
 * bubble wraps to a fixed width once it has one; without a hint it stays the
 * single nowrap line it has always been.
 *
 * Exported because every icon-only control in a message's footer needs it,
 * not only the ones this file owns — `MessageBubble.tsx`'s translate, edit,
 * and side-task buttons sit in the same row and would otherwise be the only
 * ones falling back to `title`. Wrap the button in
 * `<span className="group/action relative">` for the hover/focus target.
 */
/** Distance between the trigger and the tooltip, matching the old `mb-1`. */
const TOOLTIP_GAP_PX = 4;
/** Below this much room above the trigger, the tooltip opens downwards instead.
 * Three lines of 11px text plus padding is about 60px; the extra covers the
 * tallest hint in the app without measuring, which would cost a second layout
 * pass on every hover. */
const TOOLTIP_FLIP_ABOVE_PX = 80;

/**
 * Where the tooltip's room runs out above: the top of the scrolling area its
 * trigger lives in, or the top of the window when there is none.
 *
 * The window is the wrong boundary. Above the message list sits a title bar in
 * normal flow, and a tooltip that only avoids running off the top of the screen
 * happily opens into it. Escaping the scrollport's *clipping* is what a portal
 * is for; staying inside the scrollport's *bounds* is a separate question, and
 * this is the answer to it.
 */
function scrollTop(from: HTMLElement): number {
  for (let node = from.parentElement; node; node = node.parentElement) {
    const overflowY = getComputedStyle(node).overflowY;
    if (overflowY === "auto" || overflowY === "scroll") return node.getBoundingClientRect().top;
  }
  return 0;
}

export function Tooltip({ text, hint }: { text: string; hint?: string }) {
  const anchor = useRef<HTMLSpanElement>(null);
  const [at, setAt] = useState<{ left: number; top: number; below: boolean } | null>(null);

  // The trigger is this span's parent — `<span className="group/action relative">`
  // around the button — so call sites keep passing nothing but their strings.
  useEffect(() => {
    const trigger = anchor.current?.parentElement;
    if (!trigger) return;
    const open = () => {
      const rect = trigger.getBoundingClientRect();
      const below = rect.top - scrollTop(trigger) < TOOLTIP_FLIP_ABOVE_PX;
      setAt({
        left: rect.left + rect.width / 2,
        top: below ? rect.bottom + TOOLTIP_GAP_PX : rect.top - TOOLTIP_GAP_PX,
        below,
      });
    };
    const close = () => setAt(null);
    trigger.addEventListener("mouseenter", open);
    trigger.addEventListener("mouseleave", close);
    trigger.addEventListener("focusin", open);
    trigger.addEventListener("focusout", close);
    // A tooltip pinned to viewport coordinates is wrong the moment the list
    // moves under it, and it cannot follow what it no longer overlaps.
    window.addEventListener("scroll", close, true);
    return () => {
      trigger.removeEventListener("mouseenter", open);
      trigger.removeEventListener("mouseleave", close);
      trigger.removeEventListener("focusin", open);
      trigger.removeEventListener("focusout", close);
      window.removeEventListener("scroll", close, true);
    };
  }, []);

  return (
    <>
      <span ref={anchor} className="hidden" aria-hidden="true" />
      {at !== null &&
        createPortal(
          <span
            role="tooltip"
            style={{ left: at.left, top: at.top }}
            className={`pointer-events-none fixed z-50 -translate-x-1/2 rounded-md border border-border bg-background px-2 py-1 text-[11px] text-foreground shadow-lg ${
              at.below ? "" : "-translate-y-full"
            } ${hint ? "w-max max-w-[15rem] text-left" : "whitespace-nowrap"}`}
          >
            {text}
            {/* `muted`, not `faint`: this is an 11px sentence someone has to
                read, not a de-emphasised label they can skim past. `faint` on
                the tooltip's background is 4.83:1 in light and 5.08:1 in dark —
                over the AA line for normal text and under it for text this
                small, which is exactly how it reads. `muted` is 7.73:1 and
                7.24:1. */}
            {hint && <span className="mt-0.5 block text-muted">{hint}</span>}
          </span>,
          document.body,
        )}
    </>
  );
}

function ActionButton({
  label,
  hint,
  pressed,
  onClick,
  children,
}: {
  /** Names the action — both the tooltip's text and the button's a11y name. */
  label: string;
  /** Optional sentence explaining the action's effect, shown under `label`. */
  hint?: string;
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
      <Tooltip text={label} hint={hint} />
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
          hint={t("MessageBubble.pinChapterHint")}
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
