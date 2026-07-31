import { useEffect, useMemo, useState } from 'react';
import {
  AlertTriangle,
  Bug,
  ExternalLink,
  FilePlus2,
  GitBranch,
  Globe,
  Paperclip,
  Play,
  Square,
  SquareTerminal,
  Trash2,
  X,
} from 'lucide-react';

import {
  createProductionEvidence,
  type DebugCommandExecution,
  type DebugConfidence,
  type ProductionEvidenceKind,
} from '../../lib/productionDebugging';
import { useT } from '../../lib/i18n';
import { useBrowserWorkbenchStore } from '../../store/browserWorkbenchStore';
import {
  useProductionDebuggingStore,
  type ProductionDebugCaseStatus,
} from '../../store/productionDebuggingStore';
import { buildTerminalEvidence, useTerminalStore } from '../../store/terminalStore';
import { Button, IconButton, StatusPill, type PillTone } from '../ui';
import { errorMessage } from "../../lib/errors";
import { statusTone as sharedStatusTone } from "../../lib/statusTone";

interface ProductionDebuggingPanelProps {
  onClose: () => void;
  onOpenRunCapsule?: (runId: string) => void;
}

const EVIDENCE_KINDS: readonly ProductionEvidenceKind[] = [
  'log', 'trace', 'error', 'release', 'commit', 'deploy', 'code',
];

const FIELD_CLASS = 'mt-1 w-full rounded-md border border-border bg-background px-2.5 py-2 text-sm text-foreground outline-none focus:border-accent focus:ring-1 focus:ring-accent';

function statusTone(status: ProductionDebugCaseStatus): PillTone {
  return sharedStatusTone(status, {
    fix_prepared: 'success',
    diagnosed: 'success',
    diagnosing: 'warning',
    creating_worktree: 'warning',
    fixing: 'warning',
  });
}

function commandTone(status: DebugCommandExecution['status']): PillTone {
  if (status === 'passed') return 'success';
  if (status === 'failed') return 'danger';
  if (status === 'inconclusive') return 'warning';
  return 'neutral';
}

function confidenceTone(confidence: DebugConfidence): PillTone {
  if (confidence === 'high') return 'danger';
  if (confidence === 'medium') return 'warning';
  return 'neutral';
}

function CommandEvidence({
  title,
  execution,
}: {
  title: string;
  execution: DebugCommandExecution;
}) {
  const { t } = useT();
  return (
    <div className="rounded-md border border-border bg-background p-3">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <h5 className="text-xs font-semibold text-foreground">{title}</h5>
        <StatusPill tone={commandTone(execution.status)}>
          {t(`ProductionDebug.commandStatus.${execution.status}`)}
        </StatusPill>
      </div>
      {execution.command && <pre className="mt-2 overflow-x-auto text-[11px] text-muted">$ {execution.command}</pre>}
      {execution.outputExcerpt && (
        <pre className="mt-2 max-h-48 overflow-auto whitespace-pre-wrap break-words rounded bg-surface-2 p-2 text-[10px] text-muted">
          {execution.outputExcerpt}
        </pre>
      )}
    </div>
  );
}

