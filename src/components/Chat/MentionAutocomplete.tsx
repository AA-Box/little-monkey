import { FileText, Folder } from "lucide-react";
import { useT } from "../../lib/i18n";

/**
 * A single workspace-relative path offered by the "@"-mention autocomplete,
 * mirroring `WorkspacePathEntry` returned by the Rust `list_workspace_paths`
 * command (see src-tauri/src/tools.rs).
 */
export interface MentionEntry {
  path: string;
  is_dir: boolean;
}

export interface MentionAutocompleteProps {
  /** The raw text typed after "@" — filtering/sorting is done by the caller. */
  query: string;
  /** Already filtered/sorted/capped entries to render, in order. */
  entries: MentionEntry[];
  /** Index into `entries` that is currently keyboard-highlighted. */
  activeIndex: number;
  /** Called when a row is chosen (click, or Enter/Tab in the caller). */
  onSelect: (entry: MentionEntry) => void;
  /** Called when the pointer hovers a row, so the caller can sync `activeIndex`. */
  onHoverIndex: (index: number) => void;
}

/**
 * Floating dropdown rendered above the chat textarea while an "@"-mention is
 * being typed. Purely presentational: `ChatWindow` owns the trigger
 * detection, filtering, and keyboard navigation; this component just renders
 * whatever `entries` it is given (plus a "No matches" empty state).
 */
export function MentionAutocomplete({ entries, activeIndex, onSelect, onHoverIndex }: MentionAutocompleteProps) {
  const { t } = useT();
  return (
    <div className="absolute bottom-full left-0 mb-1 max-h-56 w-80 overflow-y-auto rounded-lg border border-border bg-background shadow-lg py-1 z-20">
      {entries.length === 0 ? (
        <p className="px-2.5 py-1.5 text-sm text-faint">{t("MentionAutocomplete.noMatches")}</p>
      ) : (
        entries.map((entry, index) => {
          const isActive = index === activeIndex;
          const lastSlash = entry.path.lastIndexOf("/");
          const dirPrefix = lastSlash >= 0 ? entry.path.slice(0, lastSlash + 1) : "";
          const basename = lastSlash >= 0 ? entry.path.slice(lastSlash + 1) : entry.path;
          const Icon = entry.is_dir ? Folder : FileText;

          return (
            <button
              key={entry.path}
              type="button"
              onMouseEnter={() => onHoverIndex(index)}
              onClick={() => onSelect(entry)}
              className={`flex w-full cursor-pointer items-center gap-2 px-2.5 py-1.5 text-left text-sm ${
                isActive ? "bg-accent-soft text-accent" : "hover:bg-surface-2"
              }`}
            >
              <Icon size={14} className="shrink-0 text-faint" />
              <span className="truncate font-mono text-xs">
                {dirPrefix && <span className="text-faint">{dirPrefix}</span>}
                {basename}
              </span>
            </button>
          );
        })
      )}
    </div>
  );
}

export default MentionAutocomplete;
