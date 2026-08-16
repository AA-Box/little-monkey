// @vitest-environment jsdom
/**
 * Refreshing the component registry from the catalog this project publishes.
 *
 * The claims here are about what survives the refresh, not about layout. A
 * component published as a release asset used to be reachable only by
 * downloading its catalog in a browser and picking the file, so opening the
 * panel now fetches it — and that fetch must not become a way to lose the
 * registry. It merges against what the store holds rather than what the panel
 * rendered with, and an unreachable catalog leaves the versions already known
 * to this machine exactly where they were.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";

const invoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invoke(...args),
}));

import type { M3ComponentCatalogEntry } from "../../../lib/runtimeHubClient";
import { RuntimeHubComponents } from "./RuntimeHubComponents";
import { useRuntimeHubStore } from "../../../store/runtimeHubStore";

function entry(overrides: Partial<M3ComponentCatalogEntry> = {}): M3ComponentCatalogEntry {
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

const published = entry({
  componentId: "mlx-runtime-apple-silicon",
  kind: "mlx_runtime",
  displayName: "MLX runtime (Apple silicon)",
  accelerator: null,
  version: "mlx-lm-0.28.4+py3.14",
  channel: "beta",
  downloadUrl: "https://components.example.test/mlx-runtime.tar.gz",
  sha256: "b".repeat(64),
  publishedAtMs: 2_000,
});

/** Only the fields this panel reads; the rest of the hub is out of scope. */
function seedStore(componentRegistry: M3ComponentCatalogEntry[]) {
  useRuntimeHubStore.setState({
    componentRegistry,
    installedComponents: [],
    componentUpdateChecks: [],
  });
}

beforeEach(() => {
  invoke.mockReset();
  seedStore([entry()]);
});

afterEach(() => {
  cleanup();
});

describe("refreshing the registry from the published catalog", () => {
  it("merges what the catalog publishes into what this machine already knew", async () => {
    // The panel mounts before the registry has loaded and the fetch lands after
    // it has, which is the ordering the real panel sees: `refreshComponents`
    // and this fetch are both in flight while the first render happens. Held
    // open on purpose so the merge is forced to read the later value.
    let deliverCatalog: (entries: M3ComponentCatalogEntry[]) => void = () => {};
    const catalog = new Promise<M3ComponentCatalogEntry[]>((resolve) => {
      deliverCatalog = resolve;
    });
    seedStore([]);
    invoke.mockImplementation(async (command: string, args: { entries?: M3ComponentCatalogEntry[] }) => {
      if (command === "m3_component_fetch_catalog") return await catalog;
      if (command === "m3_component_replace_registry_entries") return args.entries;
      return [];
    });

    render(<RuntimeHubComponents />);
    seedStore([entry()]);
    deliverCatalog([published]);

    await waitFor(() => {
      expect(
        invoke.mock.calls.some(([command]) => command === "m3_component_replace_registry_entries"),
      ).toBe(true);
    });
    const [, args] = invoke.mock.calls.find(
      ([command]) => command === "m3_component_replace_registry_entries",
    ) as [string, { entries: M3ComponentCatalogEntry[] }];
    // Both, in one write: the backend replaces the registry file wholesale, so
    // a merge against the empty list this panel first rendered with would have
    // deleted the locally registered component instead of adding to it.
    expect(args.entries.map((held) => held.componentId).sort()).toEqual([
      "llama-cpp-server-metal",
      "mlx-runtime-apple-silicon",
    ]);
  });

  it("keeps the versions already known when the catalog cannot be reached", async () => {
    invoke.mockImplementation(async (command: string) => {
      if (command === "m3_component_fetch_catalog") throw new Error("offline");
      if (command === "m3_component_replace_registry_entries") {
        throw new Error("the registry must not be rewritten from a failed fetch");
      }
      return [];
    });

    render(<RuntimeHubComponents />);

    expect(await screen.findByText(/could not be reached/)).toBeTruthy();
    expect(
      invoke.mock.calls.some(([command]) => command === "m3_component_replace_registry_entries"),
    ).toBe(false);
    // The panel still lists what is on disk, which is the whole reason an
    // unreachable catalog is a notice rather than an error.
    expect(screen.getByText("llama.cpp server (Metal)")).toBeTruthy();
  });
});
