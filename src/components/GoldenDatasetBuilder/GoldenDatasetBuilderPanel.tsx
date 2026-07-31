import { useMemo, useState } from "react";
import { ClipboardCheck, Database, Plus, RefreshCw, Trash2, Upload, X } from "lucide-react";

import { useT } from "../../lib/i18n";
import type { DatasetExample } from "../../lib/goldenDatasetBuilder";
import { useGoldenDatasetBuilderStore } from "../../store/goldenDatasetBuilderStore";
import { Button, IconButton, StatusPill, type PillTone } from "../ui";
import { errorMessage } from "../../lib/errors";

/**
 * Synthetic Data and Golden Dataset Builder (ROADMAP.md Phase 7, item 30): a
 * full-screen panel — same toggle pattern as `EvidenceBoardPanel`/
 * `SopCompilerPanel` (see `App.tsx`) — for building a golden dataset from a
 * seed description, generating synthetic labeled examples, importing real
 * ones through the same privacy filter, and tracing every example back to
 * its provenance, privacy verdict, duplicate verdict, and dataset version.
 * All state lives in `goldenDatasetBuilderStore.ts`; this component is
 * presentation plus the small "which dataset is on screen / is a form open"
 * UI state.
 */
interface GoldenDatasetBuilderPanelProps {
  onClose: () => void;
}

function exclusionTone(example: DatasetExample): PillTone {
  if (!example.included) return "danger";
  if (example.duplicateKind === "near") return "warning";
  return "success";
}

function ExampleRow({ datasetId, example, fields }: { datasetId: string; example: DatasetExample; fields: string[] }) {
  const { t } = useT();
  const deleteExample = useGoldenDatasetBuilderStore((state) => state.deleteExample);

  return (
    <li className="rounded-md border border-border bg-surface px-3 py-2.5 text-sm">
      <div className="flex flex-wrap items-start justify-between gap-2">
        <div className="min-w-0 flex-1 space-y-0.5">
          {fields.map((field) => (
            <p key={field} className="truncate text-foreground">
              <span className="text-faint">{field}:</span> {example.fields[field] ?? ""}
            </p>
          ))}
        </div>
        <div className="flex shrink-0 flex-wrap items-center gap-1.5">
          <StatusPill tone={example.provenance.kind === "synthetic" ? "neutral" : "warning"}>
            {example.provenance.kind === "synthetic" ? t("GoldenDatasetBuilder.provenanceSynthetic") : t("GoldenDatasetBuilder.provenanceImported")}
          </StatusPill>
          <StatusPill tone={example.privacy.passed ? "success" : "danger"}>
            {example.privacy.passed ? t("GoldenDatasetBuilder.privacyPassed") : t("GoldenDatasetBuilder.privacyFlagged")}
          </StatusPill>
          {example.duplicateKind !== "none" && (
            <StatusPill tone={exclusionTone(example)}>
              {example.duplicateKind === "exact" ? t("GoldenDatasetBuilder.duplicateExact") : t("GoldenDatasetBuilder.duplicateNear")}
            </StatusPill>
          )}
          {!example.included && (
            <StatusPill tone="danger">{t("GoldenDatasetBuilder.excluded")}</StatusPill>
          )}
          <button
            type="button"
            onClick={() => deleteExample(datasetId, example.id)}
            aria-label={t("GoldenDatasetBuilder.deleteExample")}
            className="text-faint hover:text-danger"
          >
            <Trash2 size={13} />
          </button>
        </div>
      </div>
      <p className="mt-1.5 text-xs text-faint">
        {example.provenance.kind === "synthetic"
          ? t("GoldenDatasetBuilder.generationPromptLabel", { prompt: example.provenance.generationPrompt })
          : t("GoldenDatasetBuilder.sourceLabel", { source: example.provenance.source })}
        {" · "}
        {t("GoldenDatasetBuilder.versionLabel", { version: example.version })}
      </p>
      {example.privacy.findings.length > 0 && (
        <p className="mt-1 text-xs text-danger">
          {example.privacy.findings.map((finding) => `${t(`GoldenDatasetBuilder.privacyFinding.${finding.type}`)} (${finding.count})`).join(", ")}
        </p>
      )}
    </li>
  );
}

