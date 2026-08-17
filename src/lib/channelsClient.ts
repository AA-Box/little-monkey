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
  /**
   * Deliveries this account refused to authenticate since one last verified.
   *
   * The only symptom a rotated signing secret or a stale console URL has:
   * a delivery that fails verification earns no event row, so without this
   * the messages simply stop and every other field on the account still looks
   * correct. `last_reason` is the verifier's own code, never a body or header.
   */
  callback_rejections: { count: number; last_reason: string | null; last_at_ms: number | null };
  /**
   * How this machine recognises a message it sent coming back.
   *
   * `host_adapter` for every built-in provider: the code that reads the
   * provider's payload is the host's own, holding the account's credential.
   * An extension-backed account is the exception — the code that decides is a
   * sandboxed guest — so it must declare `provider_message_id` and supply the
   * provider's own message id, which the host matches against what it recorded
   * sending. `unsupported` means it cannot, and is what an extension built
   * before this contract existed reads as.
   */
  echo_correlation: "host_adapter" | "provider_message_id" | "unsupported";
  /**
   * True when the stored reply policy is *not* the one in force.
   *
   * An account that cannot recognise its own echo may not answer anyone, or
   * answer every message in a group: both can talk to themselves forever. The
   * ingress narrows the policy rather than refusing traffic, so this flag is
   * the only way the panel can say the stored setting is not what is running.
   */
  reply_policy_restricted: boolean;
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

/** Which durable session a conversation maps onto. Matches the daemon's
 * `SessionScope`. */
export type SessionScope = "thread" | "conversation" | "sender" | "account";

/** How much of a message's identity a route pins down. Every field is
 * optional; which ones are set decides the rung the route sits on, and the
 * daemon rejects the combinations that are not on the ladder. */
export interface ChannelRouteScope {
  account_id?: string;
  kind?: string;
  conversation_id?: string;
  thread_id?: string;
  sender_id?: string;
}

/** What a matching message runs as. Mirrors the daemon's `RouteTarget`. */
export interface ChannelRouteTarget {
  recipe: string;
  params?: Record<string, string>;
  repository?: string;
  session_scope: SessionScope;
  priority: number;
  reply_to_conversation: boolean;
}

export interface ChannelRoute {
  route_id: string;
  scope: ChannelRouteScope;
  target: ChannelRouteTarget;
  enabled: boolean;
  created_at_ms: number;
  updated_at_ms: number;
}

/** The rungs of the routing ladder, most specific first — the same order
 * `resolve_route` walks. */
export const ROUTE_SPECIFICITY = [
  "sender",
  "thread",
  "conversation",
  "account",
  "channel_default",
  "global_default",
] as const;

export type RouteSpecificity = (typeof ROUTE_SPECIFICITY)[number];

/** Which rung a scope sits on, computed the same way the daemon computes it.
 * Display only — the daemon remains the authority on what a scope means. */
export function routeSpecificity(scope: ChannelRouteScope): RouteSpecificity {
  if (scope.sender_id) return "sender";
  if (scope.thread_id) return "thread";
  if (scope.conversation_id) return "conversation";
  if (scope.account_id) return "account";
  if (scope.kind) return "channel_default";
  return "global_default";
}

/** The scope and target fields `channels_add_route`/`channels_update_route`
 * accept, exactly the daemon's `RouteOptionArgs`. Sent as one object, so the
 * keys stay the daemon's snake_case rather than being renamed in flight. */
export interface RouteOptions {
  account_id?: string | null;
  conversation_id?: string | null;
  thread_id?: string | null;
  sender_id?: string | null;
  kind?: string | null;
  repository?: string | null;
  /** Recipe parameters as `name=value` strings. */
  params?: string[];
  session_scope?: SessionScope | null;
  priority?: number | null;
  /** Whether runs of this route may answer their conversation. Defaults on. */
  reply?: boolean | null;
  /** Whether the route is active. Defaults on. */
  enabled?: boolean | null;
}

/** The complete callback URL for one webhook account, as the daemon composes
 * it. `configured: false` means no public base URL is set — the frontend shows
 * `path` and says so, and never glues a host on itself. */
