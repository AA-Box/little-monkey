import { useEffect, useMemo, useRef, useState } from "react";
import { AlertTriangle, GitBranch, Loader2, RefreshCw, Search, X } from "lucide-react";

import { useT } from "../../lib/i18n";
import { toMermaidFlowchart, type EvidenceSpan } from "../../lib/knowledgeGraph";
import { useKnowledgeGraphStore } from "../../store/knowledgeGraphStore";
import { useStackStore } from "../../store/stackStore";
import { useSessionStore } from "../../store/sessionStore";
import { Button, IconButton } from "../ui";
import { errorMessage } from "../../lib/errors";

interface KnowledgeGraphExplorerPanelProps {
  onClose: () => void;
}

/** Renders one Mermaid `flowchart` diagram string into an SVG, re-rendering
 * whenever `diagram` changes. Mermaid is dynamically imported (it's a large
 * dependency already used only where actually needed elsewhere in the app)
 * and each render uses a fresh element id — mermaid's own render cache
 * otherwise reuses a stale SVG for an id it has seen before. */
function MermaidDiagram({ diagram }: { diagram: string }) {
  const containerRef = useRef<HTMLDivElement>(null);
  const [error, setError] = useState<string | null>(null);
  const renderId = useRef(0);

  useEffect(() => {
    let cancelled = false;
    renderId.current += 1;
    const id = `knowledge-graph-diagram-${renderId.current}`;

    void (async () => {
      try {
        const mod = await import("mermaid");
        const mermaid = mod.default;
        mermaid.initialize({
          startOnLoad: false,
          theme: window.matchMedia?.("(prefers-color-scheme: dark)").matches ? "dark" : "default",
          securityLevel: "strict",
        });
        const { svg } = await mermaid.render(id, diagram);
        if (!cancelled && containerRef.current) {
          containerRef.current.innerHTML = svg;
          setError(null);
        }
      } catch (err) {
        if (!cancelled) setError(errorMessage(err));
      }
    })();

    return () => {
      cancelled = true;
    };
  }, [diagram]);

  if (error) {
    return <p className="p-4 text-xs text-danger">{error}</p>;
  }
  return <div ref={containerRef} className="min-w-0 [&_svg]:h-auto [&_svg]:max-w-full" />;
}

function EvidenceCard({ evidence }: { evidence: EvidenceSpan }) {
  return (
    <div className="rounded-md border border-border bg-background p-2.5 text-[11px]">
      <p className="font-medium text-foreground">{evidence.sourceLabel}</p>
      <p className="mt-1 whitespace-pre-wrap break-words text-muted">“{evidence.quote}”</p>
      <p className="mt-1 truncate font-mono text-faint">{evidence.locator}</p>
    </div>
  );
}

