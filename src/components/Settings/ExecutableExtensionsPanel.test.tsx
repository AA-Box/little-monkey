// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";

const invoke = vi.fn();
const open = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invoke(...args) }));
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: (...args: unknown[]) => open(...args) }));

import { ExecutableExtensionsPanel } from "./ExecutableExtensionsPanel";
import type {
  ExtensionDetail,
  ExtensionPreview,
  PermissionView,
} from "../../lib/executableExtensionsClient";

const NETWORK: PermissionView = {
  permission_id: "api_origin",
  kind: "network_origin",
  scope: "https://api.example.test",
  reason: "Call the exact fixture API origin",
  risk: "high",
  granted: true,
  binding_label: null,
};

const BASE: ExtensionDetail = {
  manifest: {
    schema_version: 1,
    extension_id: "dev.example.fixture",
    version: "1.0.0",
    display_name: "Fixture extension",
    description: "A bounded component fixture.",
    host_api: { minimum: "1.0.0", maximum_exclusive: null },
    component: { path: "component.wasm", sha256: "a".repeat(64) },
    capabilities: [{
      capability_id: "echo",
      kind: "tool",
      display_name: "Echo",
      description: "Echo bounded JSON",
      input_schema: { type: "object" },
    }],
    permissions: [{
      permission_id: NETWORK.permission_id,
      kind: NETWORK.kind,
      scope: NETWORK.scope,
      reason: NETWORK.reason,
    }],
    config_schema: [],
    secret_slots: [{
      slot_id: "api_token",
      label: "API token",
      description: "Applied only by the HTTP broker",
      auth_header: "authorization",
      auth_scheme: "Bearer",
    }],
    dependencies: [],
    compatibility: {
      minimum_app_version: "1.0.0",
      maximum_app_version_exclusive: null,
      platforms: [],
      architectures: [],
      contract: null,
    },
    publisher: "Independent Fixture",
    provenance: {
      publisher: "Independent Fixture",
      source: { local_folder: { canonical_path: "/tmp/fixture-extension" } },
      source_revision: "fixture-v1",
      build_reproducible: true,
    },
    signature: null,
    checksums: { "component.wasm": "a".repeat(64) },
  },
  trust: {
    state: "unsigned",
    reason: "No publisher signature",
    trust_root_id: null,
    key_id: null,
    manifest_sha256: "b".repeat(64),
    component_sha256: "a".repeat(64),
  },
  installed_source: { local_folder: { canonical_path: "/tmp/observed-fixture-extension" } },
  compatible: false,
  compatibility_reason: "Host API range does not match",
  permissions: [NETWORK],
  secret_slots: [{
    slot_id: "api_token",
    label: "API token",
    description: "Applied only by the HTTP broker",
    configured: true,
  }],
  config: {},
  health: {
    state: "degraded",
    validated: true,
    enabled: true,
    running: false,
    consecutive_failures: 2,
    trap_count: 7,
    undeclared_attempts: 3,
    last_error: "guest trapped in fixture",
    last_invocation_at_ms: 1,
  },
  active_version: "1.0.0",
  previous_version: "0.9.0",
  available_versions: ["0.9.0", "1.0.0"],
  update_available: false,
  allowed_actions: ["enable", "disable", "start", "stop", "rollback"],
  blockers: ["Host API range does not match"],
};

const SECOND: ExtensionDetail = {
  ...BASE,
  manifest: {
    ...BASE.manifest,
    extension_id: "dev.example.second",
    display_name: "Second extension",
    provenance: {
      ...BASE.manifest.provenance,
      source: { local_folder: { canonical_path: "/tmp/second-extension" } },
      source_revision: "fixture-v2",
    },
  },
  installed_source: { local_folder: { canonical_path: "/tmp/second-extension" } },
  compatible: true,
  compatibility_reason: null,
  health: { ...BASE.health, last_error: null },
  active_version: "1.1.0",
  previous_version: null,
  available_versions: ["1.1.0"],
  blockers: [],
};

