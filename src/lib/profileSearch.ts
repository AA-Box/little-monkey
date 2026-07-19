import { invoke, isTauri } from "@tauri-apps/api/core";

export type SearchSourceKind = "message" | "actor_transcript" | "run_event";

export interface GlobalSearchRequest {
  query: string;
  includeArchived: boolean;
  fromMs: number | null;
  toMs: number | null;
  modelKey: string | null;
  personaId: string | null;
  workspacePath: string | null;
  limit: number;
}

export interface GlobalSearchHit {
  documentId: string;
  sourceKind: SearchSourceKind;
  sourceId: string;
  sessionId: string | null;
  runId: string | null;
  title: string;
  role: string;
  snippet: string;
  occurredAtMs: number;
  modelKey: string | null;
  personaId: string | null;
  workspacePath: string | null;
  archived: boolean;
  score: number;
}

let migrationReady: Promise<void> | null = null;

async function ensureProfileMigration(): Promise<void> {
  migrationReady ??= (async () => {
    const status = await invoke<{ state: "source_missing" | "pending" | "current" | "source_changed" }>(
      "profile_migration_status",
    );
    if (status.state === "pending" || status.state === "source_changed") {
      await invoke("profile_migrate");
    }
  })().catch((error) => {
    migrationReady = null;
    throw error;
  });
  return migrationReady;
}

export async function globalProfileSearch(request: GlobalSearchRequest): Promise<GlobalSearchHit[]> {
  const query = request.query.trim();
  if (!query || !isTauri()) return [];
  await ensureProfileMigration();
  return invoke<GlobalSearchHit[]>("profile_global_search", {
    request: { ...request, query },
  });
}
