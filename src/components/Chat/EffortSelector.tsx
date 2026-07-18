import { useEffect, useRef, useState } from "react";
import { HelpCircle } from "lucide-react";

import { useModelStore } from "../../store/modelStore";
import type { EffortLevel } from "../../store/modelStore";
import { effortForProviderModel, providerModelTargetKey } from "../../lib/modelTargets";
import { useT } from "../../lib/i18n";

const LEVEL_LABEL_KEYS: Record<EffortLevel, string> = {
  low: "EffortSelector.levelLow",
  medium: "EffortSelector.levelMedium",
  high: "EffortSelector.levelHigh",
  xhigh: "EffortSelector.levelExtra",
  max: "EffortSelector.levelMax",
};

const ALL_LEVELS: readonly EffortLevel[] = ["low", "medium", "high", "xhigh", "max"];

type SliderValue = EffortLevel | "ultracode" | null;

interface SliderPosition {
  /** `null` is the leading "Default" position: no per-model entry, no effort field sent at all.
   * `"ultracode"` is the trailing position: never persisted via `setEffortForTarget`, never sent
   * to the Rust proxy — it's a frontend-only trigger handled entirely by `onUltracodeChange`. */
  value: SliderValue;
  labelKey: string;
}

const POSITIONS: SliderPosition[] = [
  { value: null, labelKey: "EffortSelector.levelDefault" },
  ...ALL_LEVELS.map((level) => ({ value: level as SliderValue, labelKey: LEVEL_LABEL_KEYS[level] })),
  { value: "ultracode", labelKey: "EffortSelector.levelUltracode" },
];

/** Per-position body copy for the help-icon hover card — each position's
 * wording reflects where it actually applies: Default/Light/Medium/High/
 * Extra/Max persist per model target (`effortByTarget`, every chat using
 * that model), while Ultracode is local `ChatWindow` state (this chat only,
 * cleared on a fresh session) — see the two different persistence paths in
 * `onChange` below. */
const DESCRIPTION_KEYS: Record<string, string> = {
  default: "EffortSelector.effortDescription",
  low: "EffortSelector.descLow",
  medium: "EffortSelector.descMedium",
  high: "EffortSelector.descHigh",
  xhigh: "EffortSelector.descExtra",
  max: "EffortSelector.descMax",
  ultracode: "EffortSelector.ultracodeDescription",
};

function descriptionKeyFor(value: SliderValue): string {
  return DESCRIPTION_KEYS[value ?? "default"];
}

interface EffortSelectorProps {
  /** Whether this turn's send will run Ultracode's auto multi-model
   * comparison + synthesis instead of a normal single-model turn — lifted
   * to `ChatWindow` since it's a send-path decision, not per-model state. */
  ultracodeActive: boolean;
  onUltracodeChange: (active: boolean) => void;
  disabled?: boolean;
}

/**
 * Pill button + dropdown slider for the per-model reasoning-effort level,
 * mirroring ModeSelector's floating-panel idiom. Always shows the same 7
 * fixed stops — Default/Light/Medium/High/Extra/Max/Ultracode — regardless
 * of the active model's provider. Persistence (`setEffortForTarget`) only happens
 * for a "provider"-kind active target (the only kind with a wire effort
 * parameter — see `providers.rs::build_chat_request`); picking a level while
 * on local/Ollama/custom models is visually selectable but a no-op, since
 * there is nowhere to persist it. A level a provider doesn't itself offer
 * (e.g. Extra/Max on OpenAI/Gemini/OpenRouter, which only support 3) is still
 * sent as-is — the Rust proxy already clamps it to that provider's own max
 * (`clamped_reasoning_effort`), so no client-side restriction is needed.
 */
