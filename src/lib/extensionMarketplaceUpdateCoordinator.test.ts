import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  hydrate: vi.fn<() => Promise<void>>(),
  runUpdateCycle: vi.fn<() => Promise<void>>(),
}));

vi.mock("../store/extensionMarketplaceStore", () => ({
  useExtensionMarketplaceStore: {
    getState: () => ({
      hydrate: mocks.hydrate,
      runUpdateCycle: mocks.runUpdateCycle,
    }),
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
  await Promise.resolve();
}

describe("extension marketplace update coordinator", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    stopExtensionMarketplaceUpdateCoordinator();
    mocks.hydrate.mockReset();
    mocks.hydrate.mockResolvedValue(undefined);
    mocks.runUpdateCycle.mockReset();
    mocks.runUpdateCycle.mockResolvedValue(undefined);
  });

  afterEach(() => {
    stopExtensionMarketplaceUpdateCoordinator();
    vi.useRealTimers();
  });

  it("hydrates persisted policy before the startup update cycle", async () => {
    startExtensionMarketplaceUpdateCoordinator();
    expect(mocks.hydrate).toHaveBeenCalledTimes(1);
    expect(mocks.runUpdateCycle).not.toHaveBeenCalled();
    expect(extensionMarketplaceUpdateInFlight()).toBe(true);

    await flushPromises();
    expect(mocks.runUpdateCycle).toHaveBeenCalledTimes(1);
    expect(extensionMarketplaceUpdateInFlight()).toBe(false);
  });

  it("cannot begin a network-capable cycle before policy hydration completes", async () => {
    let finishHydration: () => void = () => {};
    mocks.hydrate.mockImplementation(() => new Promise<void>((resolve) => { finishHydration = resolve; }));

    startExtensionMarketplaceUpdateCoordinator();
    await flushPromises();
    expect(mocks.hydrate).toHaveBeenCalledTimes(1);
    expect(mocks.runUpdateCycle).not.toHaveBeenCalled();

    finishHydration();
    await flushPromises();
    expect(mocks.runUpdateCycle).toHaveBeenCalledTimes(1);
  });

  it("keeps a single application-level timer when startup is invoked twice", async () => {
    startExtensionMarketplaceUpdateCoordinator(60_000);
    startExtensionMarketplaceUpdateCoordinator(60_000);
    await flushPromises();
    expect(mocks.hydrate).toHaveBeenCalledTimes(1);
    expect(mocks.runUpdateCycle).toHaveBeenCalledTimes(1);

    await vi.advanceTimersByTimeAsync(60_000);
    await flushPromises();
    expect(mocks.hydrate).toHaveBeenCalledTimes(1);
    expect(mocks.runUpdateCycle).toHaveBeenCalledTimes(2);
  });

  it("suppresses overlapping cycles instead of running duplicate automatic mutations", async () => {
    let finish: () => void = () => {};
    mocks.runUpdateCycle.mockImplementation(() => new Promise<void>((resolve) => { finish = resolve; }));
    startExtensionMarketplaceUpdateCoordinator(60_000);
    await flushPromises();
    expect(mocks.runUpdateCycle).toHaveBeenCalledTimes(1);

    await vi.advanceTimersByTimeAsync(180_000);
    expect(mocks.runUpdateCycle).toHaveBeenCalledTimes(1);

    finish();
    await flushPromises();
    await vi.advanceTimersByTimeAsync(60_000);
    expect(mocks.runUpdateCycle).toHaveBeenCalledTimes(2);
  });
});
