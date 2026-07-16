import { useEffect, useRef, useState } from "react";

import { useModelStore } from "../../store/modelStore";
import type { EffortLevel } from "../../store/modelStore";
import {
  effortForProviderModel,
  effortLevelsForProvider,
  providerModelTargetKey,
} from "../../lib/modelTargets";
import { useT } from "../../lib/i18n";

const LEVEL_LABEL_KEYS: Record<EffortLevel, string> = {
  low: "EffortSelector.levelLow",
  medium: "EffortSelector.levelMedium",
  high: "EffortSelector.levelHigh",
  xhigh: "EffortSelector.levelExtra",
  max: "EffortSelector.levelMax",
};

interface SliderPosition {
  /** `null` is the leading "Default" position: no per-model entry, no effort field sent at all. */
  value: EffortLevel | null;
  labelKey: string;
}

/**
 * Pill button + dropdown slider for the per-model reasoning-effort level,
 * mirroring ModeSelector's floating-panel idiom. Left-to-right = provider
 * default -> faster/cheaper -> smarter/slower, mirroring Claude Code's own
 * effort slider. Only rendered when the active chat target belongs to a
 * provider with an effort knob (see `modelTargets.ts`'s
 * `effortLevelsForProvider`: all five levels for Anthropic, low/medium/high
 * for OpenAI/Gemini/OpenRouter) — custom providers, Ollama, and local
 * llama.cpp have no equivalent, so showing this control there would just be
 * a dead setting. The Rust proxy owns the wire shape per provider (see
 * `providers.rs::build_chat_request`).
 */
export function EffortSelector() {
  const activeProvider = useModelStore((s) => s.activeProvider);
  const activeProviderId = useModelStore((s) => s.activeProviderId);
  const activeProviderModel = useModelStore((s) => s.activeProviderModel);
  const effortByTarget = useModelStore((s) => s.effortByTarget);
  const setEffortForTarget = useModelStore((s) => s.setEffortForTarget);

  const [open, setOpen] = useState(false);
  const containerRef = useRef<HTMLDivElement>(null);
  const { t } = useT();

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

  const levels =
    activeProvider === "provider" && activeProviderId
      ? effortLevelsForProvider(activeProviderId)
      : null;
  if (!levels || !activeProviderId || !activeProviderModel) {
    return null;
  }

  const targetKey = providerModelTargetKey(activeProviderId, activeProviderModel);
  const selected = effortForProviderModel(effortByTarget, activeProviderId, activeProviderModel);
  // A persisted level this provider doesn't offer (only reachable by hand-
  // editing storage) renders as its clamped wire equivalent — the top of
  // this provider's scale, exactly what the Rust proxy would send.
  const effective = selected && !levels.includes(selected) ? levels[levels.length - 1] : selected;

  const positions: SliderPosition[] = [
    { value: null, labelKey: "EffortSelector.levelDefault" },
    ...levels.map((level) => ({ value: level, labelKey: LEVEL_LABEL_KEYS[level] })),
  ];
  const index = Math.max(0, positions.findIndex((position) => position.value === (effective ?? null)));
  const current = positions[index];

  return (
    <div ref={containerRef} className="relative inline-block">
      <button
        type="button"
        onClick={() => setOpen((prev) => !prev)}
        aria-haspopup="true"
        aria-expanded={open}
        className="cursor-pointer text-xs font-mono text-muted hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2 focus-visible:ring-offset-background"
      >
        {t(current.labelKey)}
      </button>

      {open && (
        <div className="absolute bottom-full right-0 z-20 mb-1 w-64 rounded-lg border border-border bg-background p-3 shadow-lg">
          <p className="text-xs text-muted">
            {t("EffortSelector.effortDescription")}
          </p>

          <div className="mt-3 flex items-center justify-between text-[11px] font-medium text-faint">
            <span>{t("EffortSelector.fasterLabel")}</span>
            <span>{t("EffortSelector.smarterLabel")}</span>
          </div>

          <input
            type="range"
            min={0}
            max={positions.length - 1}
            step={1}
            value={index}
            onChange={(event) => setEffortForTarget(targetKey, positions[Number(event.target.value)].value)}
            aria-label={t("EffortSelector.effortLevelAriaLabel")}
            className="mt-1 w-full cursor-pointer accent-accent"
          />

          <div className="mt-1 flex justify-between text-[11px] text-faint">
            {positions.map((position) => (
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
