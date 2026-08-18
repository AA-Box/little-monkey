import { describe, expect, it } from "vitest";
import {
  MAX_REMOTE_ARTIFACT_BYTES,
  OPEN_BACKPRESSURE,
  backpressureGate,
  backpressureMessage,
  backpressureOf,
  isDaemonManagedRun,
  type DaemonQueueRequest,
  type RemotePairRequest,
  validateDaemonQueuePolicy,
  validateRemotePairRequest,
} from "./daemonClient";

const base: DaemonQueueRequest = {
  recipe: "review",
  runKey: null,
  priority: 0,
  maxAttempts: 1,
  maxRuntimeSeconds: 3600,
  maxMemoryMb: null,
  ownedWorktree: true,
  repository: "/workspace/repo",
  branchPrefix: "codex/background/",
  allowedRemotes: ["origin"],
  allowCommit: true,
  allowPush: false,
  allowCreatePullRequest: false,
  allowReviewComment: false,
};

describe("daemon queue policy", () => {
  it("identifies only exact daemon-owned run ids for process controls", () => {
    const managed = ["run-daemon-one", "run-daemon-two"];
    expect(isDaemonManagedRun("run-daemon-one", managed)).toBe(true);
    expect(isDaemonManagedRun("run-desktop", managed)).toBe(false);
    expect(isDaemonManagedRun("run-daemon-one/../escape", managed)).toBe(false);
  });

  it("accepts a bounded local owned-worktree job", () => {
    expect(validateDaemonQueuePolicy(base)).toEqual([]);
  });

  it("flags write expansion outside the isolation policy", () => {
    const warnings = validateDaemonQueuePolicy({
      ...base,
      ownedWorktree: false,
      branchPrefix: "main/",
      allowCreatePullRequest: true,
    });
    expect(warnings).toHaveLength(3);
  });
});

const pair: RemotePairRequest = {
  output: "/tmp/little-monkey-pairing.json",
  expiresMinutes: 15,
  actions: ["view-runs", "view-events", "read-artifacts"],
  runIds: ["run-one"],
  workspaceIds: [],
  maxArtifactBytes: 8 * 1024 * 1024,
};

describe("remote pairing policy", () => {
  it("accepts a bounded exact-run invitation", () => {
    expect(validateRemotePairRequest(pair)).toEqual([]);
  });

  it("rejects the old invalid-by-default empty scope and oversized artifact budget", () => {
    const warnings = validateRemotePairRequest({
      ...pair,
      runIds: [],
      maxArtifactBytes: 64 * 1024 * 1024,
    });
    expect(warnings).toContain("Declare at least one exact run ID or workspace ID.");
    expect(warnings).toContain("Artifact access must be limited to between 1 byte and 32 MiB.");
  });

  it("enforces action dependencies, expiry, identifiers, and protocol limits", () => {
    const warnings = validateRemotePairRequest({
      ...pair,
      expiresMinutes: 0,
      actions: ["approve", "approve", "unknown"],
      runIds: ["../escape"],
      workspaceIds: Array.from({ length: 129 }, (_, index) => `workspace-${index}`),
      maxArtifactBytes: MAX_REMOTE_ARTIFACT_BYTES + 1,
    });
    expect(warnings).toHaveLength(7);
    expect(warnings.some((warning) => warning.includes("view-runs"))).toBe(true);
  });

  it("checks device hardware grants against the node's own rules", () => {
    expect(
      validateRemotePairRequest({
        ...pair,
        deviceCapabilities: ["camera_capture", "location_read"],
      }),
    ).toEqual([]);
    expect(
      validateRemotePairRequest({ ...pair, deviceCapabilities: ["root_access"] }),
    ).toContain("Unknown device hardware capability.");
    // A stream that is the only microphone grant would survive withdrawing
    // microphone capture, which is exactly what the node refuses.
    expect(
      validateRemotePairRequest({ ...pair, deviceCapabilities: ["voice_stream"] }),
    ).toContain("Streaming voice also requires microphone_capture.");
    // Hardware grants are opt-in: an invitation that names none is still valid
    // and simply cannot reach any hardware.
    expect(validateRemotePairRequest({ ...pair, deviceCapabilities: [] })).toEqual([]);
  });
});

