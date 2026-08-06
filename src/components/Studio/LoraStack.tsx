import { Plus, Trash2 } from "lucide-react";

import { Button, IconButton } from "../ui";
import { useT } from "../../lib/i18n";
import type { LoraAsset, LoraSelection } from "../../lib/studioClient";

/**
 * The LoRA stack for one generation.
 *
 * A stack rather than a slot: the engine takes an unbounded list and
 * deliberately ignores prompt-embedded `<lora:...>` tags, so this is the only
 * route. Strength is free to go negative — subtracting a style is a real thing
 * to want — and `high noise` exists for mixture models whose high-noise stage
 * is a separate network.
 *
 * Every row picks from the library rather than taking a path. A LoRA is used
 * across many runs, and retyping an absolute path for each of them is not a
 * choice anyone is making on purpose.
 */
export function LoraStack({
  loras,
  library,
  onChange,
  showHighNoise,
}: {
  loras: LoraSelection[];
  library: LoraAsset[];
  onChange: (next: LoraSelection[]) => void;
  showHighNoise: boolean;
}) {
  const { t } = useT();

  const patch = (index: number, next: Partial<LoraSelection>) =>
    onChange(loras.map((lora, at) => (at === index ? { ...lora, ...next } : lora)));

  /** The first library entry not already stacked, so adding two rows does not
   *  silently apply the same LoRA twice. */
  const nextUnused = () =>
    library.find((asset) => !loras.some((lora) => lora.path === asset.path)) ?? library[0];

  if (library.length === 0) {
    return <p className="text-[11px] text-faint">{t("Studio.lora.empty")}</p>;
  }

  return (
    <div className="grid gap-1.5">
      {loras.map((lora, index) => (
        <div key={index} className="grid gap-1.5 rounded bg-background/60 p-2">
          <div className="flex items-center gap-1.5">
            {/* Native on purpose: this sits inside the rail, which scrolls and
                therefore clips, and a LoRA name is short enough that a popup
                sized to the widest one is the right shape. */}
            <select
              className="min-w-0 flex-1 rounded border border-border bg-background px-1.5 py-1 text-[11px] text-foreground"
              aria-label={t("Studio.lora.pick")}
              value={lora.path}
              onChange={(event) => patch(index, { path: event.target.value })}
            >
              {/* A LoRA forgotten from the library while still stacked stays
                  selectable rather than silently becoming a different one. */}
              {(library.some((asset) => asset.path === lora.path)
                ? library
                : [{ name: lora.path, path: lora.path }, ...library]
              ).map((asset) => (
                <option key={asset.path} value={asset.path}>
                  {asset.name}
                </option>
              ))}
            </select>
            <input
              type="number"
              step="0.05"
              min={-10}
              max={10}
              aria-label={t("Studio.lora.strength")}
              className="w-16 shrink-0 rounded border border-border bg-background px-1.5 py-1 text-center text-[11px] text-foreground"
              value={lora.multiplier}
              onChange={(event) => patch(index, { multiplier: Number(event.target.value) })}
            />
            <IconButton
              size="sm"
              aria-label={t("Studio.lora.remove")}
              onClick={() => onChange(loras.filter((_, at) => at !== index))}
            >
              <Trash2 size={12} />
            </IconButton>
          </div>
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
        </div>
      ))}
      <Button
        size="sm"
        variant="secondary"
        onClick={() =>
          onChange([...loras, { path: nextUnused().path, multiplier: 1, isHighNoise: false }])
        }
      >
        <Plus size={13} />
        {t("Studio.lora.add")}
      </Button>
    </div>
  );
}
