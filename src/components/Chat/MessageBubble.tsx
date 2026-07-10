import { memo, useState } from "react";
import ReactMarkdown from "react-markdown";
import type { Components } from "react-markdown";
import { Pencil } from "lucide-react";

import { textContent, type ChatContentPart, type ChatMessage } from "../../lib/llamaClient";
import { useT } from "../../lib/i18n";

export interface MessageBubbleProps {
  message: ChatMessage;
  /** This message's index in the transcript, passed back to `onEditMessage`. */
  index: number;
  /** Present only when user messages can be edited-and-resubmitted;
   * omitted entirely hides the edit affordance. Kept as a stable
   * `(index, text)` callback (rather than a per-row closure) so the
   * `memo()` wrapper below actually prevents re-renders. */
  onEditMessage?: (index: number, newText: string) => void;
  /** Disables the edit affordance while a turn is in flight. */
  editDisabled?: boolean;
}

/**
 * `react-markdown` component overrides. Styling itself comes from the
 * `@tailwindcss/typography` `prose` classes applied to the wrapping element
 * below — these overrides only adjust behavior (external links open in a new
 * tab) rather than appearance.
 */
const markdownComponents: Components = {
  a: ({ children, href }) => (
    <a href={href} target="_blank" rel="noreferrer">
      {children}
    </a>
  ),
};

const PROSE_CLASSES =
  "prose prose-sm max-w-none prose-headings:font-sans prose-p:text-foreground prose-headings:text-foreground prose-strong:text-foreground prose-code:font-mono prose-code:text-foreground prose-code:before:content-none prose-code:after:content-none prose-pre:bg-surface-2 prose-pre:border prose-pre:border-border prose-pre:rounded-lg prose-a:text-accent prose-blockquote:border-l-border prose-blockquote:text-muted";

function UserBubble({
  content,
  onEdit,
  editDisabled,
}: {
  content: string | ChatContentPart[];
  onEdit?: (newText: string) => void;
  editDisabled?: boolean;
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
          {text && <div className="whitespace-pre-wrap">{text}</div>}
        </div>
      </div>
    </div>
  );
}

function AssistantMessage({ content }: { content: string }) {
  const { t } = useT();
  return (
    <div className="w-full min-w-0">
      <div className="mb-1.5 text-xs font-medium text-muted">{t("MessageBubble.assistantName")}</div>
      <div className={PROSE_CLASSES}>
        <ReactMarkdown components={markdownComponents}>{content}</ReactMarkdown>
      </div>
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
function MessageBubble({ message, index, onEditMessage, editDisabled }: MessageBubbleProps) {
  if (message.role === "user") {
    return (
      <UserBubble
        content={message.content}
        onEdit={onEditMessage ? (text) => onEditMessage(index, text) : undefined}
        editDisabled={editDisabled}
      />
    );
  }

  if (message.role === "assistant") {
    return <AssistantMessage content={textContent(message.content)} />;
  }

  return null;
}

export default memo(MessageBubble);
