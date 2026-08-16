import { readFileSync } from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { beforeEach, describe, expect, it, vi } from "vitest";

import {
  executableExtensionsClient,
  type ActiveCapability,
  type ExtensionApproval,
  type InvocationRequest,
} from "./executableExtensionsClient";

const invokeMock = vi.hoisted(() => vi.fn());

vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
}));

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = path.resolve(__dirname, "../../");

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockResolvedValue(undefined);
});

describe("the executable extensions bridge", () => {
  it("keeps every frontend command declared and registered by the Rust bridge", () => {
    const clientSource = readFileSync(
      path.join(REPO_ROOT, "src/lib/executableExtensionsClient.ts"),
      "utf8",
    );
    const commandSource = readFileSync(
      path.join(REPO_ROOT, "src-tauri/src/extension_commands.rs"),
      "utf8",
    );
    const libSource = readFileSync(path.join(REPO_ROOT, "src-tauri/src/lib.rs"), "utf8");

    const invoked = [
      ...new Set(
        [...clientSource.matchAll(/invoke(?:<[^>]*>)?\(\s*"(extensions_\w+)"/g)].map(
          (match) => match[1],
        ),
      ),
    ].sort();
    const declared = [
      ...commandSource.matchAll(
        /#\[tauri::command\]\s+pub async fn (extensions_\w+)\s*\(/g,
      ),
    ]
      .map((match) => match[1])
      .sort();

    expect(invoked.length).toBeGreaterThanOrEqual(20);
    expect(invoked).toEqual(declared);
    for (const command of invoked) {
      expect(libSource, `${command} is not registered in generate_handler!`).toContain(
        `extension_commands::${command},`,
      );
    }
  });

  it("sends the exact Tauri command names and camel-case payloads", async () => {
    const approval: ExtensionApproval = {
      approval_digest: "approval-sha",
      grants: [{ permission_id: "network", binding: "https://api.example.com" }],
      allow_unsigned: true,
      allow_untrusted: false,
      allow_high_risk: true,
    };
    const request: InvocationRequest = {
      extension_id: "dev.example.echo",
      capability_id: "echo",
      input_json: '{"message":"hello"}',
      invocation_id: "invocation-1",
      input_artifact_ids: ["artifact-1"],
      expected_kind: null,
      expected_version: null,
    };

    await Promise.all([
      executableExtensionsClient.discover("/tmp/echo-extension"),
      executableExtensionsClient.list(),
      executableExtensionsClient.activeCapabilities("stt"),
      executableExtensionsClient.inspect("dev.example.echo"),
      executableExtensionsClient.install("/tmp/echo-extension", approval),
      executableExtensionsClient.validate("dev.example.echo"),
      executableExtensionsClient.setEnabled("dev.example.echo", true),
      executableExtensionsClient.setRunning("dev.example.echo", true),
      executableExtensionsClient.previewUpdate("/tmp/echo-extension-v2"),
      executableExtensionsClient.update("/tmp/echo-extension-v2", approval),
      executableExtensionsClient.rollback("dev.example.echo"),
      executableExtensionsClient.uninstall("dev.example.echo"),
      executableExtensionsClient.status("dev.example.echo"),
      executableExtensionsClient.logs("dev.example.echo"),
      executableExtensionsClient.setConfig("dev.example.echo", { mode: "strict" }),
      executableExtensionsClient.setSecret("dev.example.echo", "api-key", "secret-value"),
      executableExtensionsClient.removeSecret("dev.example.echo", "api-key"),
      executableExtensionsClient.invoke(request),
      executableExtensionsClient.cancel("invocation-1"),
      executableExtensionsClient.webhooks("dev.example.echo"),
      executableExtensionsClient.registerWebhook(
        "incoming-mail",
        "dev.example.echo",
        "receive-mail",
        "webhook-secret",
      ),
      executableExtensionsClient.removeWebhook("incoming-mail", "dev.example.echo"),
    ]);

    expect(invokeMock.mock.calls).toEqual([
      ["extensions_discover", { sourcePath: "/tmp/echo-extension" }],
      ["extensions_list"],
      ["extensions_active_capabilities", { kind: "stt" }],
      ["extensions_inspect", { extensionId: "dev.example.echo" }],
      ["extensions_install", { sourcePath: "/tmp/echo-extension", approval }],
      ["extensions_validate", { extensionId: "dev.example.echo" }],
      ["extensions_set_enabled", { extensionId: "dev.example.echo", enabled: true }],
      ["extensions_set_running", { extensionId: "dev.example.echo", running: true }],
      ["extensions_preview_update", { sourcePath: "/tmp/echo-extension-v2" }],
      ["extensions_update", { sourcePath: "/tmp/echo-extension-v2", approval }],
      ["extensions_rollback", { extensionId: "dev.example.echo" }],
      ["extensions_uninstall", { extensionId: "dev.example.echo" }],
      ["extensions_status", { extensionId: "dev.example.echo" }],
      ["extensions_logs", { extensionId: "dev.example.echo", limit: 100 }],
      ["extensions_set_config", { extensionId: "dev.example.echo", values: { mode: "strict" } }],
      [
        "extensions_set_secret",
        { extensionId: "dev.example.echo", slotId: "api-key", secret: "secret-value" },
      ],
      ["extensions_remove_secret", { extensionId: "dev.example.echo", slotId: "api-key" }],
      ["extensions_invoke", { request }],
      ["extensions_cancel", { invocationId: "invocation-1" }],
      ["extensions_webhooks", { extensionId: "dev.example.echo" }],
      [
        "extensions_register_webhook",
        {
          triggerId: "incoming-mail",
          extensionId: "dev.example.echo",
          handlerId: "receive-mail",
          secret: "webhook-secret",
          maxSkewMs: 300_000,
        },
      ],
      [
        "extensions_remove_webhook",
        { triggerId: "incoming-mail", extensionId: "dev.example.echo" },
      ],
    ]);
  });

  it("requests the complete active-capability catalog when no kind is supplied", async () => {
    const capability: ActiveCapability = {
      kind: "channel",
      capability_id: "support",
      extension_id: "dev.example.support",
      version: "1.0.0",
      display_name: "Support",
      description: "Support channel",
      input_schema: { type: "object" },
    };
    invokeMock.mockResolvedValue([capability]);

    await expect(executableExtensionsClient.activeCapabilities()).resolves.toEqual([capability]);
    expect(invokeMock).toHaveBeenCalledWith("extensions_active_capabilities", { kind: null });
  });
});