export function KnowledgeGraphExplorerPanel({ onClose }: KnowledgeGraphExplorerPanelProps) {
  const { t } = useT();
  const store = useKnowledgeGraphStore();
  const stacks = useStackStore((s) => s.stacks);
  const refreshStacks = useStackStore((s) => s.refresh);
  const activeSessionTitle = useSessionStore((s) => s.sessions.find((session) => session.id === s.activeSessionId)?.title ?? null);

  const [selectedStackIds, setSelectedStackIds] = useState<string[]>([]);
  const [includeSession, setIncludeSession] = useState(true);
  const [queryInput, setQueryInput] = useState("");

  useEffect(() => {
    void refreshStacks();
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  const toggleStack = (id: string) => {
    setSelectedStackIds((current) => (current.includes(id) ? current.filter((x) => x !== id) : [...current, id]));
  };

  const highlightEdgeIds = useMemo(() => (store.queryResult?.path ?? []).map((edge) => edge.id), [store.queryResult]);
  const diagram = useMemo(() => toMermaidFlowchart({ nodes: store.nodes, edges: store.edges }, highlightEdgeIds), [store.nodes, store.edges, highlightEdgeIds]);

  return (
    <section className="flex h-full min-h-0 flex-col" aria-labelledby="knowledge-graph-title">
      <header className="flex shrink-0 items-start justify-between gap-3 border-b border-border px-5 py-4">
        <div>
          <h2 id="knowledge-graph-title" className="text-sm font-semibold text-foreground">
            {t("KnowledgeGraphExplorer.title")}
          </h2>
          <p className="mt-1 max-w-2xl text-xs leading-5 text-muted">{t("KnowledgeGraphExplorer.subtitle")}</p>
        </div>
        <IconButton size="sm" aria-label={t("KnowledgeGraphExplorer.close")} title={t("KnowledgeGraphExplorer.close")} onClick={onClose}>
          <X size={15} />
        </IconButton>
      </header>

      <div className="flex shrink-0 flex-wrap items-center gap-2 border-b border-border px-5 py-3">
        <span className="text-xs font-medium text-muted">{t("KnowledgeGraphExplorer.sourcesLabel")}</span>
        {stacks.length === 0 ? (
          <span className="text-xs text-faint">{t("KnowledgeGraphExplorer.noStacks")}</span>
        ) : (
          stacks.map((stack) => (
            <button
              key={stack.id}
              type="button"
              onClick={() => toggleStack(stack.id)}
              className={`rounded-full border px-2.5 py-1 text-[11px] transition-colors ${
                selectedStackIds.length === 0 || selectedStackIds.includes(stack.id)
                  ? "border-accent bg-accent/10 text-foreground"
                  : "border-border bg-background text-faint hover:border-border-strong"
              }`}
            >
              {stack.name}
            </button>
          ))
        )}
        {activeSessionTitle && (
          <label className="ml-2 flex items-center gap-1.5 text-[11px] text-muted">
            <input type="checkbox" checked={includeSession} onChange={(event) => setIncludeSession(event.target.checked)} />
            {t("KnowledgeGraphExplorer.includeSession", { title: activeSessionTitle })}
          </label>
        )}
        <Button
          size="sm"
          variant="primary"
          className="ml-auto"
          disabled={store.building}
          onClick={() =>
            void store.build({
              stackIds: selectedStackIds.length > 0 ? selectedStackIds : undefined,
              includeActiveSession: includeSession,
            })
          }
        >
          {store.building ? <Loader2 className="animate-spin" size={13} /> : <RefreshCw size={13} />}
          {t("KnowledgeGraphExplorer.buildButton")}
        </Button>
      </div>

      {store.buildError && (
        <div role="alert" className="mx-5 mt-3 flex items-start gap-2 rounded-md border border-danger/40 bg-danger/10 p-3 text-xs text-danger">
          <AlertTriangle size={13} className="mt-0.5 shrink-0" />
          <span>{store.buildError}</span>
        </div>
      )}
      {store.batchErrors.length > 0 && (
        <div className="mx-5 mt-3 rounded-md border border-warning/40 bg-warning/5 p-3 text-[11px] text-warning">
          <p className="font-medium">{t("KnowledgeGraphExplorer.partialBuildHeading")}</p>
          <ul className="mt-1 list-disc space-y-0.5 pl-4">
            {store.batchErrors.map((message, index) => (
              <li key={index}>{message}</li>
            ))}
          </ul>
        </div>
      )}

      <form
        className="flex shrink-0 flex-wrap items-end gap-2 border-b border-border px-5 py-3"
        onSubmit={(event) => {
          event.preventDefault();
          if (!queryInput.trim()) return;
          store.queryRelation(queryInput.trim());
        }}
      >
        <label className="min-w-64 flex-1 text-xs text-muted">
          {t("KnowledgeGraphExplorer.queryLabel")}
          <input
            className="mt-1 w-full rounded-md border border-border bg-background px-2.5 py-2 text-sm text-foreground outline-none focus:border-accent focus:ring-1 focus:ring-accent"
            placeholder={t("KnowledgeGraphExplorer.queryPlaceholder")}
            value={queryInput}
            onChange={(event) => setQueryInput(event.target.value)}
          />
        </label>
        <Button type="submit" variant="primary" disabled={!queryInput.trim()}>
          <Search size={13} /> {t("KnowledgeGraphExplorer.askButton")}
        </Button>
      </form>

      <div className="grid min-h-0 flex-1 gap-4 overflow-hidden p-5 xl:grid-cols-[minmax(0,1.4fr)_minmax(16rem,1fr)]">
        <div className="min-h-0 overflow-auto rounded-lg border border-border bg-surface p-4">
          {store.nodes.length === 0 && !store.building ? (
            <div className="flex h-full flex-col items-center justify-center gap-2 text-center text-xs text-faint">
              <GitBranch size={20} />
              <p>{t("KnowledgeGraphExplorer.emptyGraph")}</p>
            </div>
          ) : (
            <MermaidDiagram diagram={diagram} />
          )}
        </div>

        <div className="min-h-0 overflow-y-auto rounded-lg border border-border bg-surface p-3">
          <h3 className="text-xs font-semibold text-foreground">{t("KnowledgeGraphExplorer.evidenceHeading")}</h3>
          {!store.queryResult ? (
            <p className="mt-3 rounded-md border border-dashed border-border p-4 text-center text-[11px] text-faint">
              {t("KnowledgeGraphExplorer.evidenceEmpty")}
            </p>
          ) : store.queryResult.error ? (
            <p className="mt-3 rounded-md border border-danger/40 bg-danger/10 p-3 text-xs text-danger">{store.queryResult.error}</p>
          ) : (
            <div className="mt-3 space-y-3">
              <p className="text-xs leading-5 text-foreground">{store.queryResult.explanation}</p>
              {store.queryResult.evidence.length === 0 ? (
                <p className="text-[11px] text-faint">{t("KnowledgeGraphExplorer.noEvidence")}</p>
              ) : (
                <div className="space-y-2">
                  {store.queryResult.evidence.map((evidence, index) => (
                    <EvidenceCard key={index} evidence={evidence} />
                  ))}
                </div>
              )}
            </div>
          )}
        </div>
      </div>
    </section>
  );
}

export default KnowledgeGraphExplorerPanel;
