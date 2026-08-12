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
  /** Whether this account needs one at all. False for the helper providers,
   * whose helper holds the account, and for IRC without SASL — the daemon
   * decides, so the panel never invents a credential box nobody can fill. */
  credential_required: boolean;
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

/** One non-secret setting an account needs, collected as its own input.
 *
 * `type` is what the value becomes in the account row, not how it is typed:
 * the daemon parses `port` as a number and `use_sasl` as a boolean, and a
 * quoted string in either place is simply rejected. Assembling the object here
 * is what stops an operator having to hand-write JSON to configure a server. */
export interface ProviderConfigField {
  key: string;
  label: string;
  type: "text" | "number" | "boolean" | "list";
  placeholder?: string;
  /** The adapter refuses to build without it. */
  required?: boolean;
  /** Shown under the input when the value is not self-explanatory. */
  hint?: string;
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
  /** Extra non-secret settings this provider needs. Never a secret: these are
   * stored in the account row, not the keychain. */
  configFields: ProviderConfigField[];
  /** True when the provider authenticates some other way and there is no
   * credential for Little Monkey to hold: Signal and iMessage speak to a
   * helper that owns the account, IRC needs a password only for SASL. */
  credentialOptional?: boolean;
  /** Platforms this provider can run on at all. Absent means anywhere. */
  requiresPlatform?: "macos";
  /** The parts the credential is made of, when it is more than one value.
   * Setup collects each one and saves them as the single JSON bundle the
   * adapter parses, so nobody has to know the wire shape. Omitted means the
   * credential is one value pasted whole. */
  secretFields?: { key: string; label: string }[];
}

