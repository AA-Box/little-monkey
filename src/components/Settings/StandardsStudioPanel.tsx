import { useEffect, useMemo, useState } from "react";
import { Check, Download, Play, RefreshCw, Search, ShieldCheck, Upload, X } from "lucide-react";

import { useStandardsStore } from "../../store/standardsStore";
import { useVerifyStore } from "../../store/verifyStore";
import type { EngineeringStandard, StandardStatus } from "../../lib/standards";
import { Button, StatusPill, type PillTone } from "../ui";

const STATUS_TONE: Record<StandardStatus, PillTone> = {
  candidate: "warning",
  approved: "success",
  rejected: "neutral",
  deprecated: "neutral",
  conflicting: "danger",
  stale: "danger",
};

function StandardCard({ standard }: { standard: EngineeringStandard }) {
  const approve = useStandardsStore((state) => state.approve);
  const reject = useStandardsStore((state) => state.reject);
  const deprecate = useStandardsStore((state) => state.deprecate);
  const setCheckers = useStandardsStore((state) => state.setCheckers);
  const runCheckers = useStandardsStore((state) => state.runCheckers);
  const checkerResults = useStandardsStore((state) => state.lastCheckerResults[standard.standard_id] ?? []);
  const verifyCommands = useVerifyStore((state) => state.config.commands).filter((command) => command.enabled);
  const [busy, setBusy] = useState(false);
  const [checkerError, setCheckerError] = useState<string | null>(null);

  const run = async (operation: () => Promise<void>) => {
    setBusy(true);
    try { await operation(); } finally { setBusy(false); }
  };

  const supporting = standard.evidence.filter((entry) => entry.supports);
  const counterexamples = standard.evidence.filter((entry) => !entry.supports);

  const toggleChecker = async (commandId: string, checked: boolean) => {
    const next = checked
      ? [...new Set([...standard.checker_command_ids, commandId])]
      : standard.checker_command_ids.filter((id) => id !== commandId);
    setCheckerError(null);
    try {
      await run(() => setCheckers(standard.standard_id, next));
    } catch (error) {
      setCheckerError(error instanceof Error ? error.message : String(error));
    }
  };

  const executeCheckers = async () => {
    setCheckerError(null);
    setBusy(true);
    try {
      await runCheckers(standard.standard_id);
    } catch (error) {
      setCheckerError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusy(false);
    }
  };

  return (
    <article className="rounded-lg border border-border bg-background p-3">
      <div className="flex flex-wrap items-start justify-between gap-2">
        <div className="min-w-0 flex-1">
          <div className="flex flex-wrap items-center gap-2">
            <h4 className="text-sm font-semibold text-foreground">{standard.title}</h4>
            <StatusPill tone={STATUS_TONE[standard.status]}>{standard.status}</StatusPill>
            <StatusPill>{standard.severity}</StatusPill>
            <StatusPill tone={standard.drift === "healthy" ? "success" : standard.drift === "weakened" ? "warning" : standard.drift === "contradicted" ? "danger" : "neutral"}>
              {standard.drift}
            </StatusPill>
            {standard.pending_revision && <StatusPill tone="warning">revision v{standard.pending_revision.version} pending</StatusPill>}
          </div>
          <p className="mt-1 font-mono text-[11px] text-faint">{standard.standard_id}@v{standard.version} · confidence {Math.round(standard.confidence * 100)}%</p>
          <p className="mt-2 text-xs leading-5 text-muted">{standard.body}</p>
        </div>
        <div className="flex shrink-0 gap-1.5">
          {(standard.status !== "approved" || standard.pending_revision) && standard.status !== "deprecated" && (
            <Button size="sm" variant="primary" disabled={busy} onClick={() => void run(() => approve(standard.standard_id))}>
              <Check size={13} /> {standard.pending_revision ? `Approve v${standard.pending_revision.version}` : "Approve"}
            </Button>
          )}
          {standard.status === "candidate" && (
            <Button size="sm" disabled={busy} onClick={() => void run(() => reject(standard.standard_id))}>
              <X size={13} /> Reject
            </Button>
          )}
          {standard.status === "approved" && !standard.pending_revision && (
            <Button size="sm" disabled={busy} onClick={() => void run(() => deprecate(standard.standard_id))}>
              Deprecate
            </Button>
          )}
        </div>
      </div>

      {standard.pending_revision && (
        <section className="mt-3 rounded-md border border-warning/40 bg-warning-soft p-2 text-xs">
          <div className="flex flex-wrap items-center gap-2">
            <span className="font-semibold text-foreground">Proposed revision v{standard.pending_revision.version}</span>
            <StatusPill tone="warning">{standard.pending_revision.source}</StatusPill>
          </div>
          <p className="mt-1 font-medium text-foreground">{standard.pending_revision.title}</p>
          <p className="mt-1 whitespace-pre-wrap text-muted">{standard.pending_revision.body}</p>
          <p className="mt-1 font-mono text-[10px] text-faint">sha256:{standard.pending_revision.content_sha256}</p>
        </section>
      )}

      <details className="mt-3 text-xs">
        <summary className="cursor-pointer font-medium text-foreground">
          Evidence ({supporting.length} supporting{counterexamples.length ? `, ${counterexamples.length} counterexample${counterexamples.length === 1 ? "" : "s"}` : ""})
        </summary>
        <div className="mt-2 space-y-2">
          {standard.evidence.map((entry, index) => (
            <div key={`${entry.path}:${entry.line ?? 0}:${index}`} className={`rounded-md border p-2 ${entry.supports ? "border-border bg-surface" : "border-warning/40 bg-warning-soft"}`}>
              <div className="flex flex-wrap items-center gap-2">
                <span className="font-mono text-foreground">{entry.path}{entry.line ? `:${entry.line}` : ""}</span>
                <StatusPill tone={entry.supports ? "success" : "warning"}>{entry.supports ? "supports" : "counterexample"}</StatusPill>
                <span className="text-faint">{entry.kind}</span>
              </div>
              {entry.excerpt && <pre className="mt-1 overflow-auto whitespace-pre-wrap break-words text-[11px] text-muted">{entry.excerpt}</pre>}
              <p className="mt-1 truncate font-mono text-[10px] text-faint">sha256:{entry.sha256}</p>
            </div>
          ))}
        </div>
      </details>

      <details className="mt-3 text-xs">
        <summary className="cursor-pointer font-medium text-foreground">Revision history ({standard.revision_history.length})</summary>
        <div className="mt-2 space-y-2">
          {standard.revision_history.length === 0 ? <p className="text-muted">No superseded revisions.</p> : standard.revision_history.slice().reverse().map((revision) => (
            <div key={`${revision.version}:${revision.content_sha256}`} className="rounded-md border border-border bg-surface p-2">
              <div className="flex flex-wrap items-center gap-2"><span className="font-mono text-foreground">v{revision.version}</span><StatusPill>{revision.reason}</StatusPill></div>
              <p className="mt-1 font-medium text-foreground">{revision.title}</p>
              <p className="mt-1 whitespace-pre-wrap text-muted">{revision.body}</p>
              <p className="mt-1 font-mono text-[10px] text-faint">sha256:{revision.content_sha256}</p>
            </div>
          ))}
        </div>
      </details>

      <details className="mt-3 text-xs">
        <summary className="cursor-pointer font-medium text-foreground">Mechanical checks ({standard.checker_command_ids.length})</summary>
        <div className="mt-2 space-y-2 rounded-md border border-border bg-surface p-2">
          <p className="text-muted">Bind only app-owned Verification command IDs. Command text is never copied into repository policy, so a cloned repository cannot create executable authority.</p>
          {verifyCommands.length === 0 ? <p className="text-faint">No enabled Verification commands are configured for this workspace.</p> : verifyCommands.map((command) => (
            <label key={command.id} className="flex items-start gap-2 rounded border border-border bg-background p-2">
              <input
                type="checkbox"
                checked={standard.checker_command_ids.includes(command.id)}
                disabled={busy}
                onChange={(event) => void toggleChecker(command.id, event.target.checked)}
              />
              <span><span className="font-medium text-foreground">{command.label || command.id}</span><span className="ml-2 text-faint">{command.kind}</span></span>
            </label>
          ))}
          <Button size="sm" disabled={busy || standard.checker_command_ids.length === 0} onClick={() => void executeCheckers()}><Play size={13} /> Run bound checks</Button>
          {checkerError && <p role="alert" className="text-danger">{checkerError}</p>}
          {checkerResults.map((result) => (
            <div key={result.commandId} className="rounded border border-border bg-background p-2">
              <div className="flex flex-wrap items-center gap-2">
                <StatusPill tone={result.code === 0 && !result.timedOut ? "success" : "danger"}>{result.code === 0 && !result.timedOut ? "passed" : result.timedOut ? "timed out" : "failed"}</StatusPill>
                <span className="font-medium text-foreground">{result.label}</span>
                <span className="text-faint">{result.durationMs} ms</span>
              </div>
              {(result.stdout || result.stderr) && <pre className="mt-1 max-h-40 overflow-auto whitespace-pre-wrap break-words text-[10px] text-muted">{[result.stdout, result.stderr].filter(Boolean).join("\n")}</pre>}
            </div>
          ))}
        </div>
      </details>
    </article>
  );
}

