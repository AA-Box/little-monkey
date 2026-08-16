import { beforeEach, describe, expect, it, vi } from "vitest";

import type {
  CapabilityDeclaration,
  ExtensionDetail,
  InvocationResult,
  RuntimeHealth,
} from "./executableExtensionsClient";
import {
  executableExtensionToolDefs,
  invokeExecutableExtensionTool,
} from "./executableExtensionTools";

const { listMock, invokeMock } = vi.hoisted(() => ({
  listMock: vi.fn(),
  invokeMock: vi.fn(),
}));

vi.mock("./executableExtensionsClient", () => ({
  executableExtensionsClient: {
    list: (...args: unknown[]) => listMock(...args),
    invoke: (...args: unknown[]) => invokeMock(...args),
  },
}));

function toolCapability(
  capabilityId = "forecast.now",
  overrides: Partial<CapabilityDeclaration> = {},
): CapabilityDeclaration {
  return {
    capability_id: capabilityId,
    kind: "tool",
    display_name: "Forecast",
    description: "Return the current forecast",
    input_schema: {
      type: "object",
      properties: { city: { type: "string" } },
      required: ["city"],
    },
    ...overrides,
  };
}

function extension(
  extensionId: string,
  health: Partial<RuntimeHealth> = {},
  capabilities: CapabilityDeclaration[] = [toolCapability()],
): ExtensionDetail {
  return {
    manifest: {
      schema_version: 1,
      extension_id: extensionId,
      version: "1.0.0",
      display_name: `Extension ${extensionId}`,
      description: "Test extension",
      host_api: { minimum: "1.0.0", maximum_exclusive: null },
      component: { path: "extension.wasm", sha256: "component-sha" },
      capabilities,
      permissions: [],
      config_schema: [],
      secret_slots: [],
      dependencies: [],
      compatibility: {
        minimum_app_version: "1.0.0",
        maximum_app_version_exclusive: null,
        platforms: ["macos"],
        architectures: ["aarch64"],
      },
      publisher: "Example Publisher",
      provenance: {
        publisher: "Example Publisher",
        source: { local_folder: { canonical_path: "/tmp/example-extension" } },
        source_revision: "source-sha",
        build_reproducible: true,
      },
      signature: null,
      checksums: { "extension.wasm": "component-sha" },
    },
    trust: {
      state: "verified",
      reason: "Test root",
      trust_root_id: "test-root",
      key_id: "test-key",
      manifest_sha256: "manifest-sha",
      component_sha256: "component-sha",
    },
    installed_source: { local_folder: { canonical_path: "/tmp/example-extension" } },
    compatible: true,
    compatibility_reason: null,
    permissions: [],
    secret_slots: [],
    config: {},
    health: {
      state: "healthy",
      validated: true,
      enabled: true,
      running: true,
      consecutive_failures: 0,
      trap_count: 0,
      undeclared_attempts: 0,
      last_error: null,
      last_invocation_at_ms: null,
      ...health,
    },
    active_version: "1.0.0",
    previous_version: null,
    available_versions: ["1.0.0"],
    update_available: false,
    allowed_actions: ["stop", "disable"],
    blockers: [],
  };
}

beforeEach(() => {
  listMock.mockReset();
  invokeMock.mockReset();
});

describe("executableExtensionToolDefs", () => {
  it("offers only validated, enabled, running, healthy tool capabilities", async () => {
    listMock.mockResolvedValue([
      extension("disabled", { enabled: false }),
      extension("stopped", { running: false }),
      extension("unvalidated", { validated: false }),
      extension("degraded", { state: "degraded" }),
      extension("healthy", {}, [
        toolCapability("forecast.now"),
        toolCapability("incoming", { kind: "channel" }),
      ]),
    ]);

    const { defs, registry } = await executableExtensionToolDefs();

    expect(defs).toEqual([
      {
        type: "function",
        function: {
          name: "ext__healthy__forecast_now",
          description: "[Extension: Extension healthy] Return the current forecast",
          parameters: {
            type: "object",
            properties: { city: { type: "string" } },
            required: ["city"],
          },
        },
      },
    ]);
    expect(registry.get("ext__healthy__forecast_now")).toEqual({
      extensionId: "healthy",
      capabilityId: "forecast.now",
      kind: "tool",
      version: "1.0.0",
    });
  });

  it("sanitizes namespaces and gives collisions deterministic suffixes", async () => {
    listMock.mockResolvedValue([
      extension("acme/weather"),
      extension("acme?weather"),
    ]);

    const { defs, registry } = await executableExtensionToolDefs();

    expect(defs.map((definition) => definition.function.name)).toEqual([
      "ext__acme_weather__forecast_now",
      "ext__acme_weather__forecast_now_2",
    ]);
    expect(registry.get("ext__acme_weather__forecast_now_2")).toEqual({
      extensionId: "acme?weather",
      capabilityId: "forecast.now",
      kind: "tool",
      version: "1.0.0",
    });
  });

  it("fails closed when extension discovery is unavailable", async () => {
    listMock.mockRejectedValue(new Error("bridge unavailable"));

    await expect(executableExtensionToolDefs()).resolves.toEqual({
      defs: [],
      registry: new Map(),
    });
  });

  it("returns an independent registry for every turn", async () => {
    listMock.mockResolvedValueOnce([extension("first")]).mockResolvedValueOnce([]);

    const first = await executableExtensionToolDefs();
    const second = await executableExtensionToolDefs();

    expect(second.registry.size).toBe(0);
    expect(first.registry.get("ext__first__forecast_now")).toEqual({
      extensionId: "first",
      capabilityId: "forecast.now",
      kind: "tool",
      version: "1.0.0",
    });
  });
});

describe("invokeExecutableExtensionTool", () => {
  it("resolves the immutable turn registry and sends a bounded invocation request", async () => {
    const result: InvocationResult = {
      invocation_id: "invocation-1",
      output_json: '{"forecast":"rain"}',
      duration_ms: 4,
      fuel_consumed: 12,
      emitted_events: [],
      tool_result: null,
    };
    invokeMock.mockResolvedValue(result);
    const registry = new Map([
      [
        "ext__weather__forecast_now",
        {
          extensionId: "dev.example.weather",
          capabilityId: "forecast.now",
          kind: "tool" as const,
          version: "1.0.0",
        },
      ],
    ]);

    await expect(
      invokeExecutableExtensionTool(
        "ext__weather__forecast_now",
        {
          city: "Stockholm",
          turn_id: "turn-internal",
          tool_call_id: "call-internal",
          input_artifact_ids: ["artifact-1", 42],
        },
        "invocation-1",
        registry,
      ),
    ).resolves.toBe(result);

    expect(invokeMock).toHaveBeenCalledTimes(1);
    const request = invokeMock.mock.calls[0][0];
    expect(request).toMatchObject({
      extension_id: "dev.example.weather",
      capability_id: "forecast.now",
      invocation_id: "invocation-1",
      input_artifact_ids: [],
      expected_kind: "tool",
      expected_version: "1.0.0",
    });
    expect(JSON.parse(request.input_json)).toEqual({
      city: "Stockholm",
    });
  });

  it("rejects a tool that was not offered in this turn", async () => {
    await expect(
      invokeExecutableExtensionTool("ext__unknown__tool", {}, "invocation-2", new Map()),
    ).rejects.toThrow('Extension tool "ext__unknown__tool" was not offered this turn.');
    expect(invokeMock).not.toHaveBeenCalled();
  });
});
