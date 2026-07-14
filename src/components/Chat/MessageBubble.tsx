import { Children, isValidElement, memo, useEffect, useState, type ReactNode } from "react";
import ReactMarkdown from "react-markdown";
import type { Components } from "react-markdown";
import { Eye, Languages, LoaderCircle, Pencil, X } from "lucide-react";

import { textContent, type ChatContentPart, type ChatMessage } from "../../lib/llamaClient";
import { detectFenceKind, fingerprintArtifact, type ArtifactRef } from "../../lib/artifacts";
import { useArtifactStore } from "../../store/artifactStore";
import { useT } from "../../lib/i18n";
import {
  cancelTranslation,
  defaultTranslationLocale,
  messageTranslationKey,
  TRANSLATION_LOCALES,
  translateMessage,
} from "../../lib/translation";
import type { MessageTranslation } from "../../store/sessionStore";

export interface MessageBubbleProps {
  message: ChatMessage;
  /** This message's index in the transcript, passed back to `onEditMessage`
   * and used to build an assistant message's fenced-code-block `ArtifactRef`s
   * (see `buildAssistantMarkdownComponents` below) — must be the message's
   * real position in `sessionMessages(sessionId)`, not its position among
   * assistant messages only, since `extractArtifacts` indexes the same way. */
  index: number;
  /** Which session this bubble belongs to — threaded through only so the
   * `pre` override's Preview button can call
   * `useArtifactStore.getState().open(sessionId, ref)`; with the split pane,
   * two panes render bubbles from two different sessions at once. */
  sessionId: string;
  /** Present only when user messages can be edited-and-resubmitted;
   * omitted entirely hides the edit affordance. Kept as a stable
   * `(index, text)` callback (rather than a per-row closure) so the
   * `memo()` wrapper below actually prevents re-renders. */
  onEditMessage?: (index: number, newText: string) => void;
  /** Disables the edit affordance while a turn is in flight. */
  editDisabled?: boolean;
  /** Original-preserving translations already saved for this message. */
  translations?: readonly MessageTranslation[];
  /** Thread-level locale preference. Individual controls can still switch
   * back to the original without mutating that preference. */
  preferredTranslationLocale?: string | null;
}

/**
 * `react-markdown` component overrides. Styling itself comes from the
 * `@tailwindcss/typography` `prose` classes applied to the wrapping element
 * below — these overrides only adjust behavior (external links open in a new
 * tab) rather than appearance.
 */
export const markdownComponents: Components = {
  a: ({ children, href }) => (
    <a href={href} target="_blank" rel="noreferrer">
      {children}
    </a>
  ),
};

// Exported so `PlanCard.tsx` can render a plan's Markdown body with the exact
// same typography as an assistant message — see this app's Plan/Act design
// doc's explicit instruction to match `MessageBubble`'s prose classes.
export const PROSE_CLASSES =
  "prose prose-sm max-w-none prose-headings:font-sans prose-p:text-foreground prose-headings:text-foreground prose-strong:text-foreground prose-code:font-mono prose-code:text-foreground prose-code:before:content-none prose-code:after:content-none prose-pre:bg-surface-2 prose-pre:border prose-pre:border-border prose-pre:rounded-lg prose-a:text-accent prose-blockquote:border-l-border prose-blockquote:text-muted";

/** Flattens a `<code>` element's React children back into the plain text it
 * was rendered from — react-markdown gives a fenced code block's body as a
 * single string child in the overwhelming majority of cases, but this walks
 * arrays/nested elements defensively rather than assuming that shape. Used
 * only to recover the fence's raw body so `detectFenceKind` can inspect it
 * (the `xml`-vs-`svg` check needs to see the actual content, not just the
 * language tag). */
function flattenToString(node: ReactNode): string {
  if (typeof node === "string") return node;
  if (typeof node === "number") return String(node);
  if (Array.isArray(node)) return node.map(flattenToString).join("");
  if (isValidElement(node)) return flattenToString((node.props as { children?: ReactNode }).children);
  return "";
}

