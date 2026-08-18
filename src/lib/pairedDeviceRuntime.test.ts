// The paired device's command runtime, exercised as behaviour.
//
// `runLeasedCommand` is the orchestration the phone actually runs (see
// `daemon/remote/ui/app.js`, which supplies the browser half and nothing else).
// It is imported and driven here with a fake transport, a fake journal store
// and a physical effect that is counted rather than performed, because every
// property worth testing about it is an *ordering*:
//
//   nothing physical before the runner authorizes a start,
//   nothing awaited between the result existing and the result being durable,
//   nothing forgotten before the runner acknowledges it.
//
// A `contains("…")` assertion over the source cannot see an ordering. These can.
import { describe, expect, it } from "vitest";

import {
  PHASE,
  createJournal,
  deliverStaged,
  recordAudio,
  runLeasedCommand,
  speakText,
  // eslint-disable-next-line @typescript-eslint/ban-ts-comment
  // @ts-ignore — a plain browser module served by the runner, no types of its own.
} from "../../src-tauri/src/bin/monkey-cli/daemon/remote/ui/device-core.js";

type Entry = Record<string, any>;

/** IndexedDB's role, played by a Map that survives "reloading" the client. */
function journalStore() {
  const records = new Map<string, Entry>();
  const writes: Entry[] = [];
  const adapter = {
    get: async (commandId: string) => records.get(commandId) ?? null,
    all: async () => [...records.values()],
    put: async (record: Entry) => {
      records.set(record.commandId, record);
      writes.push({ ...record });
    },
    remove: async (ids: string[]) => {
      for (const id of ids) records.delete(id);
    },
  };
  return { records, writes, adapter };
}

const COMMAND = {
  command_id: "dcmd-camera-1",
  capability: "camera_capture",
  arguments: { position: "back" },
  arguments_sha256: "a".repeat(64),
  expires_at_ms: 900_000,
  cancel_requested: false,
};

class Cancelled extends Error {
  cancelled = true;
}

/**
 * The runner, as far as the device can tell.
 *
 * `control` is deliberately a request that never answers on its own: that is
 * what a long poll is, and it is the exact condition under which a result used
 * to sit in memory waiting for the network before it was written down.
 */
function transportFixture(overrides: Record<string, any> = {}) {
  const calls: string[] = [];
  const state = {
    controlStarted: false,
    controlSettled: false,
    resultBodies: [] as any[],
    failResultTimes: 0,
    failStart: false,
  };
  const request = async (method: string, path: string, body?: any, options: any = {}) => {
    calls.push(`${method} ${path}`);
    if (path.endsWith("/start")) {
      if (state.failStart) throw new Error("the network went away");
      return { started: true, recoverable: false, execution_id: body?.execution_id };
    }
    if (path.includes("/control")) {
      state.controlStarted = true;
      return await new Promise((_resolve, reject) => {
        options.signal?.addEventListener?.("abort", () => {
          state.controlSettled = true;
          reject(new Cancelled("This request was cancelled on the device"));
        });
      });
    }
    if (path.endsWith("/result")) {
      state.resultBodies.push(body);
      if (state.failResultTimes > 0) {
        state.failResultTimes -= 1;
        throw new Error("the reply never arrived");
      }
      return { acknowledged: true };
    }
    throw new Error(`unexpected request ${path}`);
  };
  return { calls, state, request, ...overrides };
}

/** The device half: a journal, a counted effect and the delivery path. */
function deviceFixture(options: { artifact?: Blob | null } = {}) {
  const store = journalStore();
  const journal = createJournal(store.adapter);
  const transport = transportFixture();
  const effects: string[] = [];
  const artifact = options.artifact === undefined ? new Blob(["jpeg-bytes"]) : options.artifact;

  const report = async (commandId: string, terminal: any, executionId?: string | null) =>
    transport.request("POST", `/v1/remote/device/commands/${commandId}/result`, {
      outcome: terminal.outcome,
      result: terminal.result ?? null,
      error: terminal.error ?? null,
      artifact_bytes: terminal.artifactBlob ? terminal.artifactBlob.size : 0,
      artifact_sha256: terminal.artifactSha256 ?? null,
      execution_id: executionId ?? null,
    });

  const deliver = (entry: Entry) =>
    deliverStaged(entry, {
      journal,
      send: (staged: Entry) =>
        report(
          staged.commandId,
          {
            outcome: staged.outcome,
            result: staged.result,
            error: staged.error,
            artifactBlob: staged.artifactBlob,
            artifactSha256: staged.artifactSha256,
          },
          staged.executionId,
        ),
    });

  const deps = {
    journal,
    request: transport.request,
    perform: async (command: any) => {
      effects.push(command.command_id);
      return {
        outcome: "succeeded",
        result: { width: 4, height: 3 },
        artifactBlob: artifact,
        artifactMediaType: "image/jpeg",
        artifactSha256: "b".repeat(64),
      };
    },
    deliver,
    report,
    newExecutionId: () => "exec-000000000001",
    artifactCeiling: 8 * 1024 * 1024,
    controlWaitMs: 25_000,
  };
  return { store, journal, transport, effects, deliver, deps };
}