export interface ChannelCallback {
  account_id: string;
  configured: boolean;
  url: string | null;
  path: string;
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
  /** True for accounts another subsystem creates (SMS shadows a telephony
   * account). Their settings are editable here, but the add flow must not
   * offer to create one directly. */
  editOnly?: boolean;
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
    whereToGetIt: "Your Mattermost profile → Security → Personal Access Tokens. Messages arrive over a WebSocket, so health reports the connection as well as the token: an account whose token works but whose socket is down is shown as degraded rather than connected, and so is one dropping posts because replies are running behind.",
    docsUrl: "https://developers.mattermost.com/integrate/reference/personal-access-token/",
    configFields: [
      { key: "base_url", label: "Server URL", type: "text", required: true, placeholder: "https://chat.example.com", hint: "Your own server's origin. Plain http is accepted only for localhost, so a token can never be walked to an unencrypted host." },
    ],
  },
  {
    kind: "irc", label: "IRC", transport: "socket", credentialLabel: "SASL password", credentialOptional: true,
    whereToGetIt: "The account password registered with the network's services (NickServ). Only needed if you turn SASL on. If the nickname you ask for is already in use, the connection takes the next free one (littlemonkey_, littlemonkey_2, \u2026) and health shows which one it ended up with. With SASL on, a failed login stays a failure: the connection is never completed as an anonymous one, and health names the account that could not authenticate.",
    docsUrl: "https://ircv3.net/specs/extensions/sasl-3.1",
    configFields: [
      { key: "server", label: "Server", type: "text", required: true, placeholder: "irc.libera.chat" },
      { key: "port", label: "Port", type: "number", placeholder: "6697", hint: "TLS is always used; 6697 is the usual TLS port." },
      { key: "nick", label: "Nickname", type: "text", required: true, placeholder: "littlemonkey" },
      { key: "channels", label: "Channels to join", type: "list", placeholder: "#room-one, #room-two" },
      { key: "use_sasl", label: "Log in with SASL", type: "boolean" },
      { key: "sasl_username", label: "SASL account (optional)", type: "text", placeholder: "littlemonkey", hint: "The account name registered with the network's services, when it differs from the nickname. Leave empty to authenticate as the nickname. If the nickname is taken, the connection takes the next free one \u2014 but always authenticates as this account." },
    ],
  },
  {
    // Socket, not long-poll: the SDK holds `/sync` open in a background task
    // and dispatches to handlers, which is the same shape as Discord's gateway
    // and Mattermost's WebSocket. The daemon's own capability says
    // `InboundTransport::Socket`, and there is one truthful answer.
    kind: "matrix", label: "Matrix", transport: "socket", credentialLabel: "Access token",
    whereToGetIt: "Your own homeserver account's access token \u2014 in Element: Settings \u2192 Help & About \u2192 Advanced \u2192 Access Token. Treat it like a password. Encrypted rooms, encrypted files and threads all work: this app appears as the device that token already belongs to rather than adding a new one, and it keeps that device across restarts. Messages sent before it joined stay unreadable until you verify it from another of your clients. If it ever cannot tell whether a room is encrypted, it refuses to send rather than risk sending in the clear.",
    docsUrl: "https://spec.matrix.org/latest/client-server-api/",
    configFields: [
      { key: "homeserver_url", label: "Homeserver", type: "text", required: true, placeholder: "https://matrix.example.org" },
      { key: "user_id", label: "Your user ID", type: "text", required: true, placeholder: "@you:example.org" },
      { key: "device_id", label: "Device ID (optional)", type: "text", placeholder: "ABCDEFGHIJ", hint: "The session this access token belongs to, as your other Matrix client lists it. Leave empty to ask the homeserver \u2014 either way, no new device is ever registered." },
    ],
  },
  {
    kind: "signal", label: "Signal", transport: "helper", credentialLabel: "None — signal-cli holds the account", credentialOptional: true,
    whereToGetIt: "Install signal-cli yourself and register or link your own number with it. Little Monkey never bundles, downloads or installs it, and never reads its account store. Health checks that this number is actually registered with the helper, not just that the helper starts.",
    docsUrl: "https://github.com/AsamK/signal-cli#installation",
    configFields: [
      { key: "helper_path", label: "signal-cli path", type: "text", required: true, placeholder: "/usr/local/bin/signal-cli" },
      { key: "account", label: "Registered number", type: "text", required: true, placeholder: "+15550000000", hint: "The number signal-cli is registered or linked as." },
    ],
  },
  {
    kind: "imessage", label: "iMessage", transport: "helper", credentialLabel: "None — macOS holds the account", credentialOptional: true, requiresPlatform: "macos",
    whereToGetIt: "macOS only, using the Mac you are already signed in to Messages on. Install little-monkey-imessage-helper and grant it two normal macOS permissions: Full Disk Access, so the Messages database can be read, and Automation for Messages, so replies can be sent. Both are checked for real \u2014 health tells you which one is still missing, and whether Messages has a usable account \u2014 rather than assumed from the helper being installed. The helper holds both permissions; Little Monkey itself never opens the Messages database, never sends an Apple event, and never learns where an attachment is stored. Nothing disables SIP, injects into Messages, or asks for your Apple ID password.",
    docsUrl: "https://support.apple.com/guide/messages/welcome/mac",
    configFields: [
      { key: "handle", label: "Your iMessage handle", type: "text", required: true, placeholder: "you@example.com" },
      { key: "helper_path", label: "Helper path", type: "text", required: true, placeholder: "/usr/local/bin/little-monkey-imessage-helper", hint: "Where you installed the helper. Health reports which permission is still missing if either grant has not been made yet." },
    ],
  },
  { kind: "whatsapp", label: "WhatsApp", transport: "webhook", credentialLabel: "WhatsApp credentials", whereToGetIt: "Meta for Developers → your app → WhatsApp → API Setup for the access token and phone number ID; App settings → Basic for the app secret. The verify token is yours to invent — type the same value here and into Meta's webhook form. Paste the callback URL below into Configuration → Callback URL and subscribe to the messages field.", docsUrl: "https://developers.facebook.com/docs/whatsapp/cloud-api", configFields: [{ key: "phone_number_id", label: "Phone number ID", type: "text", required: true }], secretFields: [{ key: "access_token", label: "Access token" }, { key: "app_secret", label: "App secret" }, { key: "verify_token", label: "Verify token (you choose it)" }] },
  { kind: "line", label: "LINE", transport: "webhook", credentialLabel: "LINE credentials", whereToGetIt: "LINE Developers Console → your channel → Messaging API for the access token, and Basic settings for the channel secret that verifies signatures. Paste the callback URL below into Webhook URL, then turn Use webhook on and press Verify.", docsUrl: "https://developers.line.biz/en/docs/messaging-api/", configFields: [], secretFields: [{ key: "channel_access_token", label: "Channel access token" }, { key: "channel_secret", label: "Channel secret" }] },
  { kind: "teams", label: "Microsoft Teams", transport: "webhook", credentialLabel: "Client secret", whereToGetIt: "Azure Bot resource → Configuration for the Microsoft App ID and tenant ID; Certificates & secrets for a client secret. Paste the callback URL below into that same Configuration page as the Messaging endpoint. The public Bot Framework cloud is the one supported: a bot in a sovereign cloud will not authenticate here.", docsUrl: "https://learn.microsoft.com/azure/bot-service/", configFields: [{ key: "app_id", label: "Microsoft App ID", type: "text", required: true }, { key: "tenant_id", label: "Tenant ID", type: "text", required: true }], secretFields: [{ key: "app_password", label: "Client secret" }] },
  { kind: "google_chat", label: "Google Chat", transport: "webhook", credentialLabel: "Service account key (paste the whole JSON file)", whereToGetIt: "Google Cloud Console → Chat API → create a service account and download its key. Paste the file's contents unchanged. Then on the same API's Configuration page set exactly these two, or deliveries are refused — Connection settings: HTTP endpoint URL (paste the callback URL below into it), and Authentication Audience: Project Number. App URL is the other Authentication Audience value and is not supported here.", docsUrl: "https://developers.google.com/chat/api/guides/auth", configFields: [{ key: "project_number", label: "Project number", type: "text", required: true, placeholder: "123456789012", hint: "The Cloud project number shown on the Chat API page. Authentication Audience must be set to Project Number, and this is what every delivery's token is checked against." }, { key: "bot_user_name", label: "Bot user name (optional)", type: "text", placeholder: "users/1234567890", hint: "The app's own Chat user resource name, used to recognize mentions of itself." }] },
  {
    kind: "sms", label: "SMS", transport: "webhook", credentialLabel: "None here — the carrier credential lives on the telephony account", credentialOptional: true, editOnly: true,
    whereToGetIt: "An SMS account is created automatically for a telephony account of the same id; configure the carrier and its credential under Telephony.",
    docsUrl: "https://www.twilio.com/docs/usage/webhooks/sms-webhooks",
    configFields: [
      { key: "webhook_public_key", label: "Webhook public key (optional)", type: "text", hint: "The carrier's signing key for inbound webhook verification, when the carrier publishes one." },
      { key: "session_scope", label: "Session scope (optional)", type: "text", placeholder: "conversation", hint: "Which durable session a text thread maps onto." },
    ],
  },
  {
    // One entry for every extension-backed provider, because the persisted
    // kind is a *kind*: which installation and which of its channel
    // capabilities an account belongs to is the account's own configuration,
    // validated on write and re-resolved on every use.
    kind: "extension", label: "Executable extension", transport: "long_poll",
    credentialLabel: "None here — the extension holds its own credentials in its declared secret slots",
    credentialOptional: true,
    whereToGetIt: "Install a channel extension under Settings > Extensions, grant it the origins it asks for, fill in its secret slots, then name it here.",
    docsUrl: "https://github.com/sarollahi/little-monkey/blob/develop/docs/executable-extensions.md",
    configFields: [
      { key: "extension_id", label: "Extension", type: "text", required: true, placeholder: "dev.example.chat", hint: "The installed extension that speaks for this account. Both this and the capability are required: naming only the capability would resolve to whichever extension declares that id today." },
      { key: "capability_id", label: "Channel capability", type: "text", required: true, placeholder: "room", hint: "Which of that extension's declared channel capabilities this account uses." },
    ],
  },
];

