import { invoke } from "@tauri-apps/api/core";

/**
 * Messaging channels, over the typed daemon bridge.
 *
 * Every call here is a fixed-argument Tauri command that wraps the same
 * `monkey channels` subcommand the terminal uses, so the rules live in one
 * place. Nothing in this file talks to a provider directly.
 */

/** Verified connection state. Written by a probe or a live event — never by
 * saving configuration. `unconfigured` is what a new account has. */
export type ChannelHealthState =
  | "unconfigured"
  | "disconnected"
  | "connecting"
  | "connected"
  | "degraded"
  | "unsupported"
  | "error";

export type AccessPolicy = "disabled" | "allow_list" | "pairing" | "open";
export type GroupActivation = "always" | "mention_only" | "disabled";

export interface ChannelAccessPolicy {
  direct: AccessPolicy;
  group: AccessPolicy;
  group_activation: GroupActivation;
}

export interface ChannelAccount {
  account_id: string;
  kind: string;
  label: string;
  enabled: boolean;
  /** Whether a credential is stored. The value itself never crosses the
   * bridge in this direction. */
  has_credential: boolean;
  access_policy: ChannelAccessPolicy;
  health: ChannelHealthState;
  health_detail: string | null;
  last_error: string | null;
  last_probe_at_ms: number;
  non_secret_config: Record<string, unknown>;
  created_at_ms: number;
  updated_at_ms: number;
}

export interface PendingSender {
  sender_id: string;
  state: string;
  display_label: string | null;
  requested_at_ms: number;
  expires_at_ms: number | null;
}

export interface ChannelEvent {
  event_id: string;
  direction: "inbound" | "outbound";
  conversation_id: string;
  thread_id: string | null;
  sender_id: string | null;
  disposition: string;
  ignore_reason: string | null;
  job_id: string | null;
  received_at_ms: number;
}

/** Providers the app can configure, with what each one needs from the
 * operator. Rendered by the setup flow, so it is deliberately explicit about
 * prerequisites rather than assuming the user knows them. */
export interface ProviderGuide {
  kind: string;
  label: string;
  /** How inbound messages arrive, which decides whether a public callback URL
   * is part of setup at all. */
  transport: "long_poll" | "socket" | "webhook" | "helper";
  credentialLabel: string;
  whereToGetIt: string;
  docsUrl: string;
  /** Extra non-secret settings this provider needs, as JSON keys. Never a
   * secret: these are stored in the account row, not the keychain. */
  configKeys: string[];
  /** The parts the credential is made of, when it is more than one value.
   * Setup collects each one and saves them as the single JSON bundle the
   * adapter parses, so nobody has to know the wire shape. Omitted means the
   * credential is one value pasted whole. */
  secretFields?: { key: string; label: string }[];
}

