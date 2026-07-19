import { useEffect, useMemo, useRef, useState } from "react";
import {
  Activity,
  Archive,
  BookOpen,
  CalendarDays,
  FileText,
  Filter,
  Globe,
  ListChecks,
  Loader2,
  MessageCircle,
  MessageSquare,
  Plug,
  Search,
  ShieldCheck,
  Users,
  X,
} from "lucide-react";

import { globalProfileSearch, type GlobalSearchHit, type SearchSourceKind } from "../../lib/profileSearch";
import type { UniversalSearchSourceKind } from "../../lib/universalSearch";
import { useT } from "../../lib/i18n";
import { useSessionStore } from "../../store/sessionStore";
import { useUniversalSearchStore } from "../../store/universalSearchStore";
import { IconButton, StatusPill } from "../ui";

interface GlobalSearchProps {
  onClose: () => void;
  onOpenRun: (runId: string) => void;
}

/** Every group `GlobalSearch` can render, in display order: the existing
 * backend-indexed profile search kinds first, then the client-side
 * universal-search fan-out's kinds (workspace files, run/task summaries,
 * knowledge stacks, browser evidence, connected apps). */
type CombinedSourceKind = SearchSourceKind | UniversalSearchSourceKind;

const SOURCE_KIND_ORDER: CombinedSourceKind[] = [
  "message",
  "actor_transcript",
  "run_event",
  "session",
  "task",
  "workspace_file",
  "knowledge",
  "browser_evidence",
  "connected_app",
];

const SOURCE_KIND_ICON: Record<CombinedSourceKind, typeof MessageCircle> = {
  message: MessageCircle,
  actor_transcript: Users,
  run_event: Activity,
  session: MessageSquare,
  task: ListChecks,
  workspace_file: FileText,
  knowledge: BookOpen,
  browser_evidence: Globe,
  connected_app: Plug,
};

/** A row shape both the backend profile-search hits and the client-side
 * universal-search hits normalize into, so the panel can render them in one
 * grouped list without branching on origin at render time. */
interface DisplayHit {
  key: string;
  sourceKind: CombinedSourceKind;
  title: string;
  snippet: string;
  occurredAtMs: number;
  modelKey: string | null;
  workspacePath: string | null;
  archived: boolean;
  onOpen: (() => void) | null;
}

function optional(value: string): string | null {
  return value.trim() || null;
}

function dateBoundary(value: string, end: boolean): number | null {
  if (!value) return null;
  const parsed = new Date(`${value}T${end ? "23:59:59.999" : "00:00:00.000"}`);
  return Number.isFinite(parsed.getTime()) ? parsed.getTime() : null;
}

function formatDate(value: number): string {
  return new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" }).format(value);
}

