import { invoke } from "@tauri-apps/api/core";

import { runAgentTurn } from "./agentLoop";
import { buildModelTargetInventory, type ModelTargetSnapshot } from "./modelTargets";
import type { ModelTargetSnapshotWire, RunRecord, WorkspaceContextWire } from "./runProtocol";
import { VALID_PERMISSION_MODES, type PermissionMode } from "../store/permissionStore";
import { useModelStore } from "../store/modelStore";
import { useSessionStore } from "../store/sessionStore";
import { useWorkspaceStore, type WorkspaceRootInfo } from "../store/workspaceStore";

export interface RunCapsuleReplayHandle {
  sessionId: string;
  runId: string;
  permissionMode: PermissionMode;
  done: Promise<void>;
}

function sameTarget(wire: ModelTargetSnapshotWire, target: ModelTargetSnapshot): boolean {
  if (wire.kind === "managed_llama") {
    return target.kind === "local"
      && target.modelId === wire.model_id
      && target.modelPath === wire.model_path;
  }
  if (wire.kind === "ollama") {
    return target.kind === "ollama"
      && target.model === wire.model
      && target.baseUrl.replace(/\/$/, "") === wire.base_url.replace(/\/$/, "");
  }
  return target.kind === "provider"
    && target.providerId === wire.provider_id
    && target.model === wire.model
    && target.endpoint.replace(/\/$/, "") === wire.endpoint.replace(/\/$/, "")
    && target.credentialRefId === wire.credential_ref_id;
}

/** Finds the exact currently configured model snapshot represented by a run.
 * Replays never fall back or silently route to a different model. */
export function findReplayTarget(
  wire: ModelTargetSnapshotWire,
  targets: readonly ModelTargetSnapshot[],
): ModelTargetSnapshot | null {
  return targets.find((target) => sameTarget(wire, target) && target.availability.status === "available") ?? null;
}

function normalizePath(value: string): string {
  const normalized = value.replace(/\\/g, "/").replace(/\/+$/, "");
  return /^[A-Z]:/i.test(normalized) ? normalized.toLowerCase() : normalized;
}

/** A replay uses the live workspace sandbox, so every frozen root must still
 * be attached with at least the access it had in the original run. */
export function workspaceReplayProblem(
  frozen: WorkspaceContextWire | null,
  current: readonly WorkspaceRootInfo[],
): string | null {
  if (!frozen) return null;
  for (const root of frozen.roots) {
    const match = current.find((candidate) => normalizePath(candidate.path) === normalizePath(root.canonical_path));
    if (!match) return `The frozen workspace root is no longer attached: ${root.canonical_path}`;
    if (root.access === "read_write" && !match.is_primary && frozen.primary_root_id === root.root_id) {
      return `The original writable primary workspace is no longer primary: ${root.canonical_path}`;
    }
  }
  return null;
}

export function replayPermissionMode(run: RunRecord): PermissionMode {
  const candidate = run.spec.permission_policy.mode;
  if (!VALID_PERMISSION_MODES.includes(candidate as PermissionMode)) return "manual";
  // A fresh model response may request tools even when the original run did
  // not. Never reproduce an old blanket bypass from a capsule click.
  return candidate === "bypass" ? "manual" : candidate;
}

function replayPrompt(run: RunRecord): string {
  const task = run.spec.task.trim();
  const instructions = run.spec.instructions?.trim();
  return instructions
    ? `[Frozen run instructions]\n${instructions}\n\n[Frozen task]\n${task}`
    : task;
}

/** Starts a non-daemon capsule as an ordinary, visible chat turn. It reuses
 * the exact configured target, checks workspace compatibility, installs a
 * turn-scoped permission mode, and therefore receives the normal checkpoint,
 * cancellation, tool approval, verification, and run-ledger behavior. */
export async function startRunCapsuleReplay(run: RunRecord): Promise<RunCapsuleReplayHandle> {
  const roots = useWorkspaceStore.getState().roots;
  const workspaceProblem = workspaceReplayProblem(run.spec.workspace, roots);
  if (workspaceProblem) throw new Error(workspaceProblem);

  const inventory = buildModelTargetInventory(useModelStore.getState());
  const target = findReplayTarget(run.spec.target, inventory.targets);
  if (!target) {
    throw new Error(
      `The frozen target '${run.spec.target.label}' is not currently available with the same endpoint and credential reference.`,
    );
  }

  const runId = crypto.randomUUID();
  const permissionMode = replayPermissionMode(run);
  await invoke("set_permission_mode_for_turn", { turnId: runId, mode: permissionMode });

  try {
    const sessions = useSessionStore.getState();
    sessions.newSession();
    const sessionId = useSessionStore.getState().activeSessionId;
    const title = run.spec.task.trim().replace(/\s+/g, " ").slice(0, 80) || "Run capsule";
    useSessionStore.getState().renameSession(sessionId, `Replay: ${title}`);
    useSessionStore.getState().setSessionModelTarget(sessionId, target);

    const done = runAgentTurn(sessionId, replayPrompt(run), [], undefined, runId).finally(() => {
      void invoke("clear_permission_mode_for_turn", { turnId: runId }).catch(() => {});
    });
    return { sessionId, runId, permissionMode, done };
  } catch (error) {
    await invoke("clear_permission_mode_for_turn", { turnId: runId }).catch(() => {});
    throw error;
  }
}
