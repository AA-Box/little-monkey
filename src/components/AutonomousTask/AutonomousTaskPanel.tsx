import { useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { CheckCircle2, Loader2, Pause, Play, Send, Square, X, XCircle } from "lucide-react";

import { useAutonomousTaskStore } from "../../store/autonomousTaskStore";
import { primaryRoot } from "../../store/workspaceStore";
import { Button, IconButton, StatusPill, type PillTone } from "../ui";

interface AutonomousTaskPanelProps {
  sessionId: string;
  onClose: () => void;
}

function tone(outcome: string): PillTone {
  if (outcome === "SUCCEEDED") return "success";
  if (["FAILED", "VERIFICATION_FAILED", "DELIVERY_FAILED", "CANCELLED"].includes(outcome)) return "danger";
  return "warning";
}

function label(value: string): string {
  return value.replace(/_/g, " ").toLowerCase().replace(/^./, (character) => character.toUpperCase());
}

export function AutonomousTaskPanel({ sessionId, onClose }: AutonomousTaskPanelProps) {
  const store = useAutonomousTaskStore();
  const [objective, setObjective] = useState("");
  const [guidance, setGuidance] = useState("");
  const [confirmation, setConfirmation] = useState("");
  const [targetId, setTargetId] = useState(() => localStorage.getItem("little-monkey.execution-target.workspace") || localStorage.getItem("little-monkey.execution-target.global") || "");
  const [targets, setTargets] = useState<Array<{ id: string; kind: string; name: string }>>([]);
  const [resultPreview, setResultPreview] = useState<string | null>(null);

  useEffect(() => { void store.init(); }, []); // eslint-disable-line react-hooks/exhaustive-deps
  useEffect(() => {
    void invoke<Record<string, { kind: string; identity: { displayName: string } }>>("execution_targets_list")
      .then((value) => setTargets(Object.entries(value).map(([id, target]) => ({ id, kind: target.kind, name: target.identity.displayName }))))
      .catch(() => setTargets([]));
  }, []);
  const selected = useMemo(() => store.tasks.find((task) => task.taskId === store.selectedTaskId) ?? null, [store.tasks, store.selectedTaskId]);
  const paused = selected ? Boolean(store.pausedTaskIds[selected.taskId]) : false;
  const running = selected?.outcome === "RUNNING" && !paused;
  const controllable = selected?.outcome === "RUNNING" || paused;
  const workspacePath = selected ? primaryRoot(selected.workspaceRoots)?.path : undefined;
  const resultAction = async (action: "review" | "apply" | "export" | "discard", resultId: string) => {
    try {
      if (action === "review") {
        const value = await invoke<unknown>("execution_result_review", { resultId });
        setResultPreview(JSON.stringify(value, null, 2));
      } else if (action === "apply" && workspacePath) {
        await invoke("execution_result_apply", { resultId, workspace: workspacePath });
        await store.refresh();
      } else if (action === "export" && workspacePath) {
        await invoke("execution_result_export", { resultId, output: `${workspacePath}/.little-monkey-${resultId}.json` });
      } else if (action === "discard") {
        await invoke("execution_result_discard", { resultId });
        await store.refresh();
      }
    } catch (value) {
      setResultPreview(value instanceof Error ? value.message : String(value));
    }
  };

  return (
    <section className="flex h-full min-h-0 flex-col" aria-labelledby="autonomous-task-title">
      <header className="flex shrink-0 items-start justify-between gap-3 border-b border-border px-5 py-4">
        <div><h2 id="autonomous-task-title" className="text-sm font-semibold text-foreground">Autonomous task</h2><p className="mt-1 max-w-2xl text-xs leading-5 text-muted">One objective, bounded workers, durable evidence, and optional Git delivery.</p></div>
        <IconButton size="sm" aria-label="Close autonomous tasks" title="Close" onClick={onClose}><X size={15} /></IconButton>
      </header>
      <form className="flex shrink-0 flex-wrap gap-2 border-b border-border px-5 py-3" onSubmit={(event) => { event.preventDefault(); if (objective.trim()) void store.start(objective.trim(), sessionId, targetId ? { executionPlacement: { kind: "target", targetId, nodeId: "pending", reason: "selected by the operator" } } : undefined).then(() => setObjective("")); }}>
        <input className="min-w-0 flex-1 rounded-md border border-border bg-background px-2.5 py-2 text-sm text-foreground outline-none focus:border-accent" placeholder="What should Little Monkey complete?" value={objective} onChange={(event) => setObjective(event.target.value)} />
        <label className="flex items-center gap-2 text-xs text-muted"><span>Run on</span><select className="rounded-md border border-border bg-background px-2 py-2 text-foreground" value={targetId} onChange={(event) => setTargetId(event.target.value)}><option value="">Automatic</option>{targets.map((target) => <option key={target.id} value={target.id}>{target.name} · {target.kind}</option>)}</select></label>
        <Button type="submit" variant="primary" disabled={!objective.trim() || store.busy.start}><Play size={14} /> Start</Button>
      </form>
      {store.error && <p role="alert" className="mx-5 mt-3 rounded-md border border-danger/40 bg-danger/10 p-3 text-xs text-danger">{store.error}</p>}
      <div className="grid min-h-0 flex-1 gap-4 overflow-hidden p-5 xl:grid-cols-[minmax(15rem,.8fr)_minmax(0,1.5fr)]">
        <div className="min-h-0 overflow-y-auto rounded-lg border border-border bg-surface p-3">
          <div className="flex items-center justify-between"><h3 className="text-xs font-semibold text-foreground">Tasks</h3><Button variant="ghost" onClick={() => void store.refresh()}>Refresh</Button></div>
          <div className="mt-2 space-y-1.5">
            {store.tasks.length === 0 && <p className="rounded-md border border-dashed border-border p-5 text-center text-xs text-faint">No autonomous tasks yet.</p>}
            {store.tasks.map((task) => <button key={task.taskId} type="button" onClick={() => store.select(task.taskId)} className={`w-full rounded-md border p-2.5 text-left ${task.taskId === store.selectedTaskId ? "border-accent bg-accent/10" : "border-border bg-background"}`}><p className="truncate text-xs font-medium text-foreground">{task.objective}</p><div className="mt-1.5"><StatusPill tone={tone(task.outcome)}>{label(task.outcome)}</StatusPill></div></button>)}
          </div>
        </div>
        <div className="min-h-0 overflow-y-auto rounded-lg border border-border bg-surface p-4">
          {!selected ? <p className="p-8 text-center text-xs text-faint">Select a task to inspect its plan and evidence.</p> : <div className="space-y-4">
            <div className="flex items-start justify-between gap-3"><div><h3 className="text-sm font-semibold text-foreground">{selected.objective}</h3><p className="mt-1 text-[11px] text-faint">{selected.taskId} · {selected.plan?.strategy ?? "planning"}</p></div><StatusPill tone={tone(selected.outcome)}>{label(selected.outcome)}</StatusPill></div>
            <div className="grid gap-2 text-[11px] text-faint sm:grid-cols-2"><span>Placement: {selected.constraints.executionPlacement?.kind ?? "local"}</span><span>Revision: {selected.workspaceRevision ?? "unknown"}</span><span>Workers: {selected.usage?.workersStarted ?? selected.workers.length} / {selected.budgetSnapshot.maxWorkers}</span><span>Artifacts: {selected.artifacts.length} · Repairs: {selected.repairRounds} / {selected.budgetSnapshot.maxRepairRounds}</span></div>
            {selected.waitingReason && <p className="rounded-md border border-warning/40 bg-warning/10 p-2.5 text-xs text-warning">{selected.waitingReason}</p>}
            {selected.waitingApproval && <form className="rounded-md border border-warning/40 bg-warning/10 p-3" onSubmit={(event) => { event.preventDefault(); void store.approve(selected.taskId, confirmation).then(() => setConfirmation("")); }}><p className="text-xs text-warning">Type the exact approval phrase: <code className="select-all font-mono">{selected.waitingApproval.confirmationPhrase ?? "shown in the waiting message"}</code></p><div className="mt-2 flex gap-2"><input className="min-w-0 flex-1 rounded-md border border-border bg-background px-2 py-1.5 text-xs font-mono text-foreground" value={confirmation} onChange={(event) => setConfirmation(event.target.value)} /><Button variant="primary" type="submit" disabled={!confirmation.trim()}>Approve</Button></div></form>}
            {controllable && <div className="flex flex-wrap gap-2">{running ? <><Button variant="ghost" onClick={() => void store.pause(selected.taskId)}><Pause size={13} /> Pause</Button><Button variant="ghost" onClick={() => void store.cancel(selected.taskId)}><Square size={13} /> Cancel</Button><Button variant="ghost" onClick={() => void store.continueInBackground(selected.taskId)}><Play size={13} /> Continue in background</Button></> : <Button variant="ghost" onClick={() => void store.resume(selected.taskId)}><Play size={13} /> Resume</Button>}</div>}
            {controllable && <form className="flex gap-2" onSubmit={(event) => { event.preventDefault(); if (guidance.trim()) void store.guide(selected.taskId, guidance.trim()).then(() => setGuidance("")); }}><input className="min-w-0 flex-1 rounded-md border border-border bg-background px-2 py-1.5 text-xs text-foreground" placeholder="Guide the next worker…" value={guidance} onChange={(event) => setGuidance(event.target.value)} /><Button variant="ghost" type="submit" disabled={!guidance.trim()}><Send size={13} /> Guide</Button></form>}
            <section><h4 className="text-xs font-semibold text-foreground">Plan</h4><div className="mt-2 space-y-1.5">{selected.plan?.nodes.map((node) => <div key={node.nodeId} className="flex items-center gap-2 rounded-md border border-border px-2.5 py-2 text-xs"><span className="w-24 shrink-0 text-faint">{label(node.taskClass)}</span><span className="min-w-0 flex-1 text-foreground">{node.objective}</span>{node.status === "succeeded" ? <CheckCircle2 className="text-success" size={14} /> : node.status === "failed" ? <XCircle className="text-danger" size={14} /> : node.status === "running" ? <Loader2 className="animate-spin text-accent" size={14} /> : <span className="text-faint">{label(node.status)}</span>}</div>)}</div></section>
            <section><h4 className="text-xs font-semibold text-foreground">Acceptance criteria</h4><div className="mt-2 space-y-1.5">{selected.acceptanceCriteria.map((criterion) => <div key={criterion.id} className="rounded-md border border-border px-2.5 py-2 text-xs"><div className="flex justify-between gap-2"><span className="text-foreground">{criterion.description}</span><span className={criterion.status === "passed" ? "text-success" : "text-muted"}>{label(criterion.status)}</span></div>{criterion.evidenceIds.length > 0 && <p className="mt-1 text-[11px] text-faint">Evidence: {criterion.evidenceIds.join(", ")}</p>}</div>)}</div></section>
            {selected.workers.length > 0 && <section><h4 className="text-xs font-semibold text-foreground">Workers and worktrees</h4><div className="mt-2 space-y-1.5">{selected.workers.map((worker) => <div key={worker.workerId} className="rounded-md border border-border px-2.5 py-2 text-xs"><div className="flex justify-between gap-2"><span>{worker.nodeId} · {worker.executionPlacement?.kind ?? worker.isolation}</span><span className="text-faint">{worker.finishedAtMs ? "finished" : "active"}</span></div>{worker.worktree && <p className="mt-1 break-all text-[11px] text-faint">{worker.worktree.path} · {worker.worktree.diffDigest.slice(0, 12)}</p>}{worker.changedFiles && worker.changedFiles.length > 0 && <p className="mt-1 text-[11px] text-muted">Changed: {worker.changedFiles.join(", ")}</p>}{worker.resultId && <div className="mt-2 flex flex-wrap items-center gap-1.5"><code className="text-[10px] text-faint">{worker.resultId}</code><Button size="sm" variant="ghost" onClick={() => void resultAction("review", worker.resultId!)}>Review</Button><Button size="sm" variant="ghost" disabled={!workspacePath} onClick={() => void resultAction("apply", worker.resultId!)}>Apply</Button><Button size="sm" variant="ghost" disabled={!workspacePath} onClick={() => void resultAction("export", worker.resultId!)}>Export</Button><Button size="sm" variant="ghost" onClick={() => void resultAction("discard", worker.resultId!)}>Discard</Button></div>}</div>)}</div></section>}
            {resultPreview && <pre className="max-h-64 overflow-auto rounded-md border border-border bg-background p-3 text-[10px] text-muted">{resultPreview}</pre>}
            {selected.verificationEvidence.length > 0 && <section><h4 className="text-xs font-semibold text-foreground">Verification evidence</h4><div className="mt-2 space-y-1.5">{selected.verificationEvidence.map((evidence) => <div key={evidence.evidenceId} className="rounded-md border border-border px-2.5 py-2 text-xs"><div className="flex justify-between"><span>{evidence.name}</span><span className={evidence.passed && !evidence.stale ? "text-success" : "text-danger"}>{evidence.passed && !evidence.stale ? "Passed" : evidence.stale ? "Stale" : "Failed"}</span></div><p className="mt-1 text-[11px] text-faint">exit {evidence.exitCode ?? "n/a"} · {evidence.durationMs} ms</p></div>)}</div></section>}
          </div>}
        </div>
      </div>
    </section>
  );
}

export default AutonomousTaskPanel;