/**
 * Builds a per-assistant-message `markdownComponents` override extending the
 * shared base above with a `pre` override that renders a slim header bar
 * (language + Preview button) on html/svg/mermaid fences — see the design
 * doc's "UI SURFACE" section. Deliberately NOT merged into the shared
 * `markdownComponents` constant: `PlanCard.tsx` reuses that constant as-is
 * for plan bodies, which never need a Preview button (a proposed plan isn't
 * an artifact), so extending it here keeps that call site untouched.
 *
 * Rebuilt fresh on every `AssistantMessage` render rather than memoized:
 * streaming already forces a full re-parse of the Markdown string on every
 * token (a new `content` string is a new AST regardless), so memoizing this
 * object would save nothing — see `MessageBubble`'s own doc comment on why
 * memoization happens one level up (the whole bubble) instead.
 *
 * `previewableIndex` is a closure-local counter — not React state — since it
 * only needs to count up once per render pass of this single
 * `<ReactMarkdown>` call, in document order, across the fences the `pre`
 * override actually gets invoked for. This numbering MUST match
 * `artifacts.ts`'s `extractArtifacts` `blockIndex` scheme exactly (previewable
 * fences only, in document order) so a click here resolves to the same block
 * that module would extract for the same message — hence `detectFenceKind`
 * is imported and reused unchanged rather than reimplemented here. That
 * alone isn't sufficient, though: `detectFenceKind` only agrees at both call
 * sites because each derives `lang` the same way — react-markdown's `<code>`
 * className only ever holds the FIRST word of a fence's info string (the
 * CommonMark/remark "meta" split), so `artifacts.ts`'s own fence scanner
 * normalizes to that same first token rather than using the whole trimmed
 * info string (see that module's doc comment on `scanFencedBlocks`) — a
 * fence like "```html title=\"x\"" would otherwise be previewable here but
 * invisible to `extractArtifacts`, corrupting both counters relative to each
 * other. Belt-and-suspenders beyond the numbering agreeing: the constructed
 * `ArtifactRef` also carries a `fingerprint` of this exact fence's kind+body
 * (`fingerprintArtifact`, the same function `extractArtifacts` uses), so
 * even a transcript change that coincidentally reuses this exact
 * `{messageIndex, blockIndex}` slot for different content makes
 * `findArtifact` report "no longer available" instead of resolving to the
 * wrong artifact (see `ArtifactRef`'s doc comment in `artifacts.ts`). An
 * unterminated fence never reaches this component at all: react-markdown
 * itself doesn't parse an unclosed ``` block as a `pre`/`code` element
 * (CommonMark treats it as ordinary paragraph text), so during streaming the
 * Preview button simply doesn't exist yet — it appears the moment the
 * closing fence arrives and the message re-parses, exactly the "purely
 * additive, streaming-safe" behavior the design doc calls for.
 */
function buildAssistantMarkdownComponents(
  sessionId: string,
  messageIndex: number,
  t: (key: string) => string
): Components {
  let previewableIndex = 0;

  return {
    ...markdownComponents,
    pre: ({ children }) => {
      const onlyChild = Children.count(children) === 1 ? Children.only(children) : null;
      const codeProps = isValidElement(onlyChild)
        ? (onlyChild.props as { className?: string; children?: ReactNode })
        : null;
      const lang = /language-(\S+)/.exec(codeProps?.className ?? "")?.[1] ?? "";
      const body = codeProps ? flattenToString(codeProps.children).replace(/\n$/, "") : "";
      const kind = codeProps ? detectFenceKind(lang, body) : null;

      if (!kind) return <pre>{children}</pre>;

      const ref: ArtifactRef = {
        messageIndex,
        blockIndex: previewableIndex,
        fingerprint: fingerprintArtifact(kind, body),
      };
      previewableIndex += 1;

      return (
        <div>
          <div className="mb-1 flex items-center justify-between gap-2">
            <span className="font-mono text-[11px] uppercase tracking-wide text-faint">{lang}</span>
            <button
              type="button"
              onClick={() => useArtifactStore.getState().open(sessionId, ref)}
              className="flex cursor-pointer items-center gap-1 rounded-md px-2 py-0.5 text-xs text-muted transition-colors hover:bg-surface-2 hover:text-foreground"
            >
              <Eye size={12} />
              {t("MessageBubble.previewButton")}
            </button>
          </div>
          <pre>{children}</pre>
        </div>
      );
    },
  };
}

