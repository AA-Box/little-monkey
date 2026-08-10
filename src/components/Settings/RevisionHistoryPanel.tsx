import { useCallback, useEffect, useMemo, useState } from "react";
import { GitBranch, History, RotateCcw } from "lucide-react";
import { Button } from "../ui";
import { DiffViewer } from "../Workspace/DiffViewer";
import { errorMessage } from "../../lib/errors";
import {
  DEFAULT_BRANCH,
  branchFromRevision,
  getRevision,
  isValidBranchName,
  listBranches,
  listRevisions,
  type BranchSummary,
  type RevisionMeta,
} from "../../store/configRevisionStore";

export interface RevisionHistoryPanelProps {
  /** Revision kind — `PROMPT_ENTRY_KIND`, `WORKFLOW_KIND`, ... */
  kind: string;
  /** The versioned thing's id: a prompt entry id, a workflow id. */
  entityId: string;
  /** Shown in the header so a panel opened from a list says what it is about. */
  title: string;
  /**
   * Hands a chosen revision's snapshot back to whatever owns the live
   * document, which puts it back through its own normal save path — and so
   * records the restore as an ordinary revision. The history store is
   * deliberately not allowed to write it: only the prompt library or the
   * workflow editor knows how its own content is parsed and persisted, and a
   * "Restored" revision the owning store then rejected would be a lie. Omit
   * for a read-only history.
   */
  onRestore?: (content: string) => void;
  onClose?: () => void;
}

function formatWhen(ms: number): string {
  if (!ms) return "";
  return new Date(ms).toLocaleString();
}

/**
 * The one revision-history surface (roadmap K24 / ROADMAP #3): list, compare
 * any two revisions — including two on different branches — restore an old
 * one, and fork a named branch to keep a variant instead of overwriting.
 *
 * Generic over `kind`/`entityId` rather than duplicated per feature, because a
 * persona's history and a workflow definition's history differ only in what
 * the content string means, and that difference lives entirely in `onRestore`.
 */
