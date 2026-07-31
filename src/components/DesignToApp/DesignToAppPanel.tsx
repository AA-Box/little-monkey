import { useEffect, useMemo, useRef, useState } from 'react';
import {
  Accessibility,
  AlertTriangle,
  Camera,
  CheckCircle2,
  Code2,
  Download,
  FileJson2,
  FileUp,
  GitBranch,
  Globe2,
  Image as ImageIcon,
  Layers3,
  Play,
  RefreshCw,
  Square,
  Trash2,
  X,
} from 'lucide-react';

import {
  createLocalDesignSource,
  createReferenceDesignSource,
  exportDesignProjectJson,
  exportDesignProjectMarkdown,
  validateDesignSources,
  type DesignBrowserEvidence,
  type DesignSourceKind,
} from '../../lib/designToApp';
import { artifactDataUrl, readDurableArtifact } from '../../lib/durableArtifacts';
import { useT } from '../../lib/i18n';
import {
  useDesignToAppStore,
  type DesignToAppProject,
  type DesignToAppStatus,
} from '../../store/designToAppStore';
import { useVerifyStore } from '../../store/verifyStore';
import { Button, IconButton, StatusPill, type PillTone } from '../ui';
import { errorMessage } from "../../lib/errors";
import { statusTone as sharedStatusTone } from "../../lib/statusTone";

interface DesignToAppPanelProps {
  onClose: () => void;
  onOpenRunCapsule?: (runId: string) => void;
}

const FIELD_CLASS = 'mt-1 w-full rounded-md border border-border bg-background px-2.5 py-2 text-sm text-foreground outline-none transition-colors focus:border-accent focus:ring-1 focus:ring-accent';
const SOURCE_KINDS: readonly Exclude<DesignSourceKind, 'reference_url'>[] = [
  'screenshot',
  'sketch',
  'figma_export',
  'design_tokens',
];

const RUNNING_STATUSES: ReadonlySet<DesignToAppStatus> = new Set([
  'planning',
  'capturing_before',
  'creating_worktree',
  'implementing',
  'capturing_after',
]);

function statusTone(status: DesignToAppStatus): PillTone {
  if (RUNNING_STATUSES.has(status)) return 'warning';
  return sharedStatusTone(status, { planned: 'success' });
}

function evidenceTone(evidence: DesignBrowserEvidence | null): PillTone {
  if (evidence?.status === 'captured') return evidence.accessibilityIssues.length ? 'warning' : 'success';
  if (evidence?.status === 'unavailable') return 'danger';
  return 'neutral';
}

function fileDataUrl(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(String(reader.result));
    reader.onerror = () => reject(reader.error ?? new Error(`Could not read ${file.name}.`));
    reader.readAsDataURL(file);
  });
}

function downloadText(filename: string, content: string, mediaType: string): void {
  const url = URL.createObjectURL(new Blob([content], { type: mediaType }));
  const anchor = document.createElement('a');
  anchor.href = url;
  anchor.download = filename;
  anchor.click();
  URL.revokeObjectURL(url);
}

function safeFilename(value: string): string {
  return value.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '').slice(0, 60) || 'design-to-app';
}

