export interface SegmentedItem<T extends string> {
  id: T;
  label: string;
}

export interface SegmentedControlProps<T extends string> {
  items: SegmentedItem<T>[];
  active: T;
  onChange: (id: T) => void;
  ariaLabel: string;
}

/**
 * Pill-track section switcher: one raised segment marks the active section.
 *
 * Distinct from `Tabs` on purpose — that one underlines a tab inside a panel,
 * this one switches which whole section of the app you are in, so it reads as
 * a physical control rather than a row of labels.
 */
export function SegmentedControl<T extends string>({
  items,
  active,
  onChange,
  ariaLabel,
}: SegmentedControlProps<T>) {
  return (
    <div
      role="tablist"
      aria-label={ariaLabel}
      className="flex gap-0.5 rounded-lg border border-border bg-surface-2 p-0.5"
    >
      {items.map((item) => {
        const isActive = item.id === active;
        return (
          <button
            key={item.id}
            type="button"
            role="tab"
            aria-selected={isActive}
            onClick={() => onChange(item.id)}
            className={`flex-1 cursor-pointer rounded-md px-2.5 py-1 text-xs font-medium transition-colors ${
              isActive
                ? "bg-surface text-foreground shadow-sm"
                : "text-muted hover:text-foreground"
            }`}
          >
            {item.label}
          </button>
        );
      })}
    </div>
  );
}