export const PROVIDER_GUIDES: ProviderGuide[] = [
  { kind: "telegram", label: "Telegram", transport: "long_poll", credentialLabel: "Bot token", whereToGetIt: "Create a bot with @BotFather and copy the token it gives you.", docsUrl: "https://core.telegram.org/bots#how-do-i-create-a-bot", configFields: [] },
  { kind: "discord", label: "Discord", transport: "socket", credentialLabel: "Bot token", whereToGetIt: "Discord Developer Portal → your application → Bot → Reset Token. Enable the Message Content intent.", docsUrl: "https://discord.com/developers/docs/topics/gateway", configFields: [] },
  { kind: "slack", label: "Slack", transport: "socket", credentialLabel: "Bot and app tokens (JSON)", whereToGetIt: "Slack API → your app → OAuth (xoxb bot token) and Basic Information (xapp app-level token with connections:write).", docsUrl: "https://api.slack.com/apis/socket-mode", configFields: [] },
  {
    kind: "mattermost", label: "Mattermost", transport: "socket", credentialLabel: "Personal access token",
    whereToGetIt: "Your Mattermost profile → Security → Personal Access Tokens.",
    docsUrl: "https://developers.mattermost.com/integrate/reference/personal-access-token/",
    configFields: [
      { key: "base_url", label: "Server URL", type: "text", required: true, placeholder: "https://chat.example.com", hint: "Your own server's origin. Plain http is accepted only for localhost, so a token can never be walked to an unencrypted host." },
    ],
  },
  {
    kind: "irc", label: "IRC", transport: "socket", credentialLabel: "SASL password", credentialOptional: true,
    whereToGetIt: "The account password registered with the network's services (NickServ). Only needed if you turn SASL on.",
    docsUrl: "https://ircv3.net/specs/extensions/sasl-3.1",
    configFields: [
      { key: "server", label: "Server", type: "text", required: true, placeholder: "irc.libera.chat" },
      { key: "port", label: "Port", type: "number", placeholder: "6697", hint: "TLS is always used; 6697 is the usual TLS port." },
      { key: "nick", label: "Nickname", type: "text", required: true, placeholder: "littlemonkey" },
      { key: "channels", label: "Channels to join", type: "list", placeholder: "#room-one, #room-two" },
      { key: "use_sasl", label: "Log in with SASL", type: "boolean" },
    ],
  },
  {
    kind: "matrix", label: "Matrix", transport: "long_poll", credentialLabel: "Access token",
    whereToGetIt: "Your own homeserver account's access token — in Element: Settings → Help & About → Advanced → Access Token. Treat it like a password. Encrypted rooms work: this app appears as the device that token belongs to, and messages sent before it joined stay unreadable until you verify it from another of your clients.",
    docsUrl: "https://spec.matrix.org/latest/client-server-api/",
    configFields: [
      { key: "homeserver_url", label: "Homeserver", type: "text", required: true, placeholder: "https://matrix.example.org" },
      { key: "user_id", label: "Your user ID", type: "text", required: true, placeholder: "@you:example.org" },
    ],
  },
  {
    kind: "signal", label: "Signal", transport: "helper", credentialLabel: "None — signal-cli holds the account", credentialOptional: true,
    whereToGetIt: "Install signal-cli yourself and register or link your own number with it. Little Monkey never bundles, downloads or installs it, and never reads its account store.",
    docsUrl: "https://github.com/AsamK/signal-cli#installation",
    configFields: [
      { key: "helper_path", label: "signal-cli path", type: "text", required: true, placeholder: "/usr/local/bin/signal-cli" },
      { key: "account", label: "Registered number", type: "text", required: true, placeholder: "+15550000000", hint: "The number signal-cli is registered or linked as." },
    ],
  },
  {
    kind: "imessage", label: "iMessage", transport: "helper", credentialLabel: "None — the helper holds the account", credentialOptional: true, requiresPlatform: "macos",
    whereToGetIt: "macOS only, and only through a helper you install yourself. Grant it the normal macOS permissions it asks for; nothing here disables SIP, injects into Messages, or reads its database directly.",
    docsUrl: "https://support.apple.com/guide/messages/welcome/mac",
    configFields: [
      { key: "helper_path", label: "Helper path", type: "text", required: true, placeholder: "/usr/local/bin/imessage-helper" },
      { key: "handle", label: "Your iMessage handle", type: "text", required: true, placeholder: "you@example.com" },
    ],
  },
  { kind: "whatsapp", label: "WhatsApp", transport: "webhook", credentialLabel: "WhatsApp credentials", whereToGetIt: "Meta for Developers → your app → WhatsApp → API Setup for the access token and phone number ID; App settings → Basic for the app secret. The verify token is yours to invent — type the same value here and into Meta's webhook form.", docsUrl: "https://developers.facebook.com/docs/whatsapp/cloud-api", configFields: [{ key: "phone_number_id", label: "Phone number ID", type: "text", required: true }], secretFields: [{ key: "access_token", label: "Access token" }, { key: "app_secret", label: "App secret" }, { key: "verify_token", label: "Verify token (you choose it)" }] },
  { kind: "line", label: "LINE", transport: "webhook", credentialLabel: "LINE credentials", whereToGetIt: "LINE Developers Console → your channel → Messaging API for the access token, and Basic settings for the channel secret that verifies signatures.", docsUrl: "https://developers.line.biz/en/docs/messaging-api/", configFields: [], secretFields: [{ key: "channel_access_token", label: "Channel access token" }, { key: "channel_secret", label: "Channel secret" }] },
  { kind: "teams", label: "Microsoft Teams", transport: "webhook", credentialLabel: "Client secret", whereToGetIt: "Azure Bot resource → Configuration for the Microsoft App ID and tenant ID; Certificates & secrets for a client secret.", docsUrl: "https://learn.microsoft.com/azure/bot-service/", configFields: [{ key: "app_id", label: "Microsoft App ID", type: "text", required: true }, { key: "tenant_id", label: "Tenant ID", type: "text", required: true }], secretFields: [{ key: "app_password", label: "Client secret" }] },
  { kind: "google_chat", label: "Google Chat", transport: "webhook", credentialLabel: "Service account key (paste the whole JSON file)", whereToGetIt: "Google Cloud Console → Chat API → create a service account and download its key. Paste the file's contents unchanged.", docsUrl: "https://developers.google.com/chat/api/guides/auth", configFields: [{ key: "project_number", label: "Project number", type: "text", required: true }] },
];

/** Turn what the operator typed into the account row's settings object.
 *
 * Empty values are left out entirely rather than stored as `""`: every adapter
 * treats a missing key and a blank one the same way, and an absent key is the
 * one that produces the adapter's own "is missing X" message instead of a
 * confusing validation failure further in. Numbers and booleans are converted
 * here because the daemon parses them by type, not by string. */
export function buildProviderConfig(
  fields: ProviderConfigField[],
  values: Record<string, string>,
): Record<string, unknown> {
  const config: Record<string, unknown> = {};
  for (const field of fields) {
    const raw = (values[field.key] ?? "").trim();
    if (field.type === "boolean") {
      if (raw === "true") config[field.key] = true;
      continue;
    }
    if (raw.length === 0) continue;
    if (field.type === "number") {
      const parsed = Number(raw);
      if (Number.isFinite(parsed)) config[field.key] = parsed;
      continue;
    }
    if (field.type === "list") {
      const entries = raw.split(",").map((entry) => entry.trim()).filter((entry) => entry.length > 0);
      if (entries.length > 0) config[field.key] = entries;
      continue;
    }
    config[field.key] = raw;
  }
  return config;
}

/** Which required settings are still blank, so the form can say so before the
 * daemon has to. */
export function missingRequiredConfig(
  fields: ProviderConfigField[],
  values: Record<string, string>,
): string[] {
  return fields
    .filter((field) => field.required && (values[field.key] ?? "").trim().length === 0)
    .map((field) => field.label);
}

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