/** Keys every account accepts regardless of provider: the per-account
 * attachment knobs the daemon's `AttachmentLimits::for_account` reads. They
 * bound what one inbound message may cost and what one outbound reply may
 * carry, so they are edited in the account's advanced section rather than
 * hidden behind the terminal. */
export const UNIVERSAL_CONFIG_FIELDS: ProviderConfigField[] = [
  { key: "max_attachment_bytes", label: "Max attachment size (bytes)", type: "number", placeholder: "16777216", hint: "Per file, inbound and outbound. The application ceiling still applies." },
  { key: "max_attachment_excerpt_chars", label: "Max text excerpt (characters)", type: "number", placeholder: "4000", hint: "How much of an inbound text file is quoted into the conversation." },
  { key: "max_listed_attachments", label: "Max attachments per message", type: "number", placeholder: "8", hint: "How many files one message may list or carry." },
];

/** Every non-secret setting an account of this kind can hold: the provider's
 * own schema plus the universal attachment knobs. Exactly what the daemon's
 * `validate_non_secret_config` accepts — the contract test holds the two
 * sides together. */
export function editableConfigFields(kind: string): ProviderConfigField[] {
  const guide = PROVIDER_GUIDES.find((entry) => entry.kind === kind);
  return [...(guide?.configFields ?? []), ...UNIVERSAL_CONFIG_FIELDS];
}

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
export const channelsRoutes = () => invoke<{ routes: ChannelRoute[] }>("channels_routes");
export const channelsAddRoute = (recipe: string, options: RouteOptions) =>
  invoke<{ route: ChannelRoute }>("channels_add_route", { recipe, options });
