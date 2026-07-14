import { beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  getConfig: vi.fn(),
  stageSnapshot: vi.fn(),
  runDue: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({ isTauri: () => true }));
vi.mock("./portability", () => ({
  getWebDavBackupStatus: (...args: unknown[]) => mocks.getConfig(...args),
  stageEncryptedSnapshot: (...args: unknown[]) => mocks.stageSnapshot(...args),
  runWebDavBackupDue: (...args: unknown[]) => mocks.runDue(...args),
}));

import {
  clearBackupSchedulerForTests,
  markBackupSourceDirty,
  runScheduledBackupCheck,
} from "./backupScheduler";

beforeEach(() => {
  clearBackupSchedulerForTests();
  mocks.getConfig.mockReset();
  mocks.stageSnapshot.mockReset();
  mocks.runDue.mockReset();
  mocks.stageSnapshot.mockResolvedValue({ path: "/tmp/daemon-staged.lmsnapshot" });
  mocks.runDue.mockResolvedValue({ status: "uploaded" });
});

describe("in-app backup catch-up", () => {
  it("does nothing while disabled and stages the latest frontend source before its due time", async () => {
    mocks.getConfig.mockResolvedValueOnce({ config: { enabled: false, nextDueMs: null }, stagedSnapshot: null });
    await runScheduledBackupCheck(1_000);
    mocks.getConfig.mockResolvedValueOnce({ config: { enabled: true, nextDueMs: 2_000 }, stagedSnapshot: null });
    await runScheduledBackupCheck(1_000);
    expect(mocks.stageSnapshot).toHaveBeenCalledTimes(1);
    expect(mocks.runDue).not.toHaveBeenCalled();
  });

  it("stages once and delegates one due check while overlapping desktop ticks share a promise", async () => {
    let release!: () => void;
    mocks.getConfig.mockImplementation(() => new Promise<void>((resolve) => { release = resolve; }).then(() => ({
      config: { enabled: true, nextDueMs: 1 },
      stagedSnapshot: null,
    })));
    const first = runScheduledBackupCheck(2);
    const second = runScheduledBackupCheck(2);
    expect(first).toBe(second);
    release();
    await Promise.all([first, second]);
    expect(mocks.stageSnapshot).toHaveBeenCalledTimes(1);
    expect(mocks.runDue).toHaveBeenCalledTimes(1);
    expect(mocks.runDue).toHaveBeenCalledWith(false);
  });

  it("does not rebuild the encrypted staged source until a subscribed store marks it dirty", async () => {
    mocks.getConfig.mockResolvedValue({
      config: { enabled: true, nextDueMs: 1 },
      stagedSnapshot: { sha256: "a".repeat(64) },
    });
    await runScheduledBackupCheck(2);
    await runScheduledBackupCheck(3);
    expect(mocks.stageSnapshot).toHaveBeenCalledTimes(1);
    expect(mocks.runDue).toHaveBeenCalledTimes(2);
    markBackupSourceDirty();
    await runScheduledBackupCheck(4);
    expect(mocks.stageSnapshot).toHaveBeenCalledTimes(2);
  });

  it("repairs a deleted staged source even when frontend stores are unchanged", async () => {
    mocks.getConfig
      .mockResolvedValueOnce({
        config: { enabled: true, nextDueMs: 10 },
        stagedSnapshot: { sha256: "a".repeat(64) },
      })
      .mockResolvedValueOnce({
        config: { enabled: true, nextDueMs: 10 },
        stagedSnapshot: null,
      });
    await runScheduledBackupCheck(1);
    await runScheduledBackupCheck(2);
    expect(mocks.stageSnapshot).toHaveBeenCalledTimes(2);
    expect(mocks.runDue).not.toHaveBeenCalled();
  });
});
