import { beforeEach, describe, expect, it } from "vitest";

import { useSettingsStore } from "./settingsStore";

describe("settingsStore.checkpointRetention", () => {
  beforeEach(() => {
    useSettingsStore.setState({ checkpointRetention: 20 });
  });

  it("defaults to 20", () => {
    expect(useSettingsStore.getState().checkpointRetention).toBe(20);
  });

  it("clamps below the 5-checkpoint floor", () => {
    useSettingsStore.getState().setCheckpointRetention(0);
    expect(useSettingsStore.getState().checkpointRetention).toBe(5);
  });

  it("clamps above the 100-checkpoint ceiling", () => {
    useSettingsStore.getState().setCheckpointRetention(500);
    expect(useSettingsStore.getState().checkpointRetention).toBe(100);
  });

  it("rounds fractional input", () => {
    useSettingsStore.getState().setCheckpointRetention(42.6);
    expect(useSettingsStore.getState().checkpointRetention).toBe(43);
  });

  it("accepts an in-range value unchanged", () => {
    useSettingsStore.getState().setCheckpointRetention(50);
    expect(useSettingsStore.getState().checkpointRetention).toBe(50);
  });
});
