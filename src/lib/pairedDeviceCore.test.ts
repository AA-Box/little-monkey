// The paired device's decision logic, exercised directly.
//
// `device-core.js` is served to the phone by the runner (see
// `daemon/remote/web.rs`). It is imported here rather than asserted against as
// source text, because the properties that matter — a physical effect happens
// at most once, a result is never dropped before the runner acknowledges it —
// are behaviour, and a `contains("…")` assertion cannot see behaviour.
import { describe, expect, it, vi } from "vitest";

import {
  ARTIFACT_CAPABILITIES,
  JOURNAL_LIMITS,
  PERMISSION,
  PHASE,
  READINESS,
  acquireExecutor,
  capacityRefusal,
  createJournal,
  deliverStaged,
  describeCapability,
  isEffective,
  journalUpgrade,
  leaseDecision,
  nextBackoffMs,
  prunableEntries,
  recoveryAction,
  unknownOutcomeReport,
  // eslint-disable-next-line @typescript-eslint/ban-ts-comment
  // @ts-ignore — a plain browser module served by the runner, no types of its own.
} from "../../src-tauri/src/bin/monkey-cli/daemon/remote/ui/device-core.js";

const EVERY_PHYSICAL_CAPABILITY = [
  "device_info",
  "camera_capture",
  "microphone_capture",
  "location_read",
  "notification_post",
  "screen_capture",
  "audio_playback",
  "voice_stream",
];

function probe(overrides: Record<string, unknown> = {}) {
  return {
    supported: true,
    permissions: {},
    notificationPermission: "default",
    screenShareLive: false,
    audioEnabled: false,
    foreground: true,
    ...overrides,
  };
}

describe("capability permission and readiness mapping", () => {
  it("gives every physical capability a defined permission and readiness", () => {
    for (const capability of EVERY_PHYSICAL_CAPABILITY) {
      const answer = describeCapability(capability, probe());
      expect(Object.values(PERMISSION)).toContain(answer.permission);
      expect(Object.values(READINESS)).toContain(answer.readiness);
    }
  });

  it("does not invent an OS permission for capabilities that have none", () => {
    // The defect this prevents: the runner demanded `granted`, the browser could
    // only ever answer "not asked", and the capability stayed advertised and
    // permanently unusable.
    expect(describeCapability("device_info", probe())).toEqual({
      permission: PERMISSION.notRequired,
      readiness: READINESS.ready,
    });
    expect(describeCapability("screen_capture", probe()).permission).toBe(PERMISSION.notRequired);
    expect(describeCapability("audio_playback", probe()).permission).toBe(PERMISSION.notRequired);
  });

  it("treats an unanswerable permission as promptable, never as granted", () => {
    // `navigator.permissions.query` returning nothing is "cannot answer", and
    // cannot-answer is not consent.
    const answer = describeCapability("camera_capture", probe({ permissions: {} }));
    expect(answer.permission).toBe(PERMISSION.promptable);
    expect(isEffective({ granted: true, supported: true, ...answer })).toBe(false);
  });

  it("maps the browser's own notification permission", () => {
    const cases: Array<[string, string]> = [
      ["granted", PERMISSION.granted],
      ["denied", PERMISSION.denied],
      ["default", PERMISSION.promptable],
    ];
    for (const [browser, expected] of cases) {
      expect(
        describeCapability("notification_post", probe({ notificationPermission: browser })).permission,
      ).toBe(expected);
    }
    // A notification does not need the page in front — that is the point of one.
    expect(
      describeCapability("notification_post", probe({ notificationPermission: "granted", foreground: false }))
        .readiness,
    ).toBe(READINESS.ready);
  });

  it("follows the armed screen share, in both directions", () => {
    expect(describeCapability("screen_capture", probe({ screenShareLive: false })).readiness).toBe(
      READINESS.armedRequired,
    );
    expect(describeCapability("screen_capture", probe({ screenShareLive: true })).readiness).toBe(
      READINESS.ready,
    );
    // Ending the share must take readiness with it: the honest report after the
    // user stops sharing is that a capture would need arming again.
    expect(
      isEffective({
        granted: true,
        supported: true,
        ...describeCapability("screen_capture", probe({ screenShareLive: false })),
      }),
    ).toBe(false);
  });

  it("models autoplay as readiness, not as a permission", () => {
    expect(describeCapability("audio_playback", probe({ audioEnabled: false })).readiness).toBe(
      READINESS.interactionRequired,
    );
    expect(describeCapability("audio_playback", probe({ audioEnabled: true })).readiness).toBe(
      READINESS.ready,
    );
  });

  it("reports foreground-only capabilities honestly when the page is hidden", () => {
    for (const capability of ["camera_capture", "microphone_capture", "voice_stream", "location_read"]) {
      const answer = describeCapability(
        capability,
        probe({ permissions: { [capability]: "granted" }, foreground: false }),
      );
      expect(answer.permission).toBe(PERMISSION.granted);
      expect(answer.readiness).toBe(READINESS.foregroundRequired);
      expect(isEffective({ granted: true, supported: true, ...answer })).toBe(false);
    }
  });

  it("reports an unsupported capability as unsupported and unavailable", () => {
    const answer = describeCapability("camera_capture", probe({ supported: false }));
    expect(answer).toEqual({ permission: PERMISSION.unsupported, readiness: READINESS.unavailable });
  });

  it("requires all four axes to agree", () => {
    const all = { granted: true, supported: true, permission: PERMISSION.granted, readiness: READINESS.ready };
    expect(isEffective(all)).toBe(true);
    expect(isEffective({ ...all, granted: false })).toBe(false);
    expect(isEffective({ ...all, supported: false })).toBe(false);
    expect(isEffective({ ...all, permission: PERMISSION.denied })).toBe(false);
    expect(isEffective({ ...all, permission: PERMISSION.promptable })).toBe(false);
    expect(isEffective({ ...all, readiness: READINESS.armedRequired })).toBe(false);
    // …and not-required counts as permission, which is the whole fix.
    expect(isEffective({ ...all, permission: PERMISSION.notRequired })).toBe(true);
  });
});

