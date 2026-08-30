import { invoke } from "@tauri-apps/api/core";
import { create } from "zustand";

import { taskEvent, taskEventToRunEvent, type AutonomousTask, type TaskConstraints, type TaskGuidance } from "../lib/autonomousTask";
import { AutonomousTaskControl, resumeAutonomousTask, startAutonomousTask, submitAutonomousTaskToDaemon, type StartedAutonomousTask } from "../lib/autonomousTaskRunner";
import { appendRunEvent, decideRunPermission, listRuns, loadRunEvents, requestRunCancellation } from "../lib/runProtocol";

const controls = new Map<string, AutonomousTaskControl>();
const started = new Map<string, StartedAutonomousTask>();

function upsert(tasks: AutonomousTask[], task: AutonomousTask): AutonomousTask[] {
  const index = tasks.findIndex((entry) => entry.taskId === task.taskId);
  if (index < 0) return [task, ...tasks];
  const next = tasks.slice();
  next[index] = task;
  return next;
}

function taskFromEvents(events: Awaited<ReturnType<typeof loadRunEvents>>): AutonomousTask | null {
  let task: AutonomousTask | null = null;
  for (const envelope of events) {
    if (envelope.event.type !== "task_event") continue;
    const eventPayload = envelope.event.payload.payload;
    const snapshot = eventPayload.snapshot;
    if (snapshot && typeof snapshot === "object") {
      const candidate = snapshot as Partial<AutonomousTask>;
      if (typeof candidate.taskId !== "string" || typeof candidate.objective !== "string") continue;
      if (!Array.isArray(candidate.acceptanceCriteria) || !Array.isArray(candidate.guidance) || !Array.isArray(candidate.artifacts) || !Array.isArray(candidate.verificationEvidence) || !candidate.executionOwner) continue;
      task = candidate as AutonomousTask;
    }
    if (envelope.event.payload.event_type === "guidance_received" && task) {
      const guidance = eventPayload.guidance;
      if (guidance && typeof guidance === "object") task = { ...task, guidance: [...task.guidance, guidance as TaskGuidance].slice(-32) };
    }
  }
  return task;
}

export interface AutonomousTaskStoreState {
  tasks: AutonomousTask[];
  selectedTaskId: string | null;
  pausedTaskIds: Record<string, boolean>;
  busy: Record<string, boolean>;
  error: string | null;
  init: () => Promise<void>;
  refresh: () => Promise<void>;
  select: (taskId: string | null) => void;
  start: (objective: string, sessionId: string, constraints?: Pick<TaskConstraints, "executionPlacement">) => Promise<AutonomousTask>;
  pause: (taskId: string) => Promise<void>;
  resume: (taskId: string) => Promise<void>;
  cancel: (taskId: string, reason?: string) => Promise<void>;
  guide: (taskId: string, text: string, appliesTo?: TaskGuidance["appliesTo"]) => Promise<void>;
  approve: (taskId: string, confirmation: string) => Promise<void>;
  continueInBackground: (taskId: string) => Promise<void>;
  clearError: () => void;
}

async function withBusy<T>(set: (patch: Partial<AutonomousTaskStoreState>) => void, get: () => AutonomousTaskStoreState, key: string, operation: () => Promise<T>): Promise<T> {
  set({ busy: { ...get().busy, [key]: true }, error: null });
  try { return await operation(); } catch (error) { set({ error: error instanceof Error ? error.message : String(error) }); throw error; } finally { set({ busy: { ...get().busy, [key]: false } }); }
}

async function controlActiveExternalPlacements(task: AutonomousTask, action: "cancel" | "pause" | "resume"): Promise<void> {
  const active = task.workers.filter((worker) => {
    const placement = worker.executionPlacement;
    return !worker.finishedAtMs
      && placement
      && placement.kind !== "local"
      && placement.kind !== "worktree"
      && placement.targetId !== "local";
  });
  for (const worker of active) {
    const targetId = worker.executionPlacement?.targetId;
    if (!targetId) continue;
    await invoke("autonomous_task_control_node", {
      targetId,
      runId: `${task.taskId}-${worker.nodeId}`,
      action,
    });
  }
}

