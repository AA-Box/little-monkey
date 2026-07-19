import { useMemo, useState } from "react";
import {
  AlertTriangle,
  CheckCircle2,
  ExternalLink,
  GitBranch,
  Loader2,
  ScanSearch,
  Sparkles,
  Square,
  X,
} from "lucide-react";

import type { SecuritySeverity, SecurityFinding } from "../../lib/securityAutofix";
import { useT } from "../../lib/i18n";
import { useSecurityAutofixStore, type ApplyStatus } from "../../store/securityAutofixStore";
import { Button, IconButton, StatusPill, type PillTone } from "../ui";

interface SecurityAutofixPanelProps {
  onClose: () => void;
  onOpenRunCapsule?: (runId: string) => void;
}

function severityTone(severity: SecuritySeverity): PillTone {
  if (severity === "critical" || severity === "high") return "danger";
  if (severity === "moderate") return "warning";
  return "neutral";
}

function applyStatusTone(status: ApplyStatus): PillTone {
  if (status === "done") return "success";
  if (status === "error") return "danger";
  if (status === "cancelled") return "neutral";
  return "warning";
}

function severityKey(severity: SecuritySeverity): string {
  return severity.charAt(0).toUpperCase() + severity.slice(1);
}

function applyStatusLabelSuffix(status: ApplyStatus): string {
  switch (status) {
    case "idle": return "Idle";
    case "creating_branch": return "CreatingBranch";
    case "running": return "Running";
    case "done": return "Done";
    case "error": return "Error";
    case "cancelled": return "Cancelled";
  }
}

