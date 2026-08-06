/**
 * In-app update scheduling (Claude Desktop parity).
 *
 * Claude Desktop's logic, which this mirrors: the app never interrupts you to
 * ask about an update. It checks quietly in the background, downloads and
 * installs the new bundle on its own, and only *then* surfaces a small
 * persistent card ("Relaunch to update", plus the version) in the bottom of
 * the sidebar. Nothing is modal, nothing blocks a running turn, and a failed
 * check is silent — the next scheduled check just tries again.
 *
 * The timing rules live here (rather than in the store) so they're plain,
 * testable functions: `shouldCheckNow` is the single decision point every
 * trigger goes through.
 */
import { isTauri } from "@tauri-apps/api/core";

/** Delay before the launch check, so the updater never competes with model
 * listing, MCP connection, and workspace hydration during boot. */
export const STARTUP_CHECK_DELAY_MS = 8_000;
/** Background poll for long-lived windows — the app is routinely left open
 * for days, so a launch-only check would never fire for those users. */
export const POLL_INTERVAL_MS = 6 * 60 * 60 * 1000;
/** Coming back to the window is a cheap "is this session stale?" signal, but
 * it fires constantly during normal alt-tabbing, so it's throttled far
 * tighter than the poll interval. */
export const FOCUS_MIN_INTERVAL_MS = 60 * 60 * 1000;

export type UpdateCheckReason = "startup" | "interval" | "focus" | "manual";

export interface CheckScheduleInput {
  now: number;
  /** When the last check *completed* (success or failure), or null if none
   * has completed in this window yet. */
  lastCheckedAt: number | null;
  /** True while a check/download/install is in flight, or while an installed
   * update is already waiting for the relaunch — in every one of those cases
   * another check would be wasted work at best and a double download at
   * worst. */
  busy: boolean;
}

/** The one place that decides whether a trigger turns into a real check. */
export function shouldCheckNow(reason: UpdateCheckReason, input: CheckScheduleInput): boolean {
  if (input.busy) return false;
  if (reason === "manual" || reason === "startup") return true;
  if (input.lastCheckedAt === null) return true;
  const elapsed = input.now - input.lastCheckedAt;
  const minimum = reason === "focus" ? FOCUS_MIN_INTERVAL_MS : POLL_INTERVAL_MS;
  return elapsed >= minimum;
}

/**
 * Whether this platform can install an update underneath a running app.
 *
 * macOS and Linux can: the new `.app` bundle / AppImage replaces the old file
 * on disk while the current process keeps running off its already-loaded
 * image, so the install happens in the background and only the relaunch waits
 * for the user — the Claude Desktop behaviour.
 *
 * Windows can't: the NSIS/MSI installer has to close the app to replace
 * locked files. There the download runs in the background and `install()` is
 * deferred until the user clicks the card, so an update never kills a turn
 * mid-flight.
 *
 * Detected from the user agent rather than `@tauri-apps/plugin-os` to avoid
 * adding a plugin (and its IPC round trip) for one boolean.
 */
export function installsWhileRunning(): boolean {
  const agent = typeof navigator === "undefined" ? "" : navigator.userAgent;
  return !/Windows/i.test(agent);
}

/** Progress fraction (0–1) for a download, or null when the server didn't
 * send a content length — the card stays indeterminate rather than showing a
 * made-up percentage. */
export function downloadProgress(downloadedBytes: number, contentLength: number | null): number | null {
  if (contentLength === null || contentLength <= 0) return null;
  return Math.min(1, downloadedBytes / contentLength);
}

/**
 * Wires the three background triggers (launch, poll, refocus) to the store's
 * `check`. Main-window only — see the call site in App.tsx; a secondary
 * session window sharing the same install would otherwise download the same
 * bundle a second time. Returns the cleanup, same shape as the other
 * `start*` helpers in this directory.
 */
export function startUpdateWatcher(check: (reason: UpdateCheckReason) => Promise<void>): () => void {
  if (!isTauri()) return () => {};
  const run = (reason: UpdateCheckReason) => {
    void check(reason);
  };
  const startupTimer = window.setTimeout(() => run("startup"), STARTUP_CHECK_DELAY_MS);
  const pollTimer = window.setInterval(() => run("interval"), POLL_INTERVAL_MS);
  const onFocus = () => run("focus");
  window.addEventListener("focus", onFocus);
  return () => {
    window.clearTimeout(startupTimer);
    window.clearInterval(pollTimer);
    window.removeEventListener("focus", onFocus);
  };
}
