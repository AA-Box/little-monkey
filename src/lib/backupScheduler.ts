import { isTauri } from "@tauri-apps/api/core";

import {
  getWebDavBackupStatus,
  runWebDavBackupDue,
  stageEncryptedSnapshot,
} from "./portability";
import { useLocaleStore } from "../store/localeStore";
import { usePromptStore } from "../store/promptStore";
import { useSessionStore } from "../store/sessionStore";
import { useShortcutStore } from "../store/shortcutStore";
import { useStackStore } from "../store/stackStore";

const CONFIG_POLL_MS = 60_000;
const SOURCE_STAGE_DEBOUNCE_MS = 2_000;
let activeCheck: Promise<void> | null = null;
let sourceDirty = true;

export function markBackupSourceDirty(): void {
  sourceDirty = true;
}

/** Runs one bounded launch/interval catch-up check. A persisted `nextDueMs`
 * in the past fires once immediately. The desktop only stages frontend-owned
 * profile data and asks the shared Rust scheduler to run; a SQLite lease and
 * durable upload intent deduplicate it against the resident daemon. */
export function runScheduledBackupCheck(now = Date.now()): Promise<void> {
  if (!isTauri()) return Promise.resolve();
  if (activeCheck) return activeCheck;
  activeCheck = (async () => {
    const status = await getWebDavBackupStatus();
    const config = status.config;
    if (!config.enabled) return;
    if (sourceDirty || !status.stagedSnapshot) {
      sourceDirty = false;
      try {
        await stageEncryptedSnapshot();
      } catch (error) {
        sourceDirty = true;
        throw error;
      }
    }
    if (config.nextDueMs !== null && config.nextDueMs > now) return;
    await runWebDavBackupDue(false);
  })().finally(() => {
    activeCheck = null;
  });
  return activeCheck;
}

/** Starts M1's in-app schedule. Reliable quit-time/background execution is
 * intentionally supplied by the M6A daemon; this timer only promises work
 * while a desktop window is alive, plus persisted launch catch-up. */
export function startBackupScheduler(): () => void {
  if (!isTauri()) return () => {};
  let stageTimer: number | null = null;
  const markDirty = () => {
    markBackupSourceDirty();
    if (stageTimer !== null) window.clearTimeout(stageTimer);
    stageTimer = window.setTimeout(() => {
      stageTimer = null;
      void runScheduledBackupCheck().catch((error) => console.error("Failed to stage WebDAV backup source", error));
    }, SOURCE_STAGE_DEBOUNCE_MS);
  };
  const unsubscribers = [
    useSessionStore.subscribe(markDirty),
    usePromptStore.subscribe(markDirty),
    useStackStore.subscribe(markDirty),
    useLocaleStore.subscribe(markDirty),
    useShortcutStore.subscribe(markDirty),
  ];
  void runScheduledBackupCheck().catch((error) => console.error("Scheduled WebDAV backup failed", error));
  const timer = window.setInterval(() => {
    void runScheduledBackupCheck().catch((error) => console.error("Scheduled WebDAV backup failed", error));
  }, CONFIG_POLL_MS);
  return () => {
    window.clearInterval(timer);
    if (stageTimer !== null) window.clearTimeout(stageTimer);
    for (const unsubscribe of unsubscribers) unsubscribe();
  };
}

export function clearBackupSchedulerForTests(): void {
  activeCheck = null;
  sourceDirty = true;
}
