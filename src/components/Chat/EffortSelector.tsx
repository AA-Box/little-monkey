import { useEffect, useRef, useState } from "react";

import { useModelStore } from "../../store/modelStore";
import type { EffortLevel } from "../../store/modelStore";
import { useT } from "../../lib/i18n";

interface LevelMeta {
  value: EffortLevel;
  labelKey: string;
}

/**
 * Left-to-right = faster/cheaper -> smarter/slower, mirroring Claude Code's
 * own effort slider. Sent to Anthropic as `output_config.effort` (see
 * `providers.rs::build_chat_request`) — every other provider ignores it.
 */
const LEVELS: LevelMeta[] = [
  { value: "low", labelKey: "EffortSelector.levelLow" },
  { value: "medium", labelKey: "EffortSelector.levelMedium" },
  { value: "high", labelKey: "EffortSelector.levelHigh" },
  { value: "xhigh", labelKey: "EffortSelector.levelExtra" },
  { value: "max", labelKey: "EffortSelector.levelMax" },
];

/**
 * Pill button + dropdown slider for Anthropic's `output_config.effort`
 * parameter, mirroring ModeSelector's floating-panel idiom. Only rendered
 * when the active chat target is the Anthropic provider — every other
 * provider either ignores the field or has no equivalent knob, so showing
 * this control there would just be a dead setting.
 */
export function EffortSelector() {
  const activeProvider = useModelStore((s) => s.activeProvider);
  const activeProviderId = useModelStore((s) => s.activeProviderId);
  const effort = useModelStore((s) => s.effort);
  const setEffort = useModelStore((s) => s.setEffort);

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

  if (activeProvider !== "provider" || activeProviderId !== "anthropic") {
    return null;
  }

  const index = Math.max(0, LEVELS.findIndex((l) => l.value === effort));
  const current = LEVELS[index];

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
            max={LEVELS.length - 1}
            step={1}
            value={index}
            onChange={(event) => setEffort(LEVELS[Number(event.target.value)].value)}
            aria-label={t("EffortSelector.effortLevelAriaLabel")}
            className="mt-1 w-full cursor-pointer accent-accent"
          />

          <div className="mt-1 flex justify-between text-[11px] text-faint">
            {LEVELS.map((level) => (
              <span key={level.value} className={level.value === effort ? "text-accent" : undefined}>
                {t(level.labelKey)}
              </span>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}

export default EffortSelector;