export function SecurityAutofixPanel({ onClose, onOpenRunCapsule }: SecurityAutofixPanelProps) {
  const { t } = useT();
  const store = useSecurityAutofixStore();
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [confirmingApply, setConfirmingApply] = useState<string | null>(null);

  const selected: SecurityFinding | null = useMemo(
    () => store.findings.find((finding) => finding.id === selectedId) ?? null,
    [store.findings, selectedId],
  );

  const proposal = selected ? store.proposals[selected.id] : undefined;
  const apply = selected ? store.applyState[selected.id] : undefined;
  const applyBusy = apply ? apply.status === "creating_branch" || apply.status === "running" : false;

  return (
    <section className="flex h-full min-h-0 flex-col" aria-labelledby="security-autofix-title">
      <header className="flex shrink-0 items-start justify-between gap-3 border-b border-border px-5 py-4">
        <div>
          <h2 id="security-autofix-title" className="text-sm font-semibold text-foreground">
            {t("SecurityAutofix.title")}
          </h2>
          <p className="mt-1 max-w-2xl text-xs leading-5 text-muted">{t("SecurityAutofix.subtitle")}</p>
        </div>
        <IconButton size="sm" aria-label={t("SecurityAutofix.close")} title={t("SecurityAutofix.close")} onClick={onClose}>
          <X size={15} />
        </IconButton>
      </header>

      <div className="flex shrink-0 flex-wrap items-end gap-2 border-b border-border px-5 py-3">
        <label className="min-w-64 flex-1 text-xs text-muted">
          {t("SecurityAutofix.repositoryLabel")}
          <input
            className="mt-1 w-full rounded-md border border-border bg-background px-2.5 py-2 text-sm text-foreground outline-none focus:border-accent focus:ring-1 focus:ring-accent"
            placeholder={t("SecurityAutofix.repositoryPlaceholder")}
            value={store.repositorySlug}
            onChange={(event) => store.setRepositorySlug(event.target.value)}
          />
        </label>
        <Button variant="primary" disabled={store.scanning} onClick={() => void store.scan()}>
          {store.scanning ? <Loader2 className="animate-spin" size={14} /> : <ScanSearch size={14} />}{" "}
          {store.scanning ? t("SecurityAutofix.scanning") : t("SecurityAutofix.scanButton")}
        </Button>
      </div>

      {store.error && (
        <div role="alert" className="mx-5 mt-3 rounded-md border border-danger/40 bg-danger/10 p-3 text-xs text-danger">
          {store.error}
        </div>
      )}
      {store.scanError && (
        <div className="mx-5 mt-3 rounded-md border border-warning/40 bg-warning/10 p-3 text-xs text-warning">
          <p className="flex items-center gap-1.5 font-medium">
            <AlertTriangle size={13} /> {t("SecurityAutofix.scanErrorHeading")}
          </p>
          <p className="mt-1 whitespace-pre-wrap break-words">{store.scanError}</p>
        </div>
      )}

      <div className="grid min-h-0 flex-1 gap-4 overflow-hidden p-5 xl:grid-cols-[minmax(16rem,.9fr)_minmax(0,1.3fr)]">
        <div className="min-h-0 overflow-y-auto rounded-lg border border-border bg-surface p-3">
          <h3 className="text-xs font-semibold text-foreground">{t("SecurityAutofix.findingsHeading")}</h3>
          <div className="mt-2 space-y-1.5">
            {store.findings.length === 0 && (
              <p className="rounded-md border border-dashed border-border p-5 text-center text-xs text-faint">
                {t("SecurityAutofix.emptyFindings")}
              </p>
            )}
            {store.findings.map((finding) => (
              <button
                key={finding.id}
                type="button"
                onClick={() => setSelectedId(finding.id)}
                className={`w-full rounded-md border p-2.5 text-left transition-colors ${
                  finding.id === selectedId ? "border-accent bg-accent/10" : "border-border bg-background hover:border-border-strong"
                }`}
              >
                <p className="truncate text-xs font-medium text-foreground">{finding.title}</p>
                <div className="mt-1.5 flex flex-wrap items-center gap-1.5">
                  <StatusPill tone={severityTone(finding.severity)}>
                    {t(`SecurityAutofix.severity${severityKey(finding.severity)}`)}
                  </StatusPill>
                  <StatusPill tone="neutral">
                    {finding.kind === "dependency" ? t("SecurityAutofix.kindDependency") : t("SecurityAutofix.kindSecret")}
                  </StatusPill>
                </div>
              </button>
            ))}
          </div>
        </div>

        <div className="min-h-0 overflow-y-auto rounded-lg border border-border bg-surface p-4">
          {!selected ? (
            <p className="p-8 text-center text-xs text-faint">{t("SecurityAutofix.emptyFindings")}</p>
          ) : (
            <div className="space-y-4">
              <div className="flex flex-wrap items-start justify-between gap-2">
                <h3 className="text-sm font-semibold text-foreground">{selected.title}</h3>
                <div className="flex flex-wrap gap-1.5">
                  <StatusPill tone={severityTone(selected.severity)}>
                    {t(`SecurityAutofix.severity${severityKey(selected.severity)}`)}
                  </StatusPill>
                  <StatusPill tone="neutral">
                    {selected.kind === "dependency" ? t("SecurityAutofix.kindDependency") : t("SecurityAutofix.kindSecret")}
                  </StatusPill>
                </div>
              </div>

              <p className="text-xs leading-5 text-muted">{selected.description}</p>

              {selected.kind === "dependency" && selected.dependency && (
                <dl className="grid grid-cols-[8rem_minmax(0,1fr)] gap-x-3 gap-y-1.5 rounded-md border border-border bg-background p-3 text-[11px]">
                  <dt className="text-faint">{t("SecurityAutofix.packageLabel")}</dt>
                  <dd className="font-mono text-foreground">{selected.dependency.packageName}</dd>
                  <dt className="text-faint">{t("SecurityAutofix.currentVersionLabel")}</dt>
                  <dd className="font-mono text-foreground">{selected.dependency.vulnerableRange ?? "—"}</dd>
                  <dt className="text-faint">{t("SecurityAutofix.patchedVersionsLabel")}</dt>
                  <dd className="font-mono text-foreground">{selected.dependency.patchedVersions ?? "—"}</dd>
                  {selected.dependency.advisoryUrl && (
                    <>
                      <dt className="text-faint">{t("SecurityAutofix.advisoryLinkLabel")}</dt>
                      <dd className="break-all">
                        <a
                          className="text-accent underline"
                          href={selected.dependency.advisoryUrl}
                          target="_blank"
                          rel="noopener noreferrer"
                        >
                          {selected.dependency.advisoryUrl}
                        </a>
                      </dd>
                    </>
                  )}
                </dl>
              )}

              {selected.kind === "secret" && selected.secret && (
                <dl className="grid grid-cols-[8rem_minmax(0,1fr)] gap-x-3 gap-y-1.5 rounded-md border border-border bg-background p-3 text-[11px]">
                  <dt className="text-faint">{t("SecurityAutofix.fileLabel")}</dt>
                  <dd className="break-all font-mono text-foreground">
                    {selected.secret.path}:{selected.secret.line}
                  </dd>
                  <dt className="text-faint">{t("SecurityAutofix.ruleLabel")}</dt>
                  <dd className="font-mono text-foreground">{selected.secret.ruleName}</dd>
                  <dt className="text-faint">{t("SecurityAutofix.snippetLabel")}</dt>
                  <dd className="break-all font-mono text-foreground">{selected.secret.redactedSnippet}</dd>
                </dl>
              )}

              <div className="flex flex-wrap gap-2">
                <Button size="sm" disabled={store.proposing[selected.id]} onClick={() => void store.proposeFix(selected.id)}>
                  {store.proposing[selected.id] ? <Loader2 className="animate-spin" size={13} /> : <Sparkles size={13} />}{" "}
                  {store.proposing[selected.id] ? t("SecurityAutofix.proposing") : t("SecurityAutofix.proposeButton")}
                </Button>
              </div>

              {proposal && (
                <div className="rounded-md border border-border bg-background p-3">
                  <div className="flex items-center justify-between gap-2">
                    <h4 className="text-xs font-semibold text-foreground">{t("SecurityAutofix.proposalHeading")}</h4>
                    <span className="text-[10px] text-faint">
                      {proposal.source === "model"
                        ? t("SecurityAutofix.proposalSourceModel")
                        : t("SecurityAutofix.proposalSourceFallback")}
                    </span>
                  </div>
                  <div className="mt-2 space-y-2 text-[11px] leading-5">
                    <p>
                      <span className="font-medium text-foreground">{t("SecurityAutofix.exploitabilityHeading")}: </span>
                      <span className="text-muted">{proposal.exploitabilityNote}</span>
                    </p>
                    <p>
                      <span className="font-medium text-foreground">{t("SecurityAutofix.proposedFixHeading")}: </span>
                      <span className="text-muted">{proposal.proposedFix}</span>
                    </p>
                    <p>
                      <span className="font-medium text-foreground">{t("SecurityAutofix.testPlanHeading")}: </span>
                      <span className="text-muted">{proposal.testPlan}</span>
                    </p>
                  </div>

                  <div className="mt-3 flex flex-wrap gap-2">
                    {(!apply || apply.status === "idle" || apply.status === "error" || apply.status === "cancelled") && (
                      <Button
                        size="sm"
                        variant="primary"
                        disabled={!store.repositorySlug.trim()}
                        onClick={() => setConfirmingApply(selected.id)}
                      >
                        <GitBranch size={13} /> {t("SecurityAutofix.applyButton")}
                      </Button>
                    )}
                    {applyBusy && (
                      <Button size="sm" variant="danger" onClick={() => store.cancelApply(selected.id)}>
                        <Square size={13} /> {t("SecurityAutofix.cancelApplyButton")}
                      </Button>
                    )}
                    {apply?.durableRunId && onOpenRunCapsule && (
                      <Button size="sm" onClick={() => onOpenRunCapsule(apply.durableRunId!)}>
                        <ExternalLink size={13} /> {t("SecurityAutofix.viewCapsuleButton")}
                      </Button>
                    )}
                  </div>

                  {confirmingApply === selected.id && (
                    <div className="mt-3 rounded-md border border-warning/40 bg-warning/5 p-3 text-[11px]">
                      <p className="font-medium text-foreground">{t("SecurityAutofix.applyConfirmHeading")}</p>
                      <p className="mt-1 text-muted">{t("SecurityAutofix.applyConfirmDescription")}</p>
                      <div className="mt-2 flex justify-end gap-2">
                        <Button size="sm" onClick={() => setConfirmingApply(null)}>
                          {t("SecurityAutofix.applyConfirmCancel")}
                        </Button>
                        <Button
                          size="sm"
                          variant="danger"
                          onClick={() => {
                            setConfirmingApply(null);
                            void store.applyFix(selected.id);
                          }}
                        >
                          {t("SecurityAutofix.applyConfirmButton")}
                        </Button>
                      </div>
                    </div>
                  )}

                  {apply && apply.status !== "idle" && (
                    <div className="mt-3 space-y-1.5 rounded-md border border-border bg-surface p-2.5 text-[11px]">
                      <div className="flex items-center gap-2">
                        {apply.status === "done" ? (
                          <CheckCircle2 size={13} className="shrink-0 text-success" />
                        ) : apply.status === "error" ? (
                          <AlertTriangle size={13} className="shrink-0 text-danger" />
                        ) : (
                          <Loader2 className="animate-spin shrink-0" size={13} />
                        )}
                        <StatusPill tone={applyStatusTone(apply.status)}>
                          {t(`SecurityAutofix.applyStatus${applyStatusLabelSuffix(apply.status)}`)}
                        </StatusPill>
                      </div>
                      {apply.branch && (
                        <p className="font-mono text-faint">
                          {t("SecurityAutofix.branchLabel")}: {apply.branch}
                        </p>
                      )}
                      {apply.workspaceLabel && (
                        <p className="font-mono text-faint">
                          {t("SecurityAutofix.worktreeLabel")}: {apply.workspaceLabel}
                        </p>
                      )}
                      {apply.activity && applyBusy && (
                        <p className="text-muted">{t("SecurityAutofix.currentActivity", { activity: apply.activity })}</p>
                      )}
                      {apply.summary && <p className="whitespace-pre-wrap break-words text-muted">{apply.summary}</p>}
                      {apply.error && <p className="whitespace-pre-wrap break-words text-danger">{apply.error}</p>}
                    </div>
                  )}
                </div>
              )}

              <p className="rounded-md border border-dashed border-border p-2.5 text-[10px] leading-4 text-faint">
                {t("SecurityAutofix.nonGoalsNote")}
              </p>
              <p className="rounded-md border border-dashed border-border p-2.5 text-[10px] leading-4 text-faint">
                {t("SecurityAutofix.followUpsNote")}
              </p>
            </div>
          )}
        </div>
      </div>
    </section>
  );
}

export default SecurityAutofixPanel;
