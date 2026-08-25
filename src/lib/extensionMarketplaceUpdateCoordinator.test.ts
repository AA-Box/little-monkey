import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  runUpdateCycle: vi.fn<() => Promise<void>>(),
}));

vi.mock("../store/extensionMarketplaceStore", () => ({
  useExtensionMarketplaceStore: {
    getState: () => ({ runUpdateCycle: mocks.runUpdateCycle }),
  },
}));

import {
  extensionMarketplaceUpdateInFlight,
  startExtensionMarketplaceUpdateCoordinator,
  stopExtensionMarketplaceUpdateCoordinator,
} from "./extensionMarketplaceUpdateCoordinator";

async function flushPromises(): Promise<void> {
  await Promise.resolve();
  await Promise.resolve();
}

describe("extension marketplace update coordinator", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    stopExtensionMarketplaceUpdateCoordinator();
    mocks.runUpdateCycle.mockReset();
    mocks.runUpdateCycle.mockResolvedValue(undefined);
  });

  afterEach(() => {
    stopExtensionMarketplaceUpdateCoordinator();
    vi.useRealTimers();
  });

  it("runs an update cycle immediately at application startup without Settings mounting", async () => {
    startExtensionMarketplaceUpdateCoordinator();
    expect(mocks.runUpdateCycle).toHaveBeenCalledTimes(1);
    expect(extensionMarketplaceUpdateInFlight()).toBe(true);
    await flushPromises();
    expect(extensionMarketplaceUpdateInFlight()).toBe(false);
  });

  it("keeps a single application-level timer when startup is invoked twice", async () => {
    startExtensionMarketplaceUpdateCoordinator(60_000);
    startExtensionMarketplaceUpdateCoordinator(60_000);
    await flushPromises();
    expect(mocks.runUpdateCycle).toHaveBeenCalledTimes(1);

    await vi.advanceTimersByTimeAsync(60_000);
    await flushPromises();
    expect(mocks.runUpdateCycle).toHaveBeenCalledTimes(2);
  });

  it("suppresses overlapping cycles instead of running duplicate automatic mutations", async () => {
    let finish: (() => void) | null = null;
    mocks.runUpdateCycle.mockImplementation(() => new Promise<void>((resolve) => { finish = resolve; }));
    startExtensionMarketplaceUpdateCoordinator(60_000);
    expect(mocks.runUpdateCycle).toHaveBeenCalledTimes(1);

    await vi.advanceTimersByTimeAsync(180_000);
    expect(mocks.runUpdateCycle).toHaveBeenCalledTimes(1);

    finish?.();
    await flushPromises();
    await vi.advanceTimersByTimeAsync(60_000);
    expect(mocks.runUpdateCycle).toHaveBeenCalledTimes(2);
  });
});
