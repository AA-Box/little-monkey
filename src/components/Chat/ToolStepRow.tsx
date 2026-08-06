import { memo, useId, useState, type ReactNode } from "react";
import { Check, ChevronRight, Copy } from "lucide-react";

import { useT } from "../../lib/i18n";

/** Container classes for a list of `ToolStepRow`s — the bordered, hairline-
 * divided box both call sites wrap their steps in. */
export const TOOL_STEP_LIST_CLASSES = "divide-y divide-border overflow-hidden rounded-lg border border-border bg-surface-2";

/** The copy button a step's detail places at the right edge of its command
 * line. Separate from `ToolStepRow` because a step's detail decides what is
 * worth copying (and where the button belongs) — a call's command plus
 * output, on the line that shows it. */
export const StepCopyButton = memo(function StepCopyButton({ text }: { text: string }) {
  const { t } = useT();
  const [copied, setCopied] = useState(false);

  const handleCopy = async () => {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      // Clipboard permission denied/unavailable — same silent no-op
      // `CodeBlock` takes; there's nothing destructive to fall back to.
    }
  };

  return (
    <button
      type="button"
      onClick={() => void handleCopy()}
      aria-label={copied ? t("ToolStepRow.copied") : t("ToolStepRow.copy")}
      title={copied ? t("ToolStepRow.copied") : t("ToolStepRow.copy")}
      className="flex shrink-0 cursor-pointer items-center justify-center rounded-md p-1 text-faint transition-colors duration-150 hover:bg-surface-2 hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent motion-reduce:transition-none"
    >
      {copied ? <Check size={13} /> : <Copy size={13} />}
    </button>
  );
});

/**
 * One step in a hairline-divided list of tool activity: a single-line title
 * that expands to whatever detail the caller renders.
 *
 * Shared by the parent transcript's `ActivityRow` (one step per tool call)
 * and a subagent's own mini-transcript (`SubagentRow`, one step per narrated
 * round) so both read identically — Claude-Code-desktop parity: no per-row
 * badges, pills or status dots, just the title, the chevron, and the detail
 * one click away.
 */
export const ToolStepRow = memo(function ToolStepRow({
  title,
  failed = false,
  children,
}: {
  title: string;
  /** Tints the title when the step's work failed — the only status this row
   * shows, since a failure is the one thing worth seeing while collapsed. */
  failed?: boolean;
  children: ReactNode;
}) {
  const [open, setOpen] = useState(false);
  const detailsId = useId();

  return (
    <div>
      <button
        type="button"
        aria-expanded={open}
        aria-controls={detailsId}
        onClick={() => setOpen((prev) => !prev)}
        className={`flex w-full cursor-pointer items-center gap-1.5 px-3 py-2.5 text-left text-[13px] transition-colors duration-150 hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-inset motion-reduce:transition-none ${
          failed ? "text-danger" : "text-muted"
        }`}
      >
        <span className={`min-w-0 truncate ${open ? "rounded-md px-1.5 py-0.5 ring-1 ring-border-strong" : ""}`}>{title}</span>
        <ChevronRight
          size={13}
          className={`shrink-0 text-faint transition-transform duration-150 motion-reduce:transition-none ${open ? "rotate-90" : ""}`}
          aria-hidden
        />
      </button>
      {open && (
        <div id={detailsId} className="border-t border-border bg-background px-3 py-2.5">
          {children}
        </div>
      )}
    </div>
  );
});

export default ToolStepRow;