export const PROVIDER_GUIDES: ProviderGuide[] = [
  { kind: "telegram", label: "Telegram", transport: "long_poll", credentialLabel: "Bot token", whereToGetIt: "Create a bot with @BotFather and copy the token it gives you.", docsUrl: "https://core.telegram.org/bots#how-do-i-create-a-bot", configKeys: [] },
  { kind: "discord", label: "Discord", transport: "socket", credentialLabel: "Bot token", whereToGetIt: "Discord Developer Portal → your application → Bot → Reset Token. Enable the Message Content intent.", docsUrl: "https://discord.com/developers/docs/topics/gateway", configKeys: [] },
  { kind: "slack", label: "Slack", transport: "socket", credentialLabel: "Bot and app tokens (JSON)", whereToGetIt: "Slack API → your app → OAuth (xoxb bot token) and Basic Information (xapp app-level token with connections:write).", docsUrl: "https://api.slack.com/apis/socket-mode", configKeys: [] },
  { kind: "mattermost", label: "Mattermost", transport: "socket", credentialLabel: "Personal access token", whereToGetIt: "Your Mattermost profile → Security → Personal Access Tokens.", docsUrl: "https://developers.mattermost.com/integrate/reference/personal-access-token/", configKeys: ["base_url"] },
  { kind: "irc", label: "IRC", transport: "socket", credentialLabel: "SASL password", whereToGetIt: "The account password registered with the network's services (NickServ).", docsUrl: "https://ircv3.net/specs/extensions/sasl-3.1", configKeys: ["server", "port", "nick", "channels", "use_sasl"] },
  { kind: "whatsapp", label: "WhatsApp", transport: "webhook", credentialLabel: "WhatsApp credentials", whereToGetIt: "Meta for Developers → your app → WhatsApp → API Setup for the access token and phone number ID; App settings → Basic for the app secret. The verify token is yours to invent — type the same value here and into Meta's webhook form.", docsUrl: "https://developers.facebook.com/docs/whatsapp/cloud-api", configKeys: ["phone_number_id"], secretFields: [{ key: "access_token", label: "Access token" }, { key: "app_secret", label: "App secret" }, { key: "verify_token", label: "Verify token (you choose it)" }] },
  { kind: "line", label: "LINE", transport: "webhook", credentialLabel: "LINE credentials", whereToGetIt: "LINE Developers Console → your channel → Messaging API for the access token, and Basic settings for the channel secret that verifies signatures.", docsUrl: "https://developers.line.biz/en/docs/messaging-api/", configKeys: [], secretFields: [{ key: "channel_access_token", label: "Channel access token" }, { key: "channel_secret", label: "Channel secret" }] },
  { kind: "teams", label: "Microsoft Teams", transport: "webhook", credentialLabel: "Client secret", whereToGetIt: "Azure Bot resource → Configuration for the Microsoft App ID and tenant ID; Certificates & secrets for a client secret.", docsUrl: "https://learn.microsoft.com/azure/bot-service/", configKeys: ["app_id", "tenant_id"], secretFields: [{ key: "app_password", label: "Client secret" }] },
  { kind: "google_chat", label: "Google Chat", transport: "webhook", credentialLabel: "Service account key (paste the whole JSON file)", whereToGetIt: "Google Cloud Console → Chat API → create a service account and download its key. Paste the file's contents unchanged.", docsUrl: "https://developers.google.com/chat/api/guides/auth", configKeys: ["project_number"] },
];

export const channelsList = () => invoke<{ accounts: ChannelAccount[] }>("channels_list");
export const channelsAdd = (kind: string, label: string, config: string | null) =>
  invoke<ChannelAccount>("channels_add", { kind, label, config });
export const channelsProbe = (accountId: string) =>
  invoke<{ account_id: string; health: ChannelHealthState; detail: string | null; last_error: string | null }>(
    "channels_probe",
    { accountId },
  );
export const channelsEnable = (accountId: string, enabled: boolean) =>
  invoke<void>("channels_enable", { accountId, enabled });
export const channelsSetCredential = (accountId: string, secret: string) =>
  invoke<void>("channels_set_credential", { accountId, secret });
export const channelsSetPolicy = (
  accountId: string,
  direct: AccessPolicy | null,
  group: AccessPolicy | null,
  activation: GroupActivation | null,
) => invoke<void>("channels_set_policy", { accountId, direct, group, activation });
export const channelsSenders = (accountId: string) =>
  invoke<{ pending: PendingSender[] }>("channels_senders", { accountId });
export const channelsDecideSender = (accountId: string, senderId: string, approve: boolean) =>
  invoke<void>("channels_decide_sender", { accountId, senderId, approve });
export const channelsRoutes = () => invoke<{ routes: unknown[] }>("channels_routes");
export const channelsAddRoute = (
  recipe: string,
  accountId: string | null,
  conversationId: string | null,
  kind: string | null,
  repository: string | null,
) => invoke<void>("channels_add_route", { recipe, accountId, conversationId, kind, repository });
export const channelsRemoveRoute = (routeId: string) => invoke<void>("channels_remove_route", { routeId });
export const channelsEvents = (accountId: string, limit = 20) =>
  invoke<{ events: ChannelEvent[] }>("channels_events", { accountId, limit });
export const channelsRemove = (accountId: string) => invoke<void>("channels_remove", { accountId });

/** Whether this provider needs the operator to expose a public callback URL.
 * Setup asks for one only when it is genuinely required. */
export function needsPublicCallback(kind: string): boolean {
  return PROVIDER_GUIDES.find((guide) => guide.kind === kind)?.transport === "webhook";
}

/** The callback path a webhook provider must be pointed at, under whatever
 * public base URL the operator configured. */
export function callbackPath(accountId: string): string {
  return `/v1/channels/${accountId}`;
}
