import { useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  AlertTriangle,
  CheckCircle2,
  ExternalLink,
  GitBranch,
  GitPullRequest,
  Loader2,
  ListChecks,
  Play,
  RefreshCw,
  Square,
  Trash2,
  X,
  XCircle,
} from "lucide-react";

import {
  executeDeliveryMutation,
  prepareDeliveryMutation,
  type ConfirmationPreview,
  type DeliveryMutation,
  type OwnedWorktreeRecord,
} from "../../lib/gitDelivery";
import type { MigrationRiskLevel } from "../../lib/migrationAgent";
import { useT } from "../../lib/i18n";
import {
  isTerminalMigrationRunStatus,
  useMigrationAgentStore,
  type MigrationRun,
  type MigrationRunStatus,
} from "../../store/migrationAgentStore";
import { primaryRoot, useWorkspaceStore, type WorkspaceRootInfo } from "../../store/workspaceStore";
import { Button, IconButton, StatusPill, type PillTone } from "../ui";

interface MigrationAgentPanelProps {
  onClose: () => void;
  onOpenRunCapsule?: (runId: string) => void;
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function statusTone(status: MigrationRunStatus): PillTone {
  if (status === "completed") return "success";
  if (status === "failed") return "danger";
  if (status === "cancelled") return "neutral";
  return "warning";
}

function statusLabelSuffix(status: MigrationRunStatus): string {
  switch (status) {
    case "drafting": return "Drafting";
    case "planned": return "Planned";
    case "implementing": return "Implementing";
    case "awaiting_push": return "AwaitingPush";
    case "completed": return "Completed";
    case "failed": return "Failed";
    case "cancelled": return "Cancelled";
  }
}

function riskTone(level: MigrationRiskLevel): PillTone {
  if (level === "high") return "danger";
  if (level === "medium") return "warning";
  return "success";
}

/**
 * Owned-worktree creation for a migration run, driven through the EXACT same
 * `m5_delivery` confirm-and-type-the-phrase flow `GitDeliveryPanel.tsx` uses
 * for every other worktree — nothing here bypasses that. Once the mutation
 * executes, the new worktree's canonical path is attached as a secondary
 * workspace root (the same `add_secondary_workspace_root` command
 * `workspaceStore.ts`'s `addSecondary` wraps) so the headless slice run can
 * only ever write inside it.
 */
function useWorktreeCreateFlow(
  run: MigrationRun | null,
  onCreated: (worktreeId: string, branch: string, workspaceLabel: string) => void,
) {
  const roots = useWorkspaceStore((state) => state.roots);
  const refreshRoots = useWorkspaceStore((state) => state.refreshRoots);
  const workspace = primaryRoot(roots);
  const [baseRef, setBaseRef] = useState("main");
  const [branchPrefix, setBranchPrefix] = useState("codex/migration/");
  const [preview, setPreview] = useState<ConfirmationPreview | null>(null);
  const [pendingMutation, setPendingMutation] = useState<DeliveryMutation | null>(null);
  const [confirmation, setConfirmation] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const reset = () => {
    setPreview(null);
    setPendingMutation(null);
    setConfirmation("");
    setError(null);
  };

  const prepareCreate = async () => {
    if (!run || !workspace) return;
    setError(null);
    try {
      const mutation: DeliveryMutation = {
        kind: "create_worktree",
        payload: {
          repositoryRoot: workspace.path,
          repositorySlug: run.repositorySlug,
          baseRef: baseRef.trim(),
          label: `migration-${run.runId.slice(0, 8)}`,
          allowedRemotes: ["origin"],
          branchPrefix: branchPrefix.trim(),
          protectedBranches: ["main", "master", "develop", "release"],
          allowPush: true,
          allowCreatePullRequest: true,
          allowReviewComment: false,
          allowForkWrites: false,
        },
      };
      const result = await prepareDeliveryMutation(mutation);
      setPreview(result);
      setPendingMutation(mutation);
    } catch (err) {
      setError(errorText(err));
    }
  };

  const confirmCreate = async () => {
    if (!preview || !pendingMutation) return;
    setBusy(true);
    setError(null);
    try {
      const created = (await executeDeliveryMutation(
        pendingMutation,
        preview.digest,
        confirmation,
      )) as OwnedWorktreeRecord;
      const attached = await invoke<WorkspaceRootInfo>("add_secondary_workspace_root", {
        path: created.marker.canonicalPath,
      });
      await refreshRoots();
      reset();
      onCreated(created.marker.worktreeId, created.marker.branch, attached.label);
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  };

  return {
    workspace,
    baseRef,
    setBaseRef,
    branchPrefix,
    setBranchPrefix,
    preview,
    confirmation,
    setConfirmation,
    busy,
    error,
    prepareCreate,
    confirmCreate,
    reset,
  };
}

/** Two real GitHub writes — pushing the owned branch, then opening the draft
 * PR — driven through the EXACT same confirm-and-type-the-phrase flow, same
 * shape as `IssueToPrPanel.tsx`'s own `usePrConfirmFlow`. */
function usePrConfirmFlow(run: MigrationRun | null, onOpened: (prNumber: number, prUrl: string) => void) {
  const [preview, setPreview] = useState<ConfirmationPreview | null>(null);
  const [pendingMutation, setPendingMutation] = useState<DeliveryMutation | null>(null);
  const [confirmation, setConfirmation] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const reset = () => {
    setPreview(null);
    setPendingMutation(null);
    setConfirmation("");
    setError(null);
  };

  const preparePush = async () => {
    if (!run?.worktreeId) return;
    setError(null);
    try {
      const mutation: DeliveryMutation = {
        kind: "push",
        payload: { worktreeId: run.worktreeId, remote: "origin" },
      };
      const result = await prepareDeliveryMutation(mutation);
      setPreview(result);
      setPendingMutation(mutation);
    } catch (err) {
      setError(errorText(err));
    }
  };

  const confirmPending = async () => {
    if (!run?.worktreeId || !preview || !pendingMutation) return;
    setBusy(true);
    setError(null);
    try {
      if (pendingMutation.kind === "push") {
        await executeDeliveryMutation(pendingMutation, preview.digest, confirmation);
        setConfirmation("");
        const title = `Migration: ${run.goal}`.slice(0, 512);
        const slice = run.plan?.slices[0] ?? null;
        const body = [
          `Slice 1${slice ? ` — ${slice.title}` : ""} of the migration plan for: ${run.goal}`,
          "",
          "## Checks",
          run.sliceOutcome?.summary ?? "No slice outcome was recorded.",
          "",
          "## Follow-up checklist",
          (run.plan?.slices.slice(1) ?? []).map((s) => `- [ ] Slice ${s.order}: ${s.title} (risk: ${s.riskLevel})`).join("\n") ||
            "- No further slices in this plan.",
          "",
          "## Non-goals",
          "This draft was opened by Little Monkey's Migration and Upgrade Agent. Merge, force-push, and branch deletion are handled by a human reviewer, never by this flow.",
        ].join("\n");
        const nextMutation: DeliveryMutation = {
          kind: "create_draft_pr",
          payload: { worktreeId: run.worktreeId, base: "main", title, body },
        };
        const next = await prepareDeliveryMutation(nextMutation);
        setPreview(next);
        setPendingMutation(nextMutation);
      } else {
        const result = (await executeDeliveryMutation(pendingMutation, preview.digest, confirmation)) as {
          number?: number;
          url?: string;
        };
        reset();
        if (result.number && result.url) onOpened(result.number, result.url);
      }
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  };

  return { preview, confirmation, setConfirmation, busy, error, preparePush, confirmPending, reset };
}

export function MigrationAgentPanel({ onClose, onOpenRunCapsule }: MigrationAgentPanelProps) {
  const { t } = useT();
  const store = useMigrationAgentStore();
  const [goal, setGoal] = useState("");
  const [repositorySlug, setRepositorySlug] = useState("");

  useEffect(() => {
    store.init();
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  const selected = useMemo(
    () => store.runs.find((run) => run.runId === store.selectedRunId) ?? null,
    [store.runs, store.selectedRunId],
  );

  const worktreeFlow = useWorktreeCreateFlow(selected, (worktreeId, branch, workspaceLabel) => {
    if (selected) store.attachWorktree(selected.runId, worktreeId, branch, workspaceLabel);
  });
  const pr = usePrConfirmFlow(selected, (prNumber, prUrl) => {
    if (selected) store.markPrOpened(selected.runId, prNumber, prUrl);
  });

  useEffect(() => {
    worktreeFlow.reset();
    pr.reset();
  }, [selected?.runId]); // eslint-disable-line react-hooks/exhaustive-deps

  const creating = store.busy.createRun;
  const activity = selected ? store.activityByRun[selected.runId] : undefined;
  const firstSlice = selected?.plan?.slices[0] ?? null;
  const followUpSlices = selected?.plan?.slices.slice(1) ?? [];

  return (
    <section className="flex h-full min-h-0 flex-col" aria-labelledby="migration-agent-title">
      <header className="flex shrink-0 items-start justify-between gap-3 border-b border-border px-5 py-4">
        <div>
          <h2 id="migration-agent-title" className="text-sm font-semibold text-foreground">
            {t("MigrationAgent.title")}
          </h2>
          <p className="mt-1 max-w-2xl text-xs leading-5 text-muted">{t("MigrationAgent.subtitle")}</p>
        </div>
        <IconButton size="sm" aria-label={t("MigrationAgent.close")} title={t("MigrationAgent.close")} onClick={onClose}>
          <X size={15} />
        </IconButton>
      </header>

      <form
        className="flex shrink-0 flex-wrap items-end gap-2 border-b border-border px-5 py-3"
        onSubmit={(event) => {
          event.preventDefault();
          if (!goal.trim() || creating) return;
          void store.createRun(goal.trim(), repositorySlug.trim()).then(() => setGoal(""));
        }}
      >
        <label className="min-w-64 flex-1 text-xs text-muted">
          {t("MigrationAgent.goalLabel")}
          <input
            className="mt-1 w-full rounded-md border border-border bg-background px-2.5 py-2 text-sm text-foreground outline-none focus:border-accent focus:ring-1 focus:ring-accent"
            placeholder={t("MigrationAgent.goalPlaceholder")}
            value={goal}
            onChange={(event) => setGoal(event.target.value)}
          />
        </label>
        <label className="min-w-48 text-xs text-muted">
          {t("MigrationAgent.repositoryLabel")}
          <input
            className="mt-1 w-full rounded-md border border-border bg-background px-2.5 py-2 font-mono text-sm text-foreground outline-none focus:border-accent focus:ring-1 focus:ring-accent"
            placeholder={t("MigrationAgent.repositoryPlaceholder")}
            value={repositorySlug}
            onChange={(event) => setRepositorySlug(event.target.value)}
          />
        </label>
        <Button type="submit" variant="primary" disabled={creating || !goal.trim()}>
          {creating ? <Loader2 className="animate-spin" size={14} /> : <Play size={14} />} {t("MigrationAgent.generateButton")}
        </Button>
      </form>

      {store.error && (
        <div role="alert" className="mx-5 mt-3 rounded-md border border-danger/40 bg-danger/10 p-3 text-xs text-danger">
          {store.error}
        </div>
      )}

      <div className="grid min-h-0 flex-1 gap-4 overflow-hidden p-5 xl:grid-cols-[minmax(16rem,.9fr)_minmax(0,1.4fr)]">
        <div className="min-h-0 overflow-y-auto rounded-lg border border-border bg-surface p-3">
          <div className="flex items-center justify-between gap-2">
            <h3 className="text-xs font-semibold text-foreground">{t("MigrationAgent.runsHeading")}</h3>
          </div>
          <div className="mt-2 space-y-1.5">
            {store.runs.length === 0 && (
              <p className="rounded-md border border-dashed border-border p-5 text-center text-xs text-faint">
                {t("MigrationAgent.emptyRuns")}
              </p>
            )}
            {store.runs.map((run) => (
              <button
                key={run.runId}
                type="button"
                onClick={() => store.selectRun(run.runId)}
                className={`w-full rounded-md border p-2.5 text-left transition-colors ${
                  run.runId === store.selectedRunId ? "border-accent bg-accent/10" : "border-border bg-background hover:border-border-strong"
                }`}
              >
                <p className="truncate text-xs font-medium text-foreground">{run.goal}</p>
                <p className="mt-0.5 truncate text-[11px] text-muted">{run.repositorySlug || "—"}</p>
                <div className="mt-1.5">
                  <StatusPill tone={statusTone(run.status)}>{t(`MigrationAgent.status${statusLabelSuffix(run.status)}`)}</StatusPill>
                </div>
              </button>
            ))}
          </div>
        </div>

        <div className="min-h-0 overflow-y-auto rounded-lg border border-border bg-surface p-4">
          {!selected ? (
            <p className="p-8 text-center text-xs text-faint">{t("MigrationAgent.emptyRuns")}</p>
          ) : (
            <div className="space-y-4">
              <div className="flex flex-wrap items-start justify-between gap-2">
                <div>
                  <h3 className="text-sm font-semibold text-foreground">{selected.goal}</h3>
                  {selected.repositorySlug && <p className="mt-1 font-mono text-[11px] text-faint">{selected.repositorySlug}</p>}
                </div>
                <StatusPill tone={statusTone(selected.status)}>{t(`MigrationAgent.status${statusLabelSuffix(selected.status)}`)}</StatusPill>
              </div>

              {selected.plan && (
                <div className="rounded-md border border-border bg-background p-3">
                  <h4 className="text-xs font-semibold text-foreground">{t("MigrationAgent.planSummaryHeading")}</h4>
                  <p className="mt-1 text-[11px] leading-5 text-muted">{selected.plan.summary}</p>
                  {selected.plan.usedFallback && (
                    <p className="mt-2 flex items-center gap-1.5 text-[11px] text-warning">
                      <AlertTriangle size={12} /> {t("MigrationAgent.fallbackNotice")}
                    </p>
                  )}
                  <Button
                    size="sm"
                    variant="ghost"
                    className="mt-2"
                    disabled={store.busy[`plan:${selected.runId}`]}
                    onClick={() => void store.regeneratePlan(selected.runId)}
                  >
                    <RefreshCw size={13} /> {t("MigrationAgent.regeneratePlanButton")}
                  </Button>
                </div>
              )}

              {selected.branch ? (
                <dl className="grid grid-cols-[7rem_minmax(0,1fr)] gap-x-3 gap-y-1.5 rounded-md border border-border bg-background p-3 text-[11px]">
                  <dt className="text-faint">{t("MigrationAgent.branchLabel")}</dt>
                  <dd className="break-all font-mono text-foreground">{selected.branch}</dd>
                  <dt className="text-faint">{t("MigrationAgent.worktreeLabel")}</dt>
                  <dd className="break-all font-mono text-foreground">{selected.workspaceLabel}</dd>
                </dl>
              ) : (
                selected.plan && (
                  <div className="rounded-md border border-border bg-background p-3">
                    <h4 className="flex items-center gap-1.5 text-xs font-semibold text-foreground">
                      <GitBranch size={13} /> {t("MigrationAgent.worktreeHeading")}
                    </h4>
                    <p className="mt-1 text-[11px] leading-5 text-muted">{t("MigrationAgent.worktreeHint")}</p>
                    {!worktreeFlow.preview ? (
                      <div className="mt-2 flex flex-wrap items-end gap-2">
                        <label className="text-[11px] text-muted">
                          {t("MigrationAgent.baseRefLabel")}
                          <input
                            className="mt-1 w-28 rounded-md border border-border bg-background px-2 py-1.5 font-mono text-xs text-foreground outline-none focus:border-accent"
                            value={worktreeFlow.baseRef}
                            onChange={(event) => worktreeFlow.setBaseRef(event.target.value)}
                          />
                        </label>
                        <label className="text-[11px] text-muted">
                          {t("MigrationAgent.branchPrefixLabel")}
                          <input
                            className="mt-1 w-40 rounded-md border border-border bg-background px-2 py-1.5 font-mono text-xs text-foreground outline-none focus:border-accent"
                            value={worktreeFlow.branchPrefix}
                            onChange={(event) => worktreeFlow.setBranchPrefix(event.target.value)}
                          />
                        </label>
                        <Button
                          size="sm"
                          variant="primary"
                          disabled={!worktreeFlow.workspace || !selected.repositorySlug}
                          onClick={() => void worktreeFlow.prepareCreate()}
                        >
                          <GitBranch size={13} /> {t("MigrationAgent.createWorktreeButton")}
                        </Button>
                      </div>
                    ) : (
                      <div className="mt-2 rounded-md border border-accent/40 bg-accent/5 p-3 text-[11px]">
                        <p className="font-medium text-foreground">{worktreeFlow.preview.summary}</p>
                        <p className="mt-1 text-muted">{worktreeFlow.preview.impact}</p>
                        <label className="mt-2 block text-muted">
                          {t("MigrationAgent.confirmTypePhrase", { phrase: worktreeFlow.preview.confirmationPhrase })}
                          <input
                            autoFocus
                            autoComplete="off"
                            spellCheck={false}
                            className="mt-1 w-full rounded-md border border-border bg-background px-2.5 py-1.5 font-mono text-xs text-foreground outline-none focus:border-accent"
                            value={worktreeFlow.confirmation}
                            onChange={(event) => worktreeFlow.setConfirmation(event.target.value)}
                          />
                        </label>
                        {worktreeFlow.error && <p className="mt-2 text-danger">{worktreeFlow.error}</p>}
                        <div className="mt-2 flex justify-end gap-2">
                          <Button size="sm" disabled={worktreeFlow.busy} onClick={() => worktreeFlow.reset()}>
                            {t("MigrationAgent.confirmCancel")}
                          </Button>
                          <Button
                            size="sm"
                            variant="primary"
                            disabled={worktreeFlow.busy || worktreeFlow.confirmation !== worktreeFlow.preview.confirmationPhrase}
                            onClick={() => void worktreeFlow.confirmCreate()}
                          >
                            {worktreeFlow.busy && <Loader2 className="animate-spin" size={13} />} {t("MigrationAgent.confirmExecute")}
                          </Button>
                        </div>
                      </div>
                    )}
                    {worktreeFlow.error && !worktreeFlow.preview && <p className="mt-2 text-xs text-danger">{worktreeFlow.error}</p>}
                  </div>
                )
              )}

              {activity && !isTerminalMigrationRunStatus(selected.status) && (
                <p className="flex items-center gap-2 text-xs text-muted">
                  <Loader2 className="animate-spin shrink-0" size={13} />
                  {t("MigrationAgent.currentActivity", { activity })}
                </p>
              )}

              {firstSlice && (
                <div>
                  <h4 className="text-xs font-semibold text-foreground">
                    {t("MigrationAgent.sliceOrder", { order: firstSlice.order })}: {firstSlice.title}
                  </h4>
                  <div className="mt-2 rounded-md border border-border bg-background p-3 text-[11px]">
                    <div className="flex items-center gap-2">
                      <StatusPill tone={riskTone(firstSlice.riskLevel)}>{t(`MigrationAgent.risk${firstSlice.riskLevel[0].toUpperCase()}${firstSlice.riskLevel.slice(1)}`)}</StatusPill>
                    </div>
                    <p className="mt-2 leading-5 text-muted">{firstSlice.description}</p>
                    {firstSlice.riskNotes.length > 0 && (
                      <div className="mt-2">
                        <p className="font-semibold text-foreground">{t("MigrationAgent.riskNotesHeading")}</p>
                        <ul className="mt-1 list-disc space-y-0.5 pl-4 text-muted">
                          {firstSlice.riskNotes.map((note, index) => (
                            <li key={index}>{note}</li>
                          ))}
                        </ul>
                      </div>
                    )}
                    <div className="mt-2">
                      <p className="font-semibold text-foreground">{t("MigrationAgent.rollbackHeading")}</p>
                      <p className="mt-1 text-muted">{firstSlice.rollbackNotes}</p>
                    </div>
                    {firstSlice.filesLikely.length > 0 && (
                      <div className="mt-2">
                        <p className="font-semibold text-foreground">{t("MigrationAgent.filesLikelyHeading")}</p>
                        <p className="mt-1 break-all font-mono text-faint">{firstSlice.filesLikely.join(", ")}</p>
                      </div>
                    )}
                  </div>

                  {selected.branch && (
                    <div className="mt-2 flex flex-wrap gap-2">
                      {!isTerminalMigrationRunStatus(selected.status) && selected.status !== "implementing" && selected.status !== "awaiting_push" && (
                        <Button
                          size="sm"
                          variant="primary"
                          disabled={store.busy[`attempt:${selected.runId}`]}
                          onClick={() => void store.attemptFirstSlice(selected.runId)}
                        >
                          {store.busy[`attempt:${selected.runId}`] ? <Loader2 className="animate-spin" size={13} /> : <Play size={13} />}{" "}
                          {t("MigrationAgent.attemptSliceButton")}
                        </Button>
                      )}
                      {selected.status === "implementing" && (
                        <Button size="sm" variant="danger" onClick={() => store.cancel(selected.runId)}>
                          <Square size={13} /> {t("MigrationAgent.cancelButton")}
                        </Button>
                      )}
                    </div>
                  )}
                </div>
              )}

              {selected.sliceOutcome && (
                <div className="rounded-md border border-border bg-background p-3 text-[11px]">
                  <p className="flex items-center gap-1.5 font-semibold text-foreground">
                    {selected.sliceOutcome.outcome === "completed" ? (
                      <CheckCircle2 size={13} className="text-success" />
                    ) : selected.sliceOutcome.outcome === "cancelled" ? (
                      <Square size={13} className="text-faint" />
                    ) : (
                      <XCircle size={13} className="text-danger" />
                    )}
                    {t("MigrationAgent.sliceOutcomeHeading")}
                  </p>
                  <p className="mt-1 whitespace-pre-wrap break-words text-muted">{selected.sliceOutcome.summary}</p>
                </div>
              )}

              {selected.error && (
                <div role="alert" className="rounded-md border border-danger/40 bg-danger/10 p-3 text-xs text-danger">
                  <p className="flex items-center gap-1.5 font-medium"><AlertTriangle size={13} /> {t("MigrationAgent.errorHeading")}</p>
                  <p className="mt-1 whitespace-pre-wrap break-words">{selected.error}</p>
                </div>
              )}

              {followUpSlices.length > 0 && (
                <div>
                  <h4 className="flex items-center gap-1.5 text-xs font-semibold text-foreground">
                    <ListChecks size={13} /> {t("MigrationAgent.followUpHeading")}
                  </h4>
                  <p className="mt-1 text-[11px] text-faint">{t("MigrationAgent.followUpHint")}</p>
                  <div className="mt-2 space-y-1.5">
                    {followUpSlices.map((slice) => (
                      <div key={slice.id} className="rounded-md border border-border bg-background p-2.5 text-[11px]">
                        <div className="flex items-center justify-between gap-2">
                          <p className="font-medium text-foreground">
                            {t("MigrationAgent.sliceOrder", { order: slice.order })}: {slice.title}
                          </p>
                          <StatusPill tone={riskTone(slice.riskLevel)}>{t(`MigrationAgent.risk${slice.riskLevel[0].toUpperCase()}${slice.riskLevel.slice(1)}`)}</StatusPill>
                        </div>
                        <p className="mt-1 text-muted">{slice.description}</p>
                      </div>
                    ))}
                  </div>
                </div>
              )}

              <div className="flex flex-wrap gap-2">
                {selected.sliceOutcome?.durableRunId && onOpenRunCapsule && (
                  <Button size="sm" onClick={() => onOpenRunCapsule(selected.sliceOutcome!.durableRunId!)}>
                    {t("MigrationAgent.viewCapsuleButton")}
                  </Button>
                )}
                {selected.status === "completed" && selected.prUrl && (
                  <Button size="sm" onClick={() => window.open(selected.prUrl ?? undefined, "_blank", "noopener,noreferrer")}>
                    <ExternalLink size={13} /> {t("MigrationAgent.openPrLinkButton")}
                  </Button>
                )}
                {isTerminalMigrationRunStatus(selected.status) && (
                  <Button size="sm" variant="ghost" onClick={() => store.deleteRun(selected.runId)}>
                    <Trash2 size={13} /> {t("MigrationAgent.deleteButton")}
                  </Button>
                )}
              </div>

              {selected.status === "awaiting_push" && (
                <div className="rounded-md border border-border bg-background p-3">
                  <h4 className="text-xs font-semibold text-foreground">{t("MigrationAgent.openPrHeading")}</h4>
                  <p className="mt-1 text-[11px] leading-5 text-muted">{t("MigrationAgent.openPrDescription")}</p>
                  {!pr.preview ? (
                    <Button size="sm" variant="primary" className="mt-2" onClick={() => void pr.preparePush()}>
                      <GitPullRequest size={13} /> {t("MigrationAgent.pushAndOpenPrButton")}
                    </Button>
                  ) : (
                    <div className="mt-2 rounded-md border border-warning/40 bg-warning/5 p-3 text-[11px]">
                      <p className="font-medium text-foreground">{pr.preview.summary}</p>
                      <p className="mt-1 text-muted">{pr.preview.impact}</p>
                      <label className="mt-2 block text-muted">
                        {t("MigrationAgent.confirmTypePhrase", { phrase: pr.preview.confirmationPhrase })}
                        <input
                          autoFocus
                          autoComplete="off"
                          spellCheck={false}
                          className="mt-1 w-full rounded-md border border-border bg-background px-2.5 py-1.5 font-mono text-xs text-foreground outline-none focus:border-accent"
                          value={pr.confirmation}
                          onChange={(event) => pr.setConfirmation(event.target.value)}
                        />
                      </label>
                      {pr.error && <p className="mt-2 text-danger">{pr.error}</p>}
                      <div className="mt-2 flex justify-end gap-2">
                        <Button size="sm" disabled={pr.busy} onClick={() => pr.reset()}>
                          {t("MigrationAgent.confirmCancel")}
                        </Button>
                        <Button
                          size="sm"
                          variant="danger"
                          disabled={pr.busy || pr.confirmation !== pr.preview.confirmationPhrase}
                          onClick={() => void pr.confirmPending()}
                        >
                          {pr.busy && <Loader2 className="animate-spin" size={13} />} {t("MigrationAgent.confirmExecute")}
                        </Button>
                      </div>
                    </div>
                  )}
                </div>
              )}

              <p className="rounded-md border border-dashed border-border p-2.5 text-[10px] leading-4 text-faint">
                {t("MigrationAgent.nonGoalsNote")}
              </p>
            </div>
          )}
        </div>
      </div>
    </section>
  );
}

export default MigrationAgentPanel;
