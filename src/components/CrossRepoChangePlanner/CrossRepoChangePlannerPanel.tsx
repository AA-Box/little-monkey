import { useEffect, useState } from "react";
import {
  ArrowDown,
  ArrowUp,
  CheckCircle2,
  ExternalLink,
  GitBranch,
  Loader2,
  Sparkles,
  X,
} from "lucide-react";

import { useT } from "../../lib/i18n";
import {
  useCrossRepoChangePlannerStore,
  type StepGitConfig,
} from "../../store/crossRepoChangePlannerStore";
import type { CrossRepoPlanStep } from "../../lib/crossRepoChangePlanner";
import { useWorkspaceStore } from "../../store/workspaceStore";
import { Button, IconButton, StatusPill } from "../ui";

interface CrossRepoChangePlannerPanelProps {
  onClose: () => void;
}

const FIELD_CLASSES =
  "mt-1 w-full resize-y rounded-md border border-border bg-background px-2.5 py-1.5 text-xs text-foreground outline-none focus:border-accent focus:ring-1 focus:ring-accent disabled:opacity-60";
const INPUT_CLASSES =
  "mt-1 w-full rounded-md border border-border bg-background px-2.5 py-1.5 text-xs text-foreground outline-none focus:border-accent focus:ring-1 focus:ring-accent";

