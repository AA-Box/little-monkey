import { useMemo, useState } from "react";
import { save } from "@tauri-apps/plugin-dialog";
import { writeTextFile } from "@tauri-apps/plugin-fs";
import {
  ChevronDown,
  ChevronRight,
  Copy,
  Download,
  FlaskConical,
  Play,
  Plus,
  Sparkles,
  Square,
  Trash2,
  X,
} from "lucide-react";
import { useShallow } from "zustand/react/shallow";

import { useT } from "../../lib/i18n";
import {
  BENCHMARK_CATEGORIES,
  buildLabReport,
  createLabPrompt,
  labRunFileBaseName,
  renderLabReportJson,
  renderLabReportMarkdown,
  type BenchmarkCategory,
  type BenchmarkSuite,
  type LabPrompt,
  type LabResult,
  type LabRubric,
  type LabRun,
  type LabVerifier,
  type LabVerifierKind,
  type ModelSet,
} from "../../lib/compareLab";
import { startLabRun, stopLabRun } from "../../lib/compareLabRunner";
import { promoteLabModel, promoteLabPrompt, promoteLabResponse } from "../../lib/compareLabPromote";
import { buildModelTargetInventory, type ModelTargetSnapshot } from "../../lib/modelTargets";
import { useModelStore } from "../../store/modelStore";
import { MAX_RUN_HISTORY, useCompareLabStore } from "../../store/compareLabStore";
import { Button, StatusPill, Tabs, type PillTone } from "../ui";
import { errorMessage } from "../../lib/errors";

type Section = "suites" | "modelSets" | "runs";

const RUN_STATUS_TONE: Record<LabRun["status"], PillTone> = {
  running: "warning",
  completed: "success",
  cancelled: "neutral",
};

const RESULT_STATUS_TONE: Record<LabResult["status"], PillTone> = {
  pending: "neutral",
  running: "warning",
  completed: "success",
  failed: "danger",
  cancelled: "neutral",
};

const VERIFIER_KINDS_WITH_NONE: readonly (LabVerifierKind | "none")[] = [
  "none",
  "contains",
  "not_contains",
  "regex",
  "json_valid",
  "min_length",
];

function formatMs(value: number | null): string {
  if (value === null) return "—";
  return value < 1000 ? `${Math.round(value)} ms` : `${(value / 1000).toFixed(1)} s`;
}

function formatCost(value: number | null, known: boolean): string {
  if (value === null) return "—";
  const formatted = `$${value.toFixed(4)}`;
  return known ? formatted : `~${formatted}`;
}

function formatRate(value: number | null): string {
  if (value === null) return "—";
  return `${Math.round(value * 100)}%`;
}

function formatDateTime(ms: number): string {
  return new Date(ms).toLocaleString();
}

const inputClass =
  "h-9 rounded-md border border-border bg-surface-2 px-2.5 text-sm text-foreground outline-none focus:ring-1 focus:ring-accent";
const smallInputClass =
  "h-8 rounded-md border border-border bg-background px-2 text-xs text-foreground outline-none focus:ring-1 focus:ring-accent";

/**
 * Settings surface for Model Compare Lab (ROADMAP.md Phase 2): manage saved
 * benchmark suites and model sets, launch a batch run of a suite across a
 * model set, and review the side-by-side report — latency, tokens, cost,
 * tool-use success, and verifier outcome per (prompt, model) cell — with
 * markdown/JSON export and "promote to chat" actions. All execution and
 * persistence logic already lives in `lib/compareLab.ts`,
 * `lib/compareLabRunner.ts`, `lib/compareLabPromote.ts`, and
 * `store/compareLabStore.ts`; this component only wires them to UI.
 */
export function CompareLabPanel() {
  const { t } = useT();
  const [section, setSection] = useState<Section>("suites");

  return (
    <div className="flex flex-col gap-6">
      <div>
        <h3 className="flex items-center gap-2 text-base font-semibold text-foreground">
          <FlaskConical size={18} />
          {t("CompareLab.title")}
        </h3>
        <p className="mt-1 text-sm leading-relaxed text-muted">{t("CompareLab.subtitle")}</p>
        <p className="mt-2 rounded-md border border-border bg-surface-2 px-2.5 py-1.5 text-[11px] leading-relaxed text-muted">
          {t("CompareLab.toolsDefaultOffNotice")}
        </p>
      </div>

      <Tabs
        tabs={[
          { id: "suites", label: t("CompareLab.tabSuites") },
          { id: "modelSets", label: t("CompareLab.tabModelSets") },
          { id: "runs", label: t("CompareLab.tabRuns") },
        ]}
        active={section}
        onChange={(id) => setSection(id as Section)}
      />

      {section === "suites" && <SuitesSection />}
      {section === "modelSets" && <ModelSetsSection />}
      {section === "runs" && <RunsSection />}
    </div>
  );
}

