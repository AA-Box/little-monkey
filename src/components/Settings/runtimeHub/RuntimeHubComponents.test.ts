import { describe, expect, it } from "vitest";

import type { M3ComponentCatalogEntry, M3InstalledComponent } from "../../../lib/runtimeHubClient";
import { describeRegistryAction } from "./RuntimeHubComponents";

function registryEntry(overrides: Partial<M3ComponentCatalogEntry> = {}): M3ComponentCatalogEntry {
  return {
    schemaVersion: 1,
    sourceId: "local",
    componentId: "llama-cpp-server-metal",
    kind: "llama_cpp_server",
    displayName: "llama.cpp server (Metal)",
    accelerator: "metal",
    version: "b4100",
    channel: "stable",
    downloadUrl: "https://components.example.test/llama-server",
    sha256: "a".repeat(64),
    sizeBytes: 1024,
    publishedAtMs: 1_000,
    compatibilityNote: null,
    metadata: {},
    ...overrides,
  };
}

function installedComponent(overrides: Partial<M3InstalledComponent> = {}): M3InstalledComponent {
  return {
    componentId: "llama-cpp-server-metal",
    kind: "llama_cpp_server",
    displayName: "llama.cpp server (Metal)",
    accelerator: "metal",
    channel: "stable",
    activeVersionKey: "b".repeat(64),
    versions: [
      {
        versionKey: "b".repeat(64),
        version: "b4100",
        channel: "stable",
        sha256: "a".repeat(64),
        sizeBytes: 1024,
        sourceUrl: "https://components.example.test/llama-server",
        artifactPath: "/tmp/component.bin",
        installedAtMs: 1_000,
        publishedAtMs: 1_000,
        active: true,
        compatibilityNote: null,
      },
    ],
    ...overrides,
  };
}

describe("Runtime Hub component registry actions", () => {
  it("offers install for a component id with nothing installed yet", () => {
    expect(describeRegistryAction(registryEntry(), [])).toBe("install");
  });

  it("marks the exact active version as already current", () => {
    const installed = installedComponent();
    expect(describeRegistryAction(registryEntry(), [installed])).toBe("current");
  });

  it("offers update when the same component id is installed at a different version or digest", () => {
    const installed = installedComponent();
    expect(
      describeRegistryAction(registryEntry({ version: "b4200", sha256: "c".repeat(64) }), [installed]),
    ).toBe("update");
    expect(describeRegistryAction(registryEntry({ sha256: "c".repeat(64) }), [installed])).toBe("update");
  });

  it("does not confuse unrelated component ids", () => {
    const installed = installedComponent({ componentId: "tokenizer-bpe" });
    expect(describeRegistryAction(registryEntry(), [installed])).toBe("install");
  });
});