describe("K8 backpressure signal", () => {
  it("treats an absent signal as accepting", () => {
    // An older daemon sends no `backpressure` block at all. A signal the app
    // cannot see must never become a refusal — that would make an upgrade
    // break every enqueue.
    expect(backpressureOf({}).state).toBe("accepting");
    expect(backpressureOf({ backpressure: null }).state).toBe("accepting");
    expect(backpressureOf(null)).toEqual(OPEN_BACKPRESSURE);
    expect(backpressureGate(backpressureOf({}), "batch").proceed).toBe(true);
    expect(backpressureGate(backpressureOf({}), "interactive").proceed).toBe(true);
  });

  it("reads the CLI's snake_case block and the bridge's camelCase alike", () => {
    // `monkey daemon status --json` emits snake_case; `daemon_desktop_status`
    // re-serializes the envelope camelCase, and which casing the nested block
    // arrives in depends on the Rust mirror struct's attributes. Accepting both
    // is the difference between a populated card and a silently empty one.
    const snake = backpressureOf({
      backpressure: {
        state: "slow", accepting: true, reason: "queue_deep", detail: "103 of 128 queue slots are in use; slow down",
        retry_after_ms: 2_000, queue_depth: 103, queue_capacity: 128, queued: 40, held: 0,
      },
    });
    const camel = backpressureOf({
      backpressure: {
        state: "slow", accepting: true, reason: "queue_deep", detail: "103 of 128 queue slots are in use; slow down",
        retryAfterMs: 2_000, queueDepth: 103, queueCapacity: 128, queued: 40, held: 0,
      },
    });
    expect(snake).toEqual(camel);
    expect(snake.retryAfterMs).toBe(2_000);
    expect(snake.queueDepth).toBe(103);
    expect(snake.queueCapacity).toBe(128);
  });

  it("blocks the enqueue when closed and allows it when accepting", () => {
    const closed = backpressureOf({
      backpressure: {
        state: "closed", accepting: false, reason: "queue_full",
        detail: "128 of 128 queue slots are in use; wait for a run or cancel one",
        retry_after_ms: 5_000, queue_depth: 128, queue_capacity: 128, queued: 90, held: 0,
      },
    });
    // Not deferrable: the daemon's own `enqueue` refuses here, so an override
    // would only trade the actionable sentence for a generic error.
    expect(backpressureGate(closed, "batch")).toMatchObject({ proceed: false, deferrable: false });
    expect(backpressureGate(closed, "interactive")).toMatchObject({ proceed: false, deferrable: false });

    const accepting = backpressureOf({
      backpressure: {
        state: "accepting", accepting: true, reason: null, detail: null,
        retry_after_ms: null, queue_depth: 2, queue_capacity: 128, queued: 1, held: 0,
      },
    });
    expect(backpressureGate(accepting, "batch").proceed).toBe(true);
    expect(backpressureGate(accepting, "interactive").proceed).toBe(true);
  });

  it("defers a batch job on slow but never an interactive turn", () => {
    const slow = backpressureOf({
      backpressure: {
        state: "slow", accepting: true, reason: "memory_saturated",
        detail: "all 4 queued runs are waiting on memory; more work will queue but not start",
        retry_after_ms: 8_000, queue_depth: 6, queue_capacity: 128, queued: 4, held: 4,
      },
    });
    expect(backpressureGate(slow, "batch")).toMatchObject({ proceed: false, deferrable: true });
    expect(backpressureGate(slow, "interactive").proceed).toBe(true);
  });

  it("falls back to accepting on a state token this build does not know", () => {
    // Guessing "closed" over a vocabulary mismatch would block work for a
    // reason the user cannot act on.
    expect(backpressureOf({ backpressure: { state: "wedged" } as never }).state).toBe("accepting");
  });

  it("shows the daemon's own sentence plus the advisory retry hint", () => {
    const closed = backpressureOf({
      backpressure: {
        state: "closed", accepting: false, reason: "kill_switch",
        detail: "the global kill switch is engaged; release it before queueing work",
        retry_after_ms: null, queue_depth: 0, queue_capacity: 128, queued: 0, held: 0,
      },
    });
    expect(backpressureMessage(closed, "fallback", (ms) => `retry ${ms}`))
      .toBe("the global kill switch is engaged; release it before queueing work");

    const full = backpressureOf({
      backpressure: {
        state: "closed", accepting: false, reason: "queue_full", detail: "the queue is full",
        retry_after_ms: 5_000, queue_depth: 128, queue_capacity: 128, queued: 128, held: 0,
      },
    });
    expect(backpressureMessage(full, "fallback", (ms) => `retry in ${ms}ms`)).toBe("the queue is full retry in 5000ms");
    // No detail (older or terser daemon) still yields a sentence.
    expect(backpressureMessage(OPEN_BACKPRESSURE, "fallback", (ms) => `retry ${ms}`)).toBe("fallback");
  });
});