function VisualDiff({ beforeId, afterId }: { beforeId: string | null; afterId: string | null }) {
  const { t } = useT();
  const [beforeUrl, setBeforeUrl] = useState<string | null>(null);
  const [afterUrl, setAfterUrl] = useState<string | null>(null);
  const [opacity, setOpacity] = useState(55);

  useEffect(() => {
    let cancelled = false;
    setBeforeUrl(null);
    setAfterUrl(null);
    if (!beforeId || !afterId) return () => { cancelled = true; };
    void Promise.all([readDurableArtifact(beforeId), readDurableArtifact(afterId)])
      .then(([before, after]) => {
        if (cancelled) return;
        setBeforeUrl(artifactDataUrl('image/png', before.contentBase64));
        setAfterUrl(artifactDataUrl('image/png', after.contentBase64));
      })
      .catch(() => {
        if (!cancelled) {
          setBeforeUrl(null);
          setAfterUrl(null);
        }
      });
    return () => { cancelled = true; };
  }, [afterId, beforeId]);

  if (!beforeId || !afterId) {
    return <p className="text-xs text-faint">{t('DesignToApp.visualDiffNeedsBoth')}</p>;
  }
  return (
    <div>
      <div className="relative mt-2 min-h-64 overflow-hidden rounded-lg border border-border bg-black/80">
        {beforeUrl && <img src={beforeUrl} alt={t('DesignToApp.beforeAlt')} className="absolute inset-0 h-full w-full object-contain" />}
        {afterUrl && (
          <img
            src={afterUrl}
            alt={t('DesignToApp.afterAlt')}
            style={{ opacity: opacity / 100, mixBlendMode: 'difference' }}
            className="absolute inset-0 h-full w-full object-contain"
          />
        )}
        {!beforeUrl && !afterUrl && (
          <div className="flex min-h-64 items-center justify-center px-4 text-center text-xs text-faint">
            {t('DesignToApp.artifactsUnavailable')}
          </div>
        )}
      </div>
      <label className="mt-2 block text-xs text-muted">
        {t('DesignToApp.diffOpacity', { value: opacity })}
        <input
          type="range"
          min="0"
          max="100"
          value={opacity}
          onChange={(event) => setOpacity(Number(event.target.value))}
          className="mt-1 w-full cursor-pointer accent-accent"
        />
      </label>
    </div>
  );
}

function EvidenceCard({
  evidence,
  phase,
  busy,
  onCapture,
}: {
  evidence: DesignBrowserEvidence | null;
  phase: 'before' | 'after';
  busy: boolean;
  onCapture: () => void;
}) {
  const { t } = useT();
  return (
    <div className="rounded-lg border border-border bg-background p-3">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div>
          <h4 className="text-xs font-semibold text-foreground">{t(`DesignToApp.evidence.${phase}`)}</h4>
          <p className="mt-1 text-[11px] text-faint">{evidence?.url ?? t('DesignToApp.noPreviewUrl')}</p>
        </div>
        <StatusPill tone={evidenceTone(evidence)}>
          {t(`DesignToApp.evidenceStatus.${evidence?.status ?? 'not_requested'}`)}
        </StatusPill>
      </div>
      {evidence?.status === 'captured' && (
        <p className="mt-2 text-xs text-muted">
          {t('DesignToApp.accessibilityIssueCount', { count: evidence.accessibilityIssues.length })}
        </p>
      )}
      {evidence?.error && <p role="alert" className="mt-2 text-xs text-danger">{evidence.error}</p>}
      <Button size="sm" className="mt-3" disabled={busy} onClick={onCapture}>
        <Camera size={13} /> {t('DesignToApp.captureAgain')}
      </Button>
    </div>
  );
}

function ExportButtons({ project }: { project: DesignToAppProject }) {
  const { t } = useT();
  const name = safeFilename(project.title);
  return (
    <div className="flex flex-wrap gap-2">
      <Button
        size="sm"
        onClick={() => downloadText(`${name}.design-to-app.json`, exportDesignProjectJson(project), 'application/json')}
      >
        <Download size={13} /> {t('DesignToApp.exportJson')}
      </Button>
      <Button
        size="sm"
        onClick={() => downloadText(`${name}.design-to-app.md`, exportDesignProjectMarkdown(project), 'text/markdown')}
      >
        <Download size={13} /> {t('DesignToApp.exportMarkdown')}
      </Button>
    </div>
  );
}

