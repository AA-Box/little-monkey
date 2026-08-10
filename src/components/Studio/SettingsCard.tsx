/**
 * One group of run settings in the Studio sidebar: a titled card holding its own
 * controls.
 *
 * The hint sits behind an ⓘ rather than under the title. Every group having its
 * explanation always visible is what made the old rail a wall of grey text you
 * stopped reading — and in a column this narrow the prose was taller than the
 * control it described. Native `title` is the tooltip: it needs no state, no
 * portal and no new strings, and it is the one tooltip that also works from the
 * keyboard-accessibility tree.
 */
import type { ReactNode } from "react";
import { Info } from "lucide-react";

interface Props {
  title: string;
  /** Shown on the ⓘ. Omitted where the group has no explanation worth one —
   *  an ⓘ that says nothing is worse than no ⓘ. */
  hint?: string;
  /** A control belonging to the header rather than the body, like the `+` that
   *  adds a LoRA. */
  action?: ReactNode;
  children: ReactNode;
}

export function SettingsCard({ title, hint, action, children }: Props) {
  return (
    <section className="grid gap-2 rounded-lg border border-border bg-surface p-2.5">
      <header className="flex min-h-5 items-center justify-between gap-2">
        <h3 className="text-xs font-semibold text-foreground">{title}</h3>
        <span className="flex shrink-0 items-center gap-1">
          {hint ? (
            // The wrapper carries the tooltip, not the icon: `tabIndex` so it is
            // reachable without a pointer, and a `span` rather than a `button`
            // because there is nothing to press — the text is the whole point.
            <span
              tabIndex={0}
              role="note"
              aria-label={hint}
              title={hint}
              className="flex text-faint hover:text-muted focus-visible:text-muted"
            >
              <Info size={13} aria-hidden />
            </span>
          ) : null}
          {action}
        </span>
      </header>
      {children}
    </section>
  );
}
