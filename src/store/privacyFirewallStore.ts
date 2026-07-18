import { invoke } from "@tauri-apps/api/core";
import { create } from "zustand";

/** Mirrors Rust `SensitiveDataKind` (src-tauri/src/knowledge_pipeline.rs) —
 * this store never redefines detection, only the policy layered on top of
 * it. */
export type SensitiveDataKind = "private_key" | "api_credential" | "email" | "credit_card" | "phone" | "ip_address";

export const SENSITIVE_DATA_KINDS: SensitiveDataKind[] = [
  "private_key",
  "api_credential",
  "email",
  "credit_card",
  "phone",
  "ip_address",
];

/** Mirrors Rust `PrivacyPolicyAction` (src-tauri/src/privacy_firewall.rs). */
export type PrivacyPolicyAction = "allow" | "redact" | "block" | "require_approval";

export const PRIVACY_POLICY_ACTIONS: PrivacyPolicyAction[] = ["allow", "redact", "block", "require_approval"];

/** Mirrors Rust `OutboundDestination`. Only `"cloud_model"` is wired into an
 * actual send path today (see `agentLoop.ts`'s pre-turn gate) — the others
 * exist so the backend scanner/policy engine stays destination-agnostic for
 * later connector/MCP-result/paired-device call sites, per
 * `privacy_firewall.rs`'s own module doc. */
export type OutboundDestination = "cloud_model" | "connector" | "remote_runner" | "mcp_server" | "paired_device";

/** Mirrors Rust `PrivacyPolicy`. */
export interface PrivacyPolicy {
  workspaceId: string;
  actions: Record<SensitiveDataKind, PrivacyPolicyAction>;
  localOnlyFallback: boolean;
  exceptions: string[];
}

/** Mirrors Rust `PrivacyFinding`. */
export interface PrivacyFinding {
  kind: SensitiveDataKind;
  byteStart: number;
  byteEnd: number;
  line: number;
  column: number;
  maskedPreview: string;
  action: PrivacyPolicyAction;
  exempted: boolean;
}

/** Mirrors Rust `PrivacyPreviewReport`. */
export interface PrivacyPreviewReport {
  destination: OutboundDestination;
  workspaceId: string;
  verdict: PrivacyPolicyAction;
  findings: PrivacyFinding[];
  redactedPreview: string;
  originalSha256: string;
  localOnlyFallbackAvailable: boolean;
  contentLength: number;
}

/** Mirrors Rust `PrivacySendConfirmation`. */
interface PrivacySendConfirmation {
  digest: string;
  confirmationPhrase: string;
  report: PrivacyPreviewReport;
  expiresAtMs: number;
}

/** The user's explicit decision on a pending `block`/`require_approval`
 * preview. `"switch_local"` is frontend-only — nothing crosses the boundary,
 * so it never calls `privacy_firewall_execute_send` at all (there is nothing
 * to confirm). The other three map directly onto Rust `PrivacySendDecision`. */
export type PrivacyGateDecision = "send_redacted" | "send_unredacted" | "cancel" | "switch_local";

/** Mirrors Rust `PrivacySendResult`. */
interface PrivacySendResult {
  allowed: boolean;
  content: string | null;
}

/** What `gateOutbound` resolves to — exactly enough for `agentLoop.ts` to
 * decide what happens next, without it ever needing to know whether that
 * outcome came from an automatic `allow`/`redact` verdict or an explicit
 * user decision on a paused `block`/`require_approval` prompt. */
export type PrivacyGateOutcome =
  | { action: "send"; content: string; report: PrivacyPreviewReport }
  | { action: "switch_local"; report: PrivacyPreviewReport }
  | { action: "cancelled"; report: PrivacyPreviewReport };

/** A paused `block`/`require_approval` preview awaiting the user's decision
 * — rendered by `PrivacyFirewallGate.tsx`. `resolve` settles the `Promise`
 * `gateOutbound` returned to its caller, so `runAgentTurn` genuinely
 * `await`s the user's click before any network request is made. */
interface PendingPrivacyApproval {
  content: string;
  destination: OutboundDestination;
  workspaceId: string;
  digest: string;
  confirmationPhrase: string;
  report: PrivacyPreviewReport;
  resolve: (outcome: PrivacyGateOutcome) => void;
}

interface PrivacyFirewallStore {
  /** Cached policies, keyed by workspace id — populated by `loadPolicy` and
   * kept in sync by every mutating action below so the Settings panel and
   * any other reader never need to re-fetch after an edit they themselves
   * just made. */
  policies: Record<string, PrivacyPolicy>;
  busy: boolean;
  error: string | null;
  /** Non-null exactly while `PrivacyFirewallGate.tsx` has something to show
   * the user — a `block` or `require_approval` verdict paused mid-turn. */
  pendingApproval: PendingPrivacyApproval | null;

  loadPolicy: (workspaceId: string) => Promise<PrivacyPolicy>;
  savePolicy: (policy: PrivacyPolicy) => Promise<void>;
  setActionForKind: (workspaceId: string, kind: SensitiveDataKind, action: PrivacyPolicyAction) => Promise<void>;
  setLocalOnlyFallback: (workspaceId: string, enabled: boolean) => Promise<void>;
  addException: (workspaceId: string, value: string) => Promise<void>;
  removeException: (workspaceId: string, value: string) => Promise<void>;

