import { invoke } from "@tauri-apps/api/core";

export type RecipeSchedulerAuthority = "in_app" | "daemon";

export interface RecipeScheduleSyncItem {
  entryId: string;
  recipeName: string;
  recipePath: string | null;
  cron: string;
  enabled: boolean;
  permissionModeOverride: string | null;
}

export interface RecipeScheduleSyncIssue {
  entryId: string;
  message: string;
}

export interface RecipeScheduleSyncResult {
  authority: RecipeSchedulerAuthority;
  installed: boolean;
  serviceRunning: boolean;
  synchronizedAtMs: number;
  activeTriggerIds: string[];
  disabledTriggerIds: string[];
  issues: RecipeScheduleSyncIssue[];
  lastDeliveryAtMs: Record<string, number>;
}

export interface RecipeSchedulerDaemonStatus {
  installed: boolean;
  serviceRunning: boolean;
}

/**
 * Reconciles the complete legacy recipe-schedule set. The Rust boundary uses
 * deterministic daemon-owned trigger IDs and one SQLite transaction; callers
 * must always send disabled and deleted state through the complete set rather
 * than installing one-off triggers themselves.
 */
export const synchronizeRecipeSchedules = (schedules: RecipeScheduleSyncItem[]) =>
  invoke<RecipeScheduleSyncResult>("daemon_desktop_sync_recipe_schedules", {
    request: { schedules },
  });

export const recipeSchedulerDaemonStatus = () =>
  invoke<RecipeSchedulerDaemonStatus>("daemon_desktop_status");
