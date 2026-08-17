// @vitest-environment jsdom
/**
 * Refreshing the component registry from the catalog this project publishes.
 *
 * The claims here are about what survives the refresh, not about layout. A
 * component published as a release asset used to be reachable only by
 * downloading its catalog in a browser and picking the file, so opening the
 * panel now fetches it — and that fetch must not become a way to lose the
 * registry. Adoption is one backend call that reads, merges and writes under a
 * lock, so this panel never holds a registry it is about to overwrite; an
 * unreachable catalog leaves the versions already known to this machine exactly
 * where they were, and two syncs cannot be in flight at once.
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
  it("adopts the catalog through one backend call and renders what came back", async () => {
    // The panel mounts before the registry has loaded and the sync lands after it
    // has, which is the ordering the real panel sees: `refreshComponents` and this
    // sync are both in flight while the first render happens. Held open on purpose,
    // because the frontend read-modify-write this replaced would have merged
    // against the empty list the panel rendered with and written the local
    // component out of existence.
    let deliverRegistry: (entries: M3ComponentCatalogEntry[]) => void = () => {};
    const adopted = new Promise<M3ComponentCatalogEntry[]>((resolve) => {
      deliverRegistry = resolve;
    });
    seedStore([]);
    invoke.mockImplementation(async (command: string) => {
      if (command === "m3_component_sync_catalog") return await adopted;
      // Whatever the panel did while the sync was in flight, adopting must not be
      // expressed as a replace from this process: the merge is the backend's, and
      // this is the call that used to lose the race.
      if (command === "m3_component_replace_registry_entries") {
        throw new Error("the panel must not write the registry itself");
      }
      return [];
    });

    render(<RuntimeHubComponents />);
    seedStore([entry()]);
    deliverRegistry([entry(), published]);

    await waitFor(() => {
      expect(screen.getByText("MLX runtime (Apple silicon)")).toBeTruthy();
    });
    expect(screen.getByText("llama.cpp server (Metal)")).toBeTruthy();
    // One sync per mount, and it carries no entries: the panel sends a URL at
    // most, never a registry it computed.
    const syncs = invoke.mock.calls.filter(([command]) => command === "m3_component_sync_catalog");
    expect(syncs).toHaveLength(1);
    expect((syncs[0][1] as { entries?: unknown }).entries).toBeUndefined();
  });

  it("keeps the versions already known when the catalog cannot be reached", async () => {
    invoke.mockImplementation(async (command: string) => {
      if (command === "m3_component_sync_catalog") throw new Error("offline");
      if (
        command === "m3_component_replace_registry_entries" ||
        command === "m3_component_merge_registry_entries"
      ) {
        throw new Error("the registry must not be rewritten from a failed fetch");
      }
      return [];
    });

    render(<RuntimeHubComponents />);

    expect(await screen.findByText(/could not be reached/)).toBeTruthy();
    // The panel still lists what is on disk, which is the whole reason an
    // unreachable catalog is a notice rather than an error.
    expect(screen.getByText("llama.cpp server (Metal)")).toBeTruthy();
  });

  it("does not start a second sync while one is still running", async () => {
    let deliverRegistry: (entries: M3ComponentCatalogEntry[]) => void = () => {};
    const adopted = new Promise<M3ComponentCatalogEntry[]>((resolve) => {
      deliverRegistry = resolve;
    });
    invoke.mockImplementation(async (command: string) => {
      if (command === "m3_component_sync_catalog") return await adopted;
      return [];
    });

    render(<RuntimeHubComponents />);
    // The mount sync is still in flight, so the button's click must be dropped
    // rather than queued behind it: two adoptions in flight are two writes, and
    // the loser's notice would overwrite the winner's.
    const check = await screen.findByRole("button", { name: /check for new versions/i });
    check.click();
    check.click();
    deliverRegistry([entry()]);

    await waitFor(() => {
      expect(invoke.mock.calls.some(([command]) => command === "m3_component_sync_catalog")).toBe(
        true,
      );
    });
    expect(
      invoke.mock.calls.filter(([command]) => command === "m3_component_sync_catalog"),
    ).toHaveLength(1);
  });
});
