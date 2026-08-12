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
  inbound_policy: InboundCallPolicy;
  outbound_approval: OutboundCallApproval;
  limits: CallLimits;
  health: {
    state: ChannelHealthState;
    detail: string | null;
    last_error: string | null;
    probed_at_ms: number;
  };
  updated_at_ms: number;
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
  return gaps;
}
