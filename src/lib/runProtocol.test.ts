import { beforeEach, describe, expect, it, vi } from "vitest";

const { invoke } = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn() }));

import { appendRunEvent, archiveRun, decideRunPermission, listRuns, requestRunCancellation, unarchiveRun } from "./runProtocol";

describe("run protocol client", () => {
  beforeEach(() => invoke.mockReset());

  it("lets the Rust host assign envelope identity, time, and sequence", async () => {
    invoke.mockResolvedValue({});
    const event = { type: "queued", payload: { queue: "interactive" } } as const;
    await appendRunEvent("run-1", event, "member-1");
    expect(invoke).toHaveBeenCalledWith("run_append_event", {
      runId: "run-1",
      actorId: "member-1",
      event,
    });
  });

  it("routes permission decisions through the digest-checking command", async () => {
    invoke.mockResolvedValue({});
    await decideRunPermission("run-1", "request-1", "a".repeat(64), "allow_once");
    expect(invoke).toHaveBeenCalledWith("run_decide_permission", {
      runId: "run-1",
      requestId: "request-1",
      operationSha256: "a".repeat(64),
      decision: "allow_once",
    });
  });

  it("uses a bounded run-history default that hides archived runs", async () => {
    invoke.mockResolvedValue([]);
    await listRuns();
    expect(invoke).toHaveBeenCalledWith("run_list", { limit: 200, includeArchived: false });
  });

  it("can include archived runs on request", async () => {
    invoke.mockResolvedValue([]);
    await listRuns(200, true);
    expect(invoke).toHaveBeenCalledWith("run_list", { limit: 200, includeArchived: true });
  });

  it("archives a run through a dedicated command", async () => {
    invoke.mockResolvedValue({});
    await archiveRun("run-1");
    expect(invoke).toHaveBeenCalledWith("run_archive", { runId: "run-1" });
  });

  it("unarchives a run through a dedicated command", async () => {
    invoke.mockResolvedValue({});
    await unarchiveRun("run-1");
    expect(invoke).toHaveBeenCalledWith("run_unarchive", { runId: "run-1" });
  });

  it("requests cancellation through a host-attributed command", async () => {
    invoke.mockResolvedValue({});
    await requestRunCancellation("run-1", "User stopped it");
    expect(invoke).toHaveBeenCalledWith("run_request_cancellation", {
      runId: "run-1",
      reason: "User stopped it",
    });
  });
});
