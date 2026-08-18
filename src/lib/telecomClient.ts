import { invoke } from "@tauri-apps/api/core";

/**
 * Telephony — the operator's own carrier accounts, over the typed daemon
 * bridge.
 *
 * Every call here wraps the same `monkey telecom` subcommand the terminal uses.
 * Nothing in this file talks to a carrier, and no carrier credential is ever
 * read back: the account only reports that one exists.
 */

import type { ChannelHealthState } from "./channelsClient";

/** What this number does when it rings. */
export type InboundCallPolicy = "reject" | "voicemail" | "answer";
/** Whether the agent may dial out from this number, and under what gate.
 * Separate from {@link InboundCallPolicy} because they are separate grants. */
export type OutboundCallApproval = "never" | "approval" | "allow";

export interface CallLimits {
  max_concurrent_calls: number;
  ring_timeout_s: number;
  max_duration_s: number;
  recording_enabled: boolean;
}

export interface TelecomAccount {
  account_id: string;
  kind: string;
  kind_label: string;
  label: string;
  enabled: boolean;
  carrier_account_id: string;
  from_number: string;
  has_credential: boolean;
  public_base_url: string | null;
  /** What the number says when an answered call connects. Without one, a
   * caller hears silence until they speak first. */
  greeting: string | null;
  /** Whether this carrier can record a call it is also streaming. Plivo cannot:
   * its recording element and its stream cannot both run. */
  supports_recording: boolean;
  inbound_policy: InboundCallPolicy;
  outbound_approval: OutboundCallApproval;
  limits: CallLimits;
  health: {
    state: ChannelHealthState;
    detail: string | null;
    last_error: string | null;
    probed_at_ms: number;
  };
  /** Callbacks this account has refused since one last verified. A carrier
   * posting to a URL whose signature never checks out has no other symptom:
   * texts and calls simply never arrive. */
  callback_rejections: {
    count: number;
    last_reason: string | null;
    last_at_ms: number | null;
  };
  updated_at_ms: number;
}

/** One recent text on a number, either direction. */
export interface TelecomMessage {
  direction: "inbound" | "outbound";
  peer_number: string;
  text: string;
  /** The disposition an inbound message got from the messaging gate, or the
   * outbox state an outbound one is in. */
  state: string;
  /** The carrier's separate answer to "did it arrive?". `null` until a receipt
   * lands — and forever on a carrier that sends none, which is not the same as
   * "not delivered". */
  delivery_state: string | null;
  error: string | null;
  at_ms: number;
}

export interface TelecomCall {
  call_id: string;
  direction: "inbound" | "outbound";
  peer_number: string;
  state: string;
  last_error: string | null;
  started_at_ms: number | null;
  ended_at_ms: number | null;
  created_at_ms: number;
}

/** What each carrier needs from the operator. Rendered by setup, so it is
 * explicit about prerequisites rather than assuming they are known. */
export interface CarrierGuide {
  kind: string;
  label: string;
  /** The non-secret account identifier the carrier issues. */
  accountIdLabel: string;
  credentialLabel: string;
  whereToGetIt: string;
  docsUrl: string;
  /** Extra non-secret settings, as JSON keys. */
  configKeys: string[];
}

export const CARRIER_GUIDES: CarrierGuide[] = [
  {
    kind: "twilio",
    label: "Twilio",
    accountIdLabel: "Account SID",
    credentialLabel: "Auth token",
    whereToGetIt:
      "Twilio Console → Account Info. Copy the Account SID and Auth Token, and buy a number with Voice and SMS enabled.",
    docsUrl: "https://www.twilio.com/docs/usage/api",
    configKeys: [],
  },
  {
    kind: "telnyx",
    label: "Telnyx",
    accountIdLabel: "API user / connection id",
    credentialLabel: "API key",
    whereToGetIt:
      "Telnyx Portal → API Keys. Also copy the public key from Account Settings → Keys & Credentials: callbacks are verified with it.",
    docsUrl: "https://developers.telnyx.com/docs/api/v2/overview",
    configKeys: ["webhook_public_key"],
  },
  {
    kind: "plivo",
    label: "Plivo",
    accountIdLabel: "Auth ID",
    credentialLabel: "Auth token",
    whereToGetIt: "Plivo Console → Overview. Copy the Auth ID and Auth Token.",
    docsUrl: "https://www.plivo.com/docs/",
    configKeys: [],
  },
];

export const telecomList = () => invoke<TelecomAccount[]>("telecom_list");
export const telecomAdd = (
  kind: string,
  label: string,
  carrierAccountId: string,
  fromNumber: string,
  publicUrl: string | null,
  config: string | null,
) =>
  invoke<TelecomAccount>("telecom_add", {
    kind,
    label,
    carrierAccountId,
    fromNumber,
    publicUrl,
    config,
  });
