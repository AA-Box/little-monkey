import { describe, expect, it } from "vitest";

import type { M3ComponentCatalogEntry, M3InstalledComponent } from "../../../lib/runtimeHubClient";
import { describeRegistryAction, entryKey, parseCatalogText } from "./RuntimeHubComponents";

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
 * The registry's identity, as this file spells it.
 *
 * Merging a catalog into the registry is the backend's job — it reads, merges and
 * writes under one lock, which a read-modify-write across the IPC boundary could
 * not do without losing a concurrent import. What is left here is the React key,
 * and the claim under test is only that it is keyed on the same four fields the
 * backend's `M3ComponentCatalogEntry::registry_key` is. `a_registry_key_is_the_one_
 * identity_the_registry_merges_on` in `m3_runtime_hub.rs` is the other half; the
 * two together are what keeps the definitions from drifting.
 */
describe("entryKey", () => {
  it("is keyed on exactly the four fields the backend keys a registry row on", () => {
    const base = registryEntry();
    for (const changed of [
      { componentId: "tokenizer-bpe" },
      { version: "b4200" },
      { sha256: "c".repeat(64) },
      // The field the old key was missing. Two entries differing only in where
      // their bytes come from are two rows, not one silently overwriting the
      // other.
      { downloadUrl: "https://components.example.test/elsewhere" },
    ]) {
      expect(entryKey(registryEntry(changed))).not.toBe(entryKey(base));
    }
    // And nothing else: a corrected note or display name is the same row, which
    // is how a publisher fixes one without the app listing it twice.
    expect(entryKey(registryEntry({ compatibilityNote: "needs macOS 15" }))).toBe(entryKey(base));
    expect(entryKey(registryEntry({ displayName: "llama.cpp (Metal)" }))).toBe(entryKey(base));
    expect(entryKey(registryEntry({ sourceId: "little-monkey-mlx" }))).toBe(entryKey(base));
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
