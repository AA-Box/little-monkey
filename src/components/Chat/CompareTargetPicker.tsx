import { useCallback, useEffect, useId, useMemo, useRef, useState } from "react";
import { ChevronDown, Columns2, Eye, Search, Wrench } from "lucide-react";
import { useShallow } from "zustand/react/shallow";

import {
  buildModelTargetInventory,
  findActiveModelTarget,
  MAX_COMPARISON_TARGETS,
  MIN_COMPARISON_TARGETS,
  type ModelTargetSnapshot,
  validateComparisonTargets,
} from "../../lib/modelTargets";
import {
  buildComparisonExecutionPlan,
  loadSystemMemoryInfo,
  type SystemMemoryInfo,
} from "../../lib/comparisonPlan";
import { useT } from "../../lib/i18n";
import { useModelStore } from "../../store/modelStore";
import {
  DEFAULT_PROVIDER_MODEL_FILTER,
  useSettingsStore,
} from "../../store/settingsStore";
import { Button } from "../ui/Button";

export interface CompareTargetPickerProps {
  value: readonly ModelTargetSnapshot[];
  onChange: (next: ModelTargetSnapshot[]) => void;
  disabled?: boolean;
  /** Which way the panel opens relative to the trigger. "up" suits the
   * composer-footer placement (default); "down" the title-bar placement,
   * where an upward panel would clip past the window's top edge. */
  placement?: "up" | "down";
}

type CapabilityKind = "tools" | "vision";

function formatBytes(value: number): string {
  const gib = value / 1024 ** 3;
  return `${gib.toFixed(gib < 10 ? 1 : 0)} GB`;
}

function CapabilityBadge({
  kind,
  capability,
}: {
  kind: CapabilityKind;
  capability: ModelTargetSnapshot["capabilities"]["toolCalling"];
}) {
  const { t } = useT();
  const Icon = kind === "tools" ? Wrench : Eye;
  const label = t(kind === "tools" ? "ComparePicker.toolsBadge" : "ComparePicker.visionBadge");
  const stateLabel = t(
    capability.state === "yes"
      ? "ComparePicker.capabilitySupported"
      : capability.state === "no"
        ? "ComparePicker.capabilityUnsupported"
        : "ComparePicker.capabilityUnknown",
  );
  const classes =
    capability.state === "yes"
      ? "bg-accent-soft text-accent"
      : capability.state === "no"
        ? "bg-surface-2 text-faint"
        : "border border-border bg-background text-muted";

  return (
    <span
      aria-label={t("ComparePicker.capabilityAriaLabel", { capability: label, state: stateLabel })}
      title={t("ComparePicker.capabilityTitle", {
        capability: label,
        state: stateLabel,
        evidence: capability.evidence,
      })}
      className={`inline-flex items-center gap-1 rounded-full px-1.5 py-0.5 text-[10px] font-medium ${classes}`}
    >
      <Icon size={10} aria-hidden="true" />
      <span>{label}</span>
      {capability.state === "unknown" && <span aria-hidden="true">?</span>}
    </span>
  );
}

function targetSearchText(target: ModelTargetSnapshot): string {
  if (target.kind === "provider") {
    return `${target.label} ${target.providerId} ${target.displayName} ${target.model}`.toLocaleLowerCase();
  }
  if (target.kind === "ollama") {
    return `${target.label} ollama ${target.displayName} ${target.model}`.toLocaleLowerCase();
  }
  return `${target.label} local llama.cpp ${target.displayName} ${target.modelId}`.toLocaleLowerCase();
}

/**
 * Read-only model checklist for configuring a 2–4-way comparison. Selection
 * lives in a draft until Apply is pressed, and merely opening this control
 * never starts or switches a runtime.
 */
