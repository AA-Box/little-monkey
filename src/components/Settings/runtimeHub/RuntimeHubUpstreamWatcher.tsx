import { useEffect } from "react";
import { GitPullRequestArrow, RefreshCw } from "lucide-react";
import { StatusPill } from "../../ui";
import type { RelevantPrEntry } from "../../../lib/runtimeHubClient";
import { useRuntimeHubStore } from "../../../store/runtimeHubStore";
import { BusyButton, ErrorNotice, formatDate, labelize, SectionHeading } from "./RuntimeHubShared";

/** Composes the one-line summary shown after a "Check now" run — a plain
 * count of what GitHub returned and how much of it was new, so a check that
 * (correctly, and usually) finds nothing new doesn't read as if it failed. */
export function describeCheckResult(scannedCount: number, newlyRelevantCount: number): string {
  if (scannedCount === 0) {
    return "GitHub returned no closed pull requests for this query.";
  }
  const plural = scannedCount === 1 ? "pull request" : "pull requests";
  const newPart =
    newlyRelevantCount === 0
      ? "none were newly relevant"
      : `${newlyRelevantCount} ${newlyRelevantCount === 1 ? "was" : "were"} newly relevant`;
  return `Scanned ${scannedCount} closed ${plural} — ${newPart}.`;
}

function PrCard({ entry }: { entry: RelevantPrEntry }) {
  return (
    <article className="rounded-lg border border-border bg-background p-4">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <a
          href={entry.url}
          target="_blank"
          rel="noreferrer"
          className="min-w-0 break-words text-sm font-semibold text-foreground underline-offset-2 hover:underline"
        >
          #{entry.number} {entry.title}
        </a>
        <StatusPill tone="neutral">{labelize(entry.topic)}</StatusPill>
      </div>
      <p className="mt-2 text-xs leading-5 text-muted">{entry.suggestedAction}</p>
    </article>
  );
}

export function RuntimeHubUpstreamWatcher() {
  const prWatcherState = useRuntimeHubStore((state) => state.prWatcherState);
  const lastResult = useRuntimeHubStore((state) => state.prWatcherLastResult);
  const refreshPrWatcher = useRuntimeHubStore((state) => state.refreshPrWatcher);
  const checkPrWatcherNow = useRuntimeHubStore((state) => state.checkPrWatcherNow);
  const loaded = useRuntimeHubStore((state) => state.prWatcherState !== null);
  const refreshing = useRuntimeHubStore((state) => state.busy["pr-watcher-refresh"]);
  const checking = useRuntimeHubStore((state) => state.busy["pr-watcher-check"]);
  const checkError = useRuntimeHubStore((state) => state.errors["pr-watcher-check"]);

  useEffect(() => {
    if (!loaded) void refreshPrWatcher().catch(() => {});
  }, [loaded, refreshPrWatcher]);

  const sourceRepo = prWatcherState?.sourceRepo ?? "ollama/ollama";
  const relevantPrs = prWatcherState?.relevantPrs ?? [];
  const persistedError = prWatcherState?.lastCheckError ?? null;

  return (
    <div
      role="tabpanel"
      id="runtime-hub-panel-upstream-watcher"
      aria-labelledby="runtime-hub-tab-upstream-watcher"
      className="flex flex-col gap-5"
    >
      <SectionHeading
        title="Runtime PR Watcher"
        description={`Scans ${sourceRepo}'s closed pull requests and flags the ones that plausibly touch Little Monkey's own runtime surface (GGUF/quantization, chat templates and tool calling, API routes, hardware/GPU backends, KV cache/context, and model manifest/registry), each with a short suggested action. This is an on-demand check today, not a background job — click "Check now" to scan for what's changed upstream since the last check.`}
        action={
          <BusyButton type="button" busy={checking} onClick={() => void checkPrWatcherNow().catch(() => {})}>
            <RefreshCw size={15} aria-hidden="true" /> Check now
          </BusyButton>
        }
      />

      <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-xs text-muted">
        <span>Last checked: {formatDate(prWatcherState?.lastCheckedAtMs ?? null)}</span>
        {prWatcherState?.lastSeenPrNumber != null && (
          <span>Newest PR seen: #{prWatcherState.lastSeenPrNumber}</span>
        )}
      </div>

      <ErrorNotice message={checkError ?? persistedError} />

      {lastResult && !checkError && (
        <p role="status" className="text-xs text-muted">
          {describeCheckResult(lastResult.scannedCount, lastResult.newlyRelevant.length)}
        </p>
      )}

      {refreshing && !loaded && <p className="text-xs text-muted">Loading the last saved report…</p>}

      <section className="flex flex-col gap-3" aria-labelledby="upstream-watcher-report-heading">
        <div className="flex items-center gap-2">
          <GitPullRequestArrow size={15} className="text-muted" aria-hidden="true" />
          <h3 id="upstream-watcher-report-heading" className="text-sm font-semibold text-foreground">
            Relevant upstream changes
          </h3>
        </div>
        {relevantPrs.length ? (
          <div className="flex flex-col gap-3" aria-live="polite">
            {relevantPrs.map((entry) => (
              <PrCard key={entry.number} entry={entry} />
            ))}
          </div>
        ) : (
          <div className="rounded-lg border border-dashed border-border p-6 text-center text-sm text-muted">
            No newly relevant upstream changes yet. Click &ldquo;Check now&rdquo; to scan {sourceRepo}&apos;s closed pull
            requests.
          </div>
        )}
      </section>
    </div>
  );
}

export default RuntimeHubUpstreamWatcher;
