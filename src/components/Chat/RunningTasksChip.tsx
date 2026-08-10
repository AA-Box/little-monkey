import { useBackgroundShellStore, selectRunningShellTaskCount } from "../../store/backgroundShellStore";
import { useSubagentStore, selectRunningSubagentCount } from "../../store/subagentStore";
import { selectStudioRunCount, useStudioRunStore } from "../../store/studioRunStore";
import { useT } from "../../lib/i18n";

/**
 * The "✳ N running tasks" pill above the composer — the chat-level aggregate
 * of live BACKGROUND work: agent-started background shell commands plus
 * `task`-tool subagents, the two systems that deliberately bypass the run
 * ledger (ledger runs already have the full-pane Run Center). Side tasks are
 * deliberately NOT counted here: they are conversations with their own pane,
 * tab strip, and badge, not headless work this pill would send you to the
 * wrong surface for. Renders nothing when idle, so the composer area stays
 * untouched for the common case. Clicking it opens the Background Tasks
 * panel (wired by `App.tsx` through `ChatWindow`).
 */
export default function RunningTasksChip({
  onClick,
  onOpenStudio,
}: {
  onClick?: () => void;
  onOpenStudio?: () => void;
}) {
  const { t } = useT();
  const shellCount = useBackgroundShellStore(selectRunningShellTaskCount);
  const subagentCount = useSubagentStore(selectRunningSubagentCount);
  // Studio gets its own pill rather than joining the count: a generation is
  // not in the Background Tasks drawer, so sending the user there would be
  // the wrong surface. Its click goes to Studio instead.
  const studioCount = useStudioRunStore(selectStudioRunCount);
  const count = shellCount + subagentCount;

  if (count === 0 && studioCount === 0) return null;

  return (
    <div className="mx-4 mb-2">
      <div className="mx-auto flex max-w-3xl items-center gap-2">
        {count > 0 && (
          <button
            type="button"
            onClick={onClick}
            className="flex cursor-pointer items-center gap-1.5 rounded-full px-2 py-1 text-xs text-muted transition-colors duration-150 hover:bg-surface-2 hover:text-foreground"
          >
            <span className="animate-pulse text-accent" aria-hidden>
              ✳
            </span>
            {count === 1 ? t("RunningTasksChip.one") : t("RunningTasksChip.many", { count })}
          </button>
        )}
        {studioCount > 0 && (
          <button
            type="button"
            onClick={onOpenStudio}
            className="flex cursor-pointer items-center gap-1.5 rounded-full px-2 py-1 text-xs text-muted transition-colors duration-150 hover:bg-surface-2 hover:text-foreground"
          >
            <span className="animate-pulse text-accent" aria-hidden>
              ✳
            </span>
            {studioCount === 1
              ? t("Studio.queue.chip.one")
              : t("Studio.queue.chip.many", { count: studioCount })}
          </button>
        )}
      </div>
    </div>
  );
}
