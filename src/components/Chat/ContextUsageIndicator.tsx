import { useEffect, useRef, useState } from "react";

import { useT } from "../../lib/i18n";
import { useUsageStore } from "../../store/usageStore";

function formatNumber(value: number): string {
  return value.toLocaleString("en-US");
}

/** Percentage of the context window used so far, rounded to the nearest integer. */
function percentUsed(totalTokens: number, contextLimit: number): number {
  return Math.round((totalTokens / contextLimit) * 100);
}

/** Text color for the trigger/summary line, given a known percentage. */
function percentTextClass(percent: number): string {
  if (percent >= 90) return "text-danger";
  if (percent >= 70) return "text-warning";
  return "text-muted";
}

/** Fill color for the progress bar, given a known percentage. */
function percentBarClass(percent: number): string {
  if (percent >= 90) return "bg-danger";
  if (percent >= 70) return "bg-warning";
  return "bg-accent";
}

/** Stroke color (as a CSS var reference) for the ring, given a known percentage. */
function percentStrokeVar(percent: number): string {
  if (percent >= 90) return "var(--c-danger)";
  if (percent >= 70) return "var(--c-warning)";
  return "var(--c-accent)";
}

const RING_RADIUS = 7;
const RING_CIRCUMFERENCE = 2 * Math.PI * RING_RADIUS;

/**
 * Small circular progress ring showing context-window usage at a glance
 * (mirrors the tiny ring indicator in Claude Desktop's composer), instead of
 * a bare percentage number. `percent` is null when there's nothing to
 * visualize yet (no usage sample or unknown context limit) - renders an
 * empty/neutral track only in that case.
 */
function UsageRing({ percent }: { percent: number | null }) {
  const clamped = percent === null ? 0 : Math.min(100, Math.max(0, percent));
  const offset = RING_CIRCUMFERENCE * (1 - clamped / 100);

  return (
    <svg width="16" height="16" viewBox="0 0 16 16" className="shrink-0 -rotate-90">
      <circle cx="8" cy="8" r={RING_RADIUS} fill="none" strokeWidth="2" className="stroke-border" />
      {percent !== null && (
        <circle
          cx="8"
          cy="8"
          r={RING_RADIUS}
          fill="none"
          strokeWidth="2"
          strokeLinecap="round"
          strokeDasharray={RING_CIRCUMFERENCE}
          strokeDashoffset={offset}
          style={{ stroke: percentStrokeVar(percent) }}
        />
      )}
    </svg>
  );
}

/**
 * Small trigger button + popover showing context-window token usage for the
 * most recently completed turn, read from `usageStore`. Deliberately shows
 * ONLY context-window usage (tokens used vs. the active model's context
 * size) — there is no subscription/rate-limit concept for a locally-run
 * model, so no such data is fabricated or displayed here.
 */
export function ContextUsageIndicator() {
  const lastUsage = useUsageStore((s) => s.lastUsage);
  const contextLimit = useUsageStore((s) => s.contextLimit);
  const { t } = useT();

  const [open, setOpen] = useState(false);
  const containerRef = useRef<HTMLDivElement>(null);

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

  const hasLimit = typeof contextLimit === "number" && contextLimit > 0;
  const ringPercent = lastUsage && hasLimit ? percentUsed(lastUsage.totalTokens, contextLimit as number) : null;

  let triggerLabel: string;
  let triggerClass: string;

  if (!lastUsage) {
    triggerLabel = t("ContextUsageIndicator.zeroTokens");
    triggerClass = "text-muted";
  } else if (ringPercent !== null) {
    triggerLabel = t("ContextUsageIndicator.percentUsed", { percent: ringPercent });
    triggerClass = percentTextClass(ringPercent);
  } else {
    triggerLabel = t("ContextUsageIndicator.tokensCount", { count: formatNumber(lastUsage.totalTokens) });
    triggerClass = "text-muted";
  }

  return (
    <div ref={containerRef} className="relative inline-block">
      <button
        type="button"
        onClick={() => setOpen((prev) => !prev)}
        aria-haspopup="true"
        aria-expanded={open}
        aria-label={t("ContextUsageIndicator.contextWindowUsageAriaLabel", { label: triggerLabel })}
        title={triggerLabel}
        className={`flex cursor-pointer items-center transition-colors duration-150 hover:text-foreground ${triggerClass}`}
      >
        <UsageRing percent={ringPercent} />
      </button>

      {open && (
        <div className="absolute bottom-full right-0 z-20 mb-1 w-64 rounded-lg border border-border bg-background p-3 shadow-lg">
          <p className="text-sm font-semibold text-foreground">{t("ContextUsageIndicator.contextWindowHeading")}</p>

          {!lastUsage ? (
            <p className="mt-1.5 text-xs text-faint">{t("ContextUsageIndicator.noMessagesYet")}</p>
          ) : typeof contextLimit === "number" && contextLimit > 0 ? (
            (() => {
              const percent = percentUsed(lastUsage.totalTokens, contextLimit);
              const clampedPercent = Math.min(100, Math.max(0, percent));
              return (
                <>
                  <div className="mt-2 h-1.5 w-full overflow-hidden rounded-full bg-surface-2">
                    <div
                      className={`h-full rounded-full ${percentBarClass(percent)}`}
                      style={{ width: `${clampedPercent}%` }}
                    />
                  </div>
                  <p className="mt-1.5 text-xs text-muted">
                    {t("ContextUsageIndicator.tokensUsageDetail", {
                      used: formatNumber(lastUsage.totalTokens),
                      limit: formatNumber(contextLimit),
                      percent,
                    })}
                  </p>
                </>
              );
            })()
          ) : (
            <p className="mt-1.5 text-xs text-muted">
              {t("ContextUsageIndicator.tokensUsedLimitUnknown", { count: formatNumber(lastUsage.totalTokens) })}
            </p>
          )}
        </div>
      )}
    </div>
  );
}

export default ContextUsageIndicator;
