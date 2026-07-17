import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn(async (..._args: unknown[]): Promise<unknown> => null);
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args) }));

import { usePrivacyFirewallStore, type PrivacyPolicy, type PrivacyPreviewReport } from "./privacyFirewallStore";

interface PrivacySendConfirmationLike {
  digest: string;
  confirmationPhrase: string;
  report: PrivacyPreviewReport;
  expiresAtMs: number;
}

function basePolicy(overrides: Partial<PrivacyPolicy> = {}): PrivacyPolicy {
  return {
    workspaceId: "workspace-1",
    actions: {
      private_key: "block",
      api_credential: "block",
      credit_card: "block",
      email: "redact",
      phone: "redact",
      ip_address: "redact",
    },
    localOnlyFallback: true,
    exceptions: [],
    ...overrides,
  };
}

function baseReport(overrides: Partial<PrivacyPreviewReport> = {}): PrivacyPreviewReport {
  return {
    destination: "cloud_model",
    workspaceId: "workspace-1",
    verdict: "allow",
    findings: [],
    redactedPreview: "hello",
    originalSha256: "deadbeef",
    localOnlyFallbackAvailable: true,
    contentLength: 5,
    ...overrides,
  };
}

beforeEach(() => {
  invokeMock.mockReset();
  usePrivacyFirewallStore.setState({ policies: {}, busy: false, error: null, pendingApproval: null });
});