function StepCard({
  step,
  index,
  total,
  locked,
  gitConfig,
  branch,
  isPreparing,
}: {
  step: CrossRepoPlanStep;
  index: number;
  total: number;
  locked: boolean;
  gitConfig: StepGitConfig;
  branch: { worktreeId: string; branch: string } | undefined;
  isPreparing: boolean;
}) {
  const { t } = useT();
  const store = useCrossRepoChangePlannerStore();
  const [confirmation, setConfirmation] = useState("");

  useEffect(() => {
    if (!isPreparing) setConfirmation("");
  }, [isPreparing]);

  const preview = isPreparing ? store.preview : null;
  const busyPrepare = store.busy.prepare;
  const busyConfirm = store.busy.confirm;

  return (
    <div className="rounded-lg border border-border bg-surface p-3">
      <div className="flex flex-wrap items-start justify-between gap-2">
        <div className="flex items-center gap-2">
          <span className="flex h-5 w-5 shrink-0 items-center justify-center rounded-full bg-accent/15 text-[11px] font-semibold text-accent">
            {step.order}
          </span>
          <div>
            <p className="text-xs font-semibold text-foreground">{step.rootLabel}</p>
            <p className="break-all font-mono text-[10px] text-faint">{step.rootPath}</p>
          </div>
        </div>
        {!locked && (
          <div className="flex gap-1">
            <IconButton
              size="sm"
              aria-label={t("CrossRepoChangePlanner.moveUp")}
              title={t("CrossRepoChangePlanner.moveUp")}
              disabled={index === 0}
              onClick={() => store.moveStep(step.stepId, "up")}
            >
              <ArrowUp size={13} />
            </IconButton>
            <IconButton
              size="sm"
              aria-label={t("CrossRepoChangePlanner.moveDown")}
              title={t("CrossRepoChangePlanner.moveDown")}
              disabled={index === total - 1}
              onClick={() => store.moveStep(step.stepId, "down")}
            >
              <ArrowDown size={13} />
            </IconButton>
          </div>
        )}
      </div>

      {step.dependsOnRootIds.length > 0 && (
        <p className="mt-2 text-[10px] text-faint">
          {t("CrossRepoChangePlanner.dependsOn", { roots: step.dependsOnRootIds.join(", ") })}
        </p>
      )}

      <div className="mt-2.5 grid gap-2.5 sm:grid-cols-2">
        <label className="block text-[11px] text-muted">
          {t("CrossRepoChangePlanner.summaryLabel")}
          <textarea
            className={FIELD_CLASSES}
            rows={2}
            disabled={locked}
            value={step.summary}
            onChange={(event) => store.updateStepField(step.stepId, "summary", event.target.value)}
          />
        </label>
        <label className="block text-[11px] text-muted">
          {t("CrossRepoChangePlanner.changesLabel")}
          <textarea
            className={FIELD_CLASSES}
            rows={2}
            disabled={locked}
            value={step.changes}
            onChange={(event) => store.updateStepField(step.stepId, "changes", event.target.value)}
          />
        </label>
        <label className="block text-[11px] text-muted">
          {t("CrossRepoChangePlanner.risksLabel")}
          <textarea
            className={FIELD_CLASSES}
            rows={2}
            disabled={locked}
            value={step.risks}
            onChange={(event) => store.updateStepField(step.stepId, "risks", event.target.value)}
          />
        </label>
        <label className="block text-[11px] text-muted">
          {t("CrossRepoChangePlanner.rollbackLabel")}
          <textarea
            className={FIELD_CLASSES}
            rows={2}
            disabled={locked}
            value={step.rollback}
            onChange={(event) => store.updateStepField(step.stepId, "rollback", event.target.value)}
          />
        </label>
      </div>

      <div className="mt-3 rounded-md border border-dashed border-border p-2.5">
        <p className="text-[10px] font-semibold uppercase tracking-wide text-faint">
          {t("CrossRepoChangePlanner.branchSectionHeading")}
        </p>

        {branch ? (
          <p className="mt-1.5 flex items-center gap-1.5 text-[11px] text-success">
            <CheckCircle2 size={13} /> {t("CrossRepoChangePlanner.branchCreated", { branch: branch.branch })}
          </p>
        ) : (
          <>
            <div className="mt-2 grid gap-2 sm:grid-cols-2">
              <label className="block text-[11px] text-muted">
                {t("CrossRepoChangePlanner.repositorySlugLabel")}
                <input
                  className={INPUT_CLASSES}
                  placeholder="owner/repository"
                  value={gitConfig.repositorySlug}
                  onChange={(event) => store.updateGitConfig(step.stepId, { repositorySlug: event.target.value })}
                />
              </label>
              <label className="block text-[11px] text-muted">
                {t("CrossRepoChangePlanner.baseRefLabel")}
                <input
                  className={INPUT_CLASSES}
                  value={gitConfig.baseRef}
                  onChange={(event) => store.updateGitConfig(step.stepId, { baseRef: event.target.value })}
                />
              </label>
              <label className="block text-[11px] text-muted">
                {t("CrossRepoChangePlanner.branchPrefixLabel")}
                <input
                  className={INPUT_CLASSES}
                  value={gitConfig.branchPrefix}
                  onChange={(event) => store.updateGitConfig(step.stepId, { branchPrefix: event.target.value })}
                />
              </label>
              <label className="block text-[11px] text-muted">
                {t("CrossRepoChangePlanner.labelLabel")}
                <input
                  className={INPUT_CLASSES}
                  value={gitConfig.label}
                  onChange={(event) => store.updateGitConfig(step.stepId, { label: event.target.value })}
                />
              </label>
            </div>

            {!preview ? (
              <Button
                size="sm"
                variant="primary"
                className="mt-2.5"
                disabled={!locked || busyPrepare}
                onClick={() => void store.prepareBranchForStep(step.stepId)}
                title={!locked ? t("CrossRepoChangePlanner.approveFirstHint") : undefined}
              >
                {busyPrepare && isPreparing ? <Loader2 className="animate-spin" size={13} /> : <GitBranch size={13} />}
                {t("CrossRepoChangePlanner.createBranchButton")}
              </Button>
            ) : (
              <div className="mt-2.5 rounded-md border border-warning/40 bg-warning/5 p-2.5 text-[11px]">
                <p className="font-medium text-foreground">{preview.summary}</p>
                <p className="mt-1 text-muted">{preview.impact}</p>
                <label className="mt-2 block text-muted">
                  {t("CrossRepoChangePlanner.confirmTypePhrase", { phrase: preview.confirmationPhrase })}
                  <input
                    autoFocus
                    autoComplete="off"
                    spellCheck={false}
                    className={INPUT_CLASSES}
                    value={confirmation}
                    onChange={(event) => setConfirmation(event.target.value)}
                  />
                </label>
                {store.error && <p className="mt-2 text-danger">{store.error}</p>}
                <div className="mt-2 flex justify-end gap-2">
                  <Button size="sm" disabled={busyConfirm} onClick={() => store.cancelPrepare()}>
                    {t("CrossRepoChangePlanner.confirmCancel")}
                  </Button>
                  <Button
                    size="sm"
                    variant="danger"
                    disabled={busyConfirm || confirmation !== preview.confirmationPhrase}
                    onClick={() => void store.confirmBranch(confirmation)}
                  >
                    {busyConfirm && <Loader2 className="animate-spin" size={13} />}
                    {t("CrossRepoChangePlanner.confirmExecute")}
                  </Button>
                </div>
              </div>
            )}
          </>
        )}
      </div>
    </div>
  );
}