export function ProductionDebuggingPanel({ onClose, onOpenRunCapsule }: ProductionDebuggingPanelProps) {
  const { t } = useT();
  const store = useProductionDebuggingStore();
  const terminalSessions = useTerminalStore((state) => state.sessions);
  const browserEvidenceBySession = useBrowserWorkbenchStore((state) => state.pendingBySession);
  const [newTitle, setNewTitle] = useState('');
  const [newDescription, setNewDescription] = useState('');
  const [newRepository, setNewRepository] = useState('');
  const [evidenceKind, setEvidenceKind] = useState<ProductionEvidenceKind>('log');
  const [evidenceLabel, setEvidenceLabel] = useState('');
  const [pastedEvidence, setPastedEvidence] = useState('');
  const [workspacePath, setWorkspacePath] = useState('');
  const [actionError, setActionError] = useState<string | null>(null);

  useEffect(() => {
    useProductionDebuggingStore.getState().init();
  }, []);

  const selected = useMemo(
    () => store.cases.find((item) => item.id === store.selectedCaseId) ?? null,
    [store.cases, store.selectedCaseId],
  );
  const browserEvidence = useMemo(
    () => [...new Map(Object.values(browserEvidenceBySession).map((item) => [item.id, item])).values()],
    [browserEvidenceBySession],
  );
  const busy = selected
    ? selected.status === 'diagnosing' || selected.status === 'creating_worktree' || selected.status === 'fixing'
    : false;
  const currentActivity = selected ? store.activityByCase[selected.id] : null;

  const perform = async (action: () => void | Promise<void>) => {
    setActionError(null);
    try {
      await action();
    } catch (error) {
      setActionError(errorMessage(error));
    }
  };

  const createCase = () => {
    void perform(() => {
      store.createCase({ title: newTitle, description: newDescription, repositorySlug: newRepository });
      setNewTitle('');
      setNewDescription('');
      setNewRepository('');
    });
  };

  const attachTerminal = (sessionId: string) => {
    if (!selected) return;
    const session = terminalSessions.find((item) => item.id === sessionId);
    if (!session) return;
    const terminal = buildTerminalEvidence(session);
    store.attachEvidence(selected.id, createProductionEvidence({
      id: terminal.id,
      kind: 'terminal',
      origin: 'terminal',
      label: terminal.label,
      sourceUri: terminal.path,
      content: terminal.content,
    }));
  };

  const attachBrowser = (evidenceId: string) => {
    if (!selected) return;
    const browser = browserEvidence.find((item) => item.id === evidenceId);
    if (!browser) return;
    store.attachEvidence(selected.id, createProductionEvidence({
      id: browser.id,
      kind: 'browser',
      origin: 'browser',
      label: 'Browser workbench evidence',
      sourceUri: browser.screenshot?.path ?? `browser://${browser.id}`,
      content: browser.summary,
    }));
  };

  return (
    <section className="flex h-full min-h-0 flex-col" aria-labelledby="production-debug-title">
      <header className="flex shrink-0 items-start justify-between gap-3 border-b border-border px-5 py-4">
        <div>
          <div className="flex items-center gap-2">
            <Bug size={16} className="text-accent" />
            <h2 id="production-debug-title" className="text-sm font-semibold text-foreground">
              {t('ProductionDebug.title')}
            </h2>
          </div>
          <p className="mt-1 max-w-3xl text-xs leading-5 text-muted">{t('ProductionDebug.subtitle')}</p>
        </div>
        <IconButton size="sm" aria-label={t('ProductionDebug.close')} title={t('ProductionDebug.close')} onClick={onClose}>
          <X size={15} />
        </IconButton>
      </header>

      <div className="grid min-h-0 flex-1 gap-4 overflow-hidden p-5 xl:grid-cols-[minmax(17rem,.75fr)_minmax(0,1.7fr)]">
        <aside className="min-h-0 overflow-y-auto rounded-lg border border-border bg-surface p-3">
          <h3 className="text-xs font-semibold text-foreground">{t('ProductionDebug.newTitle')}</h3>
          <label className="mt-3 block text-xs text-muted">
            {t('ProductionDebug.titleLabel')}
            <input className={FIELD_CLASS} value={newTitle} placeholder={t('ProductionDebug.titlePlaceholder')} onChange={(event) => setNewTitle(event.target.value)} />
          </label>
          <label className="mt-2 block text-xs text-muted">
            {t('ProductionDebug.descriptionLabel')}
            <textarea className={`${FIELD_CLASS} min-h-20 resize-y`} value={newDescription} placeholder={t('ProductionDebug.descriptionPlaceholder')} onChange={(event) => setNewDescription(event.target.value)} />
          </label>
          <label className="mt-2 block text-xs text-muted">
            {t('ProductionDebug.repositoryLabel')}
            <input className={FIELD_CLASS} value={newRepository} placeholder={t('ProductionDebug.repositoryPlaceholder')} onChange={(event) => setNewRepository(event.target.value)} />
          </label>
          <Button className="mt-3 w-full" variant="primary" disabled={!newTitle.trim()} onClick={createCase}>
            <FilePlus2 size={14} /> {t('ProductionDebug.create')}
          </Button>

          <h3 className="mt-5 text-xs font-semibold text-foreground">{t('ProductionDebug.cases')}</h3>
          <div className="mt-2 space-y-1.5">
            {store.cases.length === 0 && (
              <p className="rounded-md border border-dashed border-border p-4 text-center text-xs text-faint">
                {t('ProductionDebug.emptyCases')}
              </p>
            )}
            {store.cases.map((debugCase) => (
              <div key={debugCase.id} className={`rounded-md border ${debugCase.id === selected?.id ? 'border-accent bg-accent/10' : 'border-border bg-background'}`}>
                <button type="button" className="w-full p-2.5 text-left" onClick={() => store.selectCase(debugCase.id)}>
                  <p className="truncate text-xs font-medium text-foreground">{debugCase.title}</p>
                  <div className="mt-1.5"><StatusPill tone={statusTone(debugCase.status)}>{t(`ProductionDebug.status.${debugCase.status}`)}</StatusPill></div>
                </button>
                <button type="button" className="flex w-full items-center justify-center gap-1 border-t border-border px-2 py-1.5 text-[10px] text-faint hover:text-danger" onClick={() => store.deleteCase(debugCase.id)}>
                  <Trash2 size={11} /> {t('ProductionDebug.deleteCase')}
                </button>
              </div>
            ))}
          </div>
        </aside>

        <main className="min-h-0 overflow-y-auto rounded-lg border border-border bg-surface p-4">
          {!selected ? (
            <p className="p-10 text-center text-xs text-faint">{t('ProductionDebug.emptyCases')}</p>
          ) : (
            <div className="space-y-5">
              <div className="flex flex-wrap items-start justify-between gap-2">
                <div>
                  <h3 className="text-sm font-semibold text-foreground">{selected.title}</h3>
                  <p className="mt-1 text-[11px] text-faint">{new Date(selected.updatedAtMs).toLocaleString()}</p>
                </div>
                <StatusPill tone={statusTone(selected.status)}>{t(`ProductionDebug.status.${selected.status}`)}</StatusPill>
              </div>

              {(actionError || selected.error) && (
                <div role="alert" className="rounded-md border border-danger/40 bg-danger/10 p-3 text-xs text-danger">
                  <p className="flex items-center gap-1.5 font-medium"><AlertTriangle size={13} /> {actionError ?? selected.error}</p>
                </div>
              )}

              <div className="grid gap-3 lg:grid-cols-2">
                <label className="text-xs text-muted lg:col-span-2">
                  {t('ProductionDebug.titleLabel')}
                  <input disabled={busy} className={FIELD_CLASS} value={selected.title} onChange={(event) => store.updateCase(selected.id, { title: event.target.value })} />
                </label>
                <label className="text-xs text-muted lg:col-span-2">
                  {t('ProductionDebug.descriptionLabel')}
                  <textarea disabled={busy} className={`${FIELD_CLASS} min-h-20 resize-y`} value={selected.description} onChange={(event) => store.updateCase(selected.id, { description: event.target.value })} />
                </label>
                <label className="text-xs text-muted">
                  {t('ProductionDebug.repositoryLabel')}
                  <input disabled={busy} className={FIELD_CLASS} value={selected.repositorySlug} placeholder={t('ProductionDebug.repositoryPlaceholder')} onChange={(event) => store.updateCase(selected.id, { repositorySlug: event.target.value })} />
                </label>
              </div>

              <section>
                <h4 className="text-xs font-semibold text-foreground">{t('ProductionDebug.evidence')}</h4>
                <p className="mt-1 text-[11px] leading-5 text-muted">{t('ProductionDebug.evidenceHint')}</p>
                <div className="mt-3 grid gap-2 lg:grid-cols-[10rem_minmax(0,1fr)]">
                  <label className="text-xs text-muted">
                    {t('ProductionDebug.kindLabel')}
                    <select className={FIELD_CLASS} value={evidenceKind} onChange={(event) => setEvidenceKind(event.target.value as ProductionEvidenceKind)}>
                      {EVIDENCE_KINDS.map((kind) => <option key={kind} value={kind}>{t(`ProductionDebug.kind.${kind}`)}</option>)}
                    </select>
                  </label>
                  <label className="text-xs text-muted">
                    {t('ProductionDebug.evidenceLabel')}
                    <input className={FIELD_CLASS} value={evidenceLabel} placeholder={t('ProductionDebug.evidenceLabelPlaceholder')} onChange={(event) => setEvidenceLabel(event.target.value)} />
                  </label>
                  <textarea className={`${FIELD_CLASS} min-h-24 resize-y lg:col-span-2`} value={pastedEvidence} placeholder={t('ProductionDebug.pastePlaceholder')} onChange={(event) => setPastedEvidence(event.target.value)} />
                </div>
                <div className="mt-2 flex flex-wrap gap-2">
                  <Button size="sm" disabled={busy || !pastedEvidence.trim()} onClick={() => void perform(() => {
                    store.addPastedEvidence(selected.id, evidenceKind, evidenceLabel, pastedEvidence);
                    setPastedEvidence('');
                    setEvidenceLabel('');
                  })}>
                    <Paperclip size={13} /> {t('ProductionDebug.addPasted')}
                  </Button>
                </div>

                <div className="mt-3 flex flex-wrap items-end gap-2">
                  <label className="min-w-64 flex-1 text-xs text-muted">
                    {t('ProductionDebug.workspacePath')}
                    <input className={FIELD_CLASS} value={workspacePath} placeholder={t('ProductionDebug.workspacePathPlaceholder')} onChange={(event) => setWorkspacePath(event.target.value)} />
                  </label>
                  <Button size="sm" disabled={busy || !workspacePath.trim()} onClick={() => void perform(() => {
                    store.addWorkspaceEvidence(selected.id, evidenceKind, workspacePath);
                    setWorkspacePath('');
                  })}>
                    <FilePlus2 size={13} /> {t('ProductionDebug.addWorkspace')}
                  </Button>
                </div>

                {(terminalSessions.length > 0 || browserEvidence.length > 0) && (
                  <div className="mt-3 flex flex-wrap gap-2">
                    {terminalSessions.map((session) => (
                      <Button key={session.id} size="sm" disabled={busy} onClick={() => attachTerminal(session.id)}>
                        <SquareTerminal size={13} /> {t('ProductionDebug.attachTerminal', { label: session.workspace_path.split(/[\\/]/).pop() ?? session.id })}
                      </Button>
                    ))}
                    {browserEvidence.map((evidence) => (
                      <Button key={evidence.id} size="sm" disabled={busy} onClick={() => attachBrowser(evidence.id)}>
                        <Globe size={13} /> {t('ProductionDebug.attachBrowser')}
                      </Button>
                    ))}
                  </div>
                )}

                <div className="mt-3 space-y-2">
                  {selected.evidence.map((item) => (
                    <article id={`production-debug-evidence-${item.id}`} key={item.id} className="rounded-md border border-border bg-background p-3">
                      <div className="flex flex-wrap items-start justify-between gap-2">
                        <div>
                          <p className="text-xs font-medium text-foreground">{item.label}</p>
                          <p className="mt-1 break-all font-mono text-[10px] text-faint">{item.sourceUri}</p>
                        </div>
                        <div className="flex items-center gap-1.5">
                          <StatusPill>{t(`ProductionDebug.kind.${item.kind}`)}</StatusPill>
                          {item.truncated && <StatusPill tone="warning">{t('ProductionDebug.truncated')}</StatusPill>}
                          <IconButton size="sm" aria-label={t('ProductionDebug.removeEvidence')} title={t('ProductionDebug.removeEvidence')} disabled={busy} onClick={() => store.removeEvidence(selected.id, item.id)}>
                            <Trash2 size={13} />
                          </IconButton>
                        </div>
                      </div>
                      <pre className="mt-2 max-h-32 overflow-auto whitespace-pre-wrap break-words text-[10px] text-muted">{item.content}</pre>
                    </article>
                  ))}
                </div>
              </section>

              <section>
                <h4 className="text-xs font-semibold text-foreground">{t('ProductionDebug.commands')}</h4>
                <p className="mt-1 text-[11px] leading-5 text-muted">{t('ProductionDebug.commandsHint')}</p>
                <div className="mt-2 grid gap-3 lg:grid-cols-2">
                  <label className="text-xs text-muted">
                    {t('ProductionDebug.reproductionCommand')}
                    <textarea disabled={busy} className={`${FIELD_CLASS} min-h-20 resize-y font-mono text-xs`} value={selected.reproductionCommand} placeholder={t('ProductionDebug.reproductionPlaceholder')} onChange={(event) => store.updateCase(selected.id, { reproductionCommand: event.target.value })} />
                  </label>
                  <label className="text-xs text-muted">
                    {t('ProductionDebug.verificationCommand')}
                    <textarea disabled={busy} className={`${FIELD_CLASS} min-h-20 resize-y font-mono text-xs`} value={selected.verificationCommand} placeholder={t('ProductionDebug.verificationPlaceholder')} onChange={(event) => store.updateCase(selected.id, { verificationCommand: event.target.value })} />
                  </label>
                </div>
              </section>

              <div className="flex flex-wrap gap-2">
                {!busy && (
                  <Button variant="primary" disabled={selected.evidence.length === 0 && !selected.reproductionCommand.trim()} onClick={() => void perform(() => store.diagnose(selected.id))}>
                    <Play size={14} /> {t('ProductionDebug.diagnose')}
                  </Button>
                )}
                {busy && (
                  <Button variant="danger" onClick={() => store.cancel(selected.id)}>
                    <Square size={13} /> {t('ProductionDebug.cancel')}
                  </Button>
                )}
                {selected.report && !busy && selected.status !== 'fix_prepared' && (
                  <Button disabled={!selected.repositorySlug.trim()} onClick={() => void perform(() => store.prepareFix(selected.id))}>
                    <GitBranch size={14} /> {t('ProductionDebug.prepareFix')}
                  </Button>
                )}
              </div>
              {currentActivity && <p className="text-[11px] text-accent">{t('ProductionDebug.activity', { activity: currentActivity })}</p>}

              {selected.worktree && (
                <dl className="grid grid-cols-[7rem_minmax(0,1fr)] gap-x-3 gap-y-1 rounded-md border border-border bg-background p-3 text-[11px]">
                  <dt className="text-faint">{t('ProductionDebug.branch')}</dt><dd className="break-all font-mono text-foreground">{selected.worktree.branch}</dd>
                  <dt className="text-faint">{t('ProductionDebug.worktree')}</dt><dd className="break-all font-mono text-foreground">{selected.worktree.canonicalPath}</dd>
                </dl>
              )}

              {selected.report && (
                <section className="space-y-4 border-t border-border pt-4">
                  <div>
                    <h4 className="text-sm font-semibold text-foreground">{t('ProductionDebug.report')}</h4>
                    <p className="mt-1 text-xs leading-5 text-muted">{selected.report.summary}</p>
                  </div>

                  <div>
                    <h5 className="text-xs font-semibold text-foreground">{t('ProductionDebug.rootCauses')}</h5>
                    <div className="mt-2 space-y-2">
                      {selected.report.rootCauses.map((cause) => (
                        <article key={cause.rank} className="rounded-md border border-border bg-background p-3">
                          <div className="flex flex-wrap items-center gap-2">
                            <span className="text-xs font-bold text-accent">#{cause.rank}</span>
                            <p className="flex-1 text-xs font-medium text-foreground">{cause.cause}</p>
                            <StatusPill tone={confidenceTone(cause.confidence)}>{t(`ProductionDebug.confidence.${cause.confidence}`)}</StatusPill>
                          </div>
                          <p className="mt-2 text-[11px] leading-5 text-muted">{cause.reasoning}</p>
                          <div className="mt-2 flex flex-wrap gap-1.5">
                            {cause.evidenceIds.map((id) => (
                              <a key={id} href={`#production-debug-evidence-${id}`} className="rounded bg-surface-2 px-2 py-1 font-mono text-[10px] text-accent hover:underline">{id}</a>
                            ))}
                          </div>
                        </article>
                      ))}
                    </div>
                  </div>

                  <div>
                    <h5 className="text-xs font-semibold text-foreground">{t('ProductionDebug.evidenceLinks')}</h5>
                    <ul className="mt-2 space-y-1 text-[11px]">
                      {selected.report.evidenceLinks.map((link) => (
                        <li key={link.evidenceId}>
                          <a href={`#production-debug-evidence-${link.evidenceId}`} className="text-accent hover:underline">{link.label}</a>
                          <span className="ml-2 break-all font-mono text-faint">{link.sourceUri}</span>
                        </li>
                      ))}
                    </ul>
                  </div>

                  <div className="grid gap-3 lg:grid-cols-2">
                    <CommandEvidence title={t('ProductionDebug.reproduction')} execution={selected.report.reproduction} />
                    <CommandEvidence title={t('ProductionDebug.verification')} execution={selected.report.verification} />
                  </div>

                  <div className="rounded-md border border-border bg-background p-3">
                    <h5 className="text-xs font-semibold text-foreground">{t('ProductionDebug.patch')}</h5>
                    <p className="mt-2 text-[11px] leading-5 text-muted">{selected.report.proposedPatch.summary}</p>
                    {selected.report.proposedPatch.files.length > 0 && (
                      <p className="mt-2 break-all font-mono text-[10px] text-faint">{t('ProductionDebug.files')}: {selected.report.proposedPatch.files.join(', ')}</p>
                    )}
                    {selected.report.proposedPatch.diff ? (
                      <pre className="mt-3 max-h-96 overflow-auto whitespace-pre text-[10px] text-muted">{selected.report.proposedPatch.diff}</pre>
                    ) : <p className="mt-2 text-[11px] text-faint">{t('ProductionDebug.noDiff')}</p>}
                  </div>

                  {selected.fixSummary && (
                    <div className="rounded-md border border-border bg-background p-3">
                      <h5 className="text-xs font-semibold text-foreground">{t('ProductionDebug.fixSummary')}</h5>
                      <p className="mt-2 whitespace-pre-wrap text-[11px] leading-5 text-muted">{selected.fixSummary}</p>
                    </div>
                  )}

                  <div>
                    <h5 className="text-xs font-semibold text-foreground">{t('ProductionDebug.unresolvedRisks')}</h5>
                    {selected.report.unresolvedRisks.length === 0 ? (
                      <p className="mt-2 text-[11px] text-faint">{t('ProductionDebug.noRisks')}</p>
                    ) : (
                      <ul className="mt-2 list-disc space-y-1 pl-5 text-[11px] leading-5 text-muted">
                        {selected.report.unresolvedRisks.map((risk) => <li key={risk}>{risk}</li>)}
                      </ul>
                    )}
                  </div>

                  {onOpenRunCapsule && (
                    <div className="flex flex-wrap gap-2">
                      {[...new Set([
                        selected.report.diagnosisDurableRunId,
                        selected.report.reproduction.durableRunId,
                        selected.report.fixDurableRunId,
                        selected.report.verificationDurableRunId,
                      ].filter((id): id is string => Boolean(id)))].map((runId) => (
                        <Button key={runId} size="sm" onClick={() => onOpenRunCapsule(runId)}>
                          <ExternalLink size={13} /> {t('ProductionDebug.viewCapsule')}
                        </Button>
                      ))}
                    </div>
                  )}
                </section>
              )}

              <p className="rounded-md border border-warning/30 bg-warning/5 p-3 text-[11px] leading-5 text-muted">
                {t('ProductionDebug.localOnly')}
              </p>
            </div>
          )}
        </main>
      </div>
    </section>
  );
}

export default ProductionDebuggingPanel;