export function EffortSelector({ ultracodeActive, onUltracodeChange, disabled = false }: EffortSelectorProps) {
  const activeProvider = useModelStore((s) => s.activeProvider);
  const activeProviderId = useModelStore((s) => s.activeProviderId);
  const activeProviderModel = useModelStore((s) => s.activeProviderModel);
  const active = useModelStore((s) => s.active);
  const activeOllamaModel = useModelStore((s) => s.activeOllamaModel);
  const effortByTarget = useModelStore((s) => s.effortByTarget);
  const setEffortForTarget = useModelStore((s) => s.setEffortForTarget);

  const [open, setOpen] = useState(false);
  // Holds the picked level when there's nowhere to persist it (no active
  // "provider"-kind target — see `providerActive` below). Without this,
  // `selected` always recomputed to `null` on the next render for every
  // non-provider model, snapping the slider straight back to Default the
  // instant you let go of Light/Medium/High/Extra/Max — only Default and
  // Ultracode ever "stuck". Purely a local-UI nicety: nothing outside this
  // component ever reads it, since `setEffortForTarget` is skipped exactly
  // when this path is used.
  const [localSelection, setLocalSelection] = useState<EffortLevel | null>(null);
  const containerRef = useRef<HTMLDivElement>(null);
  const { t } = useT();

  // Identifies which non-provider target `localSelection` was picked for —
  // `null` while a provider target is active (where it's unused anyway).
  // Without this, switching from one non-provider model to a DIFFERENT one
  // (e.g. local llama.cpp -> a different Ollama model) would keep showing
  // whatever level was picked for the previous model, since this component
  // stays mounted across model switches (see `ChatWindow.tsx`).
  const nonProviderTargetKey =
    activeProvider === "local"
      ? `local:${active?.id ?? ""}`
      : activeProvider === "ollama"
        ? `ollama:${activeOllamaModel ?? ""}`
        : null;

  useEffect(() => {
    setLocalSelection(null);
  }, [nonProviderTargetKey]);

  useEffect(() => {
    if (!open) return;
    function handlePointerDown(event: PointerEvent) {
      if (containerRef.current && !containerRef.current.contains(event.target as Node)) {
        setOpen(false);
      }
    }
    window.addEventListener("pointerdown", handlePointerDown);
    return () => window.removeEventListener("pointerdown", handlePointerDown);
  }, [open]);

  const providerActive = activeProvider === "provider" && activeProviderId !== null && activeProviderModel !== null;
  const targetKey = providerActive ? providerModelTargetKey(activeProviderId!, activeProviderModel!) : null;
  const selected = providerActive
    ? effortForProviderModel(effortByTarget, activeProviderId!, activeProviderModel!)
    : localSelection;

  const index = ultracodeActive
    ? POSITIONS.length - 1
    : Math.max(0, POSITIONS.findIndex((position) => position.value === selected));
  const current = POSITIONS[index];
  // Default (`current.value === null`) reads as a quiet neutral pill, same as
  // its sibling pickers (Compare/Crew/Knowledge); any real selection —
  // including Ultracode — reads as a solid accent-filled pill so it's
  // obvious at a glance that something other than "leave it to the provider"
  // is active for this turn.
  const isActive = current.value !== null;

  return (
    <div ref={containerRef} className="relative inline-block">
      <button
        type="button"
        onClick={() => setOpen((prev) => !prev)}
        disabled={disabled}
        aria-haspopup="true"
        aria-expanded={open}
        className={`inline-flex items-center gap-1.5 rounded-full px-2.5 py-1 text-xs font-medium transition-colors duration-150 cursor-pointer disabled:cursor-not-allowed disabled:opacity-50 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2 focus-visible:ring-offset-background ${
          isActive
            ? "bg-accent text-accent-foreground hover:bg-accent-hover"
            : "bg-surface-2 text-muted hover:bg-surface hover:text-foreground"
        }`}
      >
        {t(current.labelKey)}
      </button>

      {open && (
        <div className="absolute bottom-full right-0 z-20 mb-1 w-80 rounded-lg border border-border bg-background p-3 shadow-lg">
          <div className="flex items-center justify-between gap-2">
            <p className="text-sm">
              <span className="text-muted">{t("EffortSelector.panelTitle")}</span>{" "}
              <span className="font-semibold text-accent">{t(current.labelKey)}</span>
            </p>
            {/* Hover-only affordance, not a button — matches the reference:
                no click target, just a tooltip-style card that appears above
                on hover (mouse) or focus (keyboard), naming and explaining
                whichever position is currently selected. */}
            <div
              tabIndex={0}
              className="group/help relative flex h-5 w-5 shrink-0 items-center justify-center rounded-full text-faint focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2 focus-visible:ring-offset-background"
            >
              <HelpCircle size={15} aria-hidden="true" />
              <div
                role="tooltip"
                className="pointer-events-none absolute bottom-full right-0 z-30 mb-2 hidden w-64 rounded-lg border border-border bg-background p-3 text-left shadow-lg group-hover/help:block group-focus/help:block"
              >
                <p className="text-sm font-semibold text-foreground">{t(current.labelKey)}</p>
                <p className="mt-1 text-xs text-muted">{t(descriptionKeyFor(current.value))}</p>
              </div>
            </div>
          </div>

          <div className="mt-3 flex items-center justify-between text-[11px] font-medium text-faint">
            <span>{t("EffortSelector.fasterLabel")}</span>
            <span>{t("EffortSelector.smarterLabel")}</span>
          </div>

          <input
            type="range"
            min={0}
            max={POSITIONS.length - 1}
            step={1}
            value={index}
            disabled={disabled}
            onChange={(event) => {
              const next = POSITIONS[Number(event.target.value)];
              if (next.value === "ultracode") {
                onUltracodeChange(true);
                return;
              }
              onUltracodeChange(false);
              if (targetKey) {
                setEffortForTarget(targetKey, next.value);
              } else {
                setLocalSelection(next.value);
              }
            }}
            aria-label={t("EffortSelector.effortLevelAriaLabel")}
            className="effort-range mt-1"
          />

          <div className="mt-1 flex flex-wrap justify-between gap-x-1.5 gap-y-0.5 text-[10px] text-faint">
            {POSITIONS.map((position) => (
              <span
                key={position.value ?? "default"}
                className={position === current ? "text-accent" : undefined}
              >
                {t(position.labelKey)}
              </span>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}

export default EffortSelector;