function UserBubble({
  content,
  onEdit,
  editDisabled,
  displayText,
  translationControls,
}: {
  content: string | ChatContentPart[];
  onEdit?: (newText: string) => void;
  editDisabled?: boolean;
  displayText?: string;
  translationControls?: ReactNode;
}) {
  // Editing only ever operates on the text portion — an edited-and-resubmitted
  // message doesn't carry its original image attachment forward (matches
  // `ChatWindow.tsx`'s `handleEditMessage`, which already resubmits edits
  // with no attachments at all).
  const text = textContent(content);
  const images = typeof content === "string" ? [] : content.filter((part) => part.type === "image_url");

  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(text);
  const { t } = useT();

  const commit = () => {
    const trimmed = draft.trim();
    if (!trimmed) return;
    onEdit?.(trimmed);
    setEditing(false);
  };

  const cancel = () => {
    setDraft(text);
    setEditing(false);
  };

  if (editing) {
    return (
      <div className="flex justify-end">
        <div className="w-full max-w-[75%] rounded-2xl border border-accent bg-surface-2 px-3 py-2">
          <textarea
            autoFocus
            value={draft}
            onChange={(event) => setDraft(event.target.value)}
            onKeyDown={(event) => {
              if (event.nativeEvent.isComposing) return;
              if (event.key === "Enter" && !event.shiftKey) {
                event.preventDefault();
                commit();
              } else if (event.key === "Escape") {
                event.preventDefault();
                cancel();
              }
            }}
            rows={Math.min(8, draft.split("\n").length)}
            className="w-full resize-none bg-transparent text-sm leading-relaxed text-foreground outline-none"
          />
          <div className="mt-1.5 flex justify-end gap-1.5">
            <button
              type="button"
              onClick={cancel}
              className="cursor-pointer rounded-md px-2 py-1 text-xs text-muted transition-colors hover:text-foreground"
            >
              {t("MessageBubble.cancelButton")}
            </button>
            <button
              type="button"
              onClick={commit}
              disabled={!draft.trim()}
              className="cursor-pointer rounded-md bg-accent px-2.5 py-1 text-xs text-accent-foreground transition-colors hover:bg-accent-hover disabled:cursor-not-allowed disabled:opacity-50"
            >
              {t("MessageBubble.saveAndSubmitButton")}
            </button>
          </div>
        </div>
      </div>
    );
  }

  return (
    <div className="group flex justify-end">
      <div className="flex max-w-[75%] items-start gap-1.5">
        {onEdit && (
          <button
            type="button"
            onClick={() => setEditing(true)}
            disabled={editDisabled}
            aria-label={t("MessageBubble.editMessageAriaLabel")}
            className="mt-2.5 shrink-0 cursor-pointer text-faint opacity-0 transition-opacity duration-150 hover:text-foreground group-hover:opacity-100 disabled:cursor-not-allowed disabled:opacity-0"
          >
            <Pencil size={13} />
          </button>
        )}
        <div className="flex flex-col gap-2 rounded-2xl border border-border bg-surface-2 px-4 py-2 text-sm text-foreground">
          {images.length > 0 && (
            <div className="flex flex-wrap gap-1.5">
              {images.map((part, index) => (
                <img
                  key={index}
                  src={part.image_url.url}
                  alt={t("MessageBubble.attachedImageAlt")}
                  className="h-24 w-24 rounded-lg border border-border object-cover"
                />
              ))}
            </div>
          )}
          {(displayText ?? text) && <div className="whitespace-pre-wrap">{displayText ?? text}</div>}
          {translationControls}
        </div>
      </div>
    </div>
  );
}

function AssistantMessage({
  content,
  sessionId,
  index,
  translationControls,
}: {
  content: string;
  sessionId: string;
  index: number;
  translationControls?: ReactNode;
}) {
  const { t } = useT();
  const components = buildAssistantMarkdownComponents(sessionId, index, t);
  return (
    <div className="w-full min-w-0">
      <div className="mb-1.5 text-xs font-medium text-muted">{t("MessageBubble.assistantName")}</div>
      <div className={PROSE_CLASSES}>
        <ReactMarkdown components={components}>{content}</ReactMarkdown>
      </div>
      {translationControls}
    </div>
  );
}

function sameContent(left: ChatMessage["content"], right: ChatMessage["content"]): boolean {
  return JSON.stringify(left) === JSON.stringify(right);
}