export function GlobalSearch({ onClose, onOpenRun }: GlobalSearchProps) {
  const { t } = useT();
  const switchSession = useSessionStore((state) => state.switchSession);
  const runUniversalSearch = useUniversalSearchStore((state) => state.run);
  const universalHits = useUniversalSearchStore((state) => state.hits);
  const universalExcludedCount = useUniversalSearchStore((state) => state.excludedCount);
  const universalLoading = useUniversalSearchStore((state) => state.loading);
  const [query, setQuery] = useState("");
  const [includeArchived, setIncludeArchived] = useState(false);
  const [filtersOpen, setFiltersOpen] = useState(false);
  const [fromDate, setFromDate] = useState("");
  const [toDate, setToDate] = useState("");
  const [modelKey, setModelKey] = useState("");
  const [personaId, setPersonaId] = useState("");
  const [workspacePath, setWorkspacePath] = useState("");
  const [results, setResults] = useState<GlobalSearchHit[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const requestSequence = useRef(0);
  const inputRef = useRef<HTMLInputElement>(null);

  const activeFilterCount = useMemo(
    () => [fromDate, toDate, modelKey, personaId, workspacePath].filter(Boolean).length + Number(includeArchived),
    [fromDate, includeArchived, modelKey, personaId, toDate, workspacePath],
  );

  useEffect(() => inputRef.current?.focus(), []);

  useEffect(() => {
    const sequence = ++requestSequence.current;
    const timer = window.setTimeout(async () => {
      if (!query.trim()) {
        setResults([]);
        setLoading(false);
        setError(null);
        useUniversalSearchStore.getState().clear();
        return;
      }
      setLoading(true);
      setError(null);
      const [profileOutcome] = await Promise.allSettled([
        globalProfileSearch({
          query,
          includeArchived,
          fromMs: dateBoundary(fromDate, false),
          toMs: dateBoundary(toDate, true),
          modelKey: optional(modelKey),
          personaId: optional(personaId),
          workspacePath: optional(workspacePath),
          limit: 100,
        }),
        runUniversalSearch(query, { includeArchived }),
      ]);
      if (sequence !== requestSequence.current) return;
      if (profileOutcome.status === "fulfilled") {
        setResults(profileOutcome.value);
        setError(null);
      } else {
        setResults([]);
        setError(errorText(profileOutcome.reason));
      }
      setLoading(false);
    }, 180);
    return () => window.clearTimeout(timer);
  }, [fromDate, includeArchived, modelKey, personaId, query, runUniversalSearch, toDate, workspacePath]);

  function openHit(hit: GlobalSearchHit) {
    if (hit.sessionId) {
      switchSession(hit.sessionId);
      onClose();
    } else if (hit.runId) {
      onOpenRun(hit.runId);
    }
  }

  const displayHits = useMemo<DisplayHit[]>(() => {
    const fromProfile: DisplayHit[] = results.map((hit) => ({
      key: `profile:${hit.documentId}`,
      sourceKind: hit.sourceKind,
      title: hit.title,
      snippet: hit.snippet,
      occurredAtMs: hit.occurredAtMs,
      modelKey: hit.modelKey,
      workspacePath: hit.workspacePath,
      archived: hit.archived,
      onOpen: hit.sessionId || hit.runId ? () => openHit(hit) : null,
    }));
    const fromUniversal: DisplayHit[] = universalHits.map((hit) => ({
      key: `universal:${hit.id}`,
      sourceKind: hit.sourceKind,
      title: hit.title,
      snippet: hit.snippet,
      occurredAtMs: hit.occurredAtMs,
      modelKey: null,
      workspacePath: hit.workspacePath,
      archived: hit.archived,
      onOpen: hit.sessionId
        ? () => {
            switchSession(hit.sessionId as string);
            onClose();
          }
        : hit.runId
          ? () => onOpenRun(hit.runId as string)
          : null,
    }));
    const combined = [...fromProfile, ...fromUniversal];
    const rank = new Map(SOURCE_KIND_ORDER.map((kind, index) => [kind, index]));
    return combined.sort((a, b) => {
      const rankDiff = (rank.get(a.sourceKind) ?? 99) - (rank.get(b.sourceKind) ?? 99);
      if (rankDiff !== 0) return rankDiff;
      return b.occurredAtMs - a.occurredAtMs;
    });
  }, [results, universalHits, switchSession, onClose, onOpenRun]);

  const groups = useMemo(() => {
    const bySource = new Map<CombinedSourceKind, DisplayHit[]>();
    for (const hit of displayHits) {
      const list = bySource.get(hit.sourceKind) ?? [];
      list.push(hit);
      bySource.set(hit.sourceKind, list);
    }
    return SOURCE_KIND_ORDER.map((kind) => ({ kind, hits: bySource.get(kind) ?? [] })).filter(
      (group) => group.hits.length > 0,
    );
  }, [displayHits]);

  const isLoading = loading || universalLoading;
  const hasAnyResults = displayHits.length > 0;

  return (
    <section className="flex min-h-0 flex-1 flex-col" aria-labelledby="global-search-title">
      <header className="shrink-0 border-b border-border px-4 py-3">
        <div className="flex items-center justify-between gap-3">
          <div>
            <h1 id="global-search-title" className="text-base font-semibold text-foreground">{t("GlobalSearch.title")}</h1>
            <p className="text-xs text-muted">{t("GlobalSearch.subtitle")}</p>
          </div>
          <IconButton size="sm" onClick={onClose} aria-label={t("GlobalSearch.close")}><X size={16} /></IconButton>
        </div>
        <div className="mt-3 flex items-center gap-2">
          <label className="flex min-w-0 flex-1 items-center gap-2 rounded-lg border border-border bg-surface px-3 focus-within:ring-2 focus-within:ring-accent">
            <Search size={16} className="shrink-0 text-faint" />
            <span className="sr-only">{t("GlobalSearch.queryLabel")}</span>
            <input
              ref={inputRef}
              value={query}
              onChange={(event) => setQuery(event.target.value)}
              className="h-10 min-w-0 flex-1 bg-transparent text-sm outline-none placeholder:text-faint"
              placeholder={t("GlobalSearch.placeholder")}
            />
            {isLoading && <Loader2 size={15} className="animate-spin text-faint" aria-label={t("GlobalSearch.loading")} />}
          </label>
          <button
            type="button"
            onClick={() => setFiltersOpen((open) => !open)}
            aria-expanded={filtersOpen}
            className="flex h-10 items-center gap-2 rounded-lg border border-border px-3 text-sm hover:bg-surface-2 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent"
          >
            <Filter size={15} /> {t("GlobalSearch.filters")}
            {activeFilterCount > 0 && <StatusPill tone="neutral">{activeFilterCount}</StatusPill>}
          </button>
        </div>
        {filtersOpen && (
          <div className="mt-3 grid gap-3 rounded-lg border border-border bg-surface p-3 md:grid-cols-3">
            <label className="text-xs text-muted"><span className="mb-1 flex items-center gap-1"><CalendarDays size={13} />{t("GlobalSearch.from")}</span><input type="date" value={fromDate} onChange={(event) => setFromDate(event.target.value)} className="w-full rounded border border-border bg-background px-2 py-1.5 text-foreground" /></label>
            <label className="text-xs text-muted"><span className="mb-1 block">{t("GlobalSearch.to")}</span><input type="date" value={toDate} onChange={(event) => setToDate(event.target.value)} className="w-full rounded border border-border bg-background px-2 py-1.5 text-foreground" /></label>
            <label className="text-xs text-muted"><span className="mb-1 block">{t("GlobalSearch.model")}</span><input value={modelKey} onChange={(event) => setModelKey(event.target.value)} placeholder={t("GlobalSearch.exactOptional")} className="w-full rounded border border-border bg-background px-2 py-1.5 text-foreground" /></label>
            <label className="text-xs text-muted"><span className="mb-1 block">{t("GlobalSearch.persona")}</span><input value={personaId} onChange={(event) => setPersonaId(event.target.value)} placeholder={t("GlobalSearch.exactOptional")} className="w-full rounded border border-border bg-background px-2 py-1.5 text-foreground" /></label>
            <label className="text-xs text-muted"><span className="mb-1 block">{t("GlobalSearch.workspace")}</span><input value={workspacePath} onChange={(event) => setWorkspacePath(event.target.value)} placeholder={t("GlobalSearch.exactOptional")} className="w-full rounded border border-border bg-background px-2 py-1.5 text-foreground" /></label>
            <label className="flex items-end gap-2 pb-1.5 text-xs text-foreground"><input type="checkbox" checked={includeArchived} onChange={(event) => setIncludeArchived(event.target.checked)} /><Archive size={13} />{t("GlobalSearch.includeArchived")}</label>
          </div>
        )}
        {universalExcludedCount > 0 && (
          <p className="mt-3 flex items-center gap-1.5 rounded-lg border border-border bg-surface px-3 py-2 text-xs text-muted">
            <ShieldCheck size={13} className="shrink-0 text-faint" />
            {t("GlobalSearch.accessFilteredNotice", { count: String(universalExcludedCount) })}
          </p>
        )}
      </header>

      <div className="min-h-0 flex-1 overflow-y-auto [overscroll-behavior:contain]">
        {error ? <p role="alert" className="m-4 rounded-lg border border-danger/30 bg-danger-soft p-3 text-sm text-danger">{error}</p>
          : !query.trim() ? <p className="p-8 text-center text-sm text-faint">{t("GlobalSearch.startHint")}</p>
            : !isLoading && !hasAnyResults ? <p className="p-8 text-center text-sm text-faint">{t("GlobalSearch.noResults")}</p>
              : <div aria-live="polite">
                  {groups.map((group) => {
                    const Icon = SOURCE_KIND_ICON[group.kind];
                    return (
                      <section key={group.kind}>
                        <h2 className="sticky top-0 z-10 flex items-center gap-1.5 border-b border-border bg-background/95 px-4 py-1.5 text-[11px] font-semibold uppercase tracking-wide text-faint backdrop-blur">
                          <Icon size={12} /> {t(`GlobalSearch.source.${group.kind}`)} <span className="font-normal normal-case text-faint">({group.hits.length})</span>
                        </h2>
                        <ul className="divide-y divide-border">
                          {group.hits.map((hit) => (
                            <li key={hit.key}>
                              {hit.onOpen ? (
                                <button
                                  type="button"
                                  onClick={hit.onOpen}
                                  className="w-full px-4 py-3 text-left hover:bg-surface-2 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-accent"
                                >
                                  <HitBody hit={hit} t={t} />
                                </button>
                              ) : (
                                <div className="px-4 py-3">
                                  <HitBody hit={hit} t={t} />
                                </div>
                              )}
                            </li>
                          ))}
                        </ul>
                      </section>
                    );
                  })}
                </div>}
      </div>
    </section>
  );
}

function HitBody({ hit, t }: { hit: DisplayHit; t: (key: string, vars?: Record<string, string>) => string }) {
  return (
    <>
      <div className="flex items-start justify-between gap-3">
        <span className="truncate text-sm font-medium text-foreground">{hit.title}</span>
        {hit.archived && <StatusPill tone="warning">{t("GlobalSearch.archived")}</StatusPill>}
      </div>
      <p className="mt-1 line-clamp-3 text-sm text-muted">{hit.snippet}</p>
      <div className="mt-2 flex flex-wrap gap-x-3 gap-y-1 text-[11px] text-faint">
        {hit.occurredAtMs > 0 && <time dateTime={new Date(hit.occurredAtMs).toISOString()}>{formatDate(hit.occurredAtMs)}</time>}
        {hit.modelKey && <span>{hit.modelKey}</span>}
        {hit.workspacePath && <span className="truncate">{hit.workspacePath}</span>}
      </div>
    </>
  );
}

function errorText(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  try {
    return JSON.stringify(error);
  } catch {
    return String(error);
  }
}