describe("recovery after a reload or a crash", () => {
  it("delivers a staged result rather than repeating the effect", () => {
    expect(recoveryAction({ phase: PHASE.resultStaged })).toEqual({ action: "deliver_staged" });
  });

  it("reports an unknown outcome when the crash landed inside the uncertainty window", () => {
    // The runner authorized a start, so the camera may have fired, and nothing
    // survived to prove it. Repeating it is the one thing that must not happen.
    const decision = recoveryAction({ phase: PHASE.startAuthorized });
    expect(decision).toEqual({ action: "report_unknown", reason: "crashed_after_start" });
    const report = unknownOutcomeReport(decision.reason);
    expect(report.outcome).toBe("failed");
    expect(report.error).toContain("execution_outcome_unknown_after_restart");
    expect(report.error).toContain("NOT repeated");
  });

  it("never executes a running command it has no record of", () => {
    expect(recoveryAction(null)).toEqual({ action: "report_unknown", reason: "no_local_record" });
    expect(recoveryAction({ phase: PHASE.received })).toEqual({
      action: "report_unknown",
      reason: "no_start_authorized",
    });
  });

  it("has nothing to do for an acknowledged command", () => {
    expect(recoveryAction({ phase: PHASE.resultAcked })).toEqual({ action: "none" });
  });

  it("refuses to execute a leased command it already started", () => {
    expect(leaseDecision(null)).toEqual({ action: "execute" });
    expect(leaseDecision({ phase: PHASE.received })).toEqual({ action: "execute" });
    expect(leaseDecision({ phase: PHASE.resultStaged })).toEqual({ action: "deliver_staged" });
    expect(leaseDecision({ phase: PHASE.resultAcked })).toEqual({ action: "none" });
    expect(leaseDecision({ phase: PHASE.startAuthorized })).toEqual({
      action: "report_unknown",
      reason: "already_started",
    });
  });
});

