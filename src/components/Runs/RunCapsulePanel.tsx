import { useEffect, useMemo, useState, type ReactNode } from "react";
import { save } from "@tauri-apps/plugin-dialog";
import { writeTextFile } from "@tauri-apps/plugin-fs";
import {
  AlertTriangle,
  CheckCircle2,
  Download,
  Eye,
  FileDiff,
  FlaskConical,
  GitCompareArrows,
  Image,
  Play,
  Route,
  ShieldCheck,
  TerminalSquare,
  Wrench,
  X,
} from "lucide-react";

import { artifactDataUrl, readDurableArtifact } from "../../lib/durableArtifacts";
import { useT } from "../../lib/i18n";
import {
  buildRunCapsule,
  capsuleHasBrowserEvidence,
  compareRunCapsules,
  runCapsuleFileName,
  serializeRedactedRunCapsule,
  type ReplayClassification,
  type RunCapsule,
} from "../../lib/runCapsule";
import { loadRunEvents, type RunEventEnvelopeWire, type RunRecord } from "../../lib/runProtocol";
import { Button, StatusPill, type PillTone } from "../ui";
import { errorMessage } from "../../lib/errors";
import { formatBytes, formatDuration, formatTimestamp } from "../../lib/format";

interface RunCapsulePanelProps {
  run: RunRecord;
  events: RunEventEnvelopeWire[];
  runs: RunRecord[];
  replayEngineAvailable: boolean;
  actionBusy: boolean;
  onReplay: () => Promise<void>;
}

interface ImagePreview {
  artifactId: string;
  name: string;
  url: string;
}

function replayTone(classification: ReplayClassification): PillTone {
  if (classification === "deterministic") return "success";
  if (classification === "best_effort") return "warning";
  return "danger";
}




function formatCost(value: number | null): string {
  return value === null ? "—" : `$${(value / 1_000_000).toFixed(4)}`;
}

function EvidenceSection({
  icon,
  title,
  count,
  children,
  open = false,
}: {
  icon: ReactNode;
  title: string;
  count: number;
  children: ReactNode;
  open?: boolean;
}) {
  return (
    <details open={open} className="group rounded-lg border border-border bg-surface">
      <summary className="flex min-h-11 cursor-pointer list-none items-center justify-between gap-3 px-3 py-2 text-sm font-medium focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-accent">
        <span className="flex items-center gap-2">{icon}{title}</span>
        <span className="rounded-full bg-surface-2 px-2 py-0.5 text-xs text-muted">{count}</span>
      </summary>
      <div className="border-t border-border p-3">{children}</div>
    </details>
  );
}

function EmptyEvidence({ children }: { children: ReactNode }) {
  return <p className="text-xs leading-relaxed text-faint">{children}</p>;
}