type StudioTab = "candidates" | "approved" | "drift" | "conflicts" | "deprecated";

export function StandardsStudioPanel() {
  const document = useStandardsStore((state) => state.document);
  const workspacePath = useStandardsStore((state) => state.workspacePath);
  const loading = useStandardsStore((state) => state.loading);
  const error = useStandardsStore((state) => state.error);
  const lastExportPath = useStandardsStore((state) => state.lastExportPath);
  const refresh = useStandardsStore((state) => state.refresh);
  const discover = useStandardsStore((state) => state.discover);
  const drift = useStandardsStore((state) => state.drift);
  const preview = useStandardsStore((state) => state.preview);
  const importFile = useStandardsStore((state) => state.importFile);
  const exportFile = useStandardsStore((state) => state.exportFile);
  const importAgentOs = useStandardsStore((state) => state.importAgentOs);
  const exportAgentOs = useStandardsStore((state) => state.exportAgentOs);
  const refreshVerify = useVerifyStore((state) => state.refresh);
  const [tab, setTab] = useState<StudioTab>("candidates");
  const [previewText, setPreviewText] = useState("");

  useEffect(() => { void Promise.allSettled([refresh(), refreshVerify()]); }, [refresh, refreshVerify]);

  const standards = document?.standards ?? [];
  const counts = useMemo(() => ({
    candidates: standards.filter((standard) => standard.status === "candidate").length,
    approved: standards.filter((standard) => standard.status === "approved").length,
    drift: standards.filter((standard) => standard.drift !== "healthy" || standard.status === "stale" || standard.pending_revision).length,
    conflicts: standards.filter((standard) => standard.status === "conflicting" || standard.conflicts_with.length > 0).length,
    deprecated: standards.filter((standard) => standard.status === "deprecated" || standard.status === "rejected").length,
  }), [standards]);

  const visible = standards.filter((standard) => {
    if (tab === "candidates") return standard.status === "candidate";
    if (tab === "approved") return standard.status === "approved";
    if (tab === "drift") return standard.drift !== "healthy" || standard.status === "stale" || Boolean(standard.pending_revision);
    if (tab === "conflicts") return standard.status === "conflicting" || standard.conflicts_with.length > 0;
    return standard.status === "deprecated" || standard.status === "rejected";
  });
  const selection = previewText.trim() ? preview(previewText) : null;

  return (
    <div className="space-y-4">
      <section className="rounded-lg border border-border bg-surface p-4">
        <div className="flex flex-wrap items-start justify-between gap-3">
          <div className="max-w-2xl">
            <h3 className="flex items-center gap-2 text-sm font-semibold text-foreground"><ShieldCheck size={16} /> Standards Studio</h3>
            <p className="mt-1 text-xs leading-5 text-muted">
              Discover repository conventions from bounded evidence, preserve immutable approved revisions, review changes before they become authoritative, bind safe mechanical verification, and detect drift as the repository changes.
            </p>
            <p className="mt-1 break-all font-mono text-[11px] text-faint">{workspacePath ? `${workspacePath}/.little-monkey/standards/index.json` : "Open a workspace to use Standards Studio."}</p>
            {lastExportPath && <p className="mt-1 break-all text-[11px] text-faint">Last export: {lastExportPath}</p>}
          </div>
          <div className="flex flex-wrap gap-2">
            <Button size="sm" disabled={loading || !workspacePath} onClick={() => void refresh()}><RefreshCw size={13} /> Refresh</Button>
            <Button size="sm" variant="primary" disabled={loading || !workspacePath} onClick={() => void discover()}><Search size={13} /> Discover</Button>
            <Button size="sm" disabled={loading || !workspacePath || !document} onClick={() => void drift()}>Check drift</Button>
            <Button size="sm" disabled={!workspacePath} onClick={() => void importFile()}><Upload size={13} /> Import JSON</Button>
            <Button size="sm" disabled={!document} onClick={() => void exportFile()}><Download size={13} /> Export JSON</Button>
            <Button size="sm" disabled={loading || !workspacePath} onClick={() => void importAgentOs()}><Upload size={13} /> Import Agent OS</Button>
            <Button size="sm" disabled={loading || !document} onClick={() => void exportAgentOs()}><Download size={13} /> Export Agent OS</Button>
          </div>
        </div>
        {error && <p role="alert" className="mt-3 rounded-md border border-danger/40 bg-danger-soft p-2 text-xs text-danger">{error}</p>}
      </section>

      <section className="rounded-lg border border-border bg-surface p-3">
        <div className="flex flex-wrap gap-1.5">
          {(["candidates", "approved", "drift", "conflicts", "deprecated"] as StudioTab[]).map((entry) => (
            <Button key={entry} size="sm" variant={tab === entry ? "primary" : "secondary"} onClick={() => setTab(entry)}>
              {entry[0].toUpperCase() + entry.slice(1)} ({counts[entry]})
            </Button>
          ))}
        </div>
        <div className="mt-3 space-y-2">
          {visible.length === 0 ? <p className="py-6 text-center text-xs text-muted">No standards in this view.</p> : visible.map((standard) => <StandardCard key={standard.standard_id} standard={standard} />)}
        </div>
      </section>

      <section className="rounded-lg border border-border bg-surface p-4">
        <h4 className="text-xs font-semibold text-foreground">Injection preview</h4>
        <p className="mt-1 text-xs text-muted">Preview exactly which approved standards the normal task selector would inject for a request. Required standards rank first; task/language/framework/file relevance then decides the bounded remainder.</p>
        <textarea value={previewText} onChange={(event) => setPreviewText(event.target.value)} placeholder="e.g. Add a React component and Vitest coverage" className="mt-3 min-h-20 w-full rounded-md border border-border bg-background p-2 text-xs text-foreground" />
        {selection && (
          <div className="mt-3 space-y-2">
            <p className="text-[11px] text-faint">{selection.selected.length} selected · {selection.omitted} omitted · {selection.total_chars}/{selection.budget_chars} chars</p>
            {selection.selected.map(({ standard, score, reasons }) => (
              <div key={standard.standard_id} className="rounded-md border border-border bg-background p-2 text-xs">
                <div className="flex items-center justify-between gap-2"><span className="font-medium text-foreground">{standard.title}</span><span className="font-mono text-faint">score {score}</span></div>
                <p className="mt-1 text-muted">{reasons.join(" · ")}</p>
              </div>
            ))}
          </div>
        )}
      </section>
    </div>
  );
}
