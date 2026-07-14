import { useEffect, useMemo, useRef, useState } from "react";
import { Archive, CalendarDays, Filter, Loader2, Search, X } from "lucide-react";

import { globalProfileSearch, type GlobalSearchHit } from "../../lib/profileSearch";
import { useT } from "../../lib/i18n";
import { useSessionStore } from "../../store/sessionStore";
import { IconButton, StatusPill } from "../ui";

interface GlobalSearchProps {
  onClose: () => void;
  onOpenRun: (runId: string) => void;
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
        return;
      }
      setLoading(true);
      setError(null);
      try {
        const hits = await globalProfileSearch({
          query,
          includeArchived,
          fromMs: dateBoundary(fromDate, false),
          toMs: dateBoundary(toDate, true),
          modelKey: optional(modelKey),
          personaId: optional(personaId),
          workspacePath: optional(workspacePath),
          limit: 100,
        });
        if (sequence === requestSequence.current) setResults(hits);
      } catch (caught) {
        if (sequence === requestSequence.current) {
          setResults([]);
          setError(caught instanceof Error ? caught.message : String(caught));
        }
      } finally {
        if (sequence === requestSequence.current) setLoading(false);
      }
    }, 180);
    return () => window.clearTimeout(timer);
  }, [fromDate, includeArchived, modelKey, personaId, query, toDate, workspacePath]);

  function openHit(hit: GlobalSearchHit) {
    if (hit.sessionId) {
      switchSession(hit.sessionId);
      onClose();
    } else if (hit.runId) {
      onOpenRun(hit.runId);
    }
  }

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
            {loading && <Loader2 size={15} className="animate-spin text-faint" aria-label={t("GlobalSearch.loading")} />}
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
      </header>

      <div className="min-h-0 flex-1 overflow-y-auto [overscroll-behavior:contain]">
        {error ? <p role="alert" className="m-4 rounded-lg border border-danger/30 bg-danger-soft p-3 text-sm text-danger">{error}</p>
          : !query.trim() ? <p className="p-8 text-center text-sm text-faint">{t("GlobalSearch.startHint")}</p>
            : !loading && results.length === 0 ? <p className="p-8 text-center text-sm text-faint">{t("GlobalSearch.noResults")}</p>
              : <ul className="divide-y divide-border" aria-live="polite">{results.map((hit) => (
                <li key={hit.documentId}>
                  <button type="button" onClick={() => openHit(hit)} className="w-full px-4 py-3 text-left hover:bg-surface-2 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-accent">
                    <div className="flex items-start justify-between gap-3"><span className="truncate text-sm font-medium text-foreground">{hit.title}</span><div className="flex shrink-0 items-center gap-2"><StatusPill tone="neutral">{t(`GlobalSearch.source.${hit.sourceKind}`)}</StatusPill>{hit.archived && <StatusPill tone="warning">{t("GlobalSearch.archived")}</StatusPill>}</div></div>
                    <p className="mt-1 line-clamp-3 text-sm text-muted">{hit.snippet}</p>
                    <div className="mt-2 flex flex-wrap gap-x-3 gap-y-1 text-[11px] text-faint"><span>{hit.role}</span><time dateTime={new Date(hit.occurredAtMs).toISOString()}>{formatDate(hit.occurredAtMs)}</time>{hit.modelKey && <span>{hit.modelKey}</span>}{hit.workspacePath && <span className="truncate">{hit.workspacePath}</span>}</div>
                  </button>
                </li>
              ))}</ul>}
      </div>
    </section>
  );
}