export const channelsUpdateRoute = (routeId: string, recipe: string, options: RouteOptions) =>
  invoke<{ route: ChannelRoute }>("channels_update_route", { routeId, recipe, options });
export const channelsEnableRoute = (routeId: string, enabled: boolean) =>
  invoke<void>("channels_enable_route", { routeId, enabled });
export const channelsRemoveRoute = (routeId: string) => invoke<void>("channels_remove_route", { routeId });
export const channelsEvents = (accountId: string, limit = 20) =>
  invoke<{ events: ChannelEvent[] }>("channels_events", { accountId, limit });
export const channelsRemove = (accountId: string) => invoke<void>("channels_remove", { accountId });
/** Replace an existing account's non-secret settings and/or label. The
 * credential is untouched: it does not travel through this command in either
 * direction. */
export const channelsSetConfig = (
  accountId: string,
  config: string | null,
  label: string | null,
) => invoke<ChannelAccount>("channels_set_config", { accountId, config, label });
export const channelsCallbackUrl = (accountId: string) =>
  invoke<ChannelCallback>("channels_callback_url", { accountId });
export const channelsSetPublicUrl = (url: string | null) =>
  invoke<void>("channels_set_public_url", { url });

/** Mirrors the Rust `callback_exposure::ExposureMode` exactly. */
export type ExposureMode = "manual" | "managed_tunnel";

/** Mirrors the Rust `callback_exposure::TunnelProvider` exactly. A closed set:
 * the daemon builds the argv from its own template and the operator supplies a
 * validated path, so this is never a command. */
export type TunnelProvider = "cloudflared";

/** Mirrors the Rust `callback_exposure::ExposureState` exactly.
 *
 * Every value is a different thing to do about it, which is why they are not
 * collapsed into "ok" and "broken": a missing helper, a missing credential and
 * a rejected credential need three different sentences. Only `connected` means
 * a provider posting to the public URL would reach this machine. */
export type ExposureState =
  | "not_configured"
  | "helper_missing"
  | "credential_missing"
  | "connecting"
  | "connected"
  | "degraded"
  | "reconnecting"
  | "authentication_failed"
  | "public_url_unavailable"
  | "stopped";

