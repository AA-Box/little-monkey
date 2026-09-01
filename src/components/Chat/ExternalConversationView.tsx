import { useEffect } from "react";
import { RefreshCw } from "lucide-react";

import { useT } from "../../lib/i18n";
import { formatMessageTimestamp } from "./messageChapters";
import { useEnvironmentLabel } from "./SessionListMenu";
import {
  conversationKey,
  useExternalConversationStore,
  type ExternalSelection,
} from "../../store/externalConversationStore";

/**
 * A conversation that happened somewhere else, read from what this machine
 * durably recorded: a paired phone's chat, or a messaging thread the agent
 * answered on a channel.
 *
 * Read-only, and not as a limitation to apologize for — a Slack thread is
 * answered on Slack and a phone's chat on the phone, by the same agent, under
 * the routing the operator configured. Typing a reply here would be a second,
 * unrouted way to speak into someone else's conversation. What this pane owes
 * the user is the transcript and where it came from.
 */
export function ExternalConversationView({ selection }: { selection: ExternalSelection }) {
  const { t } = useT();
  const environmentLabel = useEnvironmentLabel();
  const conversations = useExternalConversationStore((state) => state.conversations);
  const messages = useExternalConversationStore(
    (state) => state.messages[conversationKey(selection)],
  );
  const loadMessages = useExternalConversationStore((state) => state.loadMessages);
  const error = useExternalConversationStore((state) => state.error);

  const conversation = conversations.find(
    (candidate) =>
      candidate.environment === selection.environment && candidate.id === selection.id,
  );

  useEffect(() => {
    if (!messages) void loadMessages(selection);
    // Only when the open conversation changes: a transcript already in hand
    // is not refetched on every re-render.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selection.environment, selection.id]);

  const title = conversation?.title?.trim() || selection.id;
  const environment = environmentLabel(selection.environment);

  return (
    <div className="flex min-h-0 min-w-0 flex-1 flex-col">
      <div className="flex shrink-0 items-center justify-between gap-3 border-b border-border px-4 py-2">
        <div className="min-w-0">
          <p className="truncate text-sm font-medium text-foreground">{title}</p>
          <p className="truncate text-xs text-faint">
            {conversation?.account_label
              ? t("ExternalConversation.subtitleWithAccount", {
                  environment,
                  account: conversation.account_label,
                })
              : environment}
          </p>
        </div>
        <button
          type="button"
          onClick={() => void loadMessages(selection)}
          aria-label={t("ExternalConversation.refresh")}
          className="flex h-7 w-7 shrink-0 cursor-pointer items-center justify-center rounded-md text-faint hover:bg-surface-2 hover:text-foreground"
        >
          <RefreshCw size={14} />
        </button>
      </div>

      <div className="min-h-0 flex-1 overflow-y-auto px-4 py-3">
        <div className="mx-auto flex max-w-3xl flex-col gap-3">
          {error && (
            <p className="rounded-lg border border-danger bg-danger-soft px-3 py-2 text-sm text-danger" role="alert">
              {error}
            </p>
          )}
          {messages === undefined ? (
            // Only while it is genuinely still in flight: a transcript that
            // failed to load has its reason printed above, and saying
            // "loading" underneath it says the app is still trying when it is
            // not.
            error ? null : (
              <p className="text-sm text-faint">{t("ExternalConversation.loading")}</p>
            )
          ) : messages.length === 0 ? (
            <p className="text-sm text-faint">{t("ExternalConversation.empty")}</p>
          ) : (
            messages.map((message, index) => (
              <div
                key={`${message.at_ms}-${index}`}
                className={`flex ${message.role === "assistant" ? "justify-start" : "justify-end"}`}
              >
                <div
                  className={`flex max-w-[75%] flex-col gap-1 rounded-2xl border px-4 py-2 text-sm ${
                    message.role === "assistant"
                      ? "border-border bg-surface text-foreground"
                      : "border-border bg-surface-2 text-foreground"
                  }`}
                >
                  <span className="whitespace-pre-wrap break-words">{message.text}</span>
                  <span className="text-[11px] text-faint">
                    {message.author ? `${message.author} · ` : ""}
                    {formatMessageTimestamp(message.at_ms)}
                  </span>
                </div>
              </div>
            ))
          )}
        </div>
      </div>

      <p className="shrink-0 border-t border-border px-4 py-2 text-xs text-faint">
        {t("ExternalConversation.readOnlyNotice", { environment })}
      </p>
    </div>
  );
}

export default ExternalConversationView;
