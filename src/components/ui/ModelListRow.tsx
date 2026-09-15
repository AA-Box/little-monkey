import type { ReactNode } from "react";
import { Trash2 } from "lucide-react";
import { IconButton } from "./IconButton";
import { StatusPill } from "./StatusPill";
import { useT } from "../../lib/i18n";

export interface ModelListRowProps {
  /** Primary label — a model name/tag/id, shown in monospace. */
  title: string;
  /** Small muted line under the title (size, provider label, etc.). */
  subtitle?: ReactNode;
  /** Optional pill/badge rendered next to the title (e.g. "CLOUD"). */
  badge?: ReactNode;
  isActive: boolean;
  onUse: () => void;
  /** When provided, renders a delete icon button (e.g. to remove a pulled Ollama model). */
  onRemove?: () => void;
}

/**
 * A single "model you can switch to" row: title + optional badge/subtitle,
 * and an optional delete button on the right. The row is the control —
 * clicking it switches to that model — so the active one is marked with a
 * pill rather than a disabled button. Shared by `OllamaModelList` and
 * `ProviderCard`'s per-provider model list so they stay visually and
 * behaviorally identical.
 */
export function ModelListRow({ title, subtitle, badge, isActive, onUse, onRemove }: ModelListRowProps) {
  const { t } = useT();
  return (
    <div
      className={`flex items-center justify-between gap-3 rounded-lg border bg-background p-3 transition-colors hover:border-border-strong ${
        isActive ? "border-l-2 border-l-accent border-border pl-2.5" : "border-border"
      }`}
    >
      {/* The row itself selects the model. A separate Use button next to a row
          that does nothing when clicked is one more thing to aim at for an
          action the row already implies — so the button is gone and the text
          block is the control. It stays a real <button> rather than a click
          handler on the wrapper so it keeps focus, Enter/Space and a name;
          Remove sits outside it, because interactive elements must not nest. */}
      <button
        type="button"
        onClick={onUse}
        disabled={isActive}
        aria-current={isActive ? "true" : undefined}
        title={isActive ? t("ModelListRow.activeButton") : t("ModelListRow.useButton")}
        className="min-w-0 flex-1 rounded text-left focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent disabled:cursor-default"
      >
        <div className="flex flex-wrap items-center gap-2">
          <h3 className="truncate font-mono text-sm text-foreground">{title}</h3>
          {badge}
          {isActive && <StatusPill tone="success">{t("ModelListRow.activeButton")}</StatusPill>}
        </div>
        {subtitle && <p className="mt-0.5 truncate font-mono text-xs text-muted">{subtitle}</p>}
      </button>

      <div className="flex shrink-0 items-center gap-1.5">
        {onRemove && (
          <IconButton variant="ghost" size="sm" aria-label={t("ModelListRow.removeAriaLabel", { title })} onClick={onRemove}>
            <Trash2 size={14} />
          </IconButton>
        )}
      </div>
    </div>
  );
}