export function RunCapsulePanel({ run, events, runs, replayEngineAvailable, actionBusy, onReplay }: RunCapsulePanelProps) {
  const { t } = useT();
  const capsule = useMemo(() => buildRunCapsule(run, events), [events, run]);
  const [compareRunId, setCompareRunId] = useState("");
  const [compareCapsule, setCompareCapsule] = useState<RunCapsule | null>(null);
  const [comparisonLoading, setComparisonLoading] = useState(false);
  const [preview, setPreview] = useState<ImagePreview | null>(null);
  const [busy, setBusy] = useState<"export" | "preview" | null>(null);
  const [status, setStatus] = useState<{ tone: "success" | "danger"; message: string } | null>(null);

  useEffect(() => {
    setCompareRunId("");
    setCompareCapsule(null);
    setPreview(null);
    setStatus(null);
  }, [run.spec.run_id]);

  useEffect(() => {
    if (!compareRunId) {
      setCompareCapsule(null);
      return;
    }
    const peer = runs.find((entry) => entry.spec.run_id === compareRunId);
    if (!peer) {
      setCompareCapsule(null);
      return;
    }
    let cancelled = false;
    setComparisonLoading(true);
    void loadRunEvents(compareRunId).then((peerEvents) => {
      if (!cancelled) setCompareCapsule(buildRunCapsule(peer, peerEvents));
    }).catch((caught) => {
      if (!cancelled) setStatus({ tone: "danger", message: errorMessage(caught) });
    }).finally(() => {
      if (!cancelled) setComparisonLoading(false);
    });
    return () => { cancelled = true; };
  }, [compareRunId, runs]);

  const comparison = useMemo(
    () => compareCapsule ? compareRunCapsules(capsule, compareCapsule) : null,
    [capsule, compareCapsule],
  );
  const browserEvidence = capsuleHasBrowserEvidence(capsule);
  const safeReplayAvailable = capsule.replay.safeFromStart && replayEngineAvailable;
  const replayUnavailableReason = !capsule.replay.safeFromStart
    ? t("RunCapsule.replayUnsafe")
    : !replayEngineAvailable
      ? t("RunCapsule.replayEngineUnavailable")
      : null;
  const peerRuns = runs.filter((entry) => entry.spec.run_id !== run.spec.run_id);

  async function exportCapsule() {
    setBusy("export");
    setStatus(null);
    try {
      const destination = await save({
        defaultPath: runCapsuleFileName(capsule),
        filters: [{ name: t("RunCapsule.fileType"), extensions: ["json"] }],
      });
      if (!destination) return;
      await writeTextFile(destination, serializeRedactedRunCapsule(capsule));
      setStatus({ tone: "success", message: t("RunCapsule.exportComplete") });
    } catch (caught) {
      setStatus({ tone: "danger", message: errorMessage(caught) });
    } finally {
      setBusy(null);
    }
  }

  async function previewArtifact(artifactId: string, name: string, mediaType: string) {
    setBusy("preview");
    setStatus(null);
    try {
      const content = await readDurableArtifact(artifactId);
      setPreview({ artifactId, name, url: artifactDataUrl(mediaType, content.contentBase64) });
    } catch (caught) {
      setStatus({ tone: "danger", message: errorMessage(caught) });
    } finally {
      setBusy(null);
    }
  }

  async function replay() {
    setStatus(null);
    try {
      await onReplay();
      setStatus({ tone: "success", message: t("RunCapsule.replayQueued") });
    } catch (caught) {
      setStatus({ tone: "danger", message: errorMessage(caught) });
    }
  }

  return (
    <section aria-labelledby="run-capsule-title" className="space-y-4">
      <div className="flex flex-col gap-3 rounded-xl border border-border bg-surface p-4 sm:flex-row sm:items-start sm:justify-between">
        <div className="min-w-0">
          <div className="flex flex-wrap items-center gap-2">
            <h3 id="run-capsule-title" className="text-base font-semibold text-foreground">{t("RunCapsule.title")}</h3>
            <StatusPill tone={replayTone(capsule.replay.classification)}>
              {t(`RunCapsule.replay.${capsule.replay.classification}`)}
            </StatusPill>
            {browserEvidence && <StatusPill tone="neutral">{t("RunCapsule.browserEvidence")}</StatusPill>}
          </div>
          <p className="mt-1 max-w-2xl text-xs leading-relaxed text-muted">{t("RunCapsule.subtitle")}</p>
        </div>
        <Button className="min-h-11 shrink-0" onClick={() => void exportCapsule()} disabled={busy !== null}>
          <Download size={14} /> {busy === "export" ? t("RunCapsule.exporting") : t("RunCapsule.export")}
        </Button>
      </div>

      {status && (
        <div role={status.tone === "danger" ? "alert" : "status"} className={`rounded-lg border px-3 py-2 text-xs ${status.tone === "danger" ? "border-danger/30 bg-danger-soft text-danger" : "border-success/30 bg-success-soft text-success"}`}>
          {status.message}
        </div>
      )}

      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
        <div className="rounded-lg border border-border bg-surface p-3"><p className="text-[11px] uppercase tracking-wide text-faint">{t("RunCapsule.duration")}</p><p className="mt-1 text-sm font-semibold">{formatDuration(capsule.run.durationMs)}</p></div>
        <div className="rounded-lg border border-border bg-surface p-3"><p className="text-[11px] uppercase tracking-wide text-faint">{t("RunCapsule.tokens")}</p><p className="mt-1 text-sm font-semibold">{(capsule.usage.input_tokens + capsule.usage.output_tokens).toLocaleString()}</p></div>
        <div className="rounded-lg border border-border bg-surface p-3"><p className="text-[11px] uppercase tracking-wide text-faint">{t("RunCapsule.cost")}</p><p className="mt-1 text-sm font-semibold">{formatCost(capsule.usage.cost_micros)}</p></div>
        <div className="rounded-lg border border-border bg-surface p-3"><p className="text-[11px] uppercase tracking-wide text-faint">{t("RunCapsule.evidence")}</p><p className="mt-1 text-sm font-semibold">{capsule.artifacts.length + capsule.verifications.length + capsule.checkpoints.length}</p></div>
      </div>

      <section className="rounded-xl border border-border bg-surface p-4" aria-labelledby="run-capsule-snapshot-title">
        <h4 id="run-capsule-snapshot-title" className="flex items-center gap-2 text-sm font-semibold"><Route size={15} />{t("RunCapsule.snapshot")}</h4>
        <dl className="mt-3 grid gap-3 text-xs sm:grid-cols-2">
          <div className="sm:col-span-2"><dt className="text-faint">{t("RunCapsule.prompt")}</dt><dd className="mt-1 whitespace-pre-wrap text-sm leading-relaxed text-foreground">{capsule.prompt.task}</dd></div>
          {capsule.prompt.instructions && <div className="sm:col-span-2"><dt className="text-faint">{t("RunCapsule.instructions")}</dt><dd className="mt-1 whitespace-pre-wrap leading-relaxed text-muted">{capsule.prompt.instructions}</dd></div>}
          <div><dt className="text-faint">{t("RunCapsule.model")}</dt><dd className="mt-1 font-medium text-foreground">{capsule.target.label}</dd></div>
          <div><dt className="text-faint">{t("RunCapsule.routing")}</dt><dd className="mt-1 text-muted">{capsule.routing.description}</dd></div>
          <div><dt className="text-faint">{t("RunCapsule.permissionMode")}</dt><dd className="mt-1 font-medium text-foreground">{capsule.execution.permissionPolicy.mode}</dd></div>
          <div><dt className="text-faint">{t("RunCapsule.created")}</dt><dd className="mt-1 font-medium text-foreground">{formatTimestamp(capsule.run.createdAtMs, { timeStyle: "medium" })}</dd></div>
        </dl>
      </section>

      <section className={`rounded-xl border p-4 ${capsule.replay.classification === "non_repeatable" ? "border-danger/30 bg-danger-soft" : capsule.replay.safeFromStart ? "border-success/30 bg-success-soft" : "border-warning/30 bg-warning-soft"}`} aria-labelledby="run-capsule-replay-title">
        <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
          <div className="min-w-0">
            <h4 id="run-capsule-replay-title" className="flex items-center gap-2 text-sm font-semibold">
              {capsule.replay.safeFromStart ? <ShieldCheck size={16} /> : <AlertTriangle size={16} />}
              {t("RunCapsule.replayTitle")}
            </h4>
            <p className="mt-1 text-xs leading-relaxed text-muted">{capsule.replay.guarantee}</p>
            <p className="mt-2 text-xs font-medium text-foreground">{t("RunCapsule.boundary")}: {t(`RunCapsule.boundary.${capsule.replay.boundary}`)}</p>
          </div>
          <Button variant="primary" className="min-h-11 shrink-0" disabled={!safeReplayAvailable || actionBusy} title={replayUnavailableReason ?? undefined} onClick={() => void replay()}>
            <Play size={14} /> {t("RunCapsule.replayAction")}
          </Button>
        </div>
        {replayUnavailableReason && <p className="mt-2 text-xs text-muted">{replayUnavailableReason}</p>}
        <ul className="mt-3 list-disc space-y-1 pl-5 text-xs leading-relaxed text-muted">
          {capsule.replay.reasons.map((reason) => <li key={reason}>{reason}</li>)}
        </ul>
        {capsule.replay.externalEffects.length > 0 && (
          <div className="mt-3 rounded-lg border border-danger/20 bg-background/50 p-3">
            <p className="text-xs font-semibold text-foreground">{t("RunCapsule.externalEffects")}</p>
            <ul className="mt-1 list-disc pl-5 text-xs text-muted">{capsule.replay.externalEffects.map((effect) => <li key={effect}>{effect}</li>)}</ul>
          </div>
        )}
      </section>

      <div className="grid gap-2 lg:grid-cols-2">
        <EvidenceSection icon={<Wrench size={15} />} title={t("RunCapsule.tools")} count={capsule.tools.length} open>
          {capsule.tools.length === 0 ? <EmptyEvidence>{t("RunCapsule.noTools")}</EmptyEvidence> : (
            <ul className="space-y-2">{capsule.tools.map((tool) => <li key={tool.toolCallId} className="rounded-md bg-background p-2 text-xs"><div className="flex items-center justify-between gap-2"><span className="font-mono font-medium">{tool.name}</span><StatusPill tone={tool.outcome === "succeeded" ? "success" : tool.outcome === "failed" || tool.outcome === "denied" ? "danger" : "neutral"}>{tool.outcome.replace(/_/g, " ")}</StatusPill></div><p className="mt-1 text-faint">{tool.mutation ? t("RunCapsule.mutationCapable") : t("RunCapsule.readOnly")}{tool.durationMs === null ? "" : ` · ${formatDuration(tool.durationMs)}`}</p></li>)}</ul>
          )}
        </EvidenceSection>

        <EvidenceSection icon={<FileDiff size={15} />} title={t("RunCapsule.filesChanged")} count={capsule.fileChanges.length}>
          {capsule.fileChanges.length === 0 ? <EmptyEvidence>{t("RunCapsule.noStructuredFileChanges")}</EmptyEvidence> : (
            <ul className="space-y-2">{capsule.fileChanges.map((change) => <li key={`${change.toolCallId}:${change.path}`} className="rounded-md bg-background p-2 text-xs"><p className="break-all font-mono text-foreground">{change.path}</p><p className="mt-1 text-faint">{change.toolName} · {change.outcome}</p></li>)}</ul>
          )}
        </EvidenceSection>

        <EvidenceSection icon={<TerminalSquare size={15} />} title={t("RunCapsule.terminal")} count={capsule.terminalExcerpts.length}>
          {capsule.terminalExcerpts.length === 0 ? <EmptyEvidence>{t("RunCapsule.noTerminal")}</EmptyEvidence> : (
            <div className="space-y-2">{capsule.terminalExcerpts.map((excerpt) => <div key={excerpt.toolCallId} className="rounded-md bg-background p-2"><p className="text-xs font-medium">{excerpt.toolName} · {excerpt.outcome}</p><pre className="mt-2 max-h-48 overflow-auto whitespace-pre-wrap break-words font-mono text-[11px] text-muted">{excerpt.excerpt ?? t("RunCapsule.excerptUnavailable")}</pre></div>)}</div>
          )}
        </EvidenceSection>

        <EvidenceSection icon={<FlaskConical size={15} />} title={t("RunCapsule.verifications")} count={capsule.verifications.length}>
          {capsule.verifications.length === 0 ? <EmptyEvidence>{t("RunCapsule.noVerifications")}</EmptyEvidence> : (
            <ul className="space-y-2">{capsule.verifications.map((verification) => <li key={verification.verificationId} className="flex items-start gap-2 rounded-md bg-background p-2 text-xs">{verification.passed ? <CheckCircle2 size={15} className="mt-0.5 shrink-0 text-success" /> : <AlertTriangle size={15} className="mt-0.5 shrink-0 text-danger" />}<div><p className="font-medium text-foreground">{verification.name}</p><p className="mt-1 leading-relaxed text-muted">{verification.summary}</p></div></li>)}</ul>
          )}
        </EvidenceSection>

        <EvidenceSection icon={<Image size={15} />} title={t("RunCapsule.artifacts")} count={capsule.artifacts.length}>
          {capsule.artifacts.length === 0 ? <EmptyEvidence>{t("RunCapsule.noArtifacts")}</EmptyEvidence> : (
            <ul className="space-y-2">{capsule.artifacts.map((artifact) => <li key={artifact.artifactId} className="rounded-md bg-background p-2 text-xs"><div className="flex items-start justify-between gap-2"><div className="min-w-0"><p className="truncate font-medium text-foreground">{artifact.name}</p><p className="mt-1 text-faint">{artifact.mediaType} · {formatBytes(artifact.sizeBytes)}</p><p className="mt-1 truncate font-mono text-[10px] text-faint" title={artifact.artifactId}>{artifact.artifactId}</p></div>{artifact.mediaType.startsWith("image/") && <Button size="sm" disabled={busy !== null} onClick={() => void previewArtifact(artifact.artifactId, artifact.name, artifact.mediaType)}><Eye size={12} />{t("RunCapsule.preview")}</Button>}</div></li>)}</ul>
          )}
        </EvidenceSection>

        <EvidenceSection icon={<ShieldCheck size={15} />} title={t("RunCapsule.approvalsConnectors")} count={capsule.approvals.length + capsule.connectorCalls.length}>
          {capsule.approvals.length === 0 && capsule.connectorCalls.length === 0 ? <EmptyEvidence>{t("RunCapsule.noApprovalsConnectors")}</EmptyEvidence> : (
            <div className="space-y-3">
              {capsule.approvals.map((approval) => <div key={approval.requestId} className="rounded-md bg-background p-2 text-xs"><div className="flex items-center justify-between gap-2"><p className="font-medium">{approval.toolName}</p><StatusPill tone={approval.decision === "deny" || approval.decision === "expired" ? "danger" : approval.decision === "pending" ? "warning" : "success"}>{approval.decision.replace(/_/g, " ")}</StatusPill></div><p className="mt-1 text-muted">{approval.detail}</p></div>)}
              {capsule.connectorCalls.map((call) => <div key={call.toolCallId} className="rounded-md bg-background p-2 text-xs"><p className="font-mono font-medium">{call.toolName}</p><p className="mt-1 text-faint">{call.outcome.replace(/_/g, " ")} · {call.mutationBoundary ? t("RunCapsule.externalBoundary") : t("RunCapsule.readOnly")}</p></div>)}
            </div>
          )}
        </EvidenceSection>
      </div>

      {preview && (
        <section className="rounded-xl border border-border bg-surface p-3" aria-labelledby="run-capsule-preview-title">
          <div className="flex items-center justify-between gap-3"><h4 id="run-capsule-preview-title" className="truncate text-sm font-semibold">{preview.name}</h4><Button variant="ghost" size="sm" onClick={() => setPreview(null)}><X size={14} />{t("RunCapsule.closePreview")}</Button></div>
          <img src={preview.url} alt={preview.name} className="mt-3 max-h-[32rem] w-full rounded-lg border border-border bg-background object-contain" />
          <p className="mt-2 truncate font-mono text-[10px] text-faint" title={preview.artifactId}>{preview.artifactId}</p>
        </section>
      )}

      <section className="rounded-xl border border-border bg-surface p-4" aria-labelledby="run-capsule-timeline-title">
        <h4 id="run-capsule-timeline-title" className="text-sm font-semibold">{t("RunCapsule.timeline")}</h4>
        {capsule.timeline.length === 0 ? <EmptyEvidence>{t("RunCapsule.noTimeline")}</EmptyEvidence> : (
          <ol className="mt-3 border-l border-border pl-4">{capsule.timeline.map((entry) => <li key={entry.eventId} className="relative pb-4 last:pb-0"><span className="absolute -left-[1.18rem] top-1.5 h-2 w-2 rounded-full border border-background bg-accent" /><div className="flex flex-col gap-0.5 sm:flex-row sm:items-start sm:justify-between sm:gap-3"><p className="text-xs font-medium text-foreground">#{entry.sequence} · {entry.title}</p><time className="shrink-0 text-[10px] text-faint" dateTime={new Date(entry.occurredAtMs).toISOString()}>{formatTimestamp(entry.occurredAtMs, { timeStyle: "medium" })}</time></div><p className="mt-1 text-xs leading-relaxed text-muted">{entry.summary}</p></li>)}</ol>
        )}
      </section>

      <section className="rounded-xl border border-border bg-surface p-4" aria-labelledby="run-capsule-compare-title">
        <div className="flex flex-col gap-3 sm:flex-row sm:items-end sm:justify-between">
          <div><h4 id="run-capsule-compare-title" className="flex items-center gap-2 text-sm font-semibold"><GitCompareArrows size={15} />{t("RunCapsule.compare")}</h4><p className="mt-1 text-xs text-muted">{t("RunCapsule.compareHint")}</p></div>
          <label className="text-xs font-medium text-muted"><span className="sr-only">{t("RunCapsule.compareSelect")}</span><select className="min-h-11 max-w-full rounded-md border border-border bg-background px-3 text-xs text-foreground outline-none focus-visible:ring-2 focus-visible:ring-accent" value={compareRunId} onChange={(event) => setCompareRunId(event.target.value)}><option value="">{t("RunCapsule.compareSelect")}</option>{peerRuns.map((entry) => <option key={entry.spec.run_id} value={entry.spec.run_id}>{entry.spec.task} · {entry.spec.target.label}</option>)}</select></label>
        </div>
        {comparisonLoading && <p className="mt-3 text-xs text-faint">{t("RunCapsule.comparing")}</p>}
        {comparison && compareCapsule && (
          <div className="mt-4 overflow-x-auto">
            <table className="w-full min-w-[36rem] border-collapse text-left text-xs"><caption className="sr-only">{t("RunCapsule.compareCaption")}</caption><thead><tr className="border-b border-border text-faint"><th className="px-2 py-2 font-medium">{t("RunCapsule.metric")}</th><th className="px-2 py-2 font-medium">{capsule.target.label}</th><th className="px-2 py-2 font-medium">{compareCapsule.target.label}</th></tr></thead><tbody>{comparison.rows.map((row) => <tr key={row.key} className={row.changed ? "bg-accent-soft/40" : ""}><th className="border-b border-border px-2 py-2 font-medium text-muted">{row.label}</th><td className="border-b border-border px-2 py-2 text-foreground">{row.left}</td><td className="border-b border-border px-2 py-2 text-foreground">{row.right}</td></tr>)}</tbody></table>
            <p className="mt-3 text-xs text-muted">{t("RunCapsule.changedMetrics", { count: comparison.changedFields })}</p>
          </div>
        )}
      </section>

      <section className="rounded-lg border border-border bg-surface-2 p-3" aria-labelledby="run-capsule-limitations-title">
        <h4 id="run-capsule-limitations-title" className="text-xs font-semibold text-foreground">{t("RunCapsule.limitations")}</h4>
        <ul className="mt-2 list-disc space-y-1 pl-5 text-xs leading-relaxed text-muted">{capsule.limitations.map((limitation) => <li key={limitation}>{limitation}</li>)}</ul>
      </section>
    </section>
  );
}
