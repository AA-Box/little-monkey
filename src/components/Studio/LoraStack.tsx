import { Plus, Trash2 } from "lucide-react";

import { Button, IconButton } from "../ui";
import { useT } from "../../lib/i18n";
import type { LoraSelection } from "../../lib/studioClient";

/**
 * The LoRA stack for one generation.
 *
 * A stack rather than a slot: the engine takes an unbounded list and
 * deliberately ignores prompt-embedded `<lora:...>` tags, so this is the only
 * route. Strength is free to go negative — subtracting a style is a real thing
 * to want — and `high noise` exists for mixture models whose high-noise stage
 * is a separate network.
 */
export function LoraStack({
  loras,
  onChange,
  showHighNoise,
}: {
  loras: LoraSelection[];
  onChange: (next: LoraSelection[]) => void;
  showHighNoise: boolean;
}) {
  const { t } = useT();

  const patch = (index: number, next: Partial<LoraSelection>) =>
    onChange(loras.map((lora, at) => (at === index ? { ...lora, ...next } : lora)));

  return (
    <div className="grid gap-1.5">
      <span className="text-[11px] text-muted">{t("Studio.lora.title")}</span>
      {loras.map((lora, index) => (
        <div key={index} className="flex flex-wrap items-center gap-1.5">
          <input
            className="min-w-0 flex-1 rounded border border-border bg-background px-2 py-1 font-mono text-[11px] text-foreground"
            value={lora.path}
            placeholder="/Users/you/loras/style.safetensors"
            onChange={(event) => patch(index, { path: event.target.value })}
          />
          <input
            type="number"
            step="0.05"
            min={-10}
            max={10}
            aria-label={t("Studio.lora.strength")}
            className="w-20 rounded border border-border bg-background px-2 py-1 text-[11px] text-foreground"
            value={lora.multiplier}
            onChange={(event) => patch(index, { multiplier: Number(event.target.value) })}
          />
          {showHighNoise && (
            <label className="flex items-center gap-1 text-[11px] text-muted">
              <input
                type="checkbox"
                checked={lora.isHighNoise}
                onChange={(event) => patch(index, { isHighNoise: event.target.checked })}
              />
              {t("Studio.lora.highNoise")}
            </label>
          )}
          <IconButton
            size="sm"
            aria-label={t("Studio.lora.remove")}
            onClick={() => onChange(loras.filter((_, at) => at !== index))}
          >
            <Trash2 size={12} />
          </IconButton>
        </div>
      ))}
      <Button
        size="sm"
        variant="secondary"
        onClick={() =>
          onChange([...loras, { path: "", multiplier: 1, isHighNoise: false }])
        }
      >
        <Plus size={13} />
        {t("Studio.lora.add")}
      </Button>
    </div>
  );
}
