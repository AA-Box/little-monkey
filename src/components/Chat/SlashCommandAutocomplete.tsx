import { useT } from "../../lib/i18n";
import type { PromptEntry } from "../../store/promptStore";

export type SlashCatalogEntry = PromptEntry & { builtin?: boolean };

export interface SlashCommandAutocompleteProps {
  /** The raw text typed after "/" — filtering/sorting is done by the caller. */
  query: string;
  /** Already filtered/sorted entries to render, in order. */
  entries: SlashCatalogEntry[];
  /** Index into `entries` that is currently keyboard-highlighted. */
  activeIndex: number;
  /** Called when a row is chosen (click, or Enter/Tab in the caller). */
  onSelect: (entry: SlashCatalogEntry) => void;
  /** Called when the pointer hovers a row, so the caller can sync `activeIndex`. */
  onHoverIndex: (index: number) => void;
}

/**
 * Floating dropdown rendered above the chat textarea while a "/"-command is
 * being typed — clones `MentionAutocomplete`'s contract and styling
 * (purely presentational: `ChatWindow` owns trigger detection, filtering,
 * and keyboard navigation) with a kind badge (persona/snippet) instead of a
 * file/folder icon.
 */
export function SlashCommandAutocomplete({ entries, activeIndex, onSelect, onHoverIndex }: SlashCommandAutocompleteProps) {
  const { t } = useT();
  return (
    <div className="absolute bottom-full left-0 mb-1 max-h-56 w-80 overflow-y-auto rounded-lg border border-border bg-background shadow-lg py-1 z-20">
      {entries.length === 0 ? (
        <p className="px-2.5 py-1.5 text-sm text-faint">{t("SlashCommandAutocomplete.noMatches")}</p>
      ) : (
        entries.map((entry, index) => {
          const isActive = index === activeIndex;

          return (
            <button
              key={entry.id}
              type="button"
              onMouseEnter={() => onHoverIndex(index)}
              onClick={() => onSelect(entry)}
              className={`flex w-full cursor-pointer items-center gap-2 px-2.5 py-1.5 text-left text-sm ${
                isActive ? "bg-accent-soft text-accent" : "hover:bg-surface-2"
              }`}
            >
              <span
                className={`shrink-0 rounded px-1 py-0.5 text-[10px] font-medium uppercase ${
                  entry.builtin
                    ? "bg-accent-soft text-accent"
                    : entry.kind === "persona"
                    ? "bg-accent-soft text-accent"
                    : entry.kind === "skill"
                      ? "bg-success/10 text-success"
                      : "bg-surface-2 text-muted"
                }`}
              >
                {entry.builtin
                  ? "command"
                  : entry.kind === "persona"
                  ? t("SlashCommandAutocomplete.personaBadge")
                  : entry.kind === "skill"
                    ? t("SlashCommandAutocomplete.skillBadge")
                    : t("SlashCommandAutocomplete.snippetBadge")}
              </span>
              <span className="min-w-0 flex-1 truncate">
                <span className="font-mono text-xs text-foreground">/{entry.command}</span>
                <span className="ml-1.5 text-faint">{entry.name}</span>
              </span>
            </button>
          );
        })
      )}
    </div>
  );
}

export default SlashCommandAutocomplete;
