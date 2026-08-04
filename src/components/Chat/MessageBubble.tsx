import { Children, isValidElement, lazy, memo, Suspense, useEffect, useRef, useState, type ReactNode } from "react";
import ReactMarkdown from "react-markdown";
import type { Components } from "react-markdown";
import { Eye, Languages, LoaderCircle, Pencil, Split, X } from "lucide-react";

import { textContent, type ChatContentPart, type ChatMessage } from "../../lib/llamaClient";
import { detectFenceKind, fingerprintArtifact, type ArtifactRef } from "../../lib/artifacts";
import { isWorkspaceImageSrc } from "../../lib/imageGeneration";
import WorkspaceImagePreview from "./WorkspaceImagePreview";
import { useArtifactStore } from "../../store/artifactStore";
// Lazy: `CodeBlock` pulls in `react-syntax-highlighter`'s Prism bundle, the
// same heavy dependency `ArtifactPane.tsx` keeps out of the main entry chunk
// via `lazyComponents.tsx` — see `CodeBlock.tsx`'s doc comment.
const CodeBlock = lazy(() => import("./CodeBlock"));
import { useT } from "../../lib/i18n";
import {
  cancelTranslation,
  defaultTranslationLocale,
  messageTranslationKey,
  TRANSLATION_LOCALES,
  translateMessage,
} from "../../lib/translation";
import type { MessageTranslation } from "../../store/sessionStore";
import { errorMessage } from "../../lib/errors";

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
  /** Present to show a "Start side task" hover action on both user and
   * assistant bubbles (ROADMAP.md's "Side Tasks" acceptance: start a side
   * task "from selected chat context") — omitted entirely hides the
   * affordance, same convention as `onEditMessage`. Called with this
   * message's own transcript `index`; the handler (`ChatWindow.tsx`) reads
   * the message back out to build the side task's seed. */
  onStartSideTask?: (index: number) => void;
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
    // Workspace-relative image references (`![chart](out/chart.png)`) can't
    // load through a plain `<img>` — the webview has no filesystem origin to
    // resolve them against — so they're rendered via `WorkspaceImagePreview`,
    // which reads the file through the sandboxed `workspace_read_image`
    // command into a data URL. This is how an image produced by a plotting
    // script (or by the legacy workspace-writing image tool) previews inline
    // when the model references it in prose. Absolute/URL/data srcs
    // keep the default `<img>` untouched.
    img: ({ src, alt }) => {
      const srcString = typeof src === "string" ? src : undefined;
      if (isWorkspaceImageSrc(srcString)) {
        return <WorkspaceImagePreview path={srcString} alt={alt} />;
      }
      return <img src={srcString} alt={alt} />;
    },
    pre: ({ children }) => {
      const onlyChild = Children.count(children) === 1 ? Children.only(children) : null;
      const codeProps = isValidElement(onlyChild)
        ? (onlyChild.props as { className?: string; children?: ReactNode })
        : null;
      const lang = /language-(\S+)/.exec(codeProps?.className ?? "")?.[1] ?? "";
      const body = codeProps ? flattenToString(codeProps.children).replace(/\n$/, "") : "";
      const kind = codeProps ? detectFenceKind(lang, body) : null;

      if (!codeProps) return <pre>{children}</pre>;

      // Suspense's fallback is the same plain `<pre>` this fence rendered as
      // before `CodeBlock` existed — the lazy chunk is small and typically
      // cached after the first fence in a session, so this is a rare,
      // brief flash rather than the normal path.
      if (!kind) {
        return (
          <Suspense fallback={<pre>{children}</pre>}>
            <CodeBlock lang={lang} body={body} />
          </Suspense>
        );
      }

      const ref: ArtifactRef = {
        messageIndex,
        blockIndex: previewableIndex,
        fingerprint: fingerprintArtifact(kind, body),
      };
      previewableIndex += 1;

      return (
        <Suspense fallback={<pre>{children}</pre>}>
          <CodeBlock
            lang={lang}
            body={body}
            headerExtra={
              <button
                type="button"
                onClick={() => useArtifactStore.getState().open(sessionId, ref)}
                className="flex cursor-pointer items-center gap-1 rounded-md px-2 py-0.5 text-xs text-white/50 transition-colors hover:bg-white/10 hover:text-white"
              >
                <Eye size={12} />
                {t("MessageBubble.previewButton")}
              </button>
            }
          />
        </Suspense>
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
  onStartSideTask,
}: {
  content: string | ChatContentPart[];
  onEdit?: (newText: string) => void;
  editDisabled?: boolean;
  displayText?: string;
  translationControls?: ReactNode;
  onStartSideTask?: () => void;
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
        {(translationControls || onEdit || onStartSideTask) && (
          <div className="mt-1.5 flex shrink-0 items-center gap-0.5">
            {translationControls}
            {onStartSideTask && (
              <button
                type="button"
                onClick={onStartSideTask}
                aria-label="Start side task from this message"
                title="Start side task from this message"
                className="flex h-7 w-7 cursor-pointer items-center justify-center rounded-md text-faint opacity-0 transition-all duration-150 hover:bg-surface-2 hover:text-foreground group-hover:opacity-100 focus-visible:opacity-100"
              >
                <Split size={13} />
              </button>
            )}
            {onEdit && (
              <button
                type="button"
                onClick={() => setEditing(true)}
                disabled={editDisabled}
                aria-label={t("MessageBubble.editMessageAriaLabel")}
                className="flex h-7 w-7 cursor-pointer items-center justify-center rounded-md text-faint opacity-0 transition-all duration-150 hover:bg-surface-2 hover:text-foreground group-hover:opacity-100 focus-visible:opacity-100 disabled:cursor-not-allowed disabled:opacity-0"
              >
                <Pencil size={13} />
              </button>
            )}
          </div>
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
  onStartSideTask,
}: {
  content: string;
  sessionId: string;
  index: number;
  translationControls?: ReactNode;
  onStartSideTask?: () => void;
}) {
  const { t } = useT();
  const components = buildAssistantMarkdownComponents(sessionId, index, t);
  return (
    <div className="group relative w-full min-w-0">
      <div className="mb-1.5 flex items-center gap-1.5 text-xs font-medium text-muted">
        {t("MessageBubble.assistantName")}
        {onStartSideTask && (
          <button
            type="button"
            onClick={onStartSideTask}
            aria-label="Start side task from this message"
            title="Start side task from this message"
            className="flex h-5 w-5 cursor-pointer items-center justify-center rounded-md text-faint opacity-0 transition-all duration-150 hover:bg-surface-2 hover:text-foreground group-hover:opacity-100 focus-visible:opacity-100"
          >
            <Split size={12} />
          </button>
        )}
      </div>
      <div className={PROSE_CLASSES}>
        <ReactMarkdown components={components}>{content}</ReactMarkdown>
      </div>
      {translationControls && <div className="absolute -bottom-5 left-0 z-10">{translationControls}</div>}
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
  align,
}: {
  sessionId: string;
  index: number;
  message: ChatMessage;
  translations: readonly MessageTranslation[];
  preferredLocale: string | null;
  disabled: boolean;
  onDisplay: (translation: MessageTranslation | null) => void;
  align: "start" | "end";
}) {
  const { t } = useT();
  const [locale, setLocale] = useState(preferredLocale ?? defaultTranslationLocale());
  const [running, setRunning] = useState(false);
  const [showTranslation, setShowTranslation] = useState(Boolean(preferredLocale));
  const [latest, setLatest] = useState<MessageTranslation | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [menuOpen, setMenuOpen] = useState(false);
  const containerRef = useRef<HTMLDivElement>(null);
  const saved = [...translations].reverse().find((translation) =>
    translation.messageIndex === index &&
    translation.locale.toLowerCase() === locale.toLowerCase() &&
    sameContent(translation.originalContent, message.content),
  ) ?? null;
  const available = latest && latest.locale.toLowerCase() === locale.toLowerCase() && sameContent(latest.originalContent, message.content)
    ? latest
    : saved;

  useEffect(() => {
    if (preferredLocale) {
      setLocale(preferredLocale);
      setShowTranslation(true);
    } else {
      setShowTranslation(false);
    }
  }, [preferredLocale]);

  useEffect(() => {
    onDisplay(showTranslation ? available : null);
  }, [available, onDisplay, showTranslation]);

  useEffect(() => {
    if (!menuOpen) return;
    const handlePointerDown = (event: PointerEvent) => {
      if (!containerRef.current?.contains(event.target as Node)) setMenuOpen(false);
    };
    document.addEventListener("pointerdown", handlePointerDown);
    return () => document.removeEventListener("pointerdown", handlePointerDown);
  }, [menuOpen]);

  const start = async () => {
    setRunning(true);
    setError(null);
    try {
      const translation = await translateMessage(sessionId, index, locale);
      setLatest(translation);
      setShowTranslation(true);
      setMenuOpen(false);
      onDisplay(translation);
    } catch (caught) {
      if (!(caught instanceof DOMException && caught.name === "AbortError")) {
        setError(errorMessage(caught));
      }
    } finally {
      setRunning(false);
    }
  };

  const controlVisible = menuOpen || running || available !== null || preferredLocale !== null;

  return (
    <div
      ref={containerRef}
      className={`relative flex items-center gap-0.5 text-xs text-muted transition-opacity duration-150 ${controlVisible ? "opacity-100" : "opacity-0 group-hover:opacity-100 group-focus-within:opacity-100"}`}
      onKeyDown={(event) => {
        if (event.key === "Escape") {
          event.stopPropagation();
          setMenuOpen(false);
        }
      }}
    >
      <button
        type="button"
        aria-label={running ? t("Translation.cancel") : t("Translation.translate")}
        aria-haspopup="dialog"
        aria-expanded={menuOpen}
        title={running ? t("Translation.cancel") : t("Translation.translate")}
        disabled={disabled && !running}
        onClick={() => {
          if (running) cancelTranslation(messageTranslationKey(sessionId, index));
          else setMenuOpen((value) => !value);
        }}
        className="flex h-7 w-7 cursor-pointer items-center justify-center rounded-md text-faint transition-colors hover:bg-surface-2 hover:text-foreground focus-visible:text-foreground disabled:cursor-not-allowed disabled:opacity-40"
      >
        {running ? <LoaderCircle size={13} className="animate-spin" /> : <Languages size={13} />}
      </button>

      {available && (
        <button
          type="button"
          onClick={() => setShowTranslation((value) => !value)}
          className="flex h-7 cursor-pointer items-center gap-1 rounded-md px-1.5 text-faint transition-colors hover:bg-surface-2 hover:text-foreground focus-visible:text-foreground"
          title={showTranslation ? t("Translation.showOriginal") : t("Translation.showTranslation")}
        >
          <Eye size={12} />
          <span>{showTranslation ? t("Translation.showOriginal") : t("Translation.showTranslation")}</span>
        </button>
      )}

      {menuOpen && (
        <div
          role="dialog"
          aria-label={t("Translation.translate")}
          className={`absolute top-full z-30 mt-1 w-64 rounded-xl border border-border bg-background p-3 text-left shadow-lg ${align === "end" ? "right-0" : "left-0"}`}
        >
          <label className="block text-[11px] font-medium text-muted">
            {t("Translation.languageLabel")}
            <select
              autoFocus
              value={locale}
              disabled={running}
              onChange={(event) => {
                setLocale(event.target.value);
                setShowTranslation(false);
                setError(null);
              }}
              className="mt-1.5 w-full cursor-pointer rounded-md border border-border bg-background px-2.5 py-2 text-xs text-foreground outline-none focus-visible:border-accent focus-visible:ring-2 focus-visible:ring-accent/20"
            >
              {TRANSLATION_LOCALES.map(({ code, label }) => <option key={code} value={code}>{label}</option>)}
            </select>
          </label>

          <div className="mt-2.5 flex items-center justify-between gap-2">
            {available ? <span className="text-[10px] text-faint">{t("Translation.preserved")}</span> : <span />}
            {running ? (
              <button
                type="button"
                onClick={() => cancelTranslation(messageTranslationKey(sessionId, index))}
                className="flex min-h-8 cursor-pointer items-center gap-1.5 rounded-md px-2.5 text-xs text-muted hover:bg-surface-2 hover:text-foreground"
              >
                <LoaderCircle size={12} className="animate-spin" />
                {t("Translation.cancel")}
              </button>
            ) : (
              <button
                type="button"
                onClick={() => void start()}
                disabled={disabled}
                className="flex min-h-8 cursor-pointer items-center gap-1.5 rounded-md bg-accent px-3 text-xs font-medium text-accent-foreground hover:bg-accent-hover disabled:cursor-not-allowed disabled:opacity-50"
              >
                <Languages size={12} />
                {t("Translation.translate")}
              </button>
            )}
          </div>

          {error && (
            <div className="mt-2 flex items-start justify-between gap-2 rounded-md bg-danger-soft px-2 py-1.5 text-[11px] leading-relaxed text-danger" role="alert">
              <span>{t("Translation.error", { error })}</span>
              <button type="button" onClick={() => setError(null)} aria-label="Dismiss translation error" className="shrink-0 cursor-pointer rounded p-0.5 hover:bg-danger/10">
                <X size={12} />
              </button>
            </div>
          )}
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
  onStartSideTask,
}: MessageBubbleProps) {
  const [displayedTranslation, setDisplayedTranslation] = useState<MessageTranslation | null>(null);
  const controls = textContent(message.content).trim() ? (
    <TranslationControls
      sessionId={sessionId}
      index={index}
      message={message}
      translations={translations}
      preferredLocale={preferredTranslationLocale}
      disabled={editDisabled === true}
      onDisplay={setDisplayedTranslation}
      align={message.role === "user" ? "end" : "start"}
    />
  ) : null;
  const startSideTask = onStartSideTask ? () => onStartSideTask(index) : undefined;
  if (message.role === "user") {
    return (
      <UserBubble
        content={message.content}
        displayText={displayedTranslation?.translatedText}
        translationControls={controls}
        onEdit={onEditMessage ? (text) => onEditMessage(index, text) : undefined}
        editDisabled={editDisabled}
        onStartSideTask={startSideTask}
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
        onStartSideTask={startSideTask}
      />
    );
  }

  return null;
}

export default memo(MessageBubble);