export function CompareTargetPicker({ value, onChange, disabled = false, placement = "up" }: CompareTargetPickerProps) {
  const { t } = useT();
  const containerRef = useRef<HTMLDivElement>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const searchRef = useRef<HTMLInputElement>(null);
  const panelId = useId();
  const titleId = useId();

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
      activeProvider: state.activeProvider,
      activeOllamaModel: state.activeOllamaModel,
      activeProviderId: state.activeProviderId,
      activeProviderModel: state.activeProviderModel,
    })),
  );
  const providerModelFilters = useSettingsStore((state) => state.providerModelFilters);

  const inventory = useMemo(
    () =>
      buildModelTargetInventory({
        installed: modelState.installed,
        active: modelState.active,
        llamaStatus: modelState.llamaStatus,
        ollamaModels: modelState.ollamaModels,
        ollamaReachable: modelState.ollamaReachable,
        providers: modelState.providers,
        providerModels: modelState.providerModels,
        effortByTarget: modelState.effortByTarget,
      }),
    [modelState],
  );

  const [open, setOpen] = useState(false);
  const [draft, setDraft] = useState<ModelTargetSnapshot[]>([]);
  const [query, setQuery] = useState("");
  const [limitError, setLimitError] = useState(false);
  const [memoryInfo, setMemoryInfo] = useState<SystemMemoryInfo | null>(null);
  const [memoryChecked, setMemoryChecked] = useState(false);

  const discardDraft = useCallback((restoreFocus = false) => {
    setOpen(false);
    setDraft([]);
    setQuery("");
    setLimitError(false);
    if (restoreFocus) requestAnimationFrame(() => triggerRef.current?.focus());
  }, []);

  useEffect(() => {
    if (!open) return;

    const focusFrame = requestAnimationFrame(() => searchRef.current?.focus());
    function handlePointerDown(event: PointerEvent) {
      if (containerRef.current && !containerRef.current.contains(event.target as Node)) {
        discardDraft(false);
      }
    }
    function handleKeyDown(event: KeyboardEvent) {
      if (event.key !== "Escape") return;
      event.preventDefault();
      discardDraft(true);
    }

    window.addEventListener("pointerdown", handlePointerDown);
    window.addEventListener("keydown", handleKeyDown);
    return () => {
      cancelAnimationFrame(focusFrame);
      window.removeEventListener("pointerdown", handlePointerDown);
      window.removeEventListener("keydown", handleKeyDown);
    };
  }, [discardDraft, open]);

  useEffect(() => {
    if (!open) return;
    let active = true;
    setMemoryChecked(false);
    void loadSystemMemoryInfo().then((memory) => {
      if (!active) return;
      setMemoryInfo(memory);
      setMemoryChecked(true);
    });
    return () => {
      active = false;
    };
  }, [open]);

  function handleTrigger() {
    if (open) {
      discardDraft(false);
      return;
    }

    let initial = value.map(
      (savedTarget) => inventory.targets.find((target) => target.key === savedTarget.key) ?? savedTarget,
    );
    if (initial.length === 0) {
      const active = findActiveModelTarget(inventory, modelState);
      if (active?.availability.status === "available") initial = [active];
    }
    setDraft(initial);
    setQuery("");
    setLimitError(false);
    setOpen(true);
  }

  function toggleTarget(target: ModelTargetSnapshot) {
    if (target.availability.status !== "available") return;
    const selected = draft.some((item) => item.key === target.key);
    if (selected) {
      setDraft((current) => current.filter((item) => item.key !== target.key));
      setLimitError(false);
      return;
    }
    if (draft.length >= MAX_COMPARISON_TARGETS) {
      setLimitError(true);
      return;
    }
    setDraft((current) => [...current, target]);
    setLimitError(false);
  }

  function handleApply() {
    const currentTargets = draft.map((target) =>
      inventory.targets.find((candidate) => candidate.key === target.key),
    );
    if (currentTargets.some((target) => !target || target.availability.status !== "available")) return;
    const availableTargets = currentTargets.filter((target): target is ModelTargetSnapshot => Boolean(target));
    const validation = validateComparisonTargets(availableTargets);
    if (!validation.valid) return;
    onChange(availableTargets);
    discardDraft(true);
  }

  function handleClear() {
    onChange([]);
    discardDraft(true);
  }

  const normalizedQuery = query.trim().toLocaleLowerCase();
  const draftKeys = new Set(draft.map((target) => target.key));
  const visibleGroups = inventory.groups
    .map((group) => {
      const targets = group.targets.filter((target) => {
        if (target.kind === "provider") {
          const filter = providerModelFilters[target.providerId] ?? DEFAULT_PROVIDER_MODEL_FILTER;
          const curated =
            filter.showAll ||
            filter.selectedModelIds.length === 0 ||
            filter.selectedModelIds.includes(target.model) ||
            draftKeys.has(target.key);
          if (!curated) return false;
        }
        return normalizedQuery.length === 0 || targetSearchText(target).includes(normalizedQuery);
      });
      return { ...group, targets };
    })
    .filter((group) => group.targets.length > 0);

  const validation = validateComparisonTargets(draft);
  const executionPreview = useMemo(
    () => buildComparisonExecutionPlan(draft, memoryChecked ? memoryInfo : null),
    [draft, memoryChecked, memoryInfo],
  );
  const hasUnavailableDraft = draft.some((target) => {
    const current = inventory.targets.find((candidate) => candidate.key === target.key);
    return !current || current.availability.status !== "available";
  });
  const canApply = validation.valid && !hasUnavailableDraft;
  const triggerLabel =
    value.length === 0
      ? t("ComparePicker.compareLabel")
      : t("ComparePicker.compareCountLabel", { count: value.length });

  return (
    <div ref={containerRef} className="relative inline-block">
      <button
        ref={triggerRef}
        type="button"
        disabled={disabled}
        onClick={handleTrigger}
        aria-haspopup="dialog"
        aria-expanded={open}
        aria-controls={open ? panelId : undefined}
        className="inline-flex items-center gap-1.5 rounded-full bg-surface-2 px-2.5 py-1 text-xs font-medium text-muted transition-colors duration-150 cursor-pointer hover:bg-surface hover:text-foreground disabled:cursor-not-allowed disabled:opacity-50 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2 focus-visible:ring-offset-background"
      >
        <Columns2 size={13} aria-hidden="true" />
        <span>{triggerLabel}</span>
        <ChevronDown size={12} aria-hidden="true" className={`transition-transform ${open ? "rotate-180" : ""}`} />
      </button>

      {open && (
        <div
          id={panelId}
          role="dialog"
          aria-labelledby={titleId}
          className={`absolute z-30 flex max-h-[min(36rem,75vh)] w-[23rem] max-w-[calc(100vw-2rem)] flex-col overflow-hidden rounded-xl border border-border bg-background shadow-2xl ${
            placement === "up" ? "bottom-full right-0 mb-2" : "top-full left-0 mt-2"
          }`}
        >
          <div className="border-b border-border px-3.5 pb-3 pt-3.5">
            <h2 id={titleId} className="text-sm font-semibold text-foreground">
              {t("ComparePicker.title")}
            </h2>
            <p className="mt-0.5 text-xs leading-relaxed text-muted">{t("ComparePicker.description")}</p>

            <label className="relative mt-3 block">
              <span className="sr-only">{t("ComparePicker.searchLabel")}</span>
              <Search
                size={14}
                aria-hidden="true"
                className="pointer-events-none absolute left-2.5 top-1/2 -translate-y-1/2 text-faint"
              />
              <input
                ref={searchRef}
                type="search"
                value={query}
                onChange={(event) => setQuery(event.target.value)}
                placeholder={t("ComparePicker.searchPlaceholder")}
                className="h-8 w-full rounded-lg border border-border bg-surface-2 pl-8 pr-3 text-sm text-foreground placeholder:text-faint focus:outline-none focus:ring-1 focus:ring-accent"
              />
            </label>
          </div>

          <div className="min-h-0 flex-1 overflow-y-auto py-1 [overscroll-behavior:contain]">
            {visibleGroups.length === 0 ? (
              <div className="px-4 py-8 text-center">
                <p className="text-sm font-medium text-foreground">{t("ComparePicker.noResultsTitle")}</p>
                <p className="mt-1 text-xs text-muted">{t("ComparePicker.noResultsDescription")}</p>
              </div>
            ) : (
              visibleGroups.map((group) => (
                <fieldset key={group.key} className="py-1">
                  <legend className="w-full px-3.5 pb-1 pt-2 text-[11px] font-semibold uppercase tracking-wider text-faint">
                    {group.kind === "local"
                      ? t("ComparePicker.localSectionLabel")
                      : group.kind === "ollama"
                        ? t("ComparePicker.ollamaSectionLabel")
                        : group.label}
                  </legend>

                  {group.targets.map((target) => {
                    const checked = draftKeys.has(target.key);
                    const available = target.availability.status === "available";
                    return (
                      <label
                        key={target.key}
                        className={`mx-1.5 flex items-start gap-2.5 rounded-lg px-2 py-2 text-left transition-colors ${
                          available
                            ? "cursor-pointer hover:bg-surface-2"
                            : "cursor-not-allowed bg-surface/40 opacity-70"
                        } ${checked ? "bg-accent-soft/60" : ""}`}
                      >
                        <input
                          type="checkbox"
                          checked={checked}
                          disabled={!available}
                          onChange={() => toggleTarget(target)}
                          className="mt-1 h-4 w-4 shrink-0 cursor-pointer accent-accent disabled:cursor-not-allowed"
                        />
                        <span className="min-w-0 flex-1">
                          <span className="flex min-w-0 items-center gap-1.5">
                            <span className="min-w-0 flex-1 truncate text-sm font-medium text-foreground">
                              {target.displayName}
                            </span>
                            {!available && (
                              <span className="shrink-0 rounded-full bg-danger-soft px-1.5 py-0.5 text-[10px] font-medium text-danger">
                                {t("ComparePicker.unavailableBadge")}
                              </span>
                            )}
                          </span>
                          {!available && (
                            <span className="mt-0.5 block text-[11px] leading-snug text-danger">
                              {target.availability.evidence}
                            </span>
                          )}
                          <span className="mt-1.5 flex flex-wrap gap-1">
                            <CapabilityBadge kind="tools" capability={target.capabilities.toolCalling} />
                            <CapabilityBadge kind="vision" capability={target.capabilities.vision} />
                            {typeof target.estimatedMemoryBytes === "number" && target.estimatedMemoryBytes > 0 && (
                              <span
                                className="rounded-full border border-border bg-background px-1.5 py-0.5 text-[10px] font-medium text-muted"
                                title={t("ComparePicker.memoryEstimateTitle")}
                              >
                                {t("ComparePicker.memoryEstimate", {
                                  amount: formatBytes(target.estimatedMemoryBytes),
                                })}
                              </span>
                            )}
                          </span>
                        </span>
                      </label>
                    );
                  })}
                </fieldset>
              ))
            )}
          </div>

          <div className="border-t border-border bg-surface/30 px-3.5 py-3">
            {draft.length >= MIN_COMPARISON_TARGETS && executionPreview.localTargetKeys.length > 1 && (
              <div
                className={`mb-2.5 rounded-lg border px-2.5 py-2 text-[11px] leading-relaxed ${
                  executionPreview.mode === "local_sequential"
                    ? "border-warning/40 bg-warning-soft text-warning"
                    : "border-border bg-background text-muted"
                }`}
                role="status"
                aria-live="polite"
              >
                {!memoryChecked
                  ? t("ComparePicker.memoryChecking")
                  : executionPreview.mode === "local_sequential"
                    ? t("ComparePicker.memoryQueued", {
                        estimate:
                          executionPreview.estimatedLocalBytes === null
                            ? t("ComparePicker.memoryUnknown")
                            : formatBytes(executionPreview.estimatedLocalBytes),
                        available:
                          executionPreview.availableMemoryBytes === null
                            ? t("ComparePicker.memoryUnknown")
                            : formatBytes(executionPreview.availableMemoryBytes),
                      })
                    : t("ComparePicker.memoryConcurrent", {
                        estimate: formatBytes(executionPreview.estimatedLocalBytes ?? 0),
                        available: formatBytes(executionPreview.availableMemoryBytes ?? 0),
                      })}
              </div>
            )}
            <div className="mb-2.5 flex items-start justify-between gap-3 text-[11px]">
              <span className="font-medium text-muted">
                {t("ComparePicker.selectedCount", {
                  count: draft.length,
                  max: MAX_COMPARISON_TARGETS,
                })}
              </span>
              <span
                className={limitError || hasUnavailableDraft ? "text-danger" : "text-faint"}
                role={limitError || hasUnavailableDraft ? "alert" : "status"}
                aria-live="polite"
              >
                {limitError
                  ? t("ComparePicker.maximumSelectionError", { max: MAX_COMPARISON_TARGETS })
                  : hasUnavailableDraft
                    ? t("ComparePicker.unavailableSelectionError")
                    : draft.length < MIN_COMPARISON_TARGETS
                      ? t("ComparePicker.minimumSelectionHint", { min: MIN_COMPARISON_TARGETS })
                      : t("ComparePicker.readyHint")}
              </span>
            </div>

            <div className="flex items-center justify-between gap-2">
              <Button type="button" variant="ghost" size="sm" onClick={handleClear}>
                {t("ComparePicker.normalChatAction")}
              </Button>
              <div className="flex items-center gap-1.5">
                <Button type="button" variant="secondary" size="sm" onClick={() => discardDraft(true)}>
                  {t("ComparePicker.cancelAction")}
                </Button>
                <Button type="button" variant="primary" size="sm" disabled={!canApply} onClick={handleApply}>
                  {t("ComparePicker.applyAction")}
                </Button>
              </div>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

export default CompareTargetPicker;