describe("running one leased command", () => {
  it("stages the result durably while the control long-poll is still pending", async () => {
    const device = deviceFixture();
    let stagedWhileControlPending: boolean | null = null;
    const put = device.store.adapter.put;
    device.store.adapter.put = async (record: Entry) => {
      if (record.phase === PHASE.resultStaged) {
        stagedWhileControlPending =
          device.transport.state.controlStarted && !device.transport.state.controlSettled;
      }
      await put(record);
    };

    await runLeasedCommand(COMMAND, device.deps);

    // The whole point. If the code waited for the watcher's request before
    // writing the result down, the only way that request could have finished is
    // by being aborted — so `controlSettled` would already be true here, and a
    // crash in that window would have lost bytes a real camera produced.
    expect(stagedWhileControlPending).toBe(true);
    // And only afterwards is the watcher stopped, rather than waited out.
    expect(device.transport.state.controlSettled).toBe(true);
    expect(device.effects).toEqual(["dcmd-camera-1"]);
  });

  it("keeps the staged artifact when the result response is lost, and re-delivers it after a reload", async () => {
    const device = deviceFixture();
    device.transport.state.failResultTimes = 1;

    await runLeasedCommand(COMMAND, device.deps);

    // One effect, and the bytes are still held: an unacknowledged result is the
    // one thing a device may never drop.
    expect(device.effects).toHaveLength(1);
    const held = device.store.records.get("dcmd-camera-1");
    expect(held?.phase).toBe(PHASE.resultStaged);
    expect(held?.artifactBlob).toBeTruthy();
    expect(held?.artifactBytes).toBeGreaterThan(0);

    // "The browser was reloaded": a new journal over the same durable store.
    const reloaded = createJournal(device.store.adapter);
    const staged = (await reloaded.all()).filter((entry: Entry) => entry.phase === PHASE.resultStaged);
    expect(staged).toHaveLength(1);
    const answer = await deliverStaged(staged[0], {
      journal: reloaded,
      send: (entry: Entry) =>
        device.transport.request("POST", `/v1/remote/device/commands/${entry.commandId}/result`, {
          outcome: entry.outcome,
          artifact_bytes: entry.artifactBlob ? entry.artifactBlob.size : 0,
          execution_id: entry.executionId,
        }),
    });

    expect(answer.outcome).toBe("acked");
    // Delivered twice, performed once.
    expect(device.transport.state.resultBodies).toHaveLength(2);
    expect(device.effects).toHaveLength(1);
    // Only now are the bytes forgotten.
    const acked = device.store.records.get("dcmd-camera-1");
    expect(acked?.phase).toBe(PHASE.resultAcked);
    expect(acked?.artifactBlob).toBeNull();
  });

  it("never performs the same command twice, however often the runner hands it over", async () => {
    const device = deviceFixture();
    await runLeasedCommand(COMMAND, device.deps);
    await runLeasedCommand(COMMAND, device.deps);
    await runLeasedCommand(COMMAND, device.deps);
    expect(device.effects).toEqual(["dcmd-camera-1"]);
  });

  it("performs nothing when the start is never authorized, and leaves no record to strand", async () => {
    const device = deviceFixture();
    device.transport.state.failStart = true;
    let reported: unknown = null;
    const answer = await runLeasedCommand(COMMAND, {
      ...device.deps,
      onStartFailed: (error: unknown) => {
        reported = error;
      },
    });
    expect(answer.action).toBe("start_refused");
    expect(reported).toBeTruthy();
    expect(device.effects).toEqual([]);
    // Nothing physical was authorized, so the runner may hand it out again once
    // the lease lapses — which it can only do safely if this device kept no
    // half-written claim on it.
    expect(device.store.records.has("dcmd-camera-1")).toBe(false);
  });

  it("performs nothing when the runner answers that the command is already running", async () => {
    const device = deviceFixture();
    const deps = {
      ...device.deps,
      request: async (method: string, path: string, body?: any, options?: any) => {
        if (path.endsWith("/start")) {
          return { started: false, recoverable: true, execution_id: "exec-somebody-else" };
        }
        return device.transport.request(method, path, body, options);
      },
    };
    const answer = await runLeasedCommand(COMMAND, deps);
    expect(answer.action).toBe("already_running");
    expect(device.effects).toEqual([]);
    // Recorded as started, so recovery reports it unknown rather than repeating
    // it — the effect may have happened on the execution that holds it.
    expect(device.store.records.get("dcmd-camera-1")?.phase).toBe(PHASE.startAuthorized);
  });

  it("refuses before the effect when there is no room to stage the result", async () => {
    const device = deviceFixture();
    device.store.records.set("dcmd-older", {
      commandId: "dcmd-older",
      phase: PHASE.resultStaged,
      // Everything the journal is allowed to hold, still undelivered.
      artifactBytes: 60 * 1024 * 1024,
    });
    const answer = await runLeasedCommand(COMMAND, { ...device.deps, artifactCeiling: 8 * 1024 * 1024 });
    expect(answer.action).toBe("refused");
    expect(device.effects).toEqual([]);
    expect(device.transport.state.resultBodies[0].outcome).toBe("failed");
    // Reported from the far side of the authorization boundary, because that is
    // the only side a terminal report is accepted from — the runner refuses one
    // for a command it never authorized, and it is right to.
    expect(device.transport.calls[0]).toContain("/start");
  });

  it("lets the runner resolve a command cancelled before it was started", async () => {
    const device = deviceFixture();
    const answer = await runLeasedCommand({ ...COMMAND, cancel_requested: true }, device.deps);
    expect(answer.action).toBe("cancelled_before_start");
    expect(device.effects).toEqual([]);
    // Asked to start, which the runner refuses and records as the cancellation
    // it is. A device that could post a terminal result for a command it never
    // started could post one for any command it was ever handed.
    expect(device.transport.calls).toEqual([
      "POST /v1/remote/device/commands/dcmd-camera-1/start",
    ]);
    expect(device.transport.state.resultBodies).toEqual([]);
  });

  it("cancels the work when the control channel says cancellation was asked for", async () => {
    const device = deviceFixture();
    let observed: boolean | null = null;
    const deps = {
      ...device.deps,
      request: async (method: string, path: string, body?: any, options: any = {}) => {
        if (path.includes("/control")) return { cancel_requested: true, state: "running" };
        return device.transport.request(method, path, body, options);
      },
      perform: async (_command: any, signal: AbortSignal) => {
        // The effect notices the cancellation the watcher passed on.
        await new Promise((resolve) => setTimeout(resolve, 5));
        observed = signal.aborted;
        return { outcome: "cancelled", result: { cancellation: "cancelled_during_effect" } };
      },
    };
    await runLeasedCommand(COMMAND, deps);
    expect(observed).toBe(true);
    expect(device.store.records.get("dcmd-camera-1")?.outcome).toBe("cancelled");
  });
});