/** Mirrors the Rust `callback_exposure::ExposureStatus` exactly.
 *
 * Note what is not on it: no token, no argv, no pid. `credentialStored` is a
 * boolean about whether the keychain holds something, which is the most the
 * frontend is ever told about a secret. */
export interface ExposureStatus {
  mode: ExposureMode;
  provider?: TunnelProvider;
  state: ExposureState;
  publicBase?: string;
  credentialStored: boolean;
  executable?: string;
  lastError?: string;
  restarts: number;
  sinceMs?: number;
}

export const channelsExposureStatus = () =>
  invoke<ExposureStatus>("channels_exposure_status");
export const channelsExposureManual = () => invoke<void>("channels_exposure_manual");
export const channelsExposureSetTunnel = (
  provider: TunnelProvider,
  hostname: string,
  executable: string,
  metricsPort: number | null,
) =>
  invoke<void>("channels_exposure_set_tunnel", {
    provider,
    hostname,
    executable,
    metricsPort,
  });
/** The token goes straight to the OS keychain. It is never stored in React
 * state beyond the keystroke that submits it, and never comes back. */
export const channelsExposureSetToken = (token: string) =>
  invoke<void>("channels_exposure_set_token", { token });
export const channelsExposureClearToken = () => invoke<void>("channels_exposure_clear_token");

/** One sentence per state, in the operator's terms rather than the daemon's.
 *
 * A pure function so a test can assert every state is covered — a state that
 * fell through to a default would render as a blank line exactly when
 * something was wrong. */
export function describeExposure(status: ExposureStatus): { tone: "ok" | "warn" | "bad"; text: string } {
  switch (status.state) {
    case "connected":
      return { tone: "ok", text: "Connected. Deliveries to the public URL reach this machine." };
    case "connecting":
      return { tone: "warn", text: "Starting your tunnel…" };
    case "reconnecting":
      return {
        tone: "warn",
        text: `Reconnecting after ${status.restarts} restart${status.restarts === 1 ? "" : "s"}.`,
      };
    case "degraded":
      return { tone: "warn", text: "Running, but it has not reported a live connection." };
    case "helper_missing":
      return { tone: "bad", text: "The tunnel client is not at the path given. Install it, or correct the path." };
    case "credential_missing":
      return { tone: "bad", text: "No tunnel token is stored yet." };
    case "authentication_failed":
      return { tone: "bad", text: "Your tunnel provider rejected the stored token. Issue a new one." };
    case "public_url_unavailable":
      return { tone: "bad", text: "No hostname is set, so there is no URL to give a provider." };
    case "stopped":
      return { tone: "warn", text: "Stopped. Nothing is exposing this machine." };
    case "not_configured":
      return { tone: "ok", text: "You publish the URL for this machine yourself." };
  }
}

/** Whether this provider needs the operator to expose a public callback URL
 * on the channels listener. Setup asks for one only when it is genuinely
 * required. SMS is webhook-delivered too, but to the telephony listener —
 * its callback belongs to the telephony account, and showing the channels
 * path for it would hand the operator a URL nothing answers. */
export function needsPublicCallback(kind: string): boolean {
  return (
    kind !== "sms" && PROVIDER_GUIDES.find((guide) => guide.kind === kind)?.transport === "webhook"
  );
}

/** Merge edited guide fields back over an account's stored settings.
 *
 * `channels set-config` replaces the settings object wholesale, so anything
 * the panel does not render has to be carried across explicitly — an account
 * configured from the terminal can hold keys no provider guide describes
 * (per-account attachment limits, say), and silently dropping them on an
 * unrelated edit would be a data loss the operator never asked for. */
export function mergeProviderConfig(
  existing: Record<string, unknown>,
  fields: ProviderConfigField[],
  values: Record<string, string>,
): Record<string, unknown> {
  const edited = buildProviderConfig(fields, values);
  const untouched = Object.fromEntries(
    Object.entries(existing).filter(([key]) => !fields.some((field) => field.key === key)),
  );
  return { ...untouched, ...edited };
}

/** An account's stored settings as the edit form's string values, so the form
 * starts from what is actually configured rather than blank. */
export function configFormValues(
  fields: ProviderConfigField[],
  config: Record<string, unknown>,
): Record<string, string> {
  const values: Record<string, string> = {};
  for (const field of fields) {
    const value = config[field.key];
    if (value === undefined || value === null) {
      values[field.key] = field.type === "boolean" ? "false" : "";
    } else if (Array.isArray(value)) {
      values[field.key] = value.join(", ");
    } else {
      values[field.key] = String(value);
    }
  }
  return values;
}