// A map standing in for IndexedDB. The adapter's whole contract is four
// methods; a real object store adds nothing this test could observe.
function memoryJournal() {
  const rows = new Map<string, Record<string, unknown>>();
  return {
    rows,
    adapter: {
      get: async (id: string) => rows.get(id) ?? null,
      all: async () => [...rows.values()],
      put: async (record: Record<string, unknown>) => {
        rows.set(record.commandId as string, record);
      },
      remove: async (ids: string[]) => {
        for (const id of ids) rows.delete(id);
      },
    },
  };
}

describe("durable result delivery", () => {
  it("keeps the bytes until the runner acknowledges them", async () => {
    const { adapter, rows } = memoryJournal();
    const journal = createJournal(adapter, { now: () => 1_000 });
    const entry = {
      commandId: "dcmd-1",
      phase: PHASE.resultStaged,
      outcome: "succeeded",
      artifactBlob: { size: 4 },
      artifactBytes: 4,
    };
    await journal.write(entry);

    const failing = vi.fn().mockRejectedValue(new Error("network down"));
    const first = await deliverStaged(entry, { journal, send: failing });
    expect(first.outcome).toBe("retry");
    expect(first.backoffMs).toBe(nextBackoffMs(0));
    // The artifact is still here. Dropping it on a failed send would lose the
    // only proof of an effect that really happened.
    expect(rows.get("dcmd-1")?.artifactBlob).toEqual({ size: 4 });
    expect(rows.get("dcmd-1")?.phase).toBe(PHASE.resultStaged);
    expect(rows.get("dcmd-1")?.deliveryAttempts).toBe(1);

    const second = await deliverStaged(rows.get("dcmd-1")!, {
      journal,
      send: vi.fn().mockResolvedValue(undefined),
    });
    expect(second.outcome).toBe("acked");
    expect(rows.get("dcmd-1")?.phase).toBe(PHASE.resultAcked);
    expect(rows.get("dcmd-1")?.artifactBlob).toBeNull();
    expect(rows.get("dcmd-1")?.artifactBytes).toBe(0);
  });

  it("stops retrying when the runner holds a different authoritative result", async () => {
    const { adapter, rows } = memoryJournal();
    const journal = createJournal(adapter, { now: () => 1_000 });
    const entry = { commandId: "dcmd-2", phase: PHASE.resultStaged, artifactBlob: { size: 9 }, artifactBytes: 9 };
    await journal.write(entry);
    const conflict = Object.assign(new Error("conflicting terminal replay"), { status: 409 });
    const answer = await deliverStaged(entry, { journal, send: vi.fn().mockRejectedValue(conflict) });
    expect(answer.outcome).toBe("conflict");
    expect(rows.get("dcmd-2")?.phase).toBe(PHASE.resultAcked);
    expect(rows.get("dcmd-2")?.artifactBlob).toBeNull();
  });

  it("backs off, bounded", () => {
    expect(nextBackoffMs(0)).toBe(1_000);
    expect(nextBackoffMs(3)).toBe(8_000);
    expect(nextBackoffMs(50)).toBe(60_000);
    expect(nextBackoffMs(-1)).toBe(1_000);
  });
});

