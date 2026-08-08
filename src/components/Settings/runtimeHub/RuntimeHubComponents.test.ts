import { describe, expect, it } from "vitest";

import type { M3ComponentCatalogEntry, M3InstalledComponent } from "../../../lib/runtimeHubClient";
import {
  describeRegistryAction,
  mergeRegistryEntries,
  parseCatalogText,
} from "./RuntimeHubComponents";

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

/**
 * Importing a published component catalog into the local registry.
 *
 * The backend swaps the whole registry file atomically, so the merge here is
 * what stops importing one publisher's catalog from deleting every other
 * component the operator had registered.
 */
describe("mergeRegistryEntries", () => {
  it("keeps components the imported catalog says nothing about", () => {
    const llama = registryEntry();
    const mlx = registryEntry({
      componentId: "mlx-runtime-apple-silicon",
      kind: "mlx_runtime",
      sourceId: "little-monkey-mlx",
      version: "0.28.4",
    });
    const merged = mergeRegistryEntries([llama], [mlx]);
    expect(merged).toHaveLength(2);
    expect(merged.map((item) => item.componentId)).toContain("llama-cpp-server-metal");
  });

  it("lets an import correct an entry it already registered", () => {
    const stale = registryEntry({ downloadUrl: "https://components.example.test/wrong" });
    const fixed = registryEntry({ downloadUrl: "https://components.example.test/right" });
    const merged = mergeRegistryEntries([stale], [fixed]);
    expect(merged).toHaveLength(1);
    expect(merged[0].downloadUrl).toBe("https://components.example.test/right");
  });

  it("treats a new version of the same component as an addition, not a replacement", () => {
    const older = registryEntry({ version: "b4000", sha256: "d".repeat(64) });
    const merged = mergeRegistryEntries([older], [registryEntry()]);
    expect(merged.map((item) => item.version).sort()).toEqual(["b4000", "b4100"]);
  });
});

describe("parseCatalogText", () => {
  it("reads the bare array a published catalog is", () => {
    expect(parseCatalogText(JSON.stringify([registryEntry()]))).toHaveLength(1);
  });

  it("also reads back the wrapped shape the app writes its own registry in", () => {
    const wrapped = JSON.stringify({ schemaVersion: 1, entries: [registryEntry()] });
    expect(parseCatalogText(wrapped)).toHaveLength(1);
  });

  it("says which kind of wrong file it was given", () => {
    expect(() => parseCatalogText("not json at all")).toThrow(/not valid JSON/);
    expect(() => parseCatalogText(JSON.stringify({ hello: "world" }))).toThrow(
      /no catalog entries/,
    );
    expect(() => parseCatalogText(JSON.stringify([{ hello: "world" }]))).toThrow(
      /not a component catalog/,
    );
  });
});