  previewOutbound: (content: string, destination: OutboundDestination, workspaceId: string) => Promise<PrivacyPreviewReport>;

  /**
   * The single entry point `agentLoop.ts` calls before a turn is sent to a
   * cloud model. `allow` and `redact` verdicts resolve immediately (the
   * latter with `report.redactedPreview` substituted for `content` — no UI
   * appears for either, since neither one is a decision the user needs to
   * make). A `block` or `require_approval` verdict instead populates
   * `pendingApproval` and returns a `Promise` that only settles once
   * `resolveDecision` is called — i.e. once `PrivacyFirewallGate.tsx`
   * renders and the user actually clicks something.
   */
  gateOutbound: (content: string, destination: OutboundDestination, workspaceId: string) => Promise<PrivacyGateOutcome>;
  /** Called by `PrivacyFirewallGate.tsx`. No-ops if nothing is pending. */
  resolveDecision: (decision: PrivacyGateDecision) => Promise<void>;
}

export const usePrivacyFirewallStore = create<PrivacyFirewallStore>((set, get) => ({
  policies: {},
  busy: false,
  error: null,
  pendingApproval: null,

  loadPolicy: async (workspaceId) => {
    set({ busy: true, error: null });
    try {
      const policy = await invoke<PrivacyPolicy>("privacy_firewall_get_policy", { workspaceId });
      set((state) => ({ policies: { ...state.policies, [workspaceId]: policy }, busy: false }));
      return policy;
    } catch (error) {
      set({ busy: false, error: String(error) });
      throw error;
    }
  },

  savePolicy: async (policy) => {
    set({ busy: true, error: null });
    try {
      const saved = await invoke<PrivacyPolicy>("privacy_firewall_save_policy", { policy });
      set((state) => ({ policies: { ...state.policies, [saved.workspaceId]: saved }, busy: false }));
    } catch (error) {
      set({ busy: false, error: String(error) });
      throw error;
    }
  },

  setActionForKind: async (workspaceId, kind, action) => {
    const current = get().policies[workspaceId] ?? (await get().loadPolicy(workspaceId));
    await get().savePolicy({ ...current, actions: { ...current.actions, [kind]: action } });
  },

  setLocalOnlyFallback: async (workspaceId, enabled) => {
    const current = get().policies[workspaceId] ?? (await get().loadPolicy(workspaceId));
    await get().savePolicy({ ...current, localOnlyFallback: enabled });
  },

  addException: async (workspaceId, value) => {
    const trimmed = value.trim();
    if (trimmed.length === 0) return;
    const current = get().policies[workspaceId] ?? (await get().loadPolicy(workspaceId));
    if (current.exceptions.includes(trimmed)) return;
    await get().savePolicy({ ...current, exceptions: [...current.exceptions, trimmed] });
  },

  removeException: async (workspaceId, value) => {
    const current = get().policies[workspaceId] ?? (await get().loadPolicy(workspaceId));
    await get().savePolicy({ ...current, exceptions: current.exceptions.filter((entry) => entry !== value) });
  },

  previewOutbound: (content, destination, workspaceId) =>
    invoke<PrivacyPreviewReport>("privacy_firewall_preview", { content, destination, workspaceId }),

  gateOutbound: async (content, destination, workspaceId) => {
    const report = await get().previewOutbound(content, destination, workspaceId);

    if (report.verdict === "allow") {
      return { action: "send", content, report };
    }
    if (report.verdict === "redact") {
      return { action: "send", content: report.redactedPreview, report };
    }

    // `block` or `require_approval`: never send without an explicit decision.
    const confirmation = await invoke<PrivacySendConfirmation>("privacy_firewall_prepare_send", {
      content,
      destination,
      workspaceId,
    });
    return new Promise<PrivacyGateOutcome>((resolve) => {
      set({
        pendingApproval: {
          content,
          destination,
          workspaceId,
          digest: confirmation.digest,
          confirmationPhrase: confirmation.confirmationPhrase,
          report: confirmation.report,
          resolve,
        },
      });
    });
  },

  resolveDecision: async (decision) => {
    const pending = get().pendingApproval;
    if (!pending) return;
    set({ pendingApproval: null });

    if (decision === "switch_local") {
      pending.resolve({ action: "switch_local", report: pending.report });
      return;
    }

    try {
      const result = await invoke<PrivacySendResult>("privacy_firewall_execute_send", {
        content: pending.content,
        digest: pending.digest,
        confirmation: pending.confirmationPhrase,
        decision,
        destination: pending.destination,
        workspaceId: pending.workspaceId,
      });
      if (!result.allowed || result.content === null) {
        pending.resolve({ action: "cancelled", report: pending.report });
      } else {
        pending.resolve({ action: "send", content: result.content, report: pending.report });
      }
    } catch (error) {
      set({ error: String(error) });
      pending.resolve({ action: "cancelled", report: pending.report });
    }
  },
}));
