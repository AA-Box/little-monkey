import { invoke } from "@tauri-apps/api/core";

/**
 * Conversations this installation holds that are not the desktop app's own
 * sessions — a paired phone's chat, a messaging conversation the agent is
 * answering. The desktop's sessions live in `sessionStore`; these live in the
 * daemon, and the sidebar shows both.
 *
 * Read-only by construction: the reply to a Slack thread goes out on Slack and
 * the reply to a phone goes to the phone, so there is nothing here that sends.
 */

/** Where a conversation lives. `local` is the desktop's own list and never
 * comes back from the bridge; the channel environments carry their provider
 * (`channel:slack`), because two providers are two environments. */
export const LOCAL_ENVIRONMENT = "local";
export const REMOTE_CONTROL_ENVIRONMENT = "remote_control";
export const CHANNEL_ENVIRONMENT_PREFIX = "channel:";
export const SLACK_ENVIRONMENT = `${CHANNEL_ENVIRONMENT_PREFIX}slack`;

/** The environments the app itself supports, always offered by the sidebar's
 * filter whether or not any session has arrived on one yet: an empty Slack
 * filter means the Slack app has not been installed in a workspace, which is
 * a fact worth being able to see. */
export const BUILT_IN_ENVIRONMENTS = [
  LOCAL_ENVIRONMENT,
  REMOTE_CONTROL_ENVIRONMENT,
  SLACK_ENVIRONMENT,
] as const;

export interface ExternalConversation {
  /** `remote_control` or `channel:<provider>`. */
  environment: string;
  /** Provider token for a channel conversation; null for a paired phone. */
  provider: string | null;
  /** Opaque id, unique within its environment — pass back to `show`. */
  id: string;
  title: string;
  /** The operator's own name for the account it arrived on, when it has one. */
  account_label: string | null;
  updated_at_ms: number;
  message_count: number;
}

export interface ExternalMessage {
  role: string;
  text: string;
  at_ms: number;
  /** The provider's id for whoever sent it; absent on our own messages. */
  author: string | null;
}

export const conversationsList = (environment: string | null = null, limit = 200) =>
  invoke<{ conversations: ExternalConversation[] }>("conversations_list", { environment, limit });

export const conversationsShow = (environment: string, id: string, limit = 500) =>
  invoke<{ messages: ExternalMessage[] }>("conversations_show", { environment, id, limit });

/** The provider token an environment names, or null when it names no channel. */
export function environmentProvider(environment: string): string | null {
  return environment.startsWith(CHANNEL_ENVIRONMENT_PREFIX)
    ? environment.slice(CHANNEL_ENVIRONMENT_PREFIX.length)
    : null;
}