export const telecomProbe = (accountId: string) =>
  invoke<{
    account_id: string;
    state: ChannelHealthState;
    detail: string | null;
    last_error: string | null;
  }>("telecom_probe", { accountId });
export const telecomEnable = (accountId: string, enabled: boolean) =>
  invoke<void>("telecom_enable", { accountId, enabled });
export const telecomSetCredential = (accountId: string, secret: string) =>
  invoke<void>("telecom_set_credential", { accountId, secret });
export const telecomSetPolicy = (
  accountId: string,
  inbound: InboundCallPolicy | null,
  outbound: OutboundCallApproval | null,
) => invoke<void>("telecom_set_policy", { accountId, inbound, outbound });
export const telecomSetLimits = (
  accountId: string,
  limits: Partial<{
    maxConcurrent: number;
    ringTimeoutS: number;
    maxDurationS: number;
    recording: boolean;
  }>,
) =>
  invoke<void>("telecom_set_limits", {
    accountId,
    maxConcurrent: limits.maxConcurrent ?? null,
    ringTimeoutS: limits.ringTimeoutS ?? null,
    maxDurationS: limits.maxDurationS ?? null,
    recording: limits.recording ?? null,
  });
export const telecomSetGreeting = (accountId: string, text: string) =>
  invoke<void>("telecom_set_greeting", { accountId, text });
export const telecomMessages = (accountId: string, limit = 20) =>
  invoke<TelecomMessage[]>("telecom_messages", { accountId, limit });
/** Point a carrier somewhere else, or update its non-secret settings. Passing
 * neither clears the public URL. */
export const telecomSetPublicUrl = (
  accountId: string,
  url: string | null,
  config: string | null = null,
) => invoke<void>("telecom_set_public_url", { accountId, url, config });
export const telecomCalls = (accountId: string, limit = 20) =>
  invoke<TelecomCall[]>("telecom_calls", { accountId, limit });
export const telecomCallbackUrl = (accountId: string) =>
  invoke<{ account_id: string; callback_url: string | null }>("telecom_callback_url", {
    accountId,
  });
export const telecomRemove = (accountId: string) => invoke<void>("telecom_remove", { accountId });

/** The path the operator points their carrier's console at, under whatever
 * public base URL they configured. */
export function callbackPath(accountId: string): string {
  return `/v1/telecom/${accountId}`;
}

/** Where a carrier reports what became of a message or a call, as opposed to
 * asking what to do with a live one.
 *
 * Two paths because the replies differ: the answer URL is answered with the
 * markup that connects a call, and this one with an acknowledgement. Every
 * outbound request this app makes already carries it; an operator only needs
 * it for their number's own status callbacks. */
export function statusCallbackPath(accountId: string): string {
  return `${callbackPath(accountId)}/status`;
}

/** The full URL the operator pastes into their carrier's console, or `null`
 * when they have not configured a public base yet.
 *
 * The daemon rebuilds this exact string to check a Twilio or Plivo signature,
 * so a console pointed at anything else rejects every genuine callback. */
export function callbackUrl(account: TelecomAccount): string | null {
  if (account.public_base_url === null) return null;
  return `${account.public_base_url.replace(/\/$/, "")}${callbackPath(account.account_id)}`;
}

/** The status-callback URL under the operator's own public base. */
export function statusCallbackUrl(account: TelecomAccount): string | null {
  if (account.public_base_url === null) return null;
  return `${account.public_base_url.replace(/\/$/, "")}${statusCallbackPath(account.account_id)}`;
}

/** Whether this account can actually hold a conversation, as opposed to only
 * recording that the phone rang. Both halves are needed: somewhere for the
 * carrier to post, and a policy that says to answer. */
export function canAnswerCalls(account: TelecomAccount): boolean {
  return (
    account.enabled &&
    account.has_credential &&
    account.public_base_url !== null &&
    account.inbound_policy !== "reject"
  );
}

/** What is missing before this account works, in the order the operator should
 * fix it. Empty when the account is ready. */
export function setupGaps(account: TelecomAccount): string[] {
  const gaps: string[] = [];
  if (!account.has_credential) gaps.push("credential");
  if (account.public_base_url === null) gaps.push("public_url");
  if (!account.enabled) gaps.push("enabled");
  if (account.health.state !== "connected") gaps.push("probe");
  // A number that answers without a greeting connects the caller to silence.
  if (account.inbound_policy !== "reject" && !account.greeting) gaps.push("greeting");
  // Everything above can look right while the carrier's own console points
  // somewhere else, and this is the only symptom of that.
  if (account.callback_rejections.count > 0) gaps.push("callbacks_rejected");
  return gaps;
}