const WORKSPACE: PermissionView = {
  permission_id: "workspace",
  kind: "workspace_read",
  scope: "project",
  reason: "Read the explicitly selected project",
  risk: "medium",
  granted: true,
  binding_label: "old-workspace",
};

const REMOVED: PermissionView = {
  permission_id: "old_model",
  kind: "model_invoke",
  scope: "local:old-model",
  reason: "No longer needed",
  risk: "medium",
  granted: true,
  binding_label: null,
};

function updatePreview(): ExtensionPreview {
  return {
    source_path: "/tmp/fixture-update",
    source_digest: "c".repeat(64),
    manifest: {
      ...BASE.manifest,
      version: "2.0.0",
      signature: {
        trust_root_id: "unknown-root",
        key_id: "release-2",
        algorithm: "ed25519",
        signature_hex: "e".repeat(128),
      },
    },
    trust: {
      ...BASE.trust,
      state: "untrusted",
      reason: "Unknown publisher root",
      trust_root_id: "unknown-root",
      key_id: "release-2",
    },
    compatible: true,
    compatibility_reason: null,
    permissions: [NETWORK],
    permission_diff: {
      added: [NETWORK],
      removed: [REMOVED],
      unchanged: [{ ...NETWORK, permission_id: "unchanged_origin" }],
      expands_authority: true,
    },
    approval_digest: "d".repeat(64),
    requires_unsigned_approval: false,
    requires_untrusted_approval: true,
    requires_high_risk_approval: true,
    blockers: [],
  };
}

function workspaceUpdatePreview(): ExtensionPreview {
  const next = updatePreview();
  return {
    ...next,
    source_path: "/tmp/workspace-update",
    manifest: {
      ...next.manifest,
      permissions: [{
        permission_id: WORKSPACE.permission_id,
        kind: WORKSPACE.kind,
        scope: WORKSPACE.scope,
        reason: WORKSPACE.reason,
      }],
    },
    trust: {
      ...next.trust,
      state: "verified",
      reason: "Publisher signature verified",
    },
    permissions: [WORKSPACE],
    permission_diff: {
      added: [],
      removed: [],
      unchanged: [WORKSPACE],
      expands_authority: false,
    },
    requires_untrusted_approval: false,
    requires_high_risk_approval: false,
  };
}

let listed: ExtensionDetail[];

function mockBridge() {
  invoke.mockImplementation((command: string) => {
    if (command === "extensions_list") return Promise.resolve(listed);
    if (command === "extensions_logs") return Promise.resolve([]);
    if (command === "extensions_webhooks") return Promise.resolve([]);
    if (command === "extensions_preview_update") return Promise.resolve(updatePreview());
    return Promise.resolve(BASE);
  });
}