export function GoldenDatasetBuilderPanel({ onClose }: GoldenDatasetBuilderPanelProps) {
  const { t } = useT();
  const datasets = useGoldenDatasetBuilderStore((state) => state.datasets);
  const activeDatasetId = useGoldenDatasetBuilderStore((state) => state.activeDatasetId);
  const generating = useGoldenDatasetBuilderStore((state) => state.generating);
  const setActiveDataset = useGoldenDatasetBuilderStore((state) => state.setActiveDataset);
  const createDataset = useGoldenDatasetBuilderStore((state) => state.createDataset);
  const deleteDataset = useGoldenDatasetBuilderStore((state) => state.deleteDataset);
  const generateExamples = useGoldenDatasetBuilderStore((state) => state.generateExamples);
  const importExamples = useGoldenDatasetBuilderStore((state) => state.importExamples);
  const runEval = useGoldenDatasetBuilderStore((state) => state.runEval);

  const [composeOpen, setComposeOpen] = useState(false);
  const [newName, setNewName] = useState("");
  const [newSeed, setNewSeed] = useState("");
  const [newFields, setNewFields] = useState("");

  const [importOpen, setImportOpen] = useState(false);
  const [importSource, setImportSource] = useState("");
  const [importText, setImportText] = useState("");
  const [importNotice, setImportNotice] = useState<string | null>(null);

  const [generateCount, setGenerateCount] = useState(10);
  const [runError, setRunError] = useState<string | null>(null);

  const activeDataset = useMemo(() => datasets.find((dataset) => dataset.id === activeDatasetId) ?? null, [datasets, activeDatasetId]);

  const handleCreate = () => {
    if (!newSeed.trim() || !newFields.trim()) return;
    createDataset(newName, newSeed, newFields);
    setNewName("");
    setNewSeed("");
    setNewFields("");
    setComposeOpen(false);
    setRunError(null);
  };

  const handleGenerate = async () => {
    if (!activeDatasetId) return;
    setRunError(null);
    try {
      await generateExamples(activeDatasetId, generateCount);
    } catch (error) {
      setRunError(errorMessage(error));
    }
  };

  const handleImport = () => {
    if (!activeDatasetId || !importText.trim()) return;
    const result = importExamples(activeDatasetId, importText, importSource);
    setImportNotice(t("GoldenDatasetBuilder.importResult", { imported: result.imported, skipped: result.skippedLines }));
    setImportText("");
    setImportSource("");
  };

  return (
    <section className="flex min-h-0 flex-1 flex-col" aria-labelledby="golden-dataset-builder-title">
      <header className="flex shrink-0 flex-col gap-2 border-b border-border px-4 py-3">
        <div className="flex items-center justify-between gap-3">
          <div className="min-w-0">
            <h1 id="golden-dataset-builder-title" className="text-base font-semibold text-foreground">
              {t("GoldenDatasetBuilder.title")}
            </h1>
            <p className="truncate text-xs text-muted">{t("GoldenDatasetBuilder.subtitle")}</p>
          </div>
          <IconButton size="sm" onClick={onClose} aria-label={t("GoldenDatasetBuilder.close")}>
            <X size={16} />
          </IconButton>
        </div>

        <div className="flex flex-wrap items-center gap-2">
          <select
            value={activeDatasetId ?? ""}
            onChange={(event) => setActiveDataset(event.target.value || null)}
            aria-label={t("GoldenDatasetBuilder.selectDataset")}
            className="min-w-[180px] rounded-md border border-border bg-background px-2 py-1.5 text-sm text-foreground"
          >
            <option value="">{t("GoldenDatasetBuilder.selectDataset")}</option>
            {datasets.map((dataset) => (
              <option key={dataset.id} value={dataset.id}>
                {dataset.name} · v{dataset.currentVersion}
              </option>
            ))}
          </select>

          <Button size="sm" variant="secondary" onClick={() => setComposeOpen((value) => !value)}>
            <Plus size={13} /> {t("GoldenDatasetBuilder.newDataset")}
          </Button>

          {activeDataset && (
            <IconButton size="sm" onClick={() => deleteDataset(activeDataset.id)} aria-label={t("GoldenDatasetBuilder.deleteDataset")} className="ml-auto">
              <Trash2 size={14} />
            </IconButton>
          )}
        </div>
      </header>

      {composeOpen && (
        <div className="flex shrink-0 flex-col gap-2 border-b border-border bg-surface px-4 py-3">
          <input
            type="text"
            value={newName}
            onChange={(event) => setNewName(event.target.value)}
            placeholder={t("GoldenDatasetBuilder.datasetNamePlaceholder")}
            className="rounded-md border border-border bg-background px-2 py-1.5 text-sm text-foreground focus:outline-none focus:ring-1 focus:ring-accent"
          />
          <textarea
            value={newSeed}
            onChange={(event) => setNewSeed(event.target.value)}
            placeholder={t("GoldenDatasetBuilder.seedPlaceholder")}
            rows={3}
            className="resize-y rounded-md border border-border bg-background px-2 py-1.5 text-sm text-foreground focus:outline-none focus:ring-1 focus:ring-accent"
          />
          <input
            type="text"
            value={newFields}
            onChange={(event) => setNewFields(event.target.value)}
            placeholder={t("GoldenDatasetBuilder.fieldsPlaceholder")}
            className="rounded-md border border-border bg-background px-2 py-1.5 text-sm text-foreground focus:outline-none focus:ring-1 focus:ring-accent"
          />
          <div className="flex items-center gap-2">
            <Button size="sm" variant="primary" onClick={handleCreate} disabled={!newSeed.trim() || !newFields.trim()}>
              {t("GoldenDatasetBuilder.createDataset")}
            </Button>
            <Button size="sm" variant="ghost" onClick={() => setComposeOpen(false)}>
              {t("GoldenDatasetBuilder.cancel")}
            </Button>
          </div>
        </div>
      )}

      {(runError || activeDataset?.lastError) && (
        <div role="alert" className="flex items-start justify-between gap-3 border-b border-danger/30 bg-danger-soft px-4 py-2 text-xs text-danger">
          <span>{t("GoldenDatasetBuilder.generationError", { error: runError ?? activeDataset?.lastError ?? "" })}</span>
          <button type="button" className="shrink-0 underline" onClick={() => setRunError(null)}>
            {t("GoldenDatasetBuilder.dismiss")}
          </button>
        </div>
      )}

      <div className="min-h-0 flex-1 overflow-y-auto px-4 py-3 [overscroll-behavior:contain]">
        {!activeDataset ? (
          <div className="flex h-full flex-col items-center justify-center gap-1 text-center">
            <Database size={28} className="mb-2 text-faint" />
            <p className="text-sm font-medium text-foreground">{t("GoldenDatasetBuilder.noDatasets")}</p>
            <p className="max-w-sm text-xs text-muted">{t("GoldenDatasetBuilder.noDatasetsHint")}</p>
          </div>
        ) : (
          <div className="space-y-4">
            <div className="rounded-md border border-border bg-surface p-3">
              <p className="mb-2 text-xs font-semibold uppercase tracking-wide text-faint">{t("GoldenDatasetBuilder.seedHeading")}</p>
              <p className="mb-2 text-sm text-foreground">{activeDataset.seedDescription || t("GoldenDatasetBuilder.noSeed")}</p>
              <p className="mb-3 text-xs text-faint">{t("GoldenDatasetBuilder.schemaFields", { fields: activeDataset.fields.join(", ") })}</p>
              <div className="flex flex-wrap items-center gap-2">
                <label className="flex items-center gap-1.5 text-xs text-faint">
                  {t("GoldenDatasetBuilder.countLabel")}
                  <input
                    type="number"
                    min={1}
                    max={50}
                    value={generateCount}
                    onChange={(event) => setGenerateCount(Number(event.target.value) || 1)}
                    className="w-16 rounded border border-border bg-background px-1.5 py-1 text-foreground"
                  />
                </label>
                <Button size="sm" variant="primary" onClick={() => void handleGenerate()} disabled={generating}>
                  <RefreshCw size={13} className={generating ? "animate-spin" : ""} />
                  {generating ? t("GoldenDatasetBuilder.generating") : t("GoldenDatasetBuilder.generate")}
                </Button>
                <Button size="sm" variant="secondary" onClick={() => setImportOpen((value) => !value)}>
                  <Upload size={13} /> {t("GoldenDatasetBuilder.importReal")}
                </Button>
                <Button size="sm" variant="secondary" onClick={() => runEval(activeDataset.id)}>
                  <ClipboardCheck size={13} /> {t("GoldenDatasetBuilder.runEval")}
                </Button>
              </div>
            </div>

            {importOpen && (
              <div className="flex flex-col gap-2 rounded-md border border-border bg-surface p-3">
                <p className="text-xs text-muted">{t("GoldenDatasetBuilder.importHint")}</p>
                <input
                  type="text"
                  value={importSource}
                  onChange={(event) => setImportSource(event.target.value)}
                  placeholder={t("GoldenDatasetBuilder.importSourcePlaceholder")}
                  className="rounded-md border border-border bg-background px-2 py-1.5 text-sm text-foreground focus:outline-none focus:ring-1 focus:ring-accent"
                />
                <textarea
                  value={importText}
                  onChange={(event) => setImportText(event.target.value)}
                  placeholder={t("GoldenDatasetBuilder.importTextPlaceholder")}
                  rows={5}
                  className="resize-y rounded-md border border-border bg-background px-2 py-1.5 text-sm text-foreground focus:outline-none focus:ring-1 focus:ring-accent"
                />
                <div className="flex items-center gap-2">
                  <Button size="sm" variant="primary" onClick={handleImport} disabled={!importText.trim()}>
                    {t("GoldenDatasetBuilder.importButton")}
                  </Button>
                  <Button size="sm" variant="ghost" onClick={() => setImportOpen(false)}>
                    {t("GoldenDatasetBuilder.cancel")}
                  </Button>
                </div>
                {importNotice && <p className="text-xs text-muted">{importNotice}</p>}
              </div>
            )}

            <div>
              <p className="mb-2 flex flex-wrap items-center gap-2 text-xs text-faint">
                <span>{t("GoldenDatasetBuilder.examplesCount", { count: activeDataset.examples.length })}</span>
                <span>
                  {t("GoldenDatasetBuilder.includedCount", { count: activeDataset.examples.filter((example) => example.included).length })}
                </span>
              </p>
              {activeDataset.examples.length === 0 ? (
                <div className="flex flex-col items-center justify-center gap-1 py-8 text-center">
                  <p className="text-sm font-medium text-foreground">{t("GoldenDatasetBuilder.noExamplesYet")}</p>
                  <p className="max-w-sm text-xs text-muted">{t("GoldenDatasetBuilder.noExamplesHint")}</p>
                </div>
              ) : (
                <ul className="space-y-2">
                  {activeDataset.examples.map((example) => (
                    <ExampleRow key={example.id} datasetId={activeDataset.id} example={example} fields={activeDataset.fields} />
                  ))}
                </ul>
              )}
            </div>

            <div>
              <p className="mb-2 text-xs font-semibold uppercase tracking-wide text-faint">{t("GoldenDatasetBuilder.versionHistory")}</p>
              <ul className="space-y-1.5 text-xs">
                {[...activeDataset.versions].reverse().map((entry) => (
                  <li key={entry.version} className="flex items-center justify-between gap-2 rounded border border-border bg-surface px-2 py-1.5">
                    <span className="text-foreground">
                      {t("GoldenDatasetBuilder.versionEntry", { version: entry.version, note: entry.note })}
                    </span>
                    <span className="shrink-0 text-faint">{t("GoldenDatasetBuilder.exampleCountLabel", { count: entry.exampleCount })}</span>
                  </li>
                ))}
              </ul>
            </div>

            {activeDataset.evalRuns.length > 0 && (
              <div>
                <p className="mb-2 text-xs font-semibold uppercase tracking-wide text-faint">{t("GoldenDatasetBuilder.evalHistory")}</p>
                <ul className="space-y-1.5 text-xs">
                  {activeDataset.evalRuns.map((run) => (
                    <li key={run.id} className="rounded border border-border bg-surface px-2 py-1.5">
                      <span className="text-foreground">{run.summary}</span>{" "}
                      <span className="text-faint">{t("GoldenDatasetBuilder.evalVersionLabel", { version: run.version })}</span>
                    </li>
                  ))}
                </ul>
              </div>
            )}
          </div>
        )}
      </div>
    </section>
  );
}

export default GoldenDatasetBuilderPanel;