function TranslationControls({
  sessionId,
  index,
  message,
  translations,
  preferredLocale,
  disabled,
  onDisplay,
}: {
  sessionId: string;
  index: number;
  message: ChatMessage;
  translations: readonly MessageTranslation[];
  preferredLocale: string | null;
  disabled: boolean;
  onDisplay: (translation: MessageTranslation | null) => void;
}) {
  const { t } = useT();
  const [locale, setLocale] = useState(preferredLocale ?? defaultTranslationLocale());
  const [running, setRunning] = useState(false);
  const [showTranslation, setShowTranslation] = useState(Boolean(preferredLocale));
  const [latest, setLatest] = useState<MessageTranslation | null>(null);
  const [error, setError] = useState<string | null>(null);
  const saved = [...translations].reverse().find((translation) =>
    translation.messageIndex === index &&
    translation.locale.toLowerCase() === locale.toLowerCase() &&
    sameContent(translation.originalContent, message.content),
  ) ?? null;
  const available = latest && latest.locale.toLowerCase() === locale.toLowerCase() && sameContent(latest.originalContent, message.content)
    ? latest
    : saved;

  useEffect(() => {
    if (!preferredLocale) return;
    setLocale(preferredLocale);
    setShowTranslation(true);
  }, [preferredLocale]);

  useEffect(() => {
    onDisplay(showTranslation ? available : null);
  }, [available, onDisplay, showTranslation]);

  const start = async () => {
    setRunning(true);
    setError(null);
    try {
      const translation = await translateMessage(sessionId, index, locale);
      setLatest(translation);
      setShowTranslation(true);
      onDisplay(translation);
    } catch (caught) {
      if (!(caught instanceof DOMException && caught.name === "AbortError")) {
        setError(caught instanceof Error ? caught.message : String(caught));
      }
    } finally {
      setRunning(false);
    }
  };

  return (
    <div className="mt-2 border-t border-border/70 pt-1.5 text-xs text-muted">
      <div className="flex flex-wrap items-center gap-1.5">
        <Languages size={13} aria-hidden="true" />
        <select
          aria-label={t("Translation.languageLabel")}
          value={locale}
          disabled={running}
          onChange={(event) => {
            setLocale(event.target.value);
            setShowTranslation(false);
            setError(null);
          }}
          className="cursor-pointer rounded border border-border bg-background px-1.5 py-0.5 text-xs text-foreground outline-none focus-visible:border-accent"
        >
          {TRANSLATION_LOCALES.map(({ code, label }) => <option key={code} value={code}>{label}</option>)}
        </select>
        {running ? (
          <button
            type="button"
            onClick={() => cancelTranslation(messageTranslationKey(sessionId, index))}
            className="flex cursor-pointer items-center gap-1 rounded px-1.5 py-0.5 hover:bg-surface hover:text-foreground"
          >
            <LoaderCircle size={12} className="animate-spin" />
            {t("Translation.cancel")}
          </button>
        ) : (
          <button
            type="button"
            onClick={() => void start()}
            disabled={disabled}
            className="cursor-pointer rounded px-1.5 py-0.5 hover:bg-surface hover:text-foreground disabled:cursor-not-allowed disabled:opacity-50"
          >
            {t("Translation.translate")}
          </button>
        )}
        {available && (
          <button
            type="button"
            onClick={() => setShowTranslation((value) => !value)}
            className="cursor-pointer rounded px-1.5 py-0.5 hover:bg-surface hover:text-foreground"
          >
            {showTranslation ? t("Translation.showOriginal") : t("Translation.showTranslation")}
          </button>
        )}
        {available && <span className="text-faint">{t("Translation.preserved")}</span>}
      </div>
      {error && (
        <div className="mt-1 flex items-start justify-between gap-2 rounded bg-danger-soft px-2 py-1 text-danger" role="alert">
          <span>{t("Translation.error", { error })}</span>
          <button type="button" onClick={() => setError(null)} aria-label="Dismiss translation error" className="cursor-pointer">
            <X size={12} />
          </button>
        </div>
      )}
    </div>
  );
}

/**
 * Renders a single user or assistant chat message. Tool-call and tool-result
 * messages are handled separately by `MessageList` (see `ToolCallRow`),
 * since rendering them well requires correlating multiple `ChatMessage`
 * entries; system messages are never shown in the transcript.
 *
 * Wrapped in `memo` because the agent loop mutates the LAST message on every
 * streamed token: message object identities for all earlier messages are
 * stable across those store updates, so with shallow-equal props only the
 * actively-streaming bubble re-renders (markdown + syntax highlighting are
 * expensive enough that re-rendering the whole transcript per token visibly
 * stutters on long conversations).
 */
function MessageBubble({
  message,
  index,
  sessionId,
  onEditMessage,
  editDisabled,
  translations = [],
  preferredTranslationLocale = null,
}: MessageBubbleProps) {
  const [displayedTranslation, setDisplayedTranslation] = useState<MessageTranslation | null>(null);
  const controls = (
    <TranslationControls
      sessionId={sessionId}
      index={index}
      message={message}
      translations={translations}
      preferredLocale={preferredTranslationLocale}
      disabled={editDisabled === true}
      onDisplay={setDisplayedTranslation}
    />
  );
  if (message.role === "user") {
    return (
      <UserBubble
        content={message.content}
        displayText={displayedTranslation?.translatedText}
        translationControls={controls}
        onEdit={onEditMessage ? (text) => onEditMessage(index, text) : undefined}
        editDisabled={editDisabled}
      />
    );
  }

  if (message.role === "assistant") {
    return (
      <AssistantMessage
        content={displayedTranslation?.translatedText ?? textContent(message.content)}
        sessionId={sessionId}
        index={index}
        translationControls={controls}
      />
    );
  }

  return null;
}

export default memo(MessageBubble);
