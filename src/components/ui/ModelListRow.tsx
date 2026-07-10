import type { ReactNode } from "react";
import { Trash2 } from "lucide-react";
import { Button } from "./Button";
import { IconButton } from "./IconButton";
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
 * A single "model you can switch to" row: title + optional badge/subtitle
 * on the left, an Active/Use button on the right. Shared by `OllamaModelList`
 * and `ProviderCard`'s per-provider model list so they stay visually and
 * behaviorally identical.
 */
export function ModelListRow({ title, subtitle, badge, isActive, onUse, onRemove }: ModelListRowProps) {
  const { t } = useT();
  return (
    <div className="flex items-center justify-between gap-3 rounded-lg border border-border bg-background p-3 transition-colors hover:border-border-strong">
      <div className="min-w-0">
        <div className="flex flex-wrap items-center gap-2">
          <h3 className="truncate font-mono text-sm text-foreground">{title}</h3>
          {badge}
        </div>
        {subtitle && <p className="mt-0.5 truncate font-mono text-xs text-muted">{subtitle}</p>}
      </div>

      <div className="flex shrink-0 items-center gap-1.5">
        <Button
          variant={isActive ? "primary" : "secondary"}
          size="sm"
          disabled={isActive}
          onClick={onUse}
        >
          {isActive ? t("ModelListRow.activeButton") : t("ModelListRow.useButton")}
        </Button>
        {onRemove && (
          <IconButton variant="ghost" size="sm" aria-label={t("ModelListRow.removeAriaLabel", { title })} onClick={onRemove}>
            <Trash2 size={14} />
          </IconButton>
        )}
      </div>
    </div>
  );
}
