import { Fragment, useEffect, useMemo, useState } from "react";
import { ChevronDown, ChevronRight, Columns3, RefreshCw, X } from "lucide-react";

import { useT } from "../../lib/i18n";
import { useTrustScorecardsStore } from "../../store/trustScorecardsStore";
import { useModelStore } from "../../store/modelStore";
import { useConnectorsStore } from "../../store/connectorsStore";
import { useMcpStore } from "../../store/mcpStore";
import { useEcosystemStore } from "../../store/ecosystemStore";
import {
  scorecardWeight,
  TRUST_DIMENSION_KEYS,
  type TrustDimensionKey,
  type TrustEntityKind,
  type TrustLevel,
  type TrustScorecard,
} from "../../lib/trustScorecards";
import { Button, IconButton, StatusPill, type PillTone } from "../ui";

interface TrustScorecardsPanelProps {
  onClose: () => void;
}

type TFunction = ReturnType<typeof useT>["t"];

const KIND_ORDER: TrustEntityKind[] = ["model", "connector", "mcp_server", "skill", "workflow", "plugin"];

function levelTone(level: TrustLevel): PillTone {
  if (level === "good") return "success";
  if (level === "fair") return "warning";
  if (level === "poor") return "danger";
  return "neutral";
}

function EvidenceGrid({ card, t }: { card: TrustScorecard; t: TFunction }) {
  return (
    <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
      {TRUST_DIMENSION_KEYS.map((dimKey) => {
        const dimension = card.dimensions[dimKey];
        return (
          <div key={dimKey} className="rounded-md border border-border bg-surface p-2.5">
            <div className="flex items-center justify-between gap-2">
              <p className="text-[11px] font-semibold text-foreground">{t(`TrustScorecards.dimension.${dimKey}`)}</p>
              <StatusPill tone={levelTone(dimension.level)}>{t(`TrustScorecards.level.${dimension.level}`)}</StatusPill>
            </div>
            <ul className="mt-1.5 space-y-1.5">
              {dimension.evidence.map((item, index) => (
                <li key={index} className="text-[11px] leading-4 text-muted">
                  <span className="block font-mono text-[10px] text-faint">{item.field}</span>
                  {item.fact}
                </li>
              ))}
            </ul>
          </div>
        );
      })}
    </div>
  );
}

