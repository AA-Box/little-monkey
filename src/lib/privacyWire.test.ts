import { describe, expect, it, vi } from "vitest";

import type { ChatMessage } from "./llamaClient";
import {
  gatePrivacyWireMessages,
  type PrivacyWireCache,
} from "./privacyWire";
import type {
  PrivacyGateOutcome,
  PrivacyPreviewReport,
} from "../store/privacyFirewallStore";

function report(content: string, redactedPreview = content): PrivacyPreviewReport {
  return {
    destination: "cloud_model",
    workspaceId: "/workspace",
    verdict: content === redactedPreview ? "allow" : "redact",
    findings:
      content === redactedPreview
        ? []
        : [
            {
              kind: "api_credential",
              byteStart: 0,
              byteEnd: content.length,
              line: 1,
              column: 1,
              maskedPreview: "***",
              action: "redact",
              exempted: false,
            },
          ],
    redactedPreview,
    originalSha256: "digest",
    localOnlyFallbackAvailable: true,
    contentLength: content.length,
  };
}

function send(content: string, redactedPreview = content): PrivacyGateOutcome {
  return {
    action: "send",
    content: redactedPreview,
    report: report(content, redactedPreview),
  };
}

describe("gatePrivacyWireMessages", () => {
  it("redacts tool output without mutating transcript or tool-call pairing", async () => {
    const messages: ChatMessage[] = [
      {
        role: "assistant",
        content: "",
        tool_calls: [
          {
            id: "call-1",
            type: "function",
            function: { name: "read_file", arguments: "{}" },
          },
        ],
      },
      {
        role: "tool",
        tool_call_id: "call-1",
        content: "API_KEY=secret",
      },
    ];
    const gate = vi.fn(async (content: string) =>
      content.includes("secret") ? send(content, "API_KEY=[REDACTED]") : send(content),
    );

    const outcome = await gatePrivacyWireMessages(messages, gate, new Map());

    expect(outcome).toMatchObject({
      action: "send",
      newlyRedactedFindings: 1,
    });
    if (outcome.action !== "send") throw new Error("expected send");
    expect(outcome.messages[1]).toEqual({
      role: "tool",
      tool_call_id: "call-1",
      content: "API_KEY=[REDACTED]",
    });
    expect(outcome.messages[0].tool_calls?.[0].id).toBe("call-1");
    expect(messages[1].content).toBe("API_KEY=secret");
  });

  it("preserves image parts while gating their adjacent text", async () => {
    const messages: ChatMessage[] = [
      {
        role: "user",
        content: [
          { type: "text", text: "email me@example.com" },
          { type: "image_url", image_url: { url: "data:image/png;base64,abc" } },
        ],
      },
    ];
    const outcome = await gatePrivacyWireMessages(
      messages,
      async (content) => send(content, "email [REDACTED]"),
      new Map(),
    );
    if (outcome.action !== "send") throw new Error("expected send");
    expect(outcome.messages[0].content).toEqual([
      { type: "text", text: "email [REDACTED]" },
      { type: "image_url", image_url: { url: "data:image/png;base64,abc" } },
    ]);
  });

  it("stops before later messages when the user cancels", async () => {
    const gate = vi
      .fn<(content: string) => Promise<PrivacyGateOutcome>>()
      .mockResolvedValueOnce(send("first"))
      .mockResolvedValueOnce({
        action: "cancelled",
        report: report("second"),
      });
    const outcome = await gatePrivacyWireMessages(
      [
        { role: "system", content: "first" },
        { role: "tool", tool_call_id: "call", content: "second" },
        { role: "user", content: "must-not-scan" },
      ],
      gate,
      new Map(),
    );
    expect(outcome).toEqual({ action: "cancelled" });
    expect(gate).toHaveBeenCalledTimes(2);
  });

  it("propagates a local-switch decision without caching it", async () => {
    const cache: PrivacyWireCache = new Map();
    const outcome = await gatePrivacyWireMessages(
      [{ role: "tool", tool_call_id: "call", content: "private" }],
      async () => ({
        action: "switch_local",
        report: report("private"),
      }),
      cache,
    );
    expect(outcome).toEqual({ action: "switch_local" });
    expect(cache.size).toBe(0);
  });

  it("reuses successful decisions across unchanged tool rounds and failover", async () => {
    const cache: PrivacyWireCache = new Map();
    const gate = vi.fn(async (content: string) => send(content, "[REDACTED]"));
    const messages: ChatMessage[] = [
      { role: "tool", tool_call_id: "call", content: "secret" },
    ];

    const first = await gatePrivacyWireMessages(messages, gate, cache);
    const second = await gatePrivacyWireMessages(messages, gate, cache);

    expect(gate).toHaveBeenCalledTimes(1);
    expect(first).toMatchObject({ action: "send", newlyRedactedFindings: 1 });
    expect(second).toMatchObject({ action: "send", newlyRedactedFindings: 0 });
    if (second.action !== "send") throw new Error("expected send");
    expect(second.messages[0].content).toBe("[REDACTED]");
  });
});