beforeEach(() => {
  listed = [BASE];
  invoke.mockReset();
  open.mockReset();
  mockBridge();
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("ExecutableExtensionsPanel", () => {
  it("keeps loading, failed, and empty states distinct", async () => {
    let resolveList!: (value: ExtensionDetail[]) => void;
    invoke.mockImplementation((command: string) => command === "extensions_list"
      ? new Promise<ExtensionDetail[]>((resolve) => { resolveList = resolve; })
      : Promise.resolve([]));
    render(<ExecutableExtensionsPanel />);
    expect(screen.getByRole("status").textContent).toContain("Loading extensions");
    resolveList([]);
    expect(await screen.findByText("No executable extensions installed")).toBeTruthy();

    cleanup();
    invoke.mockImplementation((command: string) => command === "extensions_list"
      ? Promise.reject(new Error("extension registry is corrupt"))
      : Promise.resolve([]));
    render(<ExecutableExtensionsPanel />);
    expect((await screen.findByRole("alert")).textContent).toContain("extension registry is corrupt");
    expect(screen.queryByText("No executable extensions installed")).toBeNull();
  });

  it("renders backend trust, compatibility, health, traps, and exact origins without inventing healthy", async () => {
    render(<ExecutableExtensionsPanel />);
    expect((await screen.findAllByText("Fixture extension")).length).toBeGreaterThan(0);
    expect(document.body.textContent).toContain("unsigned");
    expect(document.body.textContent).toContain("Incompatible");
    expect(document.body.textContent).toContain("degraded");
    expect(document.body.textContent).not.toContain("healthy");
    expect(screen.getByText("https://api.example.test")).toBeTruthy();
    const health = screen.getByLabelText("Runtime health");
    expect(within(health).getByText("7")).toBeTruthy();
    expect(within(health).getByText("3")).toBeTruthy();
    expect(within(health).getByText("guest trapped in fixture")).toBeTruthy();

    listed = [{ ...BASE, compatible: true, blockers: [], health: { ...BASE.health, state: "healthy", running: true } }];
    fireEvent.click(screen.getAllByText("Refresh")[0]);
    await waitFor(() => expect(document.body.textContent).toContain("healthy"));
  });

  it("binds a permission-expanding update to the exact digest and explicit reviews", async () => {
    open.mockResolvedValue("/tmp/fixture-update");
    render(<ExecutableExtensionsPanel />);
    await screen.findAllByText("Fixture extension");
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Update" }));
    });

    const review = await screen.findByLabelText("Review extension");
    expect(within(review).getByText("d".repeat(64))).toBeTruthy();
    expect(within(review).getByText("Added")).toBeTruthy();
    expect(within(review).getByText("Removed")).toBeTruthy();
    expect(within(review).getByText("Unchanged")).toBeTruthy();
    expect(within(review).getAllByText("https://api.example.test").length).toBeGreaterThan(0);
    expect(within(review).getByText("dev.example.fixture")).toBeTruthy();
    expect(within(review).getByText("Independent Fixture")).toBeTruthy();
    expect(within(review).getByText("/tmp/fixture-update")).toBeTruthy();
    expect(within(review).getByText("/tmp/fixture-extension")).toBeTruthy();
    expect(within(review).getByText("fixture-v1")).toBeTruthy();
    expect(within(review).getByText("ed25519 · unknown-root/release-2")).toBeTruthy();
    expect(within(review).getByText("Echo")).toBeTruthy();
    expect(within(review).getByText("c".repeat(64))).toBeTruthy();

    const apply = within(review).getByRole("button", { name: "Update" });
    expect((apply as HTMLButtonElement).disabled).toBe(true);
    for (const text of [/signing root is not trusted/i, /reviewed the high-risk permissions/i]) {
      const label = within(review).getByText(text).closest("label");
      fireEvent.click(label?.querySelector("input") as HTMLInputElement);
    }
    const permissionLabel = within(review).getAllByText("https://api.example.test")[0].closest("label");
    const permissionToggle = permissionLabel?.querySelector("input") as HTMLInputElement;
    if (!permissionToggle.checked) fireEvent.click(permissionToggle);
    expect((apply as HTMLButtonElement).disabled).toBe(false);
    fireEvent.click(apply);

    await waitFor(() => {
      const call = invoke.mock.calls.find(([command]) => command === "extensions_update");
      expect(call?.[1]).toEqual({
        sourcePath: "/tmp/fixture-update",
        approval: {
          approval_digest: "d".repeat(64),
          grants: [{ permission_id: "api_origin", binding: null }],
          allow_unsigned: false,
          allow_untrusted: true,
          allow_high_risk: true,
        },
      });
    });
  });

  it("requires a freshly selected absolute workspace path for every approval", async () => {
    open
      .mockResolvedValueOnce("/tmp/workspace-update")
      .mockResolvedValueOnce("/tmp/reselected-workspace");
    invoke.mockImplementation((command: string) => {
      if (command === "extensions_list") return Promise.resolve(listed);
      if (command === "extensions_logs" || command === "extensions_webhooks") return Promise.resolve([]);
      if (command === "extensions_preview_update") return Promise.resolve(workspaceUpdatePreview());
      return Promise.resolve(BASE);
    });
    render(<ExecutableExtensionsPanel />);
    await screen.findAllByText("Fixture extension");
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Update" }));
    });

    const review = await screen.findByLabelText("Review extension");
    const apply = within(review).getByRole("button", { name: "Update" }) as HTMLButtonElement;
    const binding = within(review).getByPlaceholderText(
      "Choose the directory bound to this opaque handle",
    ) as HTMLInputElement;
    expect(binding.value).toBe("");
    expect(apply.disabled).toBe(true);
    expect(within(review).getByText(/old-workspace/)).toBeTruthy();

    fireEvent.click(within(review).getByRole("button", { name: "Choose" }));
    await waitFor(() => expect(binding.value).toBe("/tmp/reselected-workspace"));
    expect(apply.disabled).toBe(false);
    fireEvent.click(apply);

    await waitFor(() => expect(
      invoke.mock.calls.find(([command]) => command === "extensions_update")?.[1],
    ).toEqual({
      sourcePath: "/tmp/workspace-update",
      approval: {
        approval_digest: "d".repeat(64),
        grants: [{ permission_id: "workspace", binding: "/tmp/reselected-workspace" }],
        allow_unsigned: false,
        allow_untrusted: false,
        allow_high_risk: false,
      },
    }));
  });

  it("clears selected-extension drafts and ignores stale scoped responses", async () => {
    listed = [BASE, SECOND];
    let resolveFirstLogs!: (rows: { at_ms: number; level: string; message: string; invocation_id: null }[]) => void;
    const firstLogs = new Promise<{ at_ms: number; level: string; message: string; invocation_id: null }[]>((resolve) => {
      resolveFirstLogs = resolve;
    });
    open.mockResolvedValue("/tmp/fixture-update");
    invoke.mockImplementation((command: string, args?: { extensionId?: string }) => {
      if (command === "extensions_list") return Promise.resolve(listed);
      if (command === "extensions_logs") {
        return args?.extensionId === BASE.manifest.extension_id
          ? firstLogs
          : Promise.resolve([{ at_ms: 2, level: "info", message: "second-extension-log", invocation_id: null }]);
      }
      if (command === "extensions_webhooks") return Promise.resolve([]);
      if (command === "extensions_preview_update") return Promise.resolve(updatePreview());
      return Promise.resolve(BASE);
    });
    render(<ExecutableExtensionsPanel />);
    await screen.findAllByText("Fixture extension");
    fireEvent.change(screen.getByPlaceholderText("Enter a new secret"), {
      target: { value: "must-not-cross-extension-boundaries" },
    });
    fireEvent.click(screen.getByText("Update"));
    await screen.findByLabelText("Review extension");

    fireEvent.click(screen.getByRole("button", { name: /Second extension/i }));
    await waitFor(() => expect(screen.queryByLabelText("Review extension")).toBeNull());
    expect((screen.getByPlaceholderText("Enter a new secret") as HTMLInputElement).value).toBe("");
    expect(await screen.findByText("second-extension-log")).toBeTruthy();

    await act(async () => {
      resolveFirstLogs([{ at_ms: 1, level: "warn", message: "stale-first-extension-log", invocation_id: null }]);
      await Promise.resolve();
    });
    expect(screen.queryByText("stale-first-extension-log")).toBeNull();
    expect(screen.getByText("second-extension-log")).toBeTruthy();
  });

  it("offers the full lifecycle while keeping enabled separate from running", async () => {
    render(<ExecutableExtensionsPanel />);
    await screen.findAllByText("Fixture extension");
    for (const label of ["Validate", "Enable", "Disable", "Start", "Stop", "Update", "Rollback", "Uninstall"]) {
      expect(screen.getByText(label)).toBeTruthy();
    }
    const health = screen.getByLabelText("Runtime health");
    expect(within(health).getByText("Enabled").nextElementSibling?.textContent).toBe("Yes");
    expect(within(health).getByText("Running").nextElementSibling?.textContent).toBe("No");
  });

  it("stops before rollback and does not refresh removed extension state after uninstall", async () => {
    const running: ExtensionDetail = {
      ...BASE,
      compatible: true,
      compatibility_reason: null,
      health: { ...BASE.health, state: "healthy", running: true, last_error: null },
      blockers: [],
      allowed_actions: ["disable", "stop", "update", "rollback"],
    };
    listed = [running];
    let installed = true;
    vi.spyOn(window, "confirm").mockReturnValue(true);
    invoke.mockImplementation((command: string, args?: { running?: boolean }) => {
      if (command === "extensions_list") return Promise.resolve(listed);
      if (command === "extensions_logs") {
        return installed ? Promise.resolve([]) : Promise.reject(new Error("extension is removed"));
      }
      if (command === "extensions_webhooks") return Promise.resolve([]);
      if (command === "extensions_set_running") {
        listed = [{ ...running, health: { ...running.health, running: args?.running ?? false, state: "stopped" } }];
        return Promise.resolve(listed[0]);
      }
      if (command === "extensions_rollback") return Promise.resolve(listed[0]);
      if (command === "extensions_uninstall") {
        installed = false;
        listed = [];
        return Promise.resolve(undefined);
      }
      return Promise.resolve(running);
    });
    render(<ExecutableExtensionsPanel />);
    await screen.findAllByText("Fixture extension");

    fireEvent.click(screen.getByText("Rollback"));
    await waitFor(() => expect(
      invoke.mock.calls.some(([command]) => command === "extensions_rollback"),
    ).toBe(true));
    expect(invoke.mock.calls
      .map(([command]) => command)
      .filter((command) => command === "extensions_set_running" || command === "extensions_rollback")
      .slice(0, 2)).toEqual(["extensions_set_running", "extensions_rollback"]);

    fireEvent.click(await screen.findByText("Uninstall"));
    await screen.findByText("No executable extensions installed");
    await act(async () => { await Promise.resolve(); });
    const uninstallIndex = invoke.mock.calls.findIndex(([command]) => command === "extensions_uninstall");
    expect(invoke.mock.calls.slice(uninstallIndex + 1).some(([command]) => command === "extensions_logs")).toBe(false);
    expect(screen.queryByRole("alert")).toBeNull();
    expect(document.body.textContent).toContain("Extension and its private host state removed.");
  });

  it("sends a secret only to the keychain command, clears the field, and can remove it", async () => {
    render(<ExecutableExtensionsPanel />);
    await screen.findByText("API token");
    const input = screen.getByPlaceholderText("Enter a new secret") as HTMLInputElement;
    fireEvent.change(input, { target: { value: "fixture-super-secret" } });
    fireEvent.click(screen.getByText("Save secret"));

    await waitFor(() => expect(
      invoke.mock.calls.some(([command]) => command === "extensions_set_secret"),
    ).toBe(true));
    expect(input.value).toBe("");
    for (const [command, args] of invoke.mock.calls) {
      if (command === "extensions_set_secret") continue;
      expect(JSON.stringify(args ?? {})).not.toContain("fixture-super-secret");
    }

    fireEvent.click(screen.getByText("Clear"));
    await waitFor(() => expect(
      invoke.mock.calls.some(([command, args]) => command === "extensions_remove_secret"
        && (args as { extensionId: string; slotId: string }).extensionId === "dev.example.fixture"
        && (args as { extensionId: string; slotId: string }).slotId === "api_token"),
    ).toBe(true));
  });
});