describe("cancelling a capability that is already under way", () => {
  it("reports a cancelled recording as cut short, with the audio it did capture", async () => {
    const controller = new AbortController();
    const stopped: string[] = [];
    const chunks = [new Blob(["opus-one"])];
    const recorder: any = {
      mimeType: "audio/webm",
      start: () => {},
      stop: () => {
        recorder.onstop?.();
      },
    };
    const outcome = await recordAudio(60_000, controller.signal, {
      openStream: async () => {
        // Cancelled a moment into the recording, as an operator would.
        setTimeout(() => controller.abort(), 5);
        return { id: "stream" };
      },
      createRecorder: () => {
        setTimeout(() => recorder.ondataavailable?.({ data: chunks[0] }), 1);
        return recorder;
      },
      stopStream: (stream: any) => stopped.push(stream.id),
      createBlob: (parts: Blob[], mediaType: string) => new Blob(parts, { type: mediaType }),
      maxMs: 300_000,
      sliceMs: 2,
    });

    expect(outcome.cancelledDuringEffect).toBe(true);
    expect(outcome.cancelledBeforeEffect).toBeUndefined();
    expect(outcome.blob.size).toBeGreaterThan(0);
    expect(outcome.result.cancelled).toBe(true);
    // Always, on every path: a microphone left open is the failure that matters.
    expect(stopped).toEqual(["stream"]);
  });

  it("reports speech stopped mid-sentence as cancelled during the effect, never as failed", async () => {
    const controller = new AbortController();
    const synthesis = {
      cancel: () => {
        // What a browser really does: the utterance ends with an error event.
        utterance.onerror?.({ error: "canceled" });
      },
      speak: () => {
        utterance.onstart?.();
        setTimeout(() => controller.abort(), 1);
      },
    };
    const utterance: any = {};
    const outcome = await speakText("the runner asked me to say this", controller.signal, {
      synthesis,
      createUtterance: () => utterance,
    });
    expect(outcome.cancelledDuringEffect).toBe(true);
    expect(outcome.cancelledBeforeEffect).toBeUndefined();
  });

  it("reports speech cancelled before it began as cancelled before the effect", async () => {
    const controller = new AbortController();
    controller.abort();
    let spoke = false;
    const outcome = await speakText("never said", controller.signal, {
      synthesis: { cancel: () => {}, speak: () => (spoke = true) },
      createUtterance: () => ({}),
    });
    expect(outcome.cancelledBeforeEffect).toBe(true);
    expect(spoke).toBe(false);
  });

  it("still fails when synthesis genuinely fails", async () => {
    const utterance: any = {};
    await expect(
      speakText("boom", undefined, {
        synthesis: { cancel: () => {}, speak: () => utterance.onerror?.({ error: "synthesis-failed" }) },
        createUtterance: () => utterance,
      }),
    ).rejects.toThrow(/synthesis-failed/u);
  });
});
