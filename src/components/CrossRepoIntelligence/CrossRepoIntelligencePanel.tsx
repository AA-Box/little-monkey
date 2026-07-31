import { useMemo, useState } from "react";
import { Database, Loader2, RefreshCw, Search, X } from "lucide-react";

import { useT } from "../../lib/i18n";
import { useCrossRepoIndexStore } from "../../store/crossRepoIndexStore";
import { primaryRoot, useWorkspaceStore } from "../../store/workspaceStore";
import { IconButton, StatusPill } from "../ui";
import type { SymbolKind } from "../../lib/crossRepoIndex";

interface CrossRepoIntelligencePanelProps {
  onClose: () => void;
}

const KIND_KEY: Record<SymbolKind, string> = {
  function: "CrossRepoIntelligencePanel.kind.function",
  method: "CrossRepoIntelligencePanel.kind.method",
  class: "CrossRepoIntelligencePanel.kind.class",
  interface: "CrossRepoIntelligencePanel.kind.interface",
  type: "CrossRepoIntelligencePanel.kind.type",
  const: "CrossRepoIntelligencePanel.kind.const",
  enum: "CrossRepoIntelligencePanel.kind.enum",
  struct: "CrossRepoIntelligencePanel.kind.struct",
  trait: "CrossRepoIntelligencePanel.kind.trait",
};

interface SymbolGroup {
  name: string;
  kinds: SymbolKind[];
  locationCount: number;
}

function Section({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <section className="rounded-lg border border-border bg-background p-4">
      <h2 className="text-sm font-semibold text-foreground">{title}</h2>
      <div className="mt-2 flex flex-col gap-2">{children}</div>
    </section>
  );
}

