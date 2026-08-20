import { useCallback, useEffect, useId, useRef, useState } from "react";
import { Check, ChevronDown } from "lucide-react";

export interface ListboxOption {
  value: string;
  label: string;
  /** Second line — what the thing is, rather than only what it is called. */
  detail?: string;
}

export interface ListboxProps {
  value: string;
  options: ListboxOption[];
  onChange: (value: string) => void;
  /** Shown when nothing is selected, or when there is nothing to select. */
  placeholder?: string;
  ariaLabel?: string;
}

/** Consecutive keystrokes count as one search for this long. */
const TYPEAHEAD_RESET_MS = 600;

/**
 * The index a type-ahead search lands on, or `-1`.
 *
 * Prefix first, the way every platform menu behaves; a substring fallback so a
 * search that could only have meant one row still finds it. Exported because
 * it is the one part of this component with logic worth a test.
 */
export function matchTypeAhead(options: ListboxOption[], term: string): number {
  const needle = term.trim().toLowerCase();
  if (!needle) return -1;
  const prefix = options.findIndex((option) =>
    option.label.toLowerCase().startsWith(needle),
  );
  if (prefix >= 0) return prefix;
  return options.findIndex((option) => option.label.toLowerCase().includes(needle));
}

/**
 * A listbox that opens to the width of its own control.
 *
 * A native `<select>` cannot do that: its popup is drawn by the platform and
 * sized to the widest option, so a short label gives a menu a fraction of the
 * field. Everything a native select gives away for free is therefore rebuilt
 * here on purpose — arrow keys, Home/End, type-ahead, Escape, outside click,
 * focus return, and the listbox/option roles a screen reader needs.
 */
export function Listbox({ value, options, onChange, placeholder, ariaLabel }: ListboxProps) {
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(0);
  const rootRef = useRef<HTMLDivElement>(null);
  const listRef = useRef<HTMLUListElement>(null);
  const buttonRef = useRef<HTMLButtonElement>(null);
  const search = useRef({ term: "", at: 0 });
  const id = useId();

  const selectedIndex = options.findIndex((option) => option.value === value);
  const selected = selectedIndex >= 0 ? options[selectedIndex] : null;

  const close = useCallback((refocus = true) => {
    setOpen(false);
    if (refocus) buttonRef.current?.focus();
  }, []);

  // Anchored to the button, so anything that moves the button — a scroll in
  // any ancestor — has to dismiss it rather than leave it behind.
  useEffect(() => {
    if (!open) return;
    const dismiss = (event: Event) => {
      if (!rootRef.current?.contains(event.target as Node)) close(false);
    };
    const away = () => close(false);
    document.addEventListener("pointerdown", dismiss, true);
    window.addEventListener("scroll", away, true);
    window.addEventListener("resize", away);
    return () => {
      document.removeEventListener("pointerdown", dismiss, true);
      window.removeEventListener("scroll", away, true);
      window.removeEventListener("resize", away);
    };
  }, [open, close]);

  // Keyboard movement has to bring the row with it or it is invisible.
  useEffect(() => {
    if (!open) return;
    listRef.current?.children[active]?.scrollIntoView({ block: "nearest" });
  }, [open, active]);

  const commit = (index: number) => {
    const option = options[index];
    if (!option) return;
    onChange(option.value);
    close();
  };

  const handleKeyDown = (event: React.KeyboardEvent) => {
    if (!open) {
      if (["ArrowDown", "ArrowUp", "Enter", " "].includes(event.key)) {
        event.preventDefault();
        setActive(Math.max(selectedIndex, 0));
        setOpen(true);
      }
      return;
    }
    switch (event.key) {
      case "Escape":
        event.preventDefault();
        close();
        return;
      case "Enter":
      case " ":
        event.preventDefault();
        commit(active);
        return;
      case "ArrowDown":
        event.preventDefault();
        setActive((index) => Math.min(index + 1, options.length - 1));
        return;
      case "ArrowUp":
        event.preventDefault();
        setActive((index) => Math.max(index - 1, 0));
        return;
      case "Home":
        event.preventDefault();
        setActive(0);
        return;
      case "End":
        event.preventDefault();
        setActive(options.length - 1);
        return;
      case "Tab":
        close(false);
        return;
      default:
        break;
    }
    if (event.key.length === 1 && !event.metaKey && !event.ctrlKey && !event.altKey) {
      const now = Date.now();
      search.current.term =
        now - search.current.at > TYPEAHEAD_RESET_MS
          ? event.key
          : search.current.term + event.key;
      search.current.at = now;
      const found = matchTypeAhead(options, search.current.term);
      if (found >= 0) setActive(found);
    }
  };

  return (
    <div ref={rootRef} className="relative min-w-0" onKeyDown={handleKeyDown}>
      <button
        ref={buttonRef}
        type="button"
        role="combobox"
        aria-label={ariaLabel}
        aria-expanded={open}
        aria-controls={`${id}-list`}
        aria-activedescendant={open ? `${id}-${active}` : undefined}
        aria-haspopup="listbox"
        disabled={options.length === 0}
        className="flex w-full cursor-pointer items-center gap-2 rounded-md border border-border bg-background py-1.5 pl-2.5 pr-2 text-left text-xs text-foreground outline-none focus-visible:ring-1 focus-visible:ring-accent disabled:cursor-not-allowed disabled:opacity-60"
        onClick={() => {
          if (open) return close();
          setActive(Math.max(selectedIndex, 0));
          setOpen(true);
        }}
      >
        <span className="flex min-w-0 flex-1 items-center gap-1.5">
          <span className="min-w-0 flex-1 truncate">
            {selected?.label ?? placeholder ?? ""}
          </span>
          {selected?.detail && (
            <span className="max-w-[45%] shrink-0 truncate text-faint">{selected.detail}</span>
          )}
        </span>
        <ChevronDown size={12} className="shrink-0 text-muted" />
      </button>

      {open && (
        // `left-0 right-0` is the whole point: the menu is exactly as wide as
        // the control, which is the one thing a native popup will not do.
        <ul
          ref={listRef}
          id={`${id}-list`}
          role="listbox"
          className="absolute left-0 right-0 top-full z-30 mt-1 max-h-64 overflow-y-auto rounded-lg border border-border bg-background py-1 shadow-lg"
        >
          {options.map((option, index) => (
            <li
              key={option.value}
              id={`${id}-${index}`}
              role="option"
              aria-selected={option.value === value}
              className={`flex cursor-pointer items-center gap-2 px-2.5 py-1.5 text-xs ${
                index === active ? "bg-surface-2 text-foreground" : "text-muted"
              }`}
              onMouseEnter={() => setActive(index)}
              onClick={() => commit(index)}
            >
              <Check
                size={12}
                className={`shrink-0 ${option.value === value ? "text-accent" : "opacity-0"}`}
              />
              <span className="min-w-0 flex-1">
                <span className="block truncate text-foreground">{option.label}</span>
                {option.detail && (
                  <span className="block truncate text-[11px] text-faint">{option.detail}</span>
                )}
              </span>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

export default Listbox;