export function CrossRepoChangePlannerPanel({ onClose }: CrossRepoChangePlannerPanelProps) {
  const { t } = useT();
  const store = useCrossRepoChangePlannerStore();
  const roots = useWorkspaceStore((state) => state.roots);

  const generating = store.busy.generate;
  const locked = store.status === "approved";

  return (
    <section className="flex h-full min-h-0 flex-col" aria-labelledby="cross-repo-planner-title">
      <header className="flex shrink-0 items-start justify-between gap-3 border-b border-border px-5 py-4">
        <div>
          <h2 id="cross-repo-planner-title" className="text-sm font-semibold text-foreground">
            {t("CrossRepoChangePlanner.title")}
          </h2>
          <p className="mt-1 max-w-2xl text-xs leading-5 text-muted">{t("CrossRepoChangePlanner.subtitle")}</p>
        </div>
        <IconButton size="sm" aria-label={t("CrossRepoChangePlanner.close")} title={t("CrossRepoChangePlanner.close")} onClick={onClose}>
          <X size={15} />
        </IconButton>
      </header>

      <div className="min-h-0 flex-1 overflow-y-auto p-5">
        <div className="mx-auto max-w-3xl space-y-4">
          {roots.length === 0 && (
            <div role="alert" className="rounded-md border border-warning/40 bg-warning/5 p-3 text-xs text-warning">
              {t("CrossRepoChangePlanner.noRootsWarning")}
            </div>
          )}

          {!store.plan ? (
            <div className="rounded-lg border border-border bg-surface p-4">
              <label className="block text-xs font-medium text-foreground">
                {t("CrossRepoChangePlanner.descriptionLabel")}
                <textarea
                  className={FIELD_CLASSES}
                  rows={5}
                  placeholder={t("CrossRepoChangePlanner.descriptionPlaceholder")}
                  value={store.description}
                  onChange={(event) => store.setDescription(event.target.value)}
                />
              </label>
              {store.error && (
                <p role="alert" className="mt-2 text-xs text-danger">
                  {store.error}
                </p>
              )}
              <Button
                variant="primary"
                className="mt-3"
                disabled={generating || !store.description.trim() || roots.length === 0}
                onClick={() => void store.generate()}
              >
                {generating ? <Loader2 className="animate-spin" size={14} /> : <Sparkles size={14} />}
                {t("CrossRepoChangePlanner.generateButton")}
              </Button>
            </div>
          ) : (
            <>
              <div className="rounded-lg border border-border bg-surface p-4">
                <div className="flex flex-wrap items-center justify-between gap-2">
                  <p className="max-w-lg text-xs text-muted">{store.plan.description}</p>
                  <StatusPill tone={locked ? "success" : "warning"}>
                    {locked ? t("CrossRepoChangePlanner.statusApproved") : t("CrossRepoChangePlanner.statusDraft")}
                  </StatusPill>
                </div>
                {store.plan.notes && (
                  <p className="mt-2 rounded-md border border-dashed border-border bg-background p-2.5 text-[11px] leading-5 text-muted">
                    {store.plan.notes}
                  </p>
                )}
                {store.notice && <p className="mt-2 text-[11px] text-success">{store.notice}</p>}
                {store.error && (
                  <p role="alert" className="mt-2 text-[11px] text-danger">
                    {store.error}
                  </p>
                )}
                <div className="mt-3 flex flex-wrap gap-2">
                  {!locked && (
                    <Button variant="primary" size="sm" onClick={() => store.approvePlan()}>
                      <CheckCircle2 size={13} /> {t("CrossRepoChangePlanner.approveButton")}
                    </Button>
                  )}
                  <Button size="sm" onClick={() => store.startOver()}>
                    {t("CrossRepoChangePlanner.startOverButton")}
                  </Button>
                </div>
                {!locked && (
                  <p className="mt-2 text-[10px] leading-4 text-faint">{t("CrossRepoChangePlanner.approveGateNote")}</p>
                )}
              </div>

              <div className="space-y-3">
                {store.plan.steps.map((step, index) => (
                  <StepCard
                    key={step.stepId}
                    step={step}
                    index={index}
                    total={store.plan!.steps.length}
                    locked={locked}
                    gitConfig={store.gitConfigByStep[step.stepId]}
                    branch={store.createdBranchByStep[step.stepId]}
                    isPreparing={store.preparingStepId === step.stepId}
                  />
                ))}
              </div>

              <p className="flex items-start gap-2 rounded-md border border-dashed border-border p-2.5 text-[10px] leading-4 text-faint">
                <ExternalLink size={12} className="mt-0.5 shrink-0" />
                {t("CrossRepoChangePlanner.pushFollowUpNote")}
              </p>
            </>
          )}
        </div>
      </div>
    </section>
  );
}

export default CrossRepoChangePlannerPanel;
