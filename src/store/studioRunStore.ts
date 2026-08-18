import { create } from "zustand";

import { errorMessage } from "../lib/errors";
import { studioClient, type GenerationRequest } from "../lib/studioClient";

/**
 * Studio's in-flight generations, held outside the panel that starts them.
 *
 * Two things need this to live here rather than in `StudioPanel`:
 *
 *  - Switching to Chat used to unmount the panel, which dropped the busy
 *    flag, the job id and the progress subscription while the engine kept
 *    sampling — a run that was still going read as a cancelled one.
 *  - The chat composer's running-tasks chip counts work the app is doing on
 *    its own behalf, and a generation is exactly that.
 *
 * The queue is this store's, not the engine's. `sd-server` has a queue of its
 * own (it reports `queue_position`), but every run re-runs `ensure_ready`,
 * and a second run naming a different model would relaunch the engine out
 * from under the first. Submitting one at a time is what makes a queue safe
 * without touching the backend.
 */

export interface StudioQueueItem {
  id: string;
  /** What the user sees in the queue list — the run's prompt, trimmed. */
  label: string;
  request: GenerationRequest;
}

/** The active run's progress, as the engine last reported it. Phase codes are
 *  kept raw (`running`, `loading`…) so the panel translates them. */
export interface StudioRunProgress {
  jobId: string | null;
  phase: string;
  queuePosition: number;
  percent: number | null;
  step: number | null;
  totalSteps: number | null;
}

/** Enough to keep a leaned-on Generate button from queueing a hundred runs;
 *  high enough that a batch of variations still fits. */
export const MAX_STUDIO_QUEUE = 8;

interface StudioRunStore {
  /** Waiting runs, oldest first. The active one is not in here. */
  queue: StudioQueueItem[];
  active: StudioQueueItem | null;
  progress: StudioRunProgress | null;
  error: string | null;
  /** Bumped once per finished run. The panel reloads its gallery off this
   *  rather than the store mirroring entries it would then have to keep in
   *  sync with deletes. */
  completions: number;
  enqueue: (label: string, request: GenerationRequest) => string | null;
  /** Drops a queued run, or stops the active one. */
  cancel: (id: string) => Promise<void>;
  clearError: () => void;
}

/** Runs the user stopped: their rejection is expected, not an error to show. */
const stopped = new Set<string>();

let unlisten: Promise<() => void> | null = null;

/** One subscription for the app, attached on the first run and left in place —
 *  the payload is ignored unless a run is active. */
function listenForProgress(): void {
  if (unlisten) return;
  unlisten = studioClient.onProgress((payload) => {
    useStudioRunStore.setState((state) =>
      state.active
        ? {
            progress: {
              jobId: payload.jobId || null,
              phase: payload.phase,
              queuePosition: payload.queuePosition,
              percent: payload.percent,
              step: payload.step,
              totalSteps: payload.totalSteps,
            },
          }
        : {},
    );
  });
}

/** Submits queued runs one at a time until the queue empties. Only ever one
 *  of these is in flight: it is started when a run is enqueued into an idle
 *  store, and it loops rather than returning between runs. */
async function drain(): Promise<void> {
  for (;;) {
    const next = useStudioRunStore.getState().queue[0];
    if (!next) {
      useStudioRunStore.setState({ active: null, progress: null });
      return;
    }
    useStudioRunStore.setState((state) => ({
      queue: state.queue.slice(1),
      active: next,
      progress: { jobId: null, phase: "submitted", queuePosition: 0, percent: null, step: null, totalSteps: null },
    }));
    try {
      await studioClient.run(next.request);
      useStudioRunStore.setState((state) => ({ completions: state.completions + 1 }));
    } catch (reason) {
      if (!stopped.has(next.id)) {
        // Whatever refused this run — a missing weight, a bad size — refuses
        // the ones behind it too, so the queue is dropped rather than
        // replaying the same failure N times.
        useStudioRunStore.setState({ error: errorMessage(reason), queue: [] });
      }
    } finally {
      stopped.delete(next.id);
    }
  }
}

export const useStudioRunStore = create<StudioRunStore>((set, get) => ({
  queue: [],
  active: null,
  progress: null,
  error: null,
  completions: 0,

  enqueue: (label, request) => {
    const state = get();
    if (state.queue.length + (state.active ? 1 : 0) >= MAX_STUDIO_QUEUE) return null;
    const id = `studio-run-${crypto.randomUUID()}`;
    listenForProgress();
    set({ queue: [...state.queue, { id, label, request }], error: null });
    if (!state.active) void drain();
    return id;
  },

  cancel: async (id) => {
    const state = get();
    if (state.active?.id !== id) {
      set({ queue: state.queue.filter((item) => item.id !== id) });
      return;
    }
    stopped.add(id);
    set({ progress: state.progress ? { ...state.progress, phase: "stopping" } : null });
    try {
      // The engine drops a queued job but cannot interrupt one already
      // sampling (`cancel_generating: false` in its capabilities), so stopping
      // a running generation means stopping the engine running it. That also
      // releases its weights, which is what the user wanted from a stop.
      const jobId = get().progress?.jobId;
      if (!jobId || !(await studioClient.cancel(jobId))) {
        await studioClient.unloadEngine();
      }
    } catch (reason) {
      set({ error: errorMessage(reason) });
    }
  },

  clearError: () => set({ error: null }),
}));

export const selectStudioRunCount = (state: StudioRunStore): number =>
  state.queue.length + (state.active ? 1 : 0);
