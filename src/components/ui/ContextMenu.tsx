import type { ReactNode } from "react";

export interface ContextMenuAction {
  label: string;
  onClick: () => void;
  icon?: ReactNode;
}

export type ContextMenuEntry = ContextMenuAction | { separator: true };

export interface ContextMenuProps {
  entries: ContextMenuEntry[];
  /** Positioning classes for the floating panel, e.g. `"left-0 top-full mt-1"`
   * — left to the caller since anchors sit at different edges of the bar. */
  className?: string;
}

/**
 * Small floating action menu (right-click-style, but opened by a regular
 * click) used for the folder/branch badges in `GitStatusBar`. Visual idiom
 * matches the app's other floating panels (commit popover, mention
 * autocomplete): rounded-lg border border-border bg-background shadow-lg.
 */
export function ContextMenu({ entries, className }: ContextMenuProps) {
  return (
    <div
      className={`absolute z-30 min-w-[180px] whitespace-nowrap rounded-lg border border-border bg-background py-1 shadow-lg ${className ?? ""}`}
    >
      {entries.map((entry, index) =>
        "separator" in entry ? (
          <div key={index} className="my-1 border-t border-border" />
        ) : (
          <button
            key={entry.label}
            type="button"
            onClick={entry.onClick}
            className="flex w-full cursor-pointer items-center gap-2 px-3 py-1.5 text-left text-sm text-foreground hover:bg-surface-2"
          >
            {entry.icon}
            {entry.label}
          </button>
        ),
      )}
    </div>
  );
}

export default ContextMenu;