export function CrossRepoIntelligencePanel({ onClose }: CrossRepoIntelligencePanelProps) {
  const { t } = useT();
  const roots = useWorkspaceStore((s) => s.roots);
  const hasWorkspace = primaryRoot(roots) !== null;

  const status = useCrossRepoIndexStore((s) => s.status);
  const symbols = useCrossRepoIndexStore((s) => s.symbols);
  const builtAtMs = useCrossRepoIndexStore((s) => s.builtAtMs);
  const files = useCrossRepoIndexStore((s) => s.files);
  const buildError = useCrossRepoIndexStore((s) => s.error);
  const rebuild = useCrossRepoIndexStore((s) => s.rebuild);

  const impact = useCrossRepoIndexStore((s) => s.impact);
  const impactLoading = useCrossRepoIndexStore((s) => s.impactLoading);
  const impactError = useCrossRepoIndexStore((s) => s.impactError);
  const runImpactQuery = useCrossRepoIndexStore((s) => s.runImpactQuery);
  const clearImpact = useCrossRepoIndexStore((s) => s.clearImpact);

  const [query, setQuery] = useState("");

  const groups = useMemo<SymbolGroup[]>(() => {
    const byName = new Map<string, SymbolGroup>();
    for (const symbol of symbols) {
      const existing = byName.get(symbol.name);
      if (existing) {
        if (!existing.kinds.includes(symbol.kind)) existing.kinds.push(symbol.kind);
        existing.locationCount += 1;
      } else {
        byName.set(symbol.name, { name: symbol.name, kinds: [symbol.kind], locationCount: 1 });
      }
    }
    return [...byName.values()].sort((a, b) => a.name.localeCompare(b.name));
  }, [symbols]);

  const filteredGroups = useMemo(() => {
    const trimmed = query.trim().toLowerCase();
    if (!trimmed) return groups;
    return groups.filter((group) => group.name.toLowerCase().includes(trimmed));
  }, [groups, query]);

  const building = status === "building";

  return (
    <section className="flex min-h-0 flex-1 flex-col" aria-labelledby="cross-repo-intelligence-title">
      <header className="flex shrink-0 items-center justify-between gap-3 border-b border-border px-4 py-3">
        <div className="min-w-0">
          <h1 id="cross-repo-intelligence-title" className="text-base font-semibold text-foreground">
            {t("CrossRepoIntelligencePanel.title")}
          </h1>
          <p className="truncate text-xs text-muted">{t("CrossRepoIntelligencePanel.subtitle")}</p>
        </div>
        <div className="flex shrink-0 items-center gap-2">
          <button
            type="button"
            onClick={() => void rebuild()}
            disabled={building || !hasWorkspace}
            className="flex items-center gap-1.5 rounded-md border border-border px-2.5 py-1.5 text-xs font-medium text-foreground hover:bg-surface-2 disabled:cursor-not-allowed disabled:opacity-50"
          >
            <RefreshCw size={13} className={building ? "animate-spin" : ""} />
            {building ? t("CrossRepoIntelligencePanel.rebuilding") : t("CrossRepoIntelligencePanel.rebuild")}
          </button>
          <IconButton size="sm" onClick={onClose} aria-label={t("CrossRepoIntelligencePanel.close")}>
            <X size={16} />
          </IconButton>
        </div>
      </header>

      {buildError && (
        <div role="alert" className="border-b border-danger/30 bg-danger-soft px-4 py-2 text-xs text-danger">
          {t("CrossRepoIntelligencePanel.buildError", { error: buildError })}
        </div>
      )}

      <div className="min-h-0 flex-1 overflow-y-auto px-4 py-4">
        <div className="mx-auto flex max-w-3xl flex-col gap-4">
          {builtAtMs != null && (
            <p className="text-xs text-faint">
              {t("CrossRepoIntelligencePanel.builtAt", {
                time: new Date(builtAtMs).toLocaleString(),
                symbolCount: symbols.length,
                fileCount: files.length,
              })}
            </p>
          )}

          {!hasWorkspace ? (
            <p className="rounded-lg border border-border bg-surface p-4 text-sm text-muted">
              {t("CrossRepoIntelligencePanel.noWorkspace")}
            </p>
          ) : builtAtMs == null ? (
            <p className="rounded-lg border border-border bg-surface p-4 text-sm text-muted">
              {t("CrossRepoIntelligencePanel.notBuiltYet")}
            </p>
          ) : null}

          <label className="flex items-center gap-2 rounded-lg border border-border bg-surface px-3 focus-within:ring-2 focus-within:ring-accent">
            <Search size={15} className="shrink-0 text-faint" />
            <span className="sr-only">{t("CrossRepoIntelligencePanel.searchLabel")}</span>
            <input
              value={query}
              onChange={(event) => setQuery(event.target.value)}
              data-focus-ring="custom"
              className="h-9 min-w-0 flex-1 bg-transparent text-sm outline-none placeholder:text-faint"
              placeholder={t("CrossRepoIntelligencePanel.searchPlaceholder")}
              disabled={builtAtMs == null}
            />
          </label>

          {builtAtMs != null && (
            <>
              <p className="text-xs text-faint">
                {query.trim()
                  ? t("CrossRepoIntelligencePanel.matchesHint", { count: filteredGroups.length })
                  : t("CrossRepoIntelligencePanel.matchesHint", { count: groups.length })}
              </p>
              {filteredGroups.length === 0 ? (
                <p className="text-sm text-muted">{t("CrossRepoIntelligencePanel.noMatches", { query })}</p>
              ) : (
                <ul className="flex flex-col gap-1">
                  {filteredGroups.slice(0, 200).map((group) => (
                    <li key={group.name}>
                      <button
                        type="button"
                        onClick={() => void runImpactQuery(group.name)}
                        className={`flex w-full items-center justify-between gap-2 rounded-md border px-3 py-2 text-left text-sm hover:bg-surface-2 ${
                          impact?.symbolName === group.name ? "border-accent bg-surface-2" : "border-border bg-surface"
                        }`}
                      >
                        <span className="min-w-0 flex-1 truncate font-mono text-foreground">{group.name}</span>
                        <span className="flex shrink-0 items-center gap-1">
                          {group.kinds.map((kind) => (
                            <StatusPill key={kind} tone="neutral">
                              {t(KIND_KEY[kind])}
                            </StatusPill>
                          ))}
                        </span>
                      </button>
                    </li>
                  ))}
                </ul>
              )}
            </>
          )}

          {impactLoading && (
            <p className="flex items-center gap-2 text-sm text-muted">
              <Loader2 size={14} className="animate-spin" /> {t("CrossRepoIntelligencePanel.impact.loading")}
            </p>
          )}

          {impactError && (
            <p role="alert" className="rounded-lg border border-danger/30 bg-danger-soft p-3 text-sm text-danger">
              {t("CrossRepoIntelligencePanel.impact.error", { error: impactError })}
            </p>
          )}

          {impact && !impactLoading && (
            <div className="flex flex-col gap-3">
              <div className="flex items-center justify-between gap-2">
                <h2 className="flex items-center gap-2 text-sm font-semibold text-foreground">
                  <Database size={15} className="text-faint" aria-hidden="true" />
                  {t("CrossRepoIntelligencePanel.impact.title", { symbol: impact.symbolName })}
                </h2>
                <button
                  type="button"
                  onClick={clearImpact}
                  className="rounded-md border border-border px-2 py-1 text-xs font-medium text-foreground hover:bg-surface-2"
                >
                  {t("CrossRepoIntelligencePanel.impact.clear")}
                </button>
              </div>

              <Section title={t("CrossRepoIntelligencePanel.impact.affectedRepos")}>
                {impact.affectedRoots.length === 0 ? (
                  <p className="text-sm text-muted">—</p>
                ) : (
                  <div className="flex flex-wrap gap-1.5">
                    {impact.affectedRoots.map((label) => (
                      <StatusPill key={label} tone="neutral">
                        {label}
                      </StatusPill>
                    ))}
                  </div>
                )}
              </Section>

              <Section title={t("CrossRepoIntelligencePanel.impact.affectedFiles")}>
                {impact.affectedFiles.length === 0 ? (
                  <p className="text-sm text-muted">—</p>
                ) : (
                  <ul className="flex flex-col gap-1">
                    {impact.affectedFiles.map((file) => {
                      const owner = impact.owners.find((o) => o.file === file);
                      return (
                        <li key={file} className="flex items-center justify-between gap-2 rounded-md border border-border bg-surface px-3 py-1.5">
                          <span className="min-w-0 flex-1 truncate font-mono text-xs text-foreground">{file}</span>
                          <span className="shrink-0 text-xs text-faint">
                            {owner && owner.owners.length > 0
                              ? owner.owners.join(", ")
                              : t("CrossRepoIntelligencePanel.impact.unassigned")}
                          </span>
                        </li>
                      );
                    })}
                  </ul>
                )}
              </Section>

              <Section title={t("CrossRepoIntelligencePanel.impact.references")}>
                {impact.references.length === 0 ? (
                  <p className="text-sm text-muted">{t("CrossRepoIntelligencePanel.impact.noReferences")}</p>
                ) : (
                  <ul className="flex flex-col gap-1">
                    {impact.references.slice(0, 100).map((ref, index) => (
                      <li key={`${ref.file}:${ref.line}:${index}`} className="rounded-md border border-border bg-surface px-3 py-1.5 text-xs">
                        <span className="font-mono text-faint">
                          {ref.file}:{ref.line}
                        </span>
                        <span className="ml-2 truncate font-mono text-foreground">{ref.text.trim()}</span>
                      </li>
                    ))}
                  </ul>
                )}
              </Section>

              <Section title={t("CrossRepoIntelligencePanel.impact.tests")}>
                {impact.testMatches.length === 0 ? (
                  <p className="text-sm text-muted">{t("CrossRepoIntelligencePanel.impact.noTests")}</p>
                ) : (
                  <ul className="flex flex-col gap-1">
                    {impact.testMatches.map((test) => (
                      <li key={test.file} className="rounded-md border border-border bg-surface px-3 py-1.5 font-mono text-xs text-foreground">
                        {test.file}
                      </li>
                    ))}
                  </ul>
                )}
              </Section>

              <Section title={t("CrossRepoIntelligencePanel.impact.migrationSteps")}>
                <ul className="list-disc space-y-1 pl-4 text-sm text-foreground">
                  {impact.migrationSteps.map((step, index) => (
                    <li key={index}>{step}</li>
                  ))}
                </ul>
              </Section>
            </div>
          )}

          <p className="text-xs text-faint">{t("CrossRepoIntelligencePanel.footnote")}</p>
        </div>
      </div>
    </section>
  );
}

export default CrossRepoIntelligencePanel;