function ComparisonOverlay({ cards, onClose, t }: { cards: TrustScorecard[]; onClose: () => void; t: TFunction }) {
  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-labelledby="trust-compare-title"
      className="absolute inset-0 z-40 flex flex-col bg-background/95 backdrop-blur-sm"
    >
      <div className="flex shrink-0 items-center justify-between border-b border-border px-5 py-3">
        <h3 id="trust-compare-title" className="text-sm font-semibold text-foreground">
          {t("TrustScorecards.compareTitle", { count: cards.length })}
        </h3>
        <IconButton size="sm" aria-label={t("TrustScorecards.close")} onClick={onClose}>
          <X size={15} />
        </IconButton>
      </div>
      <div className="min-h-0 flex-1 overflow-auto p-5">
        <div className="overflow-x-auto rounded-lg border border-border">
          <table className="w-full border-collapse text-xs">
            <thead>
              <tr className="border-b border-border bg-surface text-left text-faint">
                <th className="px-3 py-2">{t("TrustScorecards.columnDimension")}</th>
                {cards.map((card) => (
                  <th key={card.id} className="min-w-52 px-3 py-2">
                    <p className="font-medium text-foreground">{card.name}</p>
                    <p className="text-[10px] font-normal text-faint">{t(`TrustScorecards.kind.${card.kind}`)}</p>
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {TRUST_DIMENSION_KEYS.map((dimKey) => (
                <tr key={dimKey} className="border-b border-border align-top last:border-b-0">
                  <td className="px-3 py-2.5 font-medium text-foreground">{t(`TrustScorecards.dimension.${dimKey}`)}</td>
                  {cards.map((card) => {
                    const dimension = card.dimensions[dimKey];
                    return (
                      <td key={card.id} className="px-3 py-2.5">
                        <StatusPill tone={levelTone(dimension.level)}>{t(`TrustScorecards.level.${dimension.level}`)}</StatusPill>
                        <ul className="mt-1.5 space-y-1">
                          {dimension.evidence.map((item, index) => (
                            <li key={index} className="text-[11px] leading-4 text-muted">
                              {item.fact}
                            </li>
                          ))}
                        </ul>
                      </td>
                    );
                  })}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </div>
    </div>
  );
}

export function TrustScorecardsPanel({ onClose }: TrustScorecardsPanelProps) {
  const { t } = useT();
  const scorecards = useTrustScorecardsStore((s) => s.scorecards);
  const loading = useTrustScorecardsStore((s) => s.loading);
  const error = useTrustScorecardsStore((s) => s.error);
  const recompute = useTrustScorecardsStore((s) => s.recompute);

  const [kindFilter, setKindFilter] = useState<TrustEntityKind | "all">("all");
  const [query, setQuery] = useState("");
  const [expandedId, setExpandedId] = useState<string | null>(null);
  const [selectedIds, setSelectedIds] = useState<string[]>([]);
  const [compareOpen, setCompareOpen] = useState(false);

  useEffect(() => {
    // Refresh every source store's own live data first — this store never
    // does that itself (see trustScorecardsStore.ts's doc comment) — then
    // recompute scorecards from what just landed. Each refresh is
    // best-effort: one source being unreachable shouldn't block scoring
    // everything else that did load.
    void Promise.all([
      useModelStore.getState().refresh().catch(() => {}),
      useModelStore.getState().refreshOllama().catch(() => {}),
      useModelStore.getState().refreshProviders().catch(() => {}),
      useConnectorsStore.getState().refresh().catch(() => {}),
      useMcpStore.getState().refresh().catch(() => {}),
      useEcosystemStore.getState().refreshPackages().catch(() => {}),
      useEcosystemStore.getState().refreshPluginRuntime().catch(() => {}),
      useEcosystemStore.getState().refreshWorkflows().catch(() => {}),
      useEcosystemStore.getState().refreshHistories().catch(() => {}),
    ]).then(() => recompute());
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  const filtered = useMemo(() => {
    const needle = query.trim().toLowerCase();
    return scorecards
      .filter((card) => (kindFilter === "all" ? true : card.kind === kindFilter))
      .filter((card) => (needle ? card.name.toLowerCase().includes(needle) || (card.subtitle ?? "").toLowerCase().includes(needle) : true))
      // Within a kind, weaker profiles first (see `LEVEL_WEIGHT`) so the
      // rows most worth a second look are never buried; name is only the
      // final tiebreak.
      .sort(
        (a, b) =>
          KIND_ORDER.indexOf(a.kind) - KIND_ORDER.indexOf(b.kind)
          || scorecardWeight(a) - scorecardWeight(b)
          || a.name.localeCompare(b.name),
      );
  }, [scorecards, kindFilter, query]);

  const toggleSelected = (id: string) => {
    setSelectedIds((prev) => (prev.includes(id) ? prev.filter((existing) => existing !== id) : [...prev, id]));
  };

  const selectedCards = scorecards.filter((card) => selectedIds.includes(card.id));

  return (
    <section className="relative flex h-full min-h-0 flex-col" aria-labelledby="trust-scorecards-title">
      <header className="flex shrink-0 items-start justify-between gap-3 border-b border-border px-5 py-4">
        <div>
          <h2 id="trust-scorecards-title" className="text-sm font-semibold text-foreground">
            {t("TrustScorecards.title")}
          </h2>
          <p className="mt-1 max-w-2xl text-xs leading-5 text-muted">{t("TrustScorecards.subtitle")}</p>
        </div>
        <div className="flex items-center gap-2">
          <IconButton
            size="sm"
            aria-label={t("TrustScorecards.refresh")}
            title={t("TrustScorecards.refresh")}
            onClick={() => void recompute()}
          >
            <RefreshCw size={15} className={loading ? "animate-spin" : ""} />
          </IconButton>
          <IconButton size="sm" aria-label={t("TrustScorecards.close")} title={t("TrustScorecards.close")} onClick={onClose}>
            <X size={15} />
          </IconButton>
        </div>
      </header>

      {error && (
        <div role="alert" className="mx-5 mt-3 rounded-md border border-danger/40 bg-danger/10 p-3 text-xs text-danger">
          {error}
        </div>
      )}

      <div className="flex shrink-0 flex-wrap items-center gap-2 border-b border-border px-5 py-3">
        <div className="flex flex-wrap gap-1.5">
          <Button size="sm" variant={kindFilter === "all" ? "primary" : "secondary"} onClick={() => setKindFilter("all")}>
            {t("TrustScorecards.filterAll")}
          </Button>
          {KIND_ORDER.map((kind) => (
            <Button key={kind} size="sm" variant={kindFilter === kind ? "primary" : "secondary"} onClick={() => setKindFilter(kind)}>
              {t(`TrustScorecards.kind.${kind}`)}
            </Button>
          ))}
        </div>
        <input
          className="ml-auto w-56 rounded-md border border-border bg-background px-2.5 py-1.5 text-xs text-foreground outline-none focus:border-accent focus:ring-1 focus:ring-accent"
          placeholder={t("TrustScorecards.searchPlaceholder")}
          value={query}
          onChange={(event) => setQuery(event.target.value)}
          aria-label={t("TrustScorecards.searchPlaceholder")}
        />
        {selectedIds.length >= 2 && (
          <Button size="sm" variant="primary" onClick={() => setCompareOpen(true)}>
            <Columns3 size={13} /> {t("TrustScorecards.compareButton", { count: selectedIds.length })}
          </Button>
        )}
      </div>

      <div className="min-h-0 flex-1 overflow-y-auto p-5">
        {filtered.length === 0 ? (
          <p className="rounded-md border border-dashed border-border p-8 text-center text-xs text-faint">
            {loading ? t("TrustScorecards.loading") : t("TrustScorecards.empty")}
          </p>
        ) : (
          <div className="overflow-x-auto rounded-lg border border-border">
            <table className="w-full min-w-[900px] border-collapse text-xs">
              <thead>
                <tr className="border-b border-border bg-surface text-left text-faint">
                  <th className="w-8 px-2 py-2" />
                  <th className="px-3 py-2">{t("TrustScorecards.columnName")}</th>
                  <th className="px-3 py-2">{t("TrustScorecards.columnKind")}</th>
                  {TRUST_DIMENSION_KEYS.map((dimKey: TrustDimensionKey) => (
                    <th key={dimKey} className="px-3 py-2">
                      {t(`TrustScorecards.dimension.${dimKey}`)}
                    </th>
                  ))}
                  <th className="w-8 px-2 py-2" />
                </tr>
              </thead>
              <tbody>
                {filtered.map((card) => {
                  const isExpanded = expandedId === card.id;
                  return (
                    <Fragment key={card.id}>
                      <tr className="border-b border-border last:border-b-0 hover:bg-surface-2">
                        <td className="px-2 py-2">
                          <input
                            type="checkbox"
                            checked={selectedIds.includes(card.id)}
                            onChange={() => toggleSelected(card.id)}
                            aria-label={t("TrustScorecards.selectForCompare", { name: card.name })}
                          />
                        </td>
                        <td className="px-3 py-2">
                          <p className="font-medium text-foreground">{card.name}</p>
                          {card.subtitle && <p className="truncate text-[11px] text-faint">{card.subtitle}</p>}
                        </td>
                        <td className="px-3 py-2 text-muted">{t(`TrustScorecards.kind.${card.kind}`)}</td>
                        {TRUST_DIMENSION_KEYS.map((dimKey) => (
                          <td key={dimKey} className="px-3 py-2">
                            <StatusPill tone={levelTone(card.dimensions[dimKey].level)}>
                              {t(`TrustScorecards.level.${card.dimensions[dimKey].level}`)}
                            </StatusPill>
                          </td>
                        ))}
                        <td className="px-2 py-2">
                          <IconButton
                            size="sm"
                            aria-label={t("TrustScorecards.expandEvidence")}
                            aria-expanded={isExpanded}
                            onClick={() => setExpandedId(isExpanded ? null : card.id)}
                          >
                            {isExpanded ? <ChevronDown size={13} /> : <ChevronRight size={13} />}
                          </IconButton>
                        </td>
                      </tr>
                      {isExpanded && (
                        <tr className="border-b border-border bg-background last:border-b-0">
                          <td colSpan={TRUST_DIMENSION_KEYS.length + 4} className="px-3 py-3">
                            <EvidenceGrid card={card} t={t} />
                          </td>
                        </tr>
                      )}
                    </Fragment>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </div>

      {compareOpen && selectedCards.length >= 2 && (
        <ComparisonOverlay cards={selectedCards} onClose={() => setCompareOpen(false)} t={t} />
      )}
    </section>
  );
}

export default TrustScorecardsPanel;