export function RevisionHistoryPanel({ kind, entityId, title, onRestore, onClose }: RevisionHistoryPanelProps) {
  const [revisions, setRevisions] = useState<RevisionMeta[]>([]);
  const [branches, setBranches] = useState<BranchSummary[]>([]);
  const [branchFilter, setBranchFilter] = useState<string | null>(null);
  const [selected, setSelected] = useState<string[]>([]);
  const [contents, setContents] = useState<Record<string, string>>({});
  const [newBranchName, setNewBranchName] = useState("");
  const [branchingFrom, setBranchingFrom] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const reload = useCallback(async () => {
    setError(null);
    try {
      const [history, branchList] = await Promise.all([
        listRevisions(kind, entityId, branchFilter ?? undefined),
        listBranches(kind, entityId),
      ]);
      setRevisions(history);
      setBranches(branchList);
      // Two newest preselected, so opening the panel already answers the
      // question it is usually opened to answer: what changed last time?
      setSelected(history.slice(0, 2).map((revision) => revision.revisionId));
    } catch (reason) {
      setError(errorMessage(reason));
    }
  }, [kind, entityId, branchFilter]);

  useEffect(() => {
    void reload();
  }, [reload]);

  // Fetch only the snapshots actually being compared — a 200-revision history
  // would otherwise ship every byte it ever stored just to draw a list.
  useEffect(() => {
    let cancelled = false;
    void (async () => {
      for (const revisionId of selected) {
        if (contents[revisionId] !== undefined) continue;
        try {
          const revision = await getRevision(kind, entityId, revisionId);
          if (cancelled) return;
          setContents((current) => ({ ...current, [revisionId]: revision.content }));
        } catch (reason) {
          if (!cancelled) setError(errorMessage(reason));
        }
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [selected, contents, kind, entityId]);

  function toggleSelected(revisionId: string) {
    setSelected((current) => {
      if (current.includes(revisionId)) return current.filter((id) => id !== revisionId);
      // Keep the two most recently clicked, oldest choice drops out.
      return [...current, revisionId].slice(-2);
    });
  }

  /** Ordered oldest-first so the diff reads as "what this change did", not
   * backwards, regardless of which row was clicked first. */
  const comparePair = useMemo(() => {
    if (selected.length !== 2) return null;
    const bySequence = selected
      .map((id) => revisions.find((revision) => revision.revisionId === id))
      .filter((revision): revision is RevisionMeta => !!revision)
      .sort((left, right) => left.sequence - right.sequence);
    if (bySequence.length !== 2) return null;
    const [older, newer] = bySequence;
    const oldValue = contents[older.revisionId];
    const newValue = contents[newer.revisionId];
    if (oldValue === undefined || newValue === undefined) return null;
    return { older, newer, oldValue, newValue };
  }, [selected, revisions, contents]);

  async function handleRestore(revisionId: string) {
    setBusy(true);
    setError(null);
    try {
      const revision = await getRevision(kind, entityId, revisionId);
      onRestore?.(revision.content);
      await reload();
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setBusy(false);
    }
  }

  async function handleBranch(revisionId: string) {
    const name = newBranchName.trim();
    if (!isValidBranchName(name)) {
      setError("Branch names use lowercase letters, digits, '.', '-' and '_' (max 48).");
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await branchFromRevision(kind, entityId, revisionId, name);
      setNewBranchName("");
      setBranchingFrom(null);
      setBranchFilter(name);
      await reload();
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="flex flex-col gap-2 rounded-lg border border-border bg-surface p-3">
      <div className="flex flex-wrap items-center gap-2">
        <History size={13} className="shrink-0 text-faint" />
        <span className="truncate text-sm font-medium text-foreground">History — {title}</span>
        {onClose && (
          <Button variant="ghost" size="sm" className="ml-auto" onClick={onClose}>
            Close
          </Button>
        )}
      </div>

      {branches.length > 0 && (
        <div className="flex flex-wrap items-center gap-1.5 text-xs">
          <GitBranch size={12} className="shrink-0 text-faint" />
          <button
            type="button"
            onClick={() => setBranchFilter(null)}
            className={`rounded px-1.5 py-0.5 ${branchFilter === null ? "bg-accent-soft text-accent" : "text-muted hover:text-foreground"}`}
          >
            All branches
          </button>
          {branches.map((branch) => (
            <button
              key={branch.name}
              type="button"
              onClick={() => setBranchFilter(branch.name)}
              className={`rounded px-1.5 py-0.5 font-mono ${branchFilter === branch.name ? "bg-accent-soft text-accent" : "text-muted hover:text-foreground"}`}
              title={`${branch.revisionCount} revision(s)`}
            >
              {branch.name}
            </button>
          ))}
        </div>
      )}

      {error && <p className="text-xs text-danger">{error}</p>}

      {revisions.length === 0 ? (
        <p className="text-xs text-faint">
          No revisions recorded yet — the next save starts the history.
        </p>
      ) : (
        <div className="flex max-h-64 flex-col gap-1 overflow-auto">
          {revisions.map((revision) => {
            const isSelected = selected.includes(revision.revisionId);
            return (
              <div
                key={revision.revisionId}
                className={`flex flex-wrap items-center gap-2 rounded-md border p-2 text-xs ${
                  isSelected ? "border-accent bg-accent-soft" : "border-border bg-background"
                }`}
              >
                <button
                  type="button"
                  onClick={() => toggleSelected(revision.revisionId)}
                  className="flex min-w-0 flex-1 items-center gap-2 text-left"
                  title="Select up to two revisions to compare"
                >
                  <span className="shrink-0 font-mono text-faint">r{revision.sequence}</span>
                  {revision.branch !== DEFAULT_BRANCH && (
                    <span className="shrink-0 rounded bg-surface-2 px-1 font-mono text-[10px] text-muted">
                      {revision.branch}
                    </span>
                  )}
                  <span className="truncate text-foreground">{revision.label}</span>
                  <span className="ml-auto shrink-0 whitespace-nowrap text-faint">
                    {formatWhen(revision.createdAt)}
                  </span>
                </button>
                <div className="flex shrink-0 items-center gap-1">
                  <Button
                    variant="ghost"
                    size="sm"
                    disabled={busy}
                    onClick={() => setBranchingFrom(branchingFrom === revision.revisionId ? null : revision.revisionId)}
                  >
                    <GitBranch size={12} />
                    Branch
                  </Button>
                  {onRestore && (
                    <Button
                      variant="ghost"
                      size="sm"
                      disabled={busy}
                      onClick={() => void handleRestore(revision.revisionId)}
                    >
                      <RotateCcw size={12} />
                      Restore
                    </Button>
                  )}
                </div>
                {branchingFrom === revision.revisionId && (
                  <div className="flex w-full items-center gap-1.5">
                    <input
                      type="text"
                      value={newBranchName}
                      onChange={(event) => setNewBranchName(event.target.value)}
                      placeholder="branch name"
                      className="h-7 min-w-0 flex-1 rounded-md border border-border bg-surface px-2 font-mono text-xs text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
                    />
                    <Button
                      variant="secondary"
                      size="sm"
                      disabled={busy}
                      onClick={() => void handleBranch(revision.revisionId)}
                    >
                      Create branch
                    </Button>
                  </div>
                )}
              </div>
            );
          })}
        </div>
      )}

      {selected.length === 2 && !comparePair && <p className="text-xs text-faint">Loading comparison…</p>}
      {comparePair && (
        <div className="h-64">
          <DiffViewer
            oldValue={comparePair.oldValue}
            newValue={comparePair.newValue}
            fileName={`r${comparePair.older.sequence} (${comparePair.older.branch}) → r${comparePair.newer.sequence} (${comparePair.newer.branch})`}
          />
        </div>
      )}
      {revisions.length > 0 && selected.length !== 2 && (
        <p className="text-xs text-faint">Select two revisions to compare them.</p>
      )}
    </div>
  );
}

export default RevisionHistoryPanel;