function SuitesSection() {
  const { t } = useT();
  const suites = useCompareLabStore((s) => s.suites);
  const saveSuite = useCompareLabStore((s) => s.saveSuite);
  const removeSuite = useCompareLabStore((s) => s.removeSuite);
  const duplicateSuite = useCompareLabStore((s) => s.duplicateSuite);
  const [editing, setEditing] = useState<BenchmarkSuite | null>(null);
  const [confirmDeleteId, setConfirmDeleteId] = useState<string | null>(null);

  function startNewSuite() {
    const now = Date.now();
    setEditing({ id: "", name: "", description: "", category: "custom", prompts: [], builtIn: false, createdAt: now, updatedAt: now });
  }

  if (editing) {
    return (
      <SuiteEditor
        suite={editing}
        onCancel={() => setEditing(null)}
        onSave={(suite) => {
          saveSuite(suite);
          setEditing(null);
        }}
      />
    );
  }

  return (
    <div className="flex flex-col gap-3">
      <div className="flex items-center justify-between gap-3">
        <p className="text-xs text-muted">{t("CompareLab.suitesHint")}</p>
        <Button size="sm" variant="primary" onClick={startNewSuite}>
          <Plus size={14} /> {t("CompareLab.newSuite")}
        </Button>
      </div>

      {suites.length === 0 && <p className="text-xs text-faint">{t("CompareLab.noSuitesState")}</p>}

      <div className="flex flex-col gap-2">
        {suites.map((suite) => (
          <div key={suite.id} className="rounded-lg border border-border bg-surface p-3">
            <div className="flex items-start justify-between gap-3">
              <div className="min-w-0">
                <div className="flex flex-wrap items-center gap-2">
                  <span className="text-sm font-medium text-foreground">{suite.name || t("CompareLab.untitledSuite")}</span>
                  <StatusPill tone="neutral">{t(`CompareLab.category.${suite.category}`)}</StatusPill>
                  {suite.builtIn && <StatusPill tone="neutral">{t("CompareLab.builtInBadge")}</StatusPill>}
                </div>
                {suite.description && <p className="mt-1 text-xs text-muted">{suite.description}</p>}
                <p className="mt-1 text-[11px] text-faint">{t("CompareLab.promptCount", { count: suite.prompts.length })}</p>
              </div>
              <div className="flex shrink-0 flex-wrap items-center justify-end gap-1.5">
                <Button size="sm" variant="secondary" onClick={() => setEditing(suite)}>
                  {t("CompareLab.edit")}
                </Button>
                <Button size="sm" variant="secondary" onClick={() => duplicateSuite(suite.id)}>
                  <Copy size={13} /> {t("CompareLab.duplicate")}
                </Button>
                {confirmDeleteId === suite.id ? (
                  <>
                    <Button
                      size="sm"
                      variant="danger"
                      onClick={() => {
                        removeSuite(suite.id);
                        setConfirmDeleteId(null);
                      }}
                    >
                      {t("CompareLab.confirmDelete")}
                    </Button>
                    <Button size="sm" variant="ghost" onClick={() => setConfirmDeleteId(null)}>
                      {t("CompareLab.cancel")}
                    </Button>
                  </>
                ) : (
                  <Button size="sm" variant="ghost" onClick={() => setConfirmDeleteId(suite.id)}>
                    <Trash2 size={13} /> {t("CompareLab.delete")}
                  </Button>
                )}
              </div>
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}

function SuiteEditor({
  suite,
  onCancel,
  onSave,
}: {
  suite: BenchmarkSuite;
  onCancel: () => void;
  onSave: (suite: BenchmarkSuite) => void;
}) {
  const { t } = useT();
  const [name, setName] = useState(suite.name);
  const [description, setDescription] = useState(suite.description);
  const [category, setCategory] = useState<BenchmarkCategory>(suite.category);
  const [prompts, setPrompts] = useState<LabPrompt[]>(suite.prompts);

  function addPrompt() {
    setPrompts((current) => [...current, createLabPrompt("")]);
  }
  function updatePrompt(id: string, patch: Partial<LabPrompt>) {
    setPrompts((current) => current.map((p) => (p.id === id ? { ...p, ...patch } : p)));
  }
  function removePrompt(id: string) {
    setPrompts((current) => current.filter((p) => p.id !== id));
  }

  const canSave = name.trim().length > 0 && prompts.length > 0 && prompts.every((p) => p.text.trim().length > 0);

  function handleSave() {
    onSave({ ...suite, name: name.trim(), description: description.trim(), category, prompts });
  }

  return (
    <div className="flex flex-col gap-4">
      <div className="flex items-center justify-between gap-3">
        <h4 className="text-sm font-semibold text-foreground">
          {suite.id ? t("CompareLab.editSuiteTitle") : t("CompareLab.newSuiteTitle")}
        </h4>
        <div className="flex items-center gap-1.5">
          <Button size="sm" variant="ghost" onClick={onCancel}>
            {t("CompareLab.cancel")}
          </Button>
          <Button size="sm" variant="primary" disabled={!canSave} onClick={handleSave}>
            {t("CompareLab.save")}
          </Button>
        </div>
      </div>

      <label className="flex flex-col gap-1 text-xs font-medium text-muted">
        {t("CompareLab.suiteName")}
        <input value={name} onChange={(e) => setName(e.target.value)} className={inputClass} placeholder={t("CompareLab.suiteNamePlaceholder")} />
      </label>

      <label className="flex flex-col gap-1 text-xs font-medium text-muted">
        {t("CompareLab.suiteDescription")}
        <textarea
          value={description}
          onChange={(e) => setDescription(e.target.value)}
          rows={2}
          className="rounded-md border border-border bg-surface-2 px-2.5 py-2 text-sm text-foreground outline-none focus:ring-1 focus:ring-accent"
        />
      </label>

      <label className="flex flex-col gap-1 text-xs font-medium text-muted">
        {t("CompareLab.suiteCategory")}
        <select value={category} onChange={(e) => setCategory(e.target.value as BenchmarkCategory)} className={`${inputClass} w-56`}>
          {BENCHMARK_CATEGORIES.map((cat) => (
            <option key={cat} value={cat}>
              {t(`CompareLab.category.${cat}`)}
            </option>
          ))}
        </select>
      </label>

      <div className="flex flex-col gap-3">
        <div className="flex items-center justify-between gap-3">
          <h5 className="text-xs font-semibold uppercase tracking-wide text-faint">{t("CompareLab.promptsHeading")}</h5>
          <Button size="sm" variant="secondary" onClick={addPrompt}>
            <Plus size={13} /> {t("CompareLab.addPrompt")}
          </Button>
        </div>
        {prompts.length === 0 && <p className="text-xs text-faint">{t("CompareLab.noPromptsState")}</p>}
        {prompts.map((prompt, index) => (
          <PromptEditorRow key={prompt.id} index={index} prompt={prompt} onChange={(patch) => updatePrompt(prompt.id, patch)} onRemove={() => removePrompt(prompt.id)} />
        ))}
      </div>
    </div>
  );
}

function PromptEditorRow({
  index,
  prompt,
  onChange,
  onRemove,
}: {
  index: number;
  prompt: LabPrompt;
  onChange: (patch: Partial<LabPrompt>) => void;
  onRemove: () => void;
}) {
  const { t } = useT();
  // Decoupled from `prompt.rubricCriteria.join(", ")` so the input's own text
  // (e.g. a trailing ", " while mid-typing the next criterion) never gets
  // clobbered by the comma-split-then-rejoin round trip on every keystroke.
  const [rubricText, setRubricText] = useState(prompt.rubricCriteria.join(", "));
  const verifierKind: LabVerifierKind | "none" = prompt.verifier?.kind ?? "none";

  function setVerifierKind(kind: LabVerifierKind | "none") {
    if (kind === "none") {
      onChange({ verifier: null });
      return;
    }
    onChange({ verifier: { kind, value: prompt.verifier?.value ?? "", flags: prompt.verifier?.flags ?? "", label: prompt.verifier?.label ?? "" } });
  }

  function patchVerifier(patch: Partial<LabVerifier>) {
    if (!prompt.verifier) return;
    onChange({ verifier: { ...prompt.verifier, ...patch } });
  }

  return (
    <div className="rounded-lg border border-border bg-surface-2 p-3">
      <div className="flex items-start gap-2">
        <span className="mt-2 shrink-0 text-xs font-semibold text-faint">#{index + 1}</span>
        <textarea
          value={prompt.text}
          onChange={(e) => onChange({ text: e.target.value })}
          rows={3}
          placeholder={t("CompareLab.promptTextPlaceholder")}
          className="min-w-0 flex-1 rounded-md border border-border bg-background px-2.5 py-2 text-sm text-foreground outline-none focus:ring-1 focus:ring-accent"
        />
        <Button size="sm" variant="ghost" onClick={onRemove} aria-label={t("CompareLab.removePrompt")}>
          <X size={14} />
        </Button>
      </div>

      <label className="mt-2 flex items-center gap-2 text-xs text-muted">
        <input type="checkbox" checked={prompt.toolsEnabled} onChange={(e) => onChange({ toolsEnabled: e.target.checked })} className="accent-accent" />
        {t("CompareLab.toolsEnabledLabel")}
      </label>
      <p className="mt-1 text-[11px] leading-relaxed text-faint">{t("CompareLab.toolsEnabledHint")}</p>

      <div className="mt-2 flex flex-wrap items-center gap-2">
        <label className="flex items-center gap-1.5 text-xs text-muted">
          {t("CompareLab.verifierLabel")}
          <select value={verifierKind} onChange={(e) => setVerifierKind(e.target.value as LabVerifierKind | "none")} className={smallInputClass}>
            {VERIFIER_KINDS_WITH_NONE.map((kind) => (
              <option key={kind} value={kind}>
                {t(`CompareLab.verifierKind.${kind}`)}
              </option>
            ))}
          </select>
        </label>
        {prompt.verifier && prompt.verifier.kind !== "json_valid" && (
          <input
            value={prompt.verifier.value ?? ""}
            onChange={(e) => patchVerifier({ value: e.target.value })}
            placeholder={t("CompareLab.verifierValuePlaceholder")}
            className={`${smallInputClass} w-40`}
          />
        )}
        {prompt.verifier && prompt.verifier.kind === "regex" && (
          <input
            value={prompt.verifier.flags ?? ""}
            onChange={(e) => patchVerifier({ flags: e.target.value })}
            placeholder={t("CompareLab.verifierFlagsPlaceholder")}
            className={`${smallInputClass} w-16`}
          />
        )}
        {prompt.verifier && (
          <input
            value={prompt.verifier.label}
            onChange={(e) => patchVerifier({ label: e.target.value })}
            placeholder={t("CompareLab.verifierDescriptionPlaceholder")}
            className={`${smallInputClass} w-48`}
          />
        )}
      </div>

      <label className="mt-2 flex flex-col gap-1 text-xs text-muted">
        {t("CompareLab.rubricCriteriaLabel")}
        <input
          value={rubricText}
          onChange={(e) => {
            setRubricText(e.target.value);
            onChange({ rubricCriteria: e.target.value.split(",").map((c) => c.trim()).filter((c) => c.length > 0) });
          }}
          placeholder={t("CompareLab.rubricCriteriaPlaceholder")}
          className={smallInputClass}
        />
      </label>
    </div>
  );
}

function ModelSetsSection() {
  const { t } = useT();
  const modelSets = useCompareLabStore((s) => s.modelSets);
  const saveModelSet = useCompareLabStore((s) => s.saveModelSet);
  const removeModelSet = useCompareLabStore((s) => s.removeModelSet);
  const [editing, setEditing] = useState<ModelSet | null>(null);
  const [confirmDeleteId, setConfirmDeleteId] = useState<string | null>(null);

  function startNewModelSet() {
    const now = Date.now();
    setEditing({ id: "", name: "", targets: [], createdAt: now, updatedAt: now });
  }

  if (editing) {
    return (
      <ModelSetEditor
        modelSet={editing}
        onCancel={() => setEditing(null)}
        onSave={(name, targets) => {
          saveModelSet(name, targets, editing.id || undefined);
          setEditing(null);
        }}
      />
    );
  }

  return (
    <div className="flex flex-col gap-3">
      <div className="flex items-center justify-between gap-3">
        <p className="text-xs text-muted">{t("CompareLab.modelSetsHint")}</p>
        <Button size="sm" variant="primary" onClick={startNewModelSet}>
          <Plus size={14} /> {t("CompareLab.newModelSet")}
        </Button>
      </div>

      {modelSets.length === 0 && <p className="text-xs text-faint">{t("CompareLab.noModelSetsState")}</p>}

      <div className="flex flex-col gap-2">
        {modelSets.map((set) => (
          <div key={set.id} className="rounded-lg border border-border bg-surface p-3">
            <div className="flex items-start justify-between gap-3">
              <div className="min-w-0">
                <p className="text-sm font-medium text-foreground">{set.name}</p>
                <p className="mt-1 truncate text-xs text-muted">
                  {set.targets.map((target) => target.displayName).join(" · ") || t("CompareLab.noTargetsState")}
                </p>
              </div>
              <div className="flex shrink-0 items-center gap-1.5">
                <Button size="sm" variant="secondary" onClick={() => setEditing(set)}>
                  {t("CompareLab.edit")}
                </Button>
                {confirmDeleteId === set.id ? (
                  <>
                    <Button
                      size="sm"
                      variant="danger"
                      onClick={() => {
                        removeModelSet(set.id);
                        setConfirmDeleteId(null);
                      }}
                    >
                      {t("CompareLab.confirmDelete")}
                    </Button>
                    <Button size="sm" variant="ghost" onClick={() => setConfirmDeleteId(null)}>
                      {t("CompareLab.cancel")}
                    </Button>
                  </>
                ) : (
                  <Button size="sm" variant="ghost" onClick={() => setConfirmDeleteId(set.id)}>
                    <Trash2 size={13} /> {t("CompareLab.delete")}
                  </Button>
                )}
              </div>
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}

function ModelSetEditor({
  modelSet,
  onCancel,
  onSave,
}: {
  modelSet: ModelSet;
  onCancel: () => void;
  onSave: (name: string, targets: ModelTargetSnapshot[]) => void;
}) {
  const { t } = useT();
  const [name, setName] = useState(modelSet.name);
  const [selected, setSelected] = useState<ModelTargetSnapshot[]>(modelSet.targets);
  const costRates = useCompareLabStore((s) => s.costRates);
  const setCostRate = useCompareLabStore((s) => s.setCostRate);

  const modelState = useModelStore(
    useShallow((state) => ({
      installed: state.installed,
      active: state.active,
      llamaStatus: state.llamaStatus,
      ollamaModels: state.ollamaModels,
      ollamaReachable: state.ollamaReachable,
      providers: state.providers,
      providerModels: state.providerModels,
      effortByTarget: state.effortByTarget,
    })),
  );

  const inventory = useMemo(() => buildModelTargetInventory(modelState), [modelState]);
  const selectedKeys = new Set(selected.map((target) => target.key));
  const missingSelected = selected.filter((target) => !inventory.targets.some((t2) => t2.key === target.key));

  function toggleTarget(target: ModelTargetSnapshot) {
    if (selectedKeys.has(target.key)) {
      setSelected((current) => current.filter((t2) => t2.key !== target.key));
    } else {
      setSelected((current) => [...current, target]);
    }
  }

  const canSave = name.trim().length > 0 && selected.length > 0;

  return (
    <div className="flex flex-col gap-4">
      <div className="flex items-center justify-between gap-3">
        <h4 className="text-sm font-semibold text-foreground">
          {modelSet.id ? t("CompareLab.editModelSetTitle") : t("CompareLab.newModelSetTitle")}
        </h4>
        <div className="flex items-center gap-1.5">
          <Button size="sm" variant="ghost" onClick={onCancel}>
            {t("CompareLab.cancel")}
          </Button>
          <Button size="sm" variant="primary" disabled={!canSave} onClick={() => onSave(name.trim(), selected)}>
            {t("CompareLab.save")}
          </Button>
        </div>
      </div>

      <label className="flex flex-col gap-1 text-xs font-medium text-muted">
        {t("CompareLab.modelSetName")}
        <input value={name} onChange={(e) => setName(e.target.value)} className={inputClass} placeholder={t("CompareLab.modelSetNamePlaceholder")} />
      </label>

      <div>
        <h5 className="text-xs font-semibold uppercase tracking-wide text-faint">{t("CompareLab.selectModelsHeading")}</h5>
        {inventory.groups.length === 0 && <p className="mt-2 text-xs text-faint">{t("CompareLab.noModelsAvailableState")}</p>}
        <div className="mt-2 flex flex-col gap-3">
          {inventory.groups.map((group) => (
            <fieldset key={group.key} className="rounded-lg border border-border bg-surface p-2.5">
              <legend className="px-1 text-[11px] font-semibold uppercase tracking-wide text-faint">{group.label}</legend>
              <div className="flex flex-col gap-1">
                {group.targets.map((target) => {
                  const available = target.availability.status === "available";
                  const checked = selectedKeys.has(target.key);
                  return (
                    <label
                      key={target.key}
                      className={`flex items-center gap-2 rounded-md px-2 py-1.5 text-sm ${available || checked ? "cursor-pointer hover:bg-surface-2" : "cursor-not-allowed opacity-60"}`}
                    >
                      <input type="checkbox" checked={checked} disabled={!available && !checked} onChange={() => toggleTarget(target)} className="accent-accent" />
                      <span className="min-w-0 flex-1 truncate text-foreground">{target.displayName}</span>
                      {!available && <span className="shrink-0 text-[10px] text-danger">{t("CompareLab.unavailableBadge")}</span>}
                    </label>
                  );
                })}
              </div>
            </fieldset>
          ))}
        </div>

        {missingSelected.length > 0 && (
          <div className="mt-2 rounded-lg border border-danger/30 bg-danger-soft p-2.5">
            <p className="text-xs font-medium text-danger">{t("CompareLab.missingModelsHeading")}</p>
            <div className="mt-1 flex flex-col gap-1">
              {missingSelected.map((target) => (
                <div key={target.key} className="flex items-center justify-between gap-2 text-xs text-danger">
                  <span className="truncate">{target.displayName}</span>
                  <Button size="sm" variant="ghost" onClick={() => toggleTarget(target)}>
                    <X size={12} /> {t("CompareLab.removeModel")}
                  </Button>
                </div>
              ))}
            </div>
          </div>
        )}
      </div>

      {selected.length > 0 && (
        <div>
          <h5 className="text-xs font-semibold uppercase tracking-wide text-faint">{t("CompareLab.costRatesHeading")}</h5>
          <p className="mt-1 text-[11px] leading-relaxed text-faint">{t("CompareLab.costRatesHint")}</p>
          <div className="mt-2 flex flex-col gap-2">
            {selected.map((target) => {
              const rate = costRates[target.key] ?? null;
              return (
                <div key={target.key} className="flex flex-wrap items-center gap-2 rounded-md border border-border bg-surface p-2 text-xs">
                  <span className="min-w-0 flex-1 truncate font-medium text-foreground">{target.displayName}</span>
                  <label className="flex items-center gap-1 text-muted">
                    {t("CompareLab.inputRateLabel")}
                    <input
                      type="number"
                      min={0}
                      step="0.01"
                      value={rate?.inputPerMillionUsd ?? ""}
                      onChange={(e) => {
                        const inputPerMillionUsd = Number(e.target.value);
                        setCostRate(target.key, {
                          inputPerMillionUsd: Number.isFinite(inputPerMillionUsd) ? inputPerMillionUsd : 0,
                          outputPerMillionUsd: rate?.outputPerMillionUsd ?? 0,
                        });
                      }}
                      className="h-7 w-20 rounded-md border border-border bg-background px-1.5 text-xs text-foreground outline-none focus:ring-1 focus:ring-accent"
                    />
                  </label>
                  <label className="flex items-center gap-1 text-muted">
                    {t("CompareLab.outputRateLabel")}
                    <input
                      type="number"
                      min={0}
                      step="0.01"
                      value={rate?.outputPerMillionUsd ?? ""}
                      onChange={(e) => {
                        const outputPerMillionUsd = Number(e.target.value);
                        setCostRate(target.key, {
                          inputPerMillionUsd: rate?.inputPerMillionUsd ?? 0,
                          outputPerMillionUsd: Number.isFinite(outputPerMillionUsd) ? outputPerMillionUsd : 0,
                        });
                      }}
                      className="h-7 w-20 rounded-md border border-border bg-background px-1.5 text-xs text-foreground outline-none focus:ring-1 focus:ring-accent"
                    />
                  </label>
                </div>
              );
            })}
          </div>
        </div>
      )}
    </div>
  );
}

function RunsSection() {
  const { t } = useT();
  const suites = useCompareLabStore((s) => s.suites);
  const modelSets = useCompareLabStore((s) => s.modelSets);
  const costRates = useCompareLabStore((s) => s.costRates);
  const runs = useCompareLabStore((s) => s.runs);
  const removeRun = useCompareLabStore((s) => s.removeRun);

  const [suiteId, setSuiteId] = useState("");
  const [modelSetId, setModelSetId] = useState("");
  const [startError, setStartError] = useState<string | null>(null);
  const [selectedRunId, setSelectedRunId] = useState<string | null>(null);

  const sortedRuns = useMemo(() => [...runs].sort((a, b) => b.createdAt - a.createdAt), [runs]);
  const selectedRun = sortedRuns.find((run) => run.id === selectedRunId) ?? null;
  const selectedSuite = suites.find((s) => s.id === suiteId) ?? null;
  const selectedModelSet = modelSets.find((m) => m.id === modelSetId) ?? null;
  const canStart = Boolean(selectedSuite && selectedSuite.prompts.length > 0 && selectedModelSet && selectedModelSet.targets.length > 0);

  function handleStart() {
    if (!selectedSuite || !selectedModelSet) return;
    setStartError(null);
    try {
      const handle = startLabRun(selectedSuite, selectedModelSet, costRates);
      setSelectedRunId(handle.runId);
      void handle.done;
    } catch (error) {
      setStartError(errorMessage(error));
    }
  }

  return (
    <div className="flex flex-col gap-4">
      <div className="rounded-lg border border-border bg-surface p-3">
        <h4 className="text-xs font-semibold uppercase tracking-wide text-faint">{t("CompareLab.startRunHeading")}</h4>
        {(suites.length === 0 || modelSets.length === 0) && (
          <p className="mt-2 text-xs text-faint">{t("CompareLab.startRunPrereqState")}</p>
        )}
        <div className="mt-2 flex flex-wrap items-center gap-2">
          <select value={suiteId} onChange={(e) => setSuiteId(e.target.value)} className={`${inputClass} min-w-[12rem]`}>
            <option value="">{t("CompareLab.selectSuitePlaceholder")}</option>
            {suites.map((suite) => (
              <option key={suite.id} value={suite.id}>
                {suite.name} ({suite.prompts.length})
              </option>
            ))}
          </select>
          <select value={modelSetId} onChange={(e) => setModelSetId(e.target.value)} className={`${inputClass} min-w-[12rem]`}>
            <option value="">{t("CompareLab.selectModelSetPlaceholder")}</option>
            {modelSets.map((set) => (
              <option key={set.id} value={set.id}>
                {set.name} ({set.targets.length})
              </option>
            ))}
          </select>
          <Button size="sm" variant="primary" disabled={!canStart} onClick={handleStart}>
            <Play size={14} /> {t("CompareLab.startRunAction")}
          </Button>
        </div>
        {startError && (
          <p role="alert" className="mt-2 rounded-md border border-danger/30 bg-danger-soft px-2.5 py-1.5 text-xs text-danger">
            {startError}
          </p>
        )}
      </div>

      <div className="flex flex-col gap-2">
        <div className="flex items-baseline justify-between gap-2">
          <h4 className="text-xs font-semibold uppercase tracking-wide text-faint">{t("CompareLab.runHistoryHeading")}</h4>
          <p className="text-[11px] text-faint">{t("CompareLab.runHistoryHint", { max: MAX_RUN_HISTORY })}</p>
        </div>
        {sortedRuns.length === 0 && <p className="text-xs text-faint">{t("CompareLab.noRunsState")}</p>}
        {sortedRuns.map((run) => (
          <div key={run.id} className={`rounded-lg border p-2.5 ${selectedRunId === run.id ? "border-accent" : "border-border"} bg-surface`}>
            <div className="flex flex-wrap items-center justify-between gap-2">
              <div className="min-w-0">
                <div className="flex flex-wrap items-center gap-2">
                  <span className="text-sm font-medium text-foreground">{run.suiteName}</span>
                  <span className="text-xs text-faint">·</span>
                  <span className="text-xs text-muted">{run.modelSetName}</span>
                  <StatusPill tone={RUN_STATUS_TONE[run.status]}>{t(`CompareLab.runStatus.${run.status}`)}</StatusPill>
                </div>
                <p className="mt-0.5 text-[11px] text-faint">{formatDateTime(run.createdAt)}</p>
              </div>
              <div className="flex shrink-0 items-center gap-1.5">
                <Button size="sm" variant="secondary" onClick={() => setSelectedRunId(run.id === selectedRunId ? null : run.id)}>
                  {selectedRunId === run.id ? t("CompareLab.hideReport") : t("CompareLab.viewReport")}
                </Button>
                {run.status === "running" ? (
                  <Button size="sm" variant="secondary" onClick={() => stopLabRun(run.id)}>
                    <Square size={12} className="fill-current" /> {t("CompareLab.stopRun")}
                  </Button>
                ) : (
                  <Button size="sm" variant="ghost" onClick={() => removeRun(run.id)}>
                    <Trash2 size={13} /> {t("CompareLab.delete")}
                  </Button>
                )}
              </div>
            </div>
          </div>
        ))}
      </div>

      {/* Keyed by run id so switching to a different run remounts the report
          instead of reusing local state (expanded prompt, status banner)
          from whichever run was previously selected. */}
      {selectedRun && <RunReport key={selectedRun.id} run={selectedRun} />}
    </div>
  );
}

function RunReport({ run }: { run: LabRun }) {
  const { t } = useT();
  const setRubric = useCompareLabStore((s) => s.setRubric);
  const [status, setStatus] = useState<{ tone: "success" | "danger"; message: string } | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [expandedPromptId, setExpandedPromptId] = useState<string | null>(run.prompts[0]?.id ?? null);

  const report = useMemo(() => buildLabReport(run), [run]);

  function resultFor(promptId: string, targetKey: string): LabResult | undefined {
    return run.results.find((r) => r.promptId === promptId && r.targetKey === targetKey);
  }

  async function copyMarkdown() {
    setBusy("copy-md");
    try {
      await navigator.clipboard.writeText(renderLabReportMarkdown(report));
      setStatus({ tone: "success", message: t("CompareLab.copiedMarkdown") });
    } catch (error) {
      setStatus({ tone: "danger", message: errorMessage(error) });
    } finally {
      setBusy(null);
    }
  }

  async function copyJson() {
    setBusy("copy-json");
    try {
      await navigator.clipboard.writeText(renderLabReportJson(report));
      setStatus({ tone: "success", message: t("CompareLab.copiedJson") });
    } catch (error) {
      setStatus({ tone: "danger", message: errorMessage(error) });
    } finally {
      setBusy(null);
    }
  }

  async function exportFile(kind: "markdown" | "json") {
    setBusy(`export-${kind}`);
    try {
      const extension = kind === "markdown" ? "md" : "json";
      const destination = await save({
        defaultPath: `${labRunFileBaseName(run)}.${extension}`,
        filters: [{ name: kind === "markdown" ? "Markdown" : "JSON", extensions: [extension] }],
      });
      if (!destination) return;
      const content = kind === "markdown" ? renderLabReportMarkdown(report) : renderLabReportJson(report);
      await writeTextFile(destination, content);
      setStatus({ tone: "success", message: t("CompareLab.exportComplete") });
    } catch (error) {
      setStatus({ tone: "danger", message: errorMessage(error) });
    } finally {
      setBusy(null);
    }
  }

  function promoteModel(target: ModelTargetSnapshot) {
    setStatus(null);
    void promoteLabModel(target)
      .then(() => setStatus({ tone: "success", message: t("CompareLab.promotedModel", { model: target.displayName }) }))
      .catch((error: unknown) => setStatus({ tone: "danger", message: errorMessage(error) }));
  }

  function promotePrompt(prompt: LabPrompt) {
    setStatus(null);
    try {
      const entry = promoteLabPrompt(prompt, run.suiteName);
      setStatus({ tone: "success", message: t("CompareLab.promotedPrompt", { command: entry.command }) });
    } catch (error) {
      setStatus({ tone: "danger", message: errorMessage(error) });
    }
  }

  function promoteResponse(prompt: LabPrompt, result: LabResult, target: ModelTargetSnapshot) {
    setStatus(null);
    try {
      promoteLabResponse(prompt, result, target);
      setStatus({ tone: "success", message: t("CompareLab.promotedResponse") });
    } catch (error) {
      setStatus({ tone: "danger", message: errorMessage(error) });
    }
  }

  return (
    <section className="flex flex-col gap-3 rounded-xl border border-border bg-surface p-3">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div>
          <h4 className="text-sm font-semibold text-foreground">{t("CompareLab.reportTitle")}</h4>
          <p className="text-[11px] text-faint">
            {formatDateTime(run.createdAt)}
            {run.completedAt !== null ? ` – ${formatDateTime(run.completedAt)}` : ""}
          </p>
        </div>
        <div className="flex flex-wrap items-center gap-1.5">
          <Button size="sm" variant="secondary" disabled={busy !== null} onClick={() => void copyMarkdown()}>
            <Copy size={13} /> {t("CompareLab.copyMarkdown")}
          </Button>
          <Button size="sm" variant="secondary" disabled={busy !== null} onClick={() => void copyJson()}>
            <Copy size={13} /> {t("CompareLab.copyJson")}
          </Button>
          <Button size="sm" variant="secondary" disabled={busy !== null} onClick={() => void exportFile("markdown")}>
            <Download size={13} /> {t("CompareLab.exportMarkdown")}
          </Button>
          <Button size="sm" variant="secondary" disabled={busy !== null} onClick={() => void exportFile("json")}>
            <Download size={13} /> {t("CompareLab.exportJson")}
          </Button>
        </div>
      </div>

      {status && (
        <div
          role={status.tone === "danger" ? "alert" : "status"}
          className={`rounded-md border px-2.5 py-1.5 text-xs ${status.tone === "danger" ? "border-danger/30 bg-danger-soft text-danger" : "border-success/30 bg-success-soft text-success"}`}
        >
          {status.message}
        </div>
      )}

      <div className="overflow-x-auto">
        <table className="w-full min-w-[48rem] border-collapse text-left text-xs">
          <thead>
            <tr className="border-b border-border text-faint">
              <th className="px-2 py-2 font-medium">{t("CompareLab.colModel")}</th>
              <th className="px-2 py-2 font-medium">{t("CompareLab.colCompleted")}</th>
              <th className="px-2 py-2 font-medium">{t("CompareLab.colLatency")}</th>
              <th className="px-2 py-2 font-medium">{t("CompareLab.colTokens")}</th>
              <th className="px-2 py-2 font-medium">{t("CompareLab.colCost")}</th>
              <th className="px-2 py-2 font-medium">{t("CompareLab.colVerifier")}</th>
              <th className="px-2 py-2 font-medium">{t("CompareLab.colToolUse")}</th>
              <th className="px-2 py-2 font-medium">{t("CompareLab.colRubric")}</th>
              <th className="px-2 py-2 font-medium">{t("CompareLab.colActions")}</th>
            </tr>
          </thead>
          <tbody>
            {report.models.map((model) => {
              const target = run.targets.find((t2) => t2.key === model.targetKey);
              return (
                <tr key={model.targetKey} className="border-b border-border">
                  <td className="px-2 py-2 font-medium text-foreground">{model.label}</td>
                  <td className="px-2 py-2 text-foreground">
                    {model.completed}/{model.totalPrompts}
                    {model.failed > 0 ? ` (${model.failed} ${t("CompareLab.failedSuffix")})` : ""}
                  </td>
                  <td className="px-2 py-2 text-foreground">{formatMs(model.avgLatencyMs)}</td>
                  <td className="px-2 py-2 text-foreground">{model.totalTokens}</td>
                  <td className="px-2 py-2 text-foreground">{formatCost(model.totalCostUsd, model.costKnownForAll)}</td>
                  <td className="px-2 py-2 text-foreground">{formatRate(model.verifierPassRate)}</td>
                  <td className="px-2 py-2 text-foreground">{formatRate(model.toolUseSuccessRate)}</td>
                  <td className="px-2 py-2 text-foreground">{model.avgRubricScore !== null ? model.avgRubricScore.toFixed(1) : "—"}</td>
                  <td className="px-2 py-2">
                    {target && (
                      <Button size="sm" variant="ghost" onClick={() => promoteModel(target)}>
                        <Sparkles size={12} /> {t("CompareLab.promoteModelAction")}
                      </Button>
                    )}
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>

      <div className="flex flex-col gap-2">
        {run.prompts.map((prompt, index) => {
          const expanded = expandedPromptId === prompt.id;
          return (
            <div key={prompt.id} className="rounded-lg border border-border bg-background">
              <button type="button" onClick={() => setExpandedPromptId(expanded ? null : prompt.id)} className="flex w-full items-center justify-between gap-2 px-3 py-2 text-left">
                <span className="min-w-0 flex-1 truncate text-sm text-foreground">
                  {index + 1}. {prompt.text}
                </span>
                <span className="flex shrink-0 items-center gap-2">
                  {prompt.toolsEnabled && <StatusPill tone="neutral">{t("CompareLab.toolsOn")}</StatusPill>}
                  {expanded ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
                </span>
              </button>
              {expanded && (
                <div className="flex flex-col gap-2 border-t border-border p-3">
                  <div className="flex items-start justify-between gap-2">
                    <p className="min-w-0 flex-1 whitespace-pre-wrap text-xs leading-relaxed text-muted">{prompt.text}</p>
                    <Button size="sm" variant="ghost" onClick={() => promotePrompt(prompt)}>
                      <Sparkles size={12} /> {t("CompareLab.promotePromptAction")}
                    </Button>
                  </div>
                  <div className="grid gap-2 lg:grid-cols-2">
                    {run.targets.map((target) => {
                      const result = resultFor(prompt.id, target.key);
                      if (!result) return null;
                      return (
                        <ResultCard
                          key={target.key}
                          prompt={prompt}
                          target={target}
                          result={result}
                          onRubricChange={(patch) => setRubric(run.id, prompt.id, target.key, patch)}
                          onPromote={() => promoteResponse(prompt, result, target)}
                        />
                      );
                    })}
                  </div>
                </div>
              )}
            </div>
          );
        })}
      </div>
    </section>
  );
}

function ResultCard({
  prompt,
  target,
  result,
  onRubricChange,
  onPromote,
}: {
  prompt: LabPrompt;
  target: ModelTargetSnapshot;
  result: LabResult;
  onRubricChange: (patch: Partial<LabRubric>) => void;
  onPromote: () => void;
}) {
  const { t } = useT();
  const [notes, setNotes] = useState(result.rubric.notes);

  return (
    <div className="rounded-lg border border-border bg-surface p-2.5">
      <div className="flex items-center justify-between gap-2">
        <span className="truncate text-xs font-medium text-foreground">{target.displayName}</span>
        <StatusPill tone={RESULT_STATUS_TONE[result.status]}>{t(`CompareLab.resultStatus.${result.status}`)}</StatusPill>
      </div>
      <div className="mt-1.5 flex flex-wrap gap-x-3 gap-y-0.5 text-[11px] text-faint">
        <span>
          {t("CompareLab.latencyLabel")}: {formatMs(result.latencyMs)}
        </span>
        <span>
          {t("CompareLab.tokensLabel")}: {result.usage?.totalTokens ?? "—"}
        </span>
        <span>
          {t("CompareLab.costLabel")}: {formatCost(result.costUsd, result.costUsd !== null)}
        </span>
        <span>{result.toolsOffered ? t("CompareLab.toolsOn") : t("CompareLab.toolsOff")}</span>
      </div>
      {result.verifierOutcome && (
        <p className={`mt-1.5 text-[11px] ${result.verifierOutcome.ok ? "text-success" : "text-danger"}`}>
          {result.verifierOutcome.ok ? t("CompareLab.verifierPass") : t("CompareLab.verifierFail")}: {result.verifierOutcome.message}
        </p>
      )}
      {result.toolUseSuccess !== null && (
        <p className={`mt-1 text-[11px] ${result.toolUseSuccess ? "text-success" : "text-danger"}`}>
          {t("CompareLab.toolUseLabel")}: {result.toolUseSuccess ? t("CompareLab.toolUseSuccess") : t("CompareLab.toolUseFailed")} ({result.toolAttempts.length})
        </p>
      )}
      {result.error && <p className="mt-1 text-[11px] text-danger">{result.error}</p>}
      <pre className="mt-2 max-h-40 overflow-auto whitespace-pre-wrap break-words rounded-md bg-background p-2 font-mono text-[11px] text-muted">
        {result.content || t("CompareLab.emptyResponseState")}
      </pre>
      <div className="mt-2 flex flex-wrap items-center gap-2">
        <label className="flex items-center gap-1 text-[11px] text-muted">
          {t("CompareLab.rubricScoreLabel")}
          <select
            value={result.rubric.score ?? ""}
            onChange={(e) => onRubricChange({ score: e.target.value === "" ? null : Number(e.target.value) })}
            className="h-7 rounded-md border border-border bg-background px-1.5 text-xs text-foreground outline-none focus:ring-1 focus:ring-accent"
          >
            <option value="">{t("CompareLab.rubricScoreUnset")}</option>
            {[1, 2, 3, 4, 5].map((n) => (
              <option key={n} value={n}>
                {n}
              </option>
            ))}
          </select>
        </label>
        <Button size="sm" variant="ghost" disabled={result.status !== "completed"} onClick={onPromote}>
          <Sparkles size={12} /> {t("CompareLab.promoteResponseAction")}
        </Button>
      </div>
      <label className="mt-1.5 flex flex-col gap-1 text-[11px] text-muted">
        {t("CompareLab.rubricNotesLabel")}
        <input
          value={notes}
          onChange={(e) => setNotes(e.target.value)}
          onBlur={() => onRubricChange({ notes })}
          className="h-7 rounded-md border border-border bg-background px-1.5 text-xs text-foreground outline-none focus:ring-1 focus:ring-accent"
        />
      </label>
      {prompt.rubricCriteria.length > 0 && (
        <p className="mt-1 text-[10px] text-faint">{t("CompareLab.rubricCriteriaHint", { criteria: prompt.rubricCriteria.join(", ") })}</p>
      )}
    </div>
  );
}

export default CompareLabPanel;
