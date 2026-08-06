import { create } from "zustand";
import { isTauri } from "@tauri-apps/api/core";
import { check as checkForUpdate, type DownloadEvent, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";

import { installsWhileRunning, shouldCheckNow, type UpdateCheckReason } from "../lib/appUpdater";

/**
 * `idle`        nothing pending — either no update exists, or the last check
 *               failed (failures are silent, see `lastError`).
 * `checking`    a `latest.json` fetch is in flight.
 * `downloading` an update was found and is downloading (plus installing, on
 *               the platforms that can install under a running app).
 * `ready`       the update is staged; only the card click is left. This is
 *               the only state the sidebar card is visible in.
 * `applying`    the card was clicked: relaunching (macOS/Linux) or handing
 *               off to the Windows installer.
 */
export type UpdateStatus = "idle" | "checking" | "downloading" | "ready" | "applying";

export interface UpdateStore {
  status: UpdateStatus;
  /** Version of the pending update (no leading "v"), or null when none. */
  version: string | null;
  /** Release notes from `latest.json`, shown as the card's tooltip. */
  notes: string | null;
  downloadedBytes: number;
  contentLength: number | null;
  lastCheckedAt: number | null;
  /** Last failure, kept for diagnostics only — never surfaced as a dialog.
   * With no `pubkey` configured, or no signed `latest.json` published, every
   * check lands here, which is the intended "updates are off" behaviour. */
  lastError: string | null;

  check: (reason: UpdateCheckReason) => Promise<void>;
  /** Card click: relaunch into the already-installed build (macOS/Linux), or
   * run the downloaded installer (Windows, which closes the app itself). */
  applyUpdate: () => Promise<void>;
}

/** The live plugin handle for the pending update. Deliberately outside the
 * zustand state: it is a non-serializable object with an IPC-backed resource
 * id, and on Windows it must survive from `download()` until the user clicks
 * the card, because that click is what runs `install()`. */
let pendingUpdate: Update | null = null;

function message(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  return String(error);
}

export const useUpdateStore = create<UpdateStore>((set, get) => ({
  status: "idle",
  version: null,
  notes: null,
  downloadedBytes: 0,
  contentLength: null,
  lastCheckedAt: null,
  lastError: null,

  check: async (reason) => {
    if (!isTauri()) return;
    const { status, lastCheckedAt } = get();
    // `ready` counts as busy: the bundle is already staged, so a second check
    // would re-download the same release on every poll.
    const busy = status !== "idle";
    if (!shouldCheckNow(reason, { now: Date.now(), lastCheckedAt, busy })) return;

    set({ status: "checking", lastError: null });
    try {
      const update = await checkForUpdate();
      set({ lastCheckedAt: Date.now() });
      if (!update) {
        pendingUpdate = null;
        set({ status: "idle", version: null, notes: null });
        return;
      }
      pendingUpdate = update;
      set({
        status: "downloading",
        version: update.version,
        notes: update.body ?? null,
        downloadedBytes: 0,
        contentLength: null,
      });
      const onProgress = (event: DownloadEvent) => {
        if (event.event === "Started") {
          set({ downloadedBytes: 0, contentLength: event.data.contentLength ?? null });
        } else if (event.event === "Progress") {
          set((state) => ({ downloadedBytes: state.downloadedBytes + event.data.chunkLength }));
        }
      };
      if (installsWhileRunning()) {
        // macOS/Linux: the new bundle replaces the old one on disk while the
        // current process keeps running off its already-loaded image, so the
        // install can happen now and the relaunch can wait for the user.
        await update.downloadAndInstall(onProgress);
      } else {
        // Windows: the NSIS/MSI installer has to close the app to replace
        // locked files, so only the download happens in the background. The
        // install waits for the card click (see `applyUpdate`).
        await update.download(onProgress);
      }
      set({ status: "ready" });
    } catch (error) {
      // Silent by design — an unreachable endpoint, an unsigned build, or a
      // missing pubkey must never interrupt the user. The next scheduled
      // check retries on its own.
      pendingUpdate = null;
      set({ status: "idle", lastCheckedAt: Date.now(), lastError: message(error) });
    }
  },

  applyUpdate: async () => {
    if (get().status !== "ready") return;
    set({ status: "applying" });
    try {
      if (installsWhileRunning()) {
        await relaunch();
      } else if (pendingUpdate) {
        // Hands off to the Windows installer, which terminates and restarts
        // the app itself — nothing after this line is guaranteed to run.
        await pendingUpdate.install();
      }
    } catch (error) {
      // The staged update is still valid — drop back to the card so the user
      // can retry (or quit and reopen manually).
      set({ status: "ready", lastError: message(error) });
    }
  },
}));

/** Test seam: the pending handle lives outside the store, so resetting state
 * in a test has to reset it too. */
export function setPendingUpdateForTests(update: Update | null): void {
  pendingUpdate = update;
}

export default useUpdateStore;