describe("local storage bounds", () => {
  it("refuses to start an artifact command with nowhere to put the result", () => {
    const held = [
      { commandId: "a", phase: PHASE.resultStaged, artifactBytes: JOURNAL_LIMITS.maxArtifactBytes - 1_000 },
    ];
    const refusal = capacityRefusal(held, "camera_capture", 8 * 1024 * 1024);
    expect(refusal).toContain("device_storage_full");
    expect(refusal).toContain("was not started");
    // A command that produces no bytes needs no room.
    expect(capacityRefusal(held, "location_read", 8 * 1024 * 1024)).toBeNull();
    // And with room, nothing is refused.
    expect(capacityRefusal([], "camera_capture", 8 * 1024 * 1024)).toBeNull();
  });

  it("counts only what the runner has not acknowledged", () => {
    const acked = [
      { commandId: "a", phase: PHASE.resultAcked, artifactBytes: JOURNAL_LIMITS.maxArtifactBytes },
    ];
    expect(capacityRefusal(acked, "camera_capture", 1_024)).toBeNull();
  });

  it("names exactly the capabilities that produce bytes", () => {
    expect([...ARTIFACT_CAPABILITIES].sort()).toEqual([
      "camera_capture",
      "microphone_capture",
      "screen_capture",
    ]);
  });

  it("never prunes an unacknowledged result to satisfy a bound", async () => {
    const now = 10_000_000;
    const entries = [
      // Old, acknowledged: fair game.
      { commandId: "old", phase: PHASE.resultAcked, updatedAtMs: now - JOURNAL_LIMITS.ackedTtlMs - 1 },
      // Old, and still owed to the runner: never.
      { commandId: "owed", phase: PHASE.resultStaged, updatedAtMs: 0 },
      // Older still, and possibly mid-effect: never.
      { commandId: "started", phase: PHASE.startAuthorized, updatedAtMs: 0 },
    ];
    expect(prunableEntries(entries, now)).toEqual(["old"]);

    const { adapter, rows } = memoryJournal();
    const journal = createJournal(adapter, { now: () => now });
    for (const entry of entries) await adapter.put(entry);
    await journal.prune();
    expect([...rows.keys()].sort()).toEqual(["owed", "started"]);
  });

  it("drops the oldest acknowledged entries when there are too many", () => {
    const entries = Array.from({ length: JOURNAL_LIMITS.maxEntries + 3 }, (_, index) => ({
      commandId: `c${index}`,
      phase: PHASE.resultAcked,
      updatedAtMs: index,
    }));
    expect(prunableEntries(entries, 0)).toEqual(["c0", "c1", "c2"]);
  });
});

describe("storage upgrade", () => {
  it("adds the journal store and leaves an existing pairing alone", () => {
    const created: string[] = [];
    const existing = ["controllers"];
    const database = {
      objectStoreNames: { contains: (name: string) => existing.includes(name) },
      createObjectStore: (name: string) => created.push(name),
    };
    expect(journalUpgrade(database, "controllers", "device_command_journal")).toEqual([
      "device_command_journal",
    ]);
    // The controller store — the device key, its sequence and its cache — is
    // never recreated, so an upgrade never costs a re-pair.
    expect(created).toEqual(["device_command_journal"]);
  });

  it("creates both stores on a fresh install", () => {
    const created: string[] = [];
    const database = {
      objectStoreNames: { contains: () => false },
      createObjectStore: (name: string) => created.push(name),
    };
    journalUpgrade(database, "controllers", "device_command_journal");
    expect(created).toEqual(["controllers", "device_command_journal"]);
  });
});

describe("one executor per profile", () => {
  it("runs the loop in the tab that holds the lock", async () => {
    const body = vi.fn().mockResolvedValue(undefined);
    const locks = {
      request: vi.fn(async (_name: string, _options: unknown, callback: (lock: unknown) => unknown) =>
        callback({ name: "held" }),
      ),
    };
    await expect(acquireExecutor(locks, "executor", body)).resolves.toEqual({ executor: true });
    expect(body).toHaveBeenCalledOnce();
    expect(locks.request).toHaveBeenCalledWith(
      "executor",
      // `ifAvailable`, so a second tab does not silently take over the moment
      // the first is closed mid-command.
      { mode: "exclusive", ifAvailable: true },
      expect.any(Function),
    );
  });

  it("does nothing at all in a second tab", async () => {
    const body = vi.fn().mockResolvedValue(undefined);
    const locks = {
      request: async (_name: string, _options: unknown, callback: (lock: unknown) => unknown) =>
        callback(null),
    };
    await expect(acquireExecutor(locks, "executor", body)).resolves.toEqual({ executor: false });
    expect(body).not.toHaveBeenCalled();
  });
});
