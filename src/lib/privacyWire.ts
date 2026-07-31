import type { ChatContentPart, ChatMessage } from "./llamaClient";
import type { PrivacyGateOutcome } from "../store/privacyFirewallStore";

export interface CachedPrivacyText {
  content: string;
}

export type PrivacyWireCache = Map<string, CachedPrivacyText>;

export type PrivacyWireOutcome =
  | {
      action: "send";
      messages: ChatMessage[];
      newlyRedactedFindings: number;
    }
  | { action: "switch_local" }
  | { action: "cancelled" };

type GateText = (content: string) => Promise<PrivacyGateOutcome>;

async function gateText(
  content: string,
  gate: GateText,
  cache: PrivacyWireCache,
): Promise<
  | { action: "send"; content: string; newlyRedactedFindings: number }
  | { action: "switch_local" }
  | { action: "cancelled" }
> {
  if (content.length === 0) {
    return { action: "send", content, newlyRedactedFindings: 0 };
  }
  const cached = cache.get(content);
  if (cached) {
    return {
      action: "send",
      content: cached.content,
      newlyRedactedFindings: 0,
    };
  }

  const outcome = await gate(content);
  if (outcome.action !== "send") {
    return { action: outcome.action };
  }
  cache.set(content, { content: outcome.content });
  return {
    action: "send",
    content: outcome.content,
    newlyRedactedFindings:
      outcome.content === content
        ? 0
        : outcome.report.findings.filter(
            (finding) => finding.action !== "allow" && !finding.exempted,
          ).length,
  };
}

/**
 * Applies the Privacy Firewall to every textual message that is about to
 * cross a cloud-model boundary while preserving the OpenAI message/tool-call
 * structure exactly. Raw transcript messages are never mutated; only the
 * returned wire copy contains redactions.
 *
 * A turn-scoped cache prevents unchanged history from re-opening the same
 * approval prompt on every tool round-trip or provider failover. Only
 * successful send decisions are cached—cancel/switch decisions remain
 * one-shot control flow and can never become an implicit future approval.
 */
export async function gatePrivacyWireMessages(
  messages: ChatMessage[],
  gate: GateText,
  cache: PrivacyWireCache,
): Promise<PrivacyWireOutcome> {
  let changed = false;
  let newlyRedactedFindings = 0;
  const next: ChatMessage[] = [];

  for (const message of messages) {
    if (typeof message.content === "string") {
      const outcome = await gateText(message.content, gate, cache);
      if (outcome.action !== "send") return outcome;
      newlyRedactedFindings += outcome.newlyRedactedFindings;
      if (outcome.content === message.content) {
        next.push(message);
      } else {
        changed = true;
        next.push({ ...message, content: outcome.content });
      }
      continue;
    }

    let partsChanged = false;
    const parts: ChatContentPart[] = [];
    for (const part of message.content) {
      if (part.type !== "text") {
        parts.push(part);
        continue;
      }
      const outcome = await gateText(part.text, gate, cache);
      if (outcome.action !== "send") return outcome;
      newlyRedactedFindings += outcome.newlyRedactedFindings;
      if (outcome.content === part.text) {
        parts.push(part);
      } else {
        partsChanged = true;
        parts.push({ ...part, text: outcome.content });
      }
    }
    if (partsChanged) {
      changed = true;
      next.push({ ...message, content: parts });
    } else {
      next.push(message);
    }
  }

  return {
    action: "send",
    messages: changed ? next : messages,
    newlyRedactedFindings,
  };
}