export function DesignToAppPanel({ onClose, onOpenRunCapsule }: DesignToAppPanelProps) {
  const { t } = useT();
  const store = useDesignToAppStore();
  const verifyConfig = useVerifyStore((state) => state.config);
  const refreshVerify = useVerifyStore((state) => state.refresh);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const [newTitle, setNewTitle] = useState('');
  const [newDescription, setNewDescription] = useState('');
  const [newRepository, setNewRepository] = useState('');
  const [sourceKind, setSourceKind] = useState<Exclude<DesignSourceKind, 'reference_url'>>('screenshot');
  const [payloadName, setPayloadName] = useState('');
  const [payloadText, setPayloadText] = useState('');
  const [referenceUrl, setReferenceUrl] = useState('');
  const [actionError, setActionError] = useState<string | null>(null);
  const [draftTitle, setDraftTitle] = useState('');
  const [draftDescription, setDraftDescription] = useState('');
  const [draftRepository, setDraftRepository] = useState('');
  const [draftPreviewUrl, setDraftPreviewUrl] = useState('');

  useEffect(() => {
    useDesignToAppStore.getState().init();
    void refreshVerify();
  }, [refreshVerify]);

  const selected = useMemo(
    () => store.projects.find((project) => project.id === store.selectedProjectId) ?? null,
    [store.projects, store.selectedProjectId],
  );

  useEffect(() => {
    setDraftTitle(selected?.title ?? '');
    setDraftDescription(selected?.description ?? '');
    setDraftRepository(selected?.repositorySlug ?? '');
    setDraftPreviewUrl(selected?.previewUrl ?? '');
    setActionError(null);
  }, [selected?.id]);

  const busy = selected ? RUNNING_STATUSES.has(selected.status) : false;
  const activity = selected ? store.activityByProject[selected.id] : null;
  const enabledVerifyCommands = verifyConfig.commands.filter((command) => command.enabled);
  const sourceErrors = selected ? validateDesignSources(selected.sources) : [];

  const perform = async (action: () => void | Promise<void>): Promise<void> => {
    setActionError(null);
    try {
      await action();
    } catch (error) {
      setActionError(errorMessage(error));
    }
  };

  const commitDraft = (): void => {
    if (!selected) return;
    store.updateProject(selected.id, {
      title: draftTitle,
      description: draftDescription,
      repositorySlug: draftRepository,
      previewUrl: draftPreviewUrl,
    });
  };

  const handleFiles = async (files: FileList | null): Promise<void> => {
    if (!selected || !files) return;
    for (const file of Array.from(files)) {
      const isImage = file.type.startsWith('image/') || /\.(png|jpe?g|gif|webp)$/i.test(file.name);
      if (isImage && sourceKind === 'design_tokens') {
        throw new Error(t('DesignToApp.tokensNeedJson'));
      }
      if (!isImage && (sourceKind === 'screenshot' || sourceKind === 'sketch')) {
        throw new Error(t('DesignToApp.imageKindNeedsImage'));
      }
      const source = isImage
        ? createLocalDesignSource({
            kind: sourceKind,
            name: file.name,
            mediaType: file.type || 'image/png',
            sourceUri: `local-file://${encodeURIComponent(file.name)}`,
            imageDataUrl: await fileDataUrl(file),
          })
        : createLocalDesignSource({
            kind: sourceKind,
            name: file.name,
            mediaType: file.type || 'application/json',
            sourceUri: `local-file://${encodeURIComponent(file.name)}`,
            textContent: await file.text(),
          });
      const missing = selected.sources.find(
        (candidate) => candidate.availability === 'needs_reimport' && candidate.name === file.name,
      );
      if (missing) store.replaceSource(selected.id, missing.id, source);
      else store.addSource(selected.id, source);
    }
    if (fileInputRef.current) fileInputRef.current.value = '';
  };

  const addPastedPayload = (): void => {
    if (!selected) return;
    if (sourceKind === 'screenshot' || sourceKind === 'sketch') {
      throw new Error(t('DesignToApp.pasteOnlyStructured'));
    }
    const source = createLocalDesignSource({
      kind: sourceKind,
      name: payloadName.trim() || (sourceKind === 'figma_export' ? 'Figma export.json' : 'Design tokens.json'),
      mediaType: 'application/json',
      sourceUri: `paste://${selected.id}/${Date.now()}`,
      textContent: payloadText,
    });
    store.addSource(selected.id, source);
    setPayloadName('');
    setPayloadText('');
  };

  const addReference = (): void => {
    if (!selected) return;
    store.addSource(selected.id, createReferenceDesignSource({ url: referenceUrl }));
    setReferenceUrl('');
  };

  const toggleVerify = (commandId: string): void => {
    if (!selected) return;
    const ids = selected.verificationCommandIds.includes(commandId)
      ? selected.verificationCommandIds.filter((id) => id !== commandId)
      : [...selected.verificationCommandIds, commandId];
    store.updateProject(selected.id, { verificationCommandIds: ids });
  };

  return (
    <section className="flex min-h-0 flex-1 flex-col bg-background" aria-labelledby="design-to-app-title">
      <header className="flex items-start justify-between gap-3 border-b border-border px-5 py-4">
        <div className="min-w-0">
          <div className="flex items-center gap-2">
            <Layers3 size={18} className="text-accent" />
            <h2 id="design-to-app-title" className="text-sm font-semibold text-foreground">{t('DesignToApp.title')}</h2>
          </div>
          <p className="mt-1 max-w-3xl text-xs text-muted">{t('DesignToApp.subtitle')}</p>
        </div>
        <IconButton size="sm" aria-label={t('DesignToApp.close')} onClick={onClose}><X size={16} /></IconButton>
      </header>

      <div className="grid min-h-0 flex-1 grid-cols-1 overflow-hidden lg:grid-cols-[18rem_minmax(0,1fr)]">
        <aside className="overflow-y-auto border-b border-border bg-surface p-4 lg:border-b-0 lg:border-r">
          <h3 className="text-xs font-semibold uppercase tracking-wide text-muted">{t('DesignToApp.newProject')}</h3>
          <label className="mt-3 block text-xs text-muted">
            {t('DesignToApp.projectTitle')}
            <input value={newTitle} onChange={(event) => setNewTitle(event.target.value)} className={FIELD_CLASS} placeholder={t('DesignToApp.projectTitlePlaceholder')} />
          </label>
          <label className="mt-3 block text-xs text-muted">
            {t('DesignToApp.description')}
            <textarea value={newDescription} onChange={(event) => setNewDescription(event.target.value)} className={`${FIELD_CLASS} min-h-20 resize-y`} placeholder={t('DesignToApp.descriptionPlaceholder')} />
          </label>
          <label className="mt-3 block text-xs text-muted">
            {t('DesignToApp.repository')}
            <input value={newRepository} onChange={(event) => setNewRepository(event.target.value)} className={FIELD_CLASS} placeholder="owner/repository" />
          </label>
          <Button
            variant="primary"
            className="mt-3 w-full"
            onClick={() => void perform(() => {
              store.createProject({ title: newTitle, description: newDescription, repositorySlug: newRepository });
              setNewTitle(''); setNewDescription(''); setNewRepository('');
            })}
          >
            <Layers3 size={14} /> {t('DesignToApp.createProject')}
          </Button>

          <h3 className="mt-6 text-xs font-semibold uppercase tracking-wide text-muted">{t('DesignToApp.history')}</h3>
          <div className="mt-2 space-y-2">
            {store.projects.length === 0 && <p className="text-xs text-faint">{t('DesignToApp.emptyHistory')}</p>}
            {store.projects.map((project) => (
              <button
                key={project.id}
                type="button"
                onClick={() => store.selectProject(project.id)}
                className={`w-full cursor-pointer rounded-lg border p-3 text-left transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent ${project.id === selected?.id ? 'border-accent bg-accent-soft' : 'border-border bg-background hover:border-border-strong'}`}
              >
                <div className="flex items-start justify-between gap-2">
                  <span className="line-clamp-2 text-xs font-medium text-foreground">{project.title}</span>
                  <StatusPill tone={statusTone(project.status)}>{t(`DesignToApp.status.${project.status}`)}</StatusPill>
                </div>
                <p className="mt-2 text-[10px] text-faint">{new Date(project.updatedAtMs).toLocaleString()}</p>
              </button>
            ))}
          </div>
        </aside>

        <main className="min-h-0 overflow-y-auto p-4 sm:p-5">
          {!selected && (
            <div className="flex min-h-72 items-center justify-center rounded-xl border border-dashed border-border px-6 text-center">
              <div><Layers3 size={28} className="mx-auto text-faint" /><p className="mt-3 text-sm font-medium">{t('DesignToApp.selectProject')}</p></div>
            </div>
          )}
          {selected && (
            <div className="mx-auto max-w-6xl space-y-4">
              {(actionError || selected.error) && (
                <div role="alert" className="flex items-start gap-2 rounded-lg border border-danger/40 bg-danger-soft p-3 text-xs text-danger">
                  <AlertTriangle size={15} className="mt-0.5 shrink-0" />
                  <span className="flex-1">{actionError || selected.error}</span>
                  <button type="button" className="cursor-pointer underline" onClick={() => { setActionError(null); store.clearError(selected.id); }}>{t('DesignToApp.dismiss')}</button>
                </div>
              )}
              {activity && <div aria-live="polite" className="rounded-md border border-warning/40 bg-warning-soft px-3 py-2 text-xs text-warning">{t('DesignToApp.activity', { activity })}</div>}

              <section className="rounded-xl border border-border bg-surface p-4">
                <div className="flex flex-wrap items-start justify-between gap-3">
                  <div><h3 className="text-sm font-semibold">{t('DesignToApp.project')}</h3><p className="mt-1 text-xs text-muted">{t('DesignToApp.projectHint')}</p></div>
                  <div className="flex items-center gap-2"><StatusPill tone={statusTone(selected.status)}>{t(`DesignToApp.status.${selected.status}`)}</StatusPill><IconButton size="sm" aria-label={t('DesignToApp.deleteProject')} disabled={busy} onClick={() => store.deleteProject(selected.id)}><Trash2 size={14} /></IconButton></div>
                </div>
                <div className="mt-3 grid gap-3 md:grid-cols-2">
                  <label className="text-xs text-muted">{t('DesignToApp.projectTitle')}<input value={draftTitle} onChange={(event) => setDraftTitle(event.target.value)} onBlur={() => void perform(commitDraft)} className={FIELD_CLASS} /></label>
                  <label className="text-xs text-muted">{t('DesignToApp.repository')}<input value={draftRepository} disabled={Boolean(selected.worktree)} onChange={(event) => setDraftRepository(event.target.value)} onBlur={() => void perform(commitDraft)} className={FIELD_CLASS} /></label>
                  <label className="text-xs text-muted md:col-span-2">{t('DesignToApp.description')}<textarea value={draftDescription} onChange={(event) => setDraftDescription(event.target.value)} onBlur={() => void perform(commitDraft)} className={`${FIELD_CLASS} min-h-20 resize-y`} /></label>
                  <label className="text-xs text-muted md:col-span-2">{t('DesignToApp.previewUrl')}<input value={draftPreviewUrl} onChange={(event) => setDraftPreviewUrl(event.target.value)} onBlur={() => void perform(commitDraft)} className={FIELD_CLASS} placeholder="http://localhost:5173" /><span className="mt-1 block text-[11px] text-faint">{t('DesignToApp.previewUrlHint')}</span></label>
                </div>
                {selected.worktree && <div className="mt-3 flex flex-wrap gap-2 text-xs text-muted"><span className="inline-flex items-center gap-1"><GitBranch size={13} />{selected.worktree.branch}</span><code className="break-all">{selected.worktree.canonicalPath}</code></div>}
              </section>

              <section className="rounded-xl border border-border bg-surface p-4">
                <div className="flex flex-wrap items-start justify-between gap-3"><div><h3 className="text-sm font-semibold">{t('DesignToApp.sources')}</h3><p className="mt-1 text-xs text-muted">{t('DesignToApp.sourcesHint')}</p></div><StatusPill tone={sourceErrors.length ? 'warning' : selected.sources.length ? 'success' : 'neutral'}>{t('DesignToApp.sourceCount', { count: selected.sources.length })}</StatusPill></div>
                <div className="mt-3 grid gap-3 md:grid-cols-[12rem_1fr]">
                  <label className="text-xs text-muted">{t('DesignToApp.sourceKind')}<select value={sourceKind} onChange={(event) => setSourceKind(event.target.value as Exclude<DesignSourceKind, 'reference_url'>)} className={FIELD_CLASS}>{SOURCE_KINDS.map((kind) => <option key={kind} value={kind}>{t(`DesignToApp.sourceKind.${kind}`)}</option>)}</select></label>
                  <div className="flex items-end gap-2"><input ref={fileInputRef} type="file" multiple accept="image/png,image/jpeg,image/gif,image/webp,application/json,.json" className="sr-only" onChange={(event) => void perform(() => handleFiles(event.target.files))} /><Button className="w-full md:w-auto" onClick={() => fileInputRef.current?.click()}><FileUp size={14} />{t('DesignToApp.importFiles')}</Button></div>
                </div>
                <div className="mt-3 grid gap-3 md:grid-cols-2">
                  <div className="rounded-lg border border-border bg-background p-3"><h4 className="text-xs font-semibold">{t('DesignToApp.pastePayload')}</h4><input value={payloadName} onChange={(event) => setPayloadName(event.target.value)} className={FIELD_CLASS} placeholder={t('DesignToApp.payloadNamePlaceholder')} /><textarea value={payloadText} onChange={(event) => setPayloadText(event.target.value)} className={`${FIELD_CLASS} min-h-28 resize-y font-mono text-xs`} placeholder={t('DesignToApp.payloadPlaceholder')} /><Button size="sm" className="mt-2" disabled={busy || !payloadText.trim()} onClick={() => void perform(addPastedPayload)}><FileJson2 size={13} />{t('DesignToApp.addPayload')}</Button></div>
                  <div className="rounded-lg border border-border bg-background p-3"><h4 className="text-xs font-semibold">{t('DesignToApp.referenceUrl')}</h4><p className="mt-1 text-[11px] text-faint">{t('DesignToApp.referenceUrlHint')}</p><input value={referenceUrl} onChange={(event) => setReferenceUrl(event.target.value)} className={FIELD_CLASS} placeholder="https://example.com/reference" /><Button size="sm" className="mt-2" disabled={busy || !referenceUrl.trim()} onClick={() => void perform(addReference)}><Globe2 size={13} />{t('DesignToApp.addReference')}</Button></div>
                </div>
                <div className="mt-3 space-y-2">
                  {selected.sources.map((source) => (
                    <div key={source.id} className="rounded-lg border border-border bg-background p-3">
                      <div className="flex items-start gap-3">{source.imageDataUrl ? <img src={source.imageDataUrl} alt={source.name} className="h-16 w-20 shrink-0 rounded border border-border object-cover" /> : source.kind === 'reference_url' ? <Globe2 size={20} className="mt-1 shrink-0 text-accent" /> : <FileJson2 size={20} className="mt-1 shrink-0 text-accent" />}<div className="min-w-0 flex-1"><div className="flex flex-wrap items-center gap-2"><span className="truncate text-xs font-medium">{source.name}</span><StatusPill tone={source.availability === 'ready' || source.availability === 'reference_only' ? 'success' : 'warning'}>{t(`DesignToApp.sourceAvailability.${source.availability}`)}</StatusPill></div><p className="mt-1 break-all font-mono text-[10px] text-faint">{source.id} · {source.sourceUri}</p>{source.warnings.map((warning) => <p key={warning} className="mt-1 text-[11px] text-warning">{warning}</p>)}</div><IconButton size="sm" aria-label={t('DesignToApp.removeSource')} disabled={busy} onClick={() => store.removeSource(selected.id, source.id)}><Trash2 size={13} /></IconButton></div>
                    </div>
                  ))}
                </div>
                {sourceErrors.length > 0 && <ul role="alert" className="mt-3 list-disc space-y-1 pl-5 text-xs text-warning">{sourceErrors.map((error) => <li key={error}>{error}</li>)}</ul>}
              </section>

              <section className="rounded-xl border border-border bg-surface p-4">
                <div className="flex flex-wrap items-start justify-between gap-3"><div><h3 className="text-sm font-semibold">{t('DesignToApp.plan')}</h3><p className="mt-1 text-xs text-muted">{t('DesignToApp.planHint')}</p></div><Button variant="primary" disabled={busy || sourceErrors.length > 0} onClick={() => void perform(() => { commitDraft(); return store.analyze(selected.id); })}>{busy && selected.status === 'planning' ? <RefreshCw size={14} className="animate-spin motion-reduce:animate-none" /> : <Code2 size={14} />}{t(selected.plan ? 'DesignToApp.replan' : 'DesignToApp.analyze')}</Button></div>
                {!selected.plan && <p className="mt-4 text-xs text-faint">{t('DesignToApp.noPlan')}</p>}
                {selected.plan && <div className="mt-4 space-y-3"><p className="text-sm text-foreground">{selected.plan.summary}</p><div className="grid gap-3 md:grid-cols-2"><div className="rounded-lg border border-border bg-background p-3"><h4 className="text-xs font-semibold">{t('DesignToApp.routes')}</h4><ul className="mt-2 space-y-2">{selected.plan.routes.map((route) => <li key={route.routeId} className="text-xs"><code className="text-accent">{route.path}</code><p className="mt-1 text-muted">{route.purpose}</p><p className="mt-1 font-mono text-[10px] text-faint">{route.sourceIds.join(', ')}</p></li>)}</ul></div><div className="rounded-lg border border-border bg-background p-3"><h4 className="text-xs font-semibold">{t('DesignToApp.steps')}</h4><ol className="mt-2 space-y-2">{selected.plan.steps.map((step, index) => <li key={step.stepId} className="text-xs"><span className="font-medium">{index + 1}. {step.title}</span><p className="mt-1 text-muted">{step.details}</p><p className="mt-1 font-mono text-[10px] text-faint">{step.sourceIds.join(', ')}</p></li>)}</ol></div></div>{selected.plan.accessibilityChecklist.length > 0 && <div className="rounded-lg border border-border bg-background p-3"><h4 className="flex items-center gap-2 text-xs font-semibold"><Accessibility size={14} />{t('DesignToApp.accessibilityPlan')}</h4><ul className="mt-2 list-disc space-y-1 pl-5 text-xs text-muted">{selected.plan.accessibilityChecklist.map((item) => <li key={item}>{item}</li>)}</ul></div>}{selected.plan.durableRunId && onOpenRunCapsule && <Button size="sm" onClick={() => onOpenRunCapsule(selected.plan!.durableRunId!)}>{t('DesignToApp.openPlanCapsule')}</Button>}</div>}
              </section>

              <section className="rounded-xl border border-border bg-surface p-4">
                <div className="flex flex-wrap items-start justify-between gap-3"><div><h3 className="text-sm font-semibold">{t('DesignToApp.run')}</h3><p className="mt-1 text-xs text-muted">{t('DesignToApp.runHint')}</p></div>{busy ? <Button variant="danger" onClick={() => store.cancel(selected.id)}><Square size={13} />{t('DesignToApp.cancel')}</Button> : <Button variant="primary" disabled={!selected.plan || sourceErrors.length > 0} onClick={() => void perform(() => { commitDraft(); return store.run(selected.id); })}><Play size={14} />{t('DesignToApp.runButton')}</Button>}</div>
                <div className="mt-3 rounded-lg border border-border bg-background p-3"><h4 className="text-xs font-semibold">{t('DesignToApp.verificationCommands')}</h4><p className="mt-1 text-[11px] text-faint">{t('DesignToApp.verificationHint')}</p>{enabledVerifyCommands.length === 0 ? <p className="mt-2 text-xs text-warning">{t('DesignToApp.noVerificationCommands')}</p> : <div className="mt-2 grid gap-2 md:grid-cols-2">{enabledVerifyCommands.map((command) => <label key={command.id} className="flex cursor-pointer items-start gap-2 rounded-md border border-border p-2 text-xs"><input type="checkbox" checked={selected.verificationCommandIds.includes(command.id)} disabled={busy} onChange={() => toggleVerify(command.id)} className="mt-0.5 accent-accent" /><span><span className="font-medium">{command.label || command.command}</span><code className="mt-1 block break-all text-[10px] text-faint">{command.command}</code></span></label>)}</div>}</div>
                {selected.implementationSummary && <div className="mt-3 rounded-lg border border-border bg-background p-3"><h4 className="text-xs font-semibold">{t('DesignToApp.implementationSummary')}</h4><p className="mt-2 whitespace-pre-wrap text-xs text-muted">{selected.implementationSummary}</p></div>}
                {selected.patch && <div className="mt-3 rounded-lg border border-border bg-background p-3"><div className="flex items-center justify-between gap-2"><h4 className="text-xs font-semibold">{t('DesignToApp.patch')}</h4><span className="text-[10px] text-faint">{t('DesignToApp.fileCount', { count: selected.patch.files.length })}</span></div>{selected.patch.diff ? <pre className="mt-2 max-h-96 overflow-auto whitespace-pre-wrap break-words rounded bg-surface-2 p-2 text-[10px] text-muted">{selected.patch.diff}</pre> : <p className="mt-2 text-xs text-warning">{t('DesignToApp.noDiff')}</p>}</div>}
                {selected.verification.length > 0 && <div className="mt-3 space-y-2">{selected.verification.map((result) => <div key={result.commandId} className="rounded-lg border border-border bg-background p-3"><div className="flex flex-wrap items-center justify-between gap-2"><span className="text-xs font-medium">{result.label}</span><StatusPill tone={result.status === 'passed' ? 'success' : result.status === 'failed' ? 'danger' : 'warning'}>{t(`DesignToApp.checkStatus.${result.status}`)}</StatusPill></div>{result.output && <pre className="mt-2 max-h-40 overflow-auto whitespace-pre-wrap break-words text-[10px] text-muted">{result.output}</pre>}{result.durableRunId && onOpenRunCapsule && <Button size="sm" className="mt-2" onClick={() => onOpenRunCapsule(result.durableRunId!)}>{t('DesignToApp.openCheckCapsule')}</Button>}</div>)}</div>}
              </section>

              <section className="rounded-xl border border-border bg-surface p-4">
                <div><h3 className="text-sm font-semibold">{t('DesignToApp.evidence')}</h3><p className="mt-1 text-xs text-muted">{t('DesignToApp.evidenceHint')}</p></div>
                <div className="mt-3 grid gap-3 md:grid-cols-2"><EvidenceCard evidence={selected.beforeEvidence} phase="before" busy={busy} onCapture={() => void perform(() => { commitDraft(); return store.captureEvidence(selected.id, 'before'); })} /><EvidenceCard evidence={selected.afterEvidence} phase="after" busy={busy} onCapture={() => void perform(() => { commitDraft(); return store.captureEvidence(selected.id, 'after'); })} /></div>
                <div className="mt-3 rounded-lg border border-border bg-background p-3"><h4 className="flex items-center gap-2 text-xs font-semibold"><ImageIcon size={14} />{t('DesignToApp.visualDiff')}</h4><VisualDiff beforeId={selected.beforeEvidence?.screenshotArtifactId ?? null} afterId={selected.afterEvidence?.screenshotArtifactId ?? null} /></div>
                {(selected.beforeEvidence?.accessibilityIssues.length || selected.afterEvidence?.accessibilityIssues.length) ? <div className="mt-3 rounded-lg border border-warning/40 bg-warning-soft p-3"><h4 className="flex items-center gap-2 text-xs font-semibold text-warning"><AlertTriangle size={14} />{t('DesignToApp.accessibilityFindings')}</h4><ul className="mt-2 list-disc space-y-1 pl-5 text-xs text-muted">{[...(selected.beforeEvidence?.accessibilityIssues ?? []), ...(selected.afterEvidence?.accessibilityIssues ?? [])].slice(0, 20).map((issue, index) => <li key={`${issue}-${index}`}>{issue}</li>)}</ul></div> : selected.afterEvidence?.status === 'captured' ? <div className="mt-3 flex items-center gap-2 rounded-lg border border-success/30 bg-success-soft p-3 text-xs text-success"><CheckCircle2 size={14} />{t('DesignToApp.noBaselineIssues')}</div> : null}
              </section>

              <section className="rounded-xl border border-border bg-surface p-4"><div className="flex flex-wrap items-center justify-between gap-3"><div><h3 className="text-sm font-semibold">{t('DesignToApp.export')}</h3><p className="mt-1 text-xs text-muted">{t('DesignToApp.exportHint')}</p></div><ExportButtons project={selected} /></div></section>
            </div>
          )}
        </main>
      </div>
    </section>
  );
}

export default DesignToAppPanel;