describe("privacyFirewallStore", () => {
  it("loadPolicy calls invoke with the exact command name and caches the result", async () => {
    const policy = basePolicy();
    invokeMock.mockResolvedValueOnce(policy);

    const result = await usePrivacyFirewallStore.getState().loadPolicy("workspace-1");

    expect(invokeMock).toHaveBeenCalledWith("privacy_firewall_get_policy", { workspaceId: "workspace-1" });
    expect(result).toEqual(policy);
    expect(usePrivacyFirewallStore.getState().policies["workspace-1"]).toEqual(policy);
  });

  it("savePolicy calls invoke with the policy payload and updates the cache from the response", async () => {
    const policy = basePolicy();
    invokeMock.mockResolvedValueOnce(policy);

    await usePrivacyFirewallStore.getState().savePolicy(policy);

    expect(invokeMock).toHaveBeenCalledWith("privacy_firewall_save_policy", { policy });
    expect(usePrivacyFirewallStore.getState().policies["workspace-1"]).toEqual(policy);
  });

  it("setActionForKind loads the current policy once, then saves the merged actions map", async () => {
    const policy = basePolicy();
    invokeMock.mockResolvedValueOnce(policy); // get_policy
    invokeMock.mockImplementationOnce(async (_cmd, args: unknown) => (args as { policy: PrivacyPolicy }).policy); // save_policy echoes back

    await usePrivacyFirewallStore.getState().setActionForKind("workspace-1", "email", "allow");

    expect(invokeMock).toHaveBeenNthCalledWith(1, "privacy_firewall_get_policy", { workspaceId: "workspace-1" });
    expect(invokeMock).toHaveBeenNthCalledWith(2, "privacy_firewall_save_policy", {
      policy: { ...policy, actions: { ...policy.actions, email: "allow" } },
    });
  });

  it("addException is a no-op for blank input and never calls invoke", async () => {
    await usePrivacyFirewallStore.getState().addException("workspace-1", "   ");
    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("addException skips the save entirely when the trimmed value already exists", async () => {
    const policy = basePolicy({ exceptions: ["existing@example.com"] });
    invokeMock.mockResolvedValueOnce(policy);

    await usePrivacyFirewallStore.getState().addException("workspace-1", "existing@example.com");

    expect(invokeMock).toHaveBeenCalledTimes(1); // only the load — save is skipped, already present
    expect(invokeMock).toHaveBeenCalledWith("privacy_firewall_get_policy", { workspaceId: "workspace-1" });
  });

  it("addException trims whitespace and appends a genuinely new value", async () => {
    const policy = basePolicy({ exceptions: ["existing@example.com"] });
    invokeMock.mockResolvedValueOnce(policy); // get_policy
    invokeMock.mockImplementationOnce(async (_cmd, args: unknown) => (args as { policy: PrivacyPolicy }).policy); // save_policy echoes back

    await usePrivacyFirewallStore.getState().addException("workspace-1", "  new@example.com  ");

    expect(invokeMock).toHaveBeenNthCalledWith(2, "privacy_firewall_save_policy", {
      policy: { ...policy, exceptions: ["existing@example.com", "new@example.com"] },
    });
  });

  it("previewOutbound calls invoke with content, destination, and workspaceId verbatim", async () => {
    const report = baseReport();
    invokeMock.mockResolvedValueOnce(report);

    const result = await usePrivacyFirewallStore.getState().previewOutbound("hello", "cloud_model", "workspace-1");

    expect(invokeMock).toHaveBeenCalledWith("privacy_firewall_preview", {
      content: "hello",
      destination: "cloud_model",
      workspaceId: "workspace-1",
    });
    expect(result).toEqual(report);
  });

  describe("gateOutbound", () => {
    it("resolves immediately with the original content on an allow verdict, with no pending approval", async () => {
      invokeMock.mockResolvedValueOnce(baseReport({ verdict: "allow" }));

      const outcome = await usePrivacyFirewallStore.getState().gateOutbound("hello", "cloud_model", "workspace-1");

      expect(outcome).toEqual({ action: "send", content: "hello", report: baseReport({ verdict: "allow" }) });
      expect(usePrivacyFirewallStore.getState().pendingApproval).toBeNull();
      expect(invokeMock).toHaveBeenCalledTimes(1); // preview only — never prepare/execute
    });

    it("resolves immediately with the redacted content on a redact verdict, with no pending approval", async () => {
      const report = baseReport({ verdict: "redact", redactedPreview: "[REDACTED:EMAIL]" });
      invokeMock.mockResolvedValueOnce(report);

      const outcome = await usePrivacyFirewallStore.getState().gateOutbound("me@example.com", "cloud_model", "workspace-1");

      expect(outcome).toEqual({ action: "send", content: "[REDACTED:EMAIL]", report });
      expect(usePrivacyFirewallStore.getState().pendingApproval).toBeNull();
      expect(invokeMock).toHaveBeenCalledTimes(1);
    });

    it("blocks on a block verdict until resolveDecision is called, then never sends on cancel", async () => {
      const report = baseReport({ verdict: "block" });
      invokeMock.mockResolvedValueOnce(report); // preview
      const confirmation: PrivacySendConfirmationLike = {
        digest: "digest-1",
        confirmationPhrase: "CONFIRM digest-1",
        report,
        expiresAtMs: 999,
      };
      invokeMock.mockResolvedValueOnce(confirmation); // prepare_send

      const outcomePromise = usePrivacyFirewallStore.getState().gateOutbound("secret", "cloud_model", "workspace-1");
      await Promise.resolve();
      await Promise.resolve();

      expect(usePrivacyFirewallStore.getState().pendingApproval).not.toBeNull();
      expect(invokeMock).toHaveBeenNthCalledWith(2, "privacy_firewall_prepare_send", {
        content: "secret",
        destination: "cloud_model",
        workspaceId: "workspace-1",
      });

      await usePrivacyFirewallStore.getState().resolveDecision("cancel");
      expect(invokeMock).toHaveBeenNthCalledWith(3, "privacy_firewall_execute_send", {
        content: "secret",
        digest: "digest-1",
        confirmation: "CONFIRM digest-1",
        decision: "cancel",
        destination: "cloud_model",
        workspaceId: "workspace-1",
      });

      const outcome = await outcomePromise;
      expect(outcome.action).toBe("cancelled");
      expect(usePrivacyFirewallStore.getState().pendingApproval).toBeNull();
    });

    it("switch_local never calls execute_send — nothing crosses the boundary", async () => {
      const report = baseReport({ verdict: "block" });
      invokeMock.mockResolvedValueOnce(report);
      invokeMock.mockResolvedValueOnce({
        digest: "digest-2",
        confirmationPhrase: "CONFIRM digest-2",
        report,
        expiresAtMs: 999,
      });

      const outcomePromise = usePrivacyFirewallStore.getState().gateOutbound("secret", "cloud_model", "workspace-1");
      await Promise.resolve();
      await Promise.resolve();

      await usePrivacyFirewallStore.getState().resolveDecision("switch_local");
      const outcome = await outcomePromise;

      expect(outcome).toEqual({ action: "switch_local", report });
      expect(invokeMock).toHaveBeenCalledTimes(2); // preview + prepare only, never execute_send
    });

    it("send_redacted resolves to the server-returned redacted content", async () => {
      const report = baseReport({ verdict: "require_approval" });
      invokeMock.mockResolvedValueOnce(report);
      invokeMock.mockResolvedValueOnce({
        digest: "digest-3",
        confirmationPhrase: "CONFIRM digest-3",
        report,
        expiresAtMs: 999,
      });
      invokeMock.mockResolvedValueOnce({ allowed: true, content: "[REDACTED:CREDIT_CARD]" });

      const outcomePromise = usePrivacyFirewallStore.getState().gateOutbound("card 4111", "cloud_model", "workspace-1");
      await Promise.resolve();
      await Promise.resolve();
      await usePrivacyFirewallStore.getState().resolveDecision("send_redacted");

      const outcome = await outcomePromise;
      expect(outcome).toEqual({ action: "send", content: "[REDACTED:CREDIT_CARD]", report });
    });
  });

  it("resolveDecision is a no-op when nothing is pending", async () => {
    await usePrivacyFirewallStore.getState().resolveDecision("cancel");
    expect(invokeMock).not.toHaveBeenCalled();
  });
});