export const useAutonomousTaskStore = create<AutonomousTaskStoreState>((set, get) => {
  const publish = (task: AutonomousTask) => set((state) => ({ tasks: upsert(state.tasks, task), selectedTaskId: state.selectedTaskId ?? task.taskId }));
  return {
    tasks: [], selectedTaskId: null, pausedTaskIds: {}, busy: {}, error: null,
    clearError: () => set({ error: null }),
    select: (taskId) => set({ selectedTaskId: taskId }),
    refresh: () => withBusy(set, get, "refresh", async () => {
      const runs = await listRuns(200, false);
      const taskRuns = runs.filter((run) => run.spec.kind === "autonomous_task");
      const hydrated = (await Promise.all(taskRuns.map(async (run) => taskFromEvents(await loadRunEvents(run.spec.run_id))))).filter((task): task is AutonomousTask => task !== null);
      set((state) => ({ tasks: hydrated, pausedTaskIds: Object.fromEntries(runs.filter((run) => run.status === "paused").map((run) => [run.spec.run_id, true])), selectedTaskId: state.selectedTaskId && hydrated.some((task) => task.taskId === state.selectedTaskId) ? state.selectedTaskId : hydrated[0]?.taskId ?? null }));
      await Promise.all(taskRuns.filter((run) => run.status === "running").map(async (run) => {
        const task = hydrated.find((entry) => entry.taskId === run.spec.run_id);
        if (task?.outcome !== "RUNNING" || controls.has(task.taskId)) return;
        const handle = await resumeAutonomousTask({ task, onUpdate: publish });
        started.set(handle.runId, handle); controls.set(handle.runId, handle.control); publish(handle.task);
        void handle.completion.then((result) => { publish(result); started.delete(handle.runId); controls.delete(handle.runId); });
      }));
    }),
    init: async () => { await get().refresh(); },
    start: (objective, sessionId, constraints) => withBusy(set, get, "start", async () => {
      const handle = await startAutonomousTask({ objective, sessionId, constraints, onUpdate: publish });
      set((state) => { const pausedTaskIds = { ...state.pausedTaskIds }; delete pausedTaskIds[handle.runId]; return { pausedTaskIds }; });
      started.set(handle.runId, handle); controls.set(handle.runId, handle.control); publish(handle.task);
      void handle.completion.then((task) => { publish(task); started.delete(handle.runId); controls.delete(handle.runId); }).catch((error) => { set({ error: error instanceof Error ? error.message : String(error) }); started.delete(handle.runId); controls.delete(handle.runId); });
      return handle.task;
    }),
    pause: (taskId) => withBusy(set, get, `pause:${taskId}`, async () => { const current = get().tasks.find((task) => task.taskId === taskId); if (current) await controlActiveExternalPlacements(current, "pause"); controls.get(taskId)?.pause(); await appendRunEvent(taskId, { type: "paused", payload: { reason: "Paused by the user." } }); set((state) => ({ pausedTaskIds: { ...state.pausedTaskIds, [taskId]: true } })); }),
    resume: (taskId) => withBusy(set, get, `resume:${taskId}`, async () => {
      const currentTask = get().tasks.find((task) => task.taskId === taskId);
      if (currentTask) await controlActiveExternalPlacements(currentTask, "resume");
      const existing = controls.get(taskId);
      if (existing) existing.resume();
      else { const task = get().tasks.find((entry) => entry.taskId === taskId); if (!task) throw new Error("Task is not available for resume."); const handle = await resumeAutonomousTask({ task, onUpdate: publish }); started.set(handle.runId, handle); controls.set(handle.runId, handle.control); publish(handle.task); void handle.completion.then((result) => { publish(result); started.delete(handle.runId); controls.delete(handle.runId); }); }
      await appendRunEvent(taskId, { type: "started", payload: { engine_id: "autonomous-task-resume" } }); set((state) => { const pausedTaskIds = { ...state.pausedTaskIds }; delete pausedTaskIds[taskId]; return { pausedTaskIds }; });
    }),
    cancel: (taskId, reason = "Cancelled by the user.") => withBusy(set, get, `cancel:${taskId}`, async () => { const current = get().tasks.find((task) => task.taskId === taskId); if (current) await controlActiveExternalPlacements(current, "cancel"); controls.get(taskId)?.cancel(); await requestRunCancellation(taskId, reason); }),
    guide: (taskId, text, appliesTo = "future_nodes") => withBusy(set, get, `guide:${taskId}`, async () => {
      const current = get().tasks.find((task) => task.taskId === taskId);
      if (!current || !text.trim()) throw new Error("Task guidance requires a non-empty task and message.");
      const guidance: TaskGuidance = { guidanceId: `guidance-${Date.now()}`, text: text.trim().slice(0, 8_000), receivedAtMs: Date.now(), appliesTo };
      const next = { ...current, guidance: [...current.guidance, guidance].slice(-32), updatedAtMs: Date.now() };
      publish(next);
      controls.get(taskId)?.guide(guidance);
      await appendRunEvent(taskId, taskEventToRunEvent(taskEvent("guidance_received", next, { guidance })));
    }),
    approve: (taskId, confirmation) => withBusy(set, get, `approve:${taskId}`, async () => {
      const current = get().tasks.find((task) => task.taskId === taskId);
      if (!current?.waitingApproval || !confirmation.trim()) throw new Error("Task approval requires the exact confirmation phrase.");
      if (current.waitingApproval.confirmationPhrase && confirmation.trim() !== current.waitingApproval.confirmationPhrase) throw new Error("The confirmation phrase does not match the frozen delivery preview.");
      if (current.executionOwner.kind === "daemon") {
        await decideRunPermission(taskId, current.waitingApproval.requestId, current.waitingApproval.operationDigest, "allow_once");
        return;
      }
      const existing = controls.get(taskId);
      if (existing) existing.approve(current.waitingApproval.requestId, confirmation.trim(), current.waitingApproval.operationDigest);
      else {
        const handle = await resumeAutonomousTask({ task: current, approval: { requestId: current.waitingApproval.requestId, confirmation: confirmation.trim(), operationDigest: current.waitingApproval.operationDigest }, onUpdate: publish });
        started.set(handle.runId, handle); controls.set(handle.runId, handle.control); publish(handle.task);
        void handle.completion.then((result) => { publish(result); started.delete(handle.runId); controls.delete(handle.runId); });
      }
    }),
    continueInBackground: (taskId) => withBusy(set, get, `handoff:${taskId}`, async () => {
      const current = get().tasks.find((task) => task.taskId === taskId);
      if (!current || current.outcome !== "RUNNING") throw new Error("Only a running autonomous task can continue in the background.");
      const control = controls.get(taskId);
      const frozenTask = control
        ? await control.freezeForHandoff(() => get().tasks.find((task) => task.taskId === taskId))
        : get().tasks.find((task) => task.taskId === taskId);
      if (!frozenTask || frozenTask.outcome !== "RUNNING") throw new Error("Autonomous task changed before handoff could be frozen.");
      let accepted;
      try { accepted = await submitAutonomousTaskToDaemon(frozenTask); }
      catch (error) { control?.resume(); throw error; }
      if (accepted.rollback_owner) {
        const rollbackOwner = { kind: "desktop" as const, instanceId: accepted.rollback_owner.instance_id, leaseEpoch: accepted.rollback_owner.lease_epoch, leaseExpiresAtMs: accepted.rollback_owner.lease_expires_at_ms };
        const rollbackTask = { ...frozenTask, executionOwner: rollbackOwner, updatedAtMs: Date.now() };
        await appendRunEvent(taskId, taskEventToRunEvent(taskEvent("execution_handoff_rollback", rollbackTask, { run_id: accepted.run_id, owner: rollbackOwner, error: accepted.error ?? "Daemon activation failed." })));
        control?.adoptExecutionOwner(rollbackOwner);
        control?.resume();
        publish(rollbackTask);
        throw new Error(accepted.error ?? "Could not activate parked autonomous daemon job; desktop ownership was restored.");
      }
      const next = { ...frozenTask, executionOwner: { kind: "daemon" as const, instanceId: `daemon-${taskId}`, leaseEpoch: frozenTask.executionOwner.leaseEpoch + 1, leaseExpiresAtMs: Date.now() + frozenTask.budgetSnapshot.wallTimeMs }, updatedAtMs: Date.now() };
      try {
        await appendRunEvent(taskId, taskEventToRunEvent(taskEvent("execution_handoff", next, { job_id: accepted.job_id, run_id: accepted.run_id, owner: next.executionOwner })));
      } catch (error) {
        control?.relinquish();
        throw error;
      }
      control?.relinquish();
      publish(next);
    }),
  };
});

export function __resetAutonomousTaskControllersForTests(): void { controls.clear(); started.clear(); }
