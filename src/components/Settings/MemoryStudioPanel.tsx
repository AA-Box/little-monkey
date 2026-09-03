import { useEffect, useMemo, useState } from "react";
import { open, save } from "@tauri-apps/plugin-dialog";
import { Download, Merge, Pencil, Pin, PinOff, Power, RefreshCw, Trash2, Undo2, Upload } from "lucide-react";

import { Button, StatusPill } from "../ui";
import { useT } from "../../lib/i18n";
import {
  deleteMemory,
  exportMemories,
  importMemories,
  listAllMemories,
  mergeMemories,
  purgeExpiredMemories,
  setMemoryEnabled,
  setMemoryExpiry,
  setMemoryPinned,
  unmergeMemories,
  updateMemory,
  wouldReachPrompt,
  type MemoryEntry,
} from "../../lib/memoryStudio";
import { errorMessage } from "../../lib/errors";

/** Must match `MAX_FACT_CHARS` in `src-tauri/src/memory.rs`. */
const MAX_FACT_CHARS = 500;

/** Same code-point counting rationale as `RulesMemoryPanel.tsx`'s
 * `charCount` — Rust caps by `chars().count()`, not UTF-16 length. */
function charCount(text: string): number {
  return Array.from(text).length;
}

function formatDate(iso: string): string {
  const parsed = new Date(iso);
  return Number.isNaN(parsed.getTime()) ? iso : parsed.toLocaleString();
}

/** The last path segment, for a compact project heading — the full
 * canonical path is still shown underneath for disambiguation. */
function projectBasename(path: string): string {
  const parts = path.split(/[\\/]/).filter(Boolean);
  return parts[parts.length - 1] ?? path;
}

type ScopeFilter = "all" | "global" | "project";

/** The lifecycle filter beside the scope filter. `"active"` is the default
 * and means "not retired by a merge" — without it every merge would triple
 * the rows in the list the user just tidied (the merged memory plus both
 * originals). Retired memories are one click away, never hidden for good. */
type StateFilter = "active" | "pinned" | "expired" | "merged" | "retired";

function isExpired(entry: MemoryEntry, now: string): boolean {
  return !entry.pinned && entry.expires_at !== null && entry.expires_at <= now;
}

function statusBanner(error: string | null, success: string | null) {
  if (!error && !success) return null;
  return (
    <div
      role={error ? "alert" : "status"}
      className={`rounded-lg border px-3 py-2 text-sm ${
        error ? "border-danger/30 bg-danger-soft text-danger" : "border-success/30 bg-success-soft text-success"
      }`}
    >
      {error ?? success}
    </div>
  );
}

/** One memory row. Shows everything the store actually records — source,
 * scope, created date, source turn when known, enabled/pinned/expired/
 * retired state, what it was merged from, and when a prompt was last built
 * from it — and offers the actions that back those: inline edit, pin,
 * expiry (a native date input), enable/disable, undo merge, delete. What is
 * still missing, and why, is stated in the panel's honesty note: no
 * confidence score, no source file or connector, and no "why do you know
 * this" link from a chat answer, because nothing in this app records any of
 * the three. */
function MemoryRow({
  entry,
  now,
  selected,
  onToggleSelected,
  onChanged,
}: {
  entry: MemoryEntry;
  now: string;
  selected: boolean;
  onToggleSelected: () => void;
  onChanged: () => Promise<void>;
}) {
  const { t } = useT();
  const [editing, setEditing] = useState(false);
  const [text, setText] = useState(entry.text);
  const [saving, setSaving] = useState(false);
  const [togglingEnabled, setTogglingEnabled] = useState(false);
  const [rowBusy, setRowBusy] = useState(false);
  const [confirmingDelete, setConfirmingDelete] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const [rowError, setRowError] = useState<string | null>(null);

  const overLimit = charCount(text) > MAX_FACT_CHARS;
  const expired = isExpired(entry, now);
  const retired = entry.retired_at !== null;
  // The same predicate the store filters on, mirrored from Rust's
  // `reaches_prompt` — one definition of "this isn't reaching the model".
  const dimmed = !wouldReachPrompt(entry, now);

  /** Every row mutation is the same shape: clear the error, run, refresh. */
  async function runRow(operation: () => Promise<unknown>) {
    setRowBusy(true);
    setRowError(null);
    try {
      await operation();
      await onChanged();
    } catch (e) {
      setRowError(errorMessage(e));
    } finally {
      setRowBusy(false);
    }
  }

  async function handleSave() {
    setSaving(true);
    setRowError(null);
    try {
      await updateMemory(entry.id, entry.project_root, text);
      await onChanged();
      setEditing(false);
    } catch (e) {
      setRowError(errorMessage(e));
    } finally {
      setSaving(false);
    }
  }

  async function handleToggleEnabled() {
    setTogglingEnabled(true);
    setRowError(null);
    try {
      await setMemoryEnabled(entry.id, entry.project_root, !entry.enabled);
      await onChanged();
    } catch (e) {
      setRowError(errorMessage(e));
    } finally {
      setTogglingEnabled(false);
    }
  }

  async function handleDelete() {
    setDeleting(true);
    setRowError(null);
    try {
      await deleteMemory(entry.id, entry.project_root);
      await onChanged();
    } catch (e) {
      setRowError(errorMessage(e));
      setDeleting(false);
    }
  }

  return (
    <div
      id={`memory-${entry.id}`}
      className={`rounded-lg border p-2.5 ${dimmed ? "border-border/60 bg-surface-2/50" : "border-border bg-background"}`}
    >
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="flex flex-wrap items-center gap-1.5">
          <input
            type="checkbox"
            checked={selected}
            onChange={onToggleSelected}
            aria-label={t("MemoryStudioPanel.selectLabel")}
            title={t("MemoryStudioPanel.selectLabel")}
            className="h-3.5 w-3.5"
          />
          <StatusPill tone="neutral">
            {entry.source === "agent" ? t("MemoryStudioPanel.sourceAgent") : t("MemoryStudioPanel.sourceUser")}
          </StatusPill>
          <StatusPill tone={entry.enabled ? "success" : "warning"}>
            {entry.enabled ? t("MemoryStudioPanel.statusEnabled") : t("MemoryStudioPanel.statusDisabled")}
          </StatusPill>
          {entry.pinned && <StatusPill tone="success">{t("MemoryStudioPanel.statusPinned")}</StatusPill>}
          {expired && <StatusPill tone="warning">{t("MemoryStudioPanel.statusExpired")}</StatusPill>}
          {retired && <StatusPill tone="warning">{t("MemoryStudioPanel.statusRetired")}</StatusPill>}
          {entry.merged_from.length > 0 && (
            <span
              className="rounded-full bg-surface-2 px-1.5 py-0.5 text-[10px] text-faint"
              title={t("MemoryStudioPanel.mergedFromTitle", { ids: entry.merged_from.join(", ") })}
            >
              {t("MemoryStudioPanel.statusMerged", { count: entry.merged_from.length })}
            </span>
          )}
          {entry.source_turn_id && (
            <span
              className="rounded-full bg-surface-2 px-1.5 py-0.5 text-[10px] text-faint"
              title={t("MemoryStudioPanel.sourceTurnTitle", { id: entry.source_turn_id })}
            >
              {t("MemoryStudioPanel.sourceTurnLabel", { id: entry.source_turn_id.slice(0, 8) })}
            </span>
          )}
        </div>
        <div className="flex shrink-0 items-center gap-1">
          {!editing && (
            <>
              <button
                type="button"
                onClick={() => {
                  setText(entry.text);
                  setEditing(true);
                  setRowError(null);
                }}
                className="flex cursor-pointer items-center gap-1 rounded-md border border-border px-1.5 py-0.5 text-xs text-muted transition-colors hover:text-foreground"
              >
                <Pencil size={11} />
                {t("MemoryStudioPanel.editButton")}
              </button>
              <button
                type="button"
                onClick={() => void handleToggleEnabled()}
                disabled={togglingEnabled}
                className="flex cursor-pointer items-center gap-1 rounded-md border border-border px-1.5 py-0.5 text-xs text-muted transition-colors hover:text-foreground disabled:cursor-not-allowed disabled:opacity-50"
              >
                <Power size={11} />
                {entry.enabled ? t("MemoryStudioPanel.disableButton") : t("MemoryStudioPanel.enableButton")}
              </button>
              <button
                type="button"
                onClick={() => void runRow(() => setMemoryPinned(entry.id, entry.project_root, !entry.pinned))}
                disabled={rowBusy}
                className="flex cursor-pointer items-center gap-1 rounded-md border border-border px-1.5 py-0.5 text-xs text-muted transition-colors hover:text-foreground disabled:cursor-not-allowed disabled:opacity-50"
              >
                {entry.pinned ? <PinOff size={11} /> : <Pin size={11} />}
                {entry.pinned ? t("MemoryStudioPanel.unpinButton") : t("MemoryStudioPanel.pinButton")}
              </button>
              {entry.merged_from.length > 0 && (
                <button
                  type="button"
                  onClick={() => void runRow(() => unmergeMemories(entry.id, entry.project_root))}
                  disabled={rowBusy}
                  className="flex cursor-pointer items-center gap-1 rounded-md border border-border px-1.5 py-0.5 text-xs text-muted transition-colors hover:text-foreground disabled:cursor-not-allowed disabled:opacity-50"
                >
                  <Undo2 size={11} />
                  {t("MemoryStudioPanel.unmergeButton")}
                </button>
              )}
              {confirmingDelete ? (
                <span className="flex items-center gap-1">
                  {entry.merged_from.length > 0 && (
                    <span className="text-[10px] text-warning">
                      {t("MemoryStudioPanel.deleteMergedWarning", { count: entry.merged_from.length })}
                    </span>
                  )}
                  <button
                    type="button"
                    onClick={() => setConfirmingDelete(false)}
                    disabled={deleting}
                    className="rounded-md border border-border px-1.5 py-0.5 text-xs text-muted hover:text-foreground"
                  >
                    {t("MemoryStudioPanel.cancelButton")}
                  </button>
                  <button
                    type="button"
                    onClick={() => void handleDelete()}
                    disabled={deleting}
                    className="rounded-md border border-danger/40 bg-danger-soft px-1.5 py-0.5 text-xs text-danger hover:bg-danger/20"
                  >
                    {deleting ? t("MemoryStudioPanel.deletingButton") : t("MemoryStudioPanel.confirmDeleteButton")}
                  </button>
                </span>
              ) : (
                <button
                  type="button"
                  onClick={() => setConfirmingDelete(true)}
                  className="flex cursor-pointer items-center gap-1 rounded-md border border-border px-1.5 py-0.5 text-xs text-muted transition-colors hover:text-danger"
                >
                  <Trash2 size={11} />
                  {t("MemoryStudioPanel.deleteButton")}
                </button>
              )}
            </>
          )}
        </div>
      </div>

      {editing ? (
        <div className="mt-2 flex flex-col gap-1.5">
          <textarea
            value={text}
            onChange={(e) => setText(e.target.value)}
            rows={2}
            spellCheck={false}
            className="w-full resize-y rounded-md border border-border bg-surface px-2 py-1.5 text-xs text-foreground focus:outline-none focus:ring-2 focus:ring-accent"
          />
          <div className="flex items-center justify-between gap-2">
            <span className={`text-xs ${overLimit ? "text-danger" : "text-faint"}`}>
              {t("MemoryStudioPanel.charCount", { count: charCount(text), max: MAX_FACT_CHARS })}
            </span>
            <div className="flex gap-1.5">
              <Button size="sm" variant="ghost" onClick={() => setEditing(false)} disabled={saving}>
                {t("MemoryStudioPanel.cancelButton")}
              </Button>
              <Button
                size="sm"
                variant="primary"
                onClick={() => void handleSave()}
                disabled={saving || overLimit || text.trim().length === 0}
              >
                {saving ? t("MemoryStudioPanel.savingButton") : t("MemoryStudioPanel.saveButton")}
              </Button>
            </div>
          </div>
        </div>
      ) : (
        <p className="mt-1.5 whitespace-pre-wrap break-words text-xs text-foreground">{entry.text}</p>
      )}

      {expired && (
        <p className="mt-1.5 text-[10px] text-warning">
          {t("MemoryStudioPanel.expiredReason", { date: formatDate(entry.expires_at as string) })}
        </p>
      )}
      {retired && (
        <p className="mt-1.5 text-[10px] text-warning">
          {t("MemoryStudioPanel.retiredReason", { date: formatDate(entry.retired_at as string) })}
        </p>
      )}

      <div className="mt-1.5 flex flex-wrap items-center gap-1.5 text-[10px] text-faint">
        <label className="flex items-center gap-1">
          {t("MemoryStudioPanel.expiryLabel")}
          {/* Native <input type="date">, not a picker dependency. It emits a
              bare YYYY-MM-DD, which the backend expands to the END of that
              day — so picking today does not expire the memory on save. */}
          <input
            type="date"
            value={entry.expires_at?.slice(0, 10) ?? ""}
            // A pinned memory is exempt from expiry, so an expiry set here
            // would persist and do nothing — the control says so instead.
            disabled={rowBusy || entry.pinned}
            onChange={(e) =>
              void runRow(() => setMemoryExpiry(entry.id, entry.project_root, e.target.value || null))
            }
            className="rounded-md border border-border bg-surface px-1.5 py-0.5 text-[10px] text-foreground"
          />
        </label>
        {entry.pinned ? (
          <span>{t("MemoryStudioPanel.expiryPinnedHint")}</span>
        ) : entry.expires_at ? (
          <button
            type="button"
            disabled={rowBusy}
            onClick={() => void runRow(() => setMemoryExpiry(entry.id, entry.project_root, null))}
            className="cursor-pointer rounded-md border border-border px-1.5 py-0.5 text-[10px] text-muted hover:text-foreground disabled:opacity-50"
          >
            {t("MemoryStudioPanel.expiryClearButton")}
          </button>
        ) : (
          <span>{t("MemoryStudioPanel.expiryHint")}</span>
        )}
      </div>

      <p className="mt-1.5 text-[10px] text-faint">{t("MemoryStudioPanel.createdAt", { date: formatDate(entry.created_at) })}</p>
      <p className="text-[10px] text-faint">
        {entry.last_used_at
          ? t("MemoryStudioPanel.lastUsed", { date: formatDate(entry.last_used_at) })
          : t("MemoryStudioPanel.lastUsedNever")}
      </p>
      {rowError && <p className="mt-1.5 text-xs text-danger">{t("MemoryStudioPanel.errorPrefix", { error: rowError })}</p>}
    </div>
  );
}

/**
 * Memory Studio: the full-control surface over every durable memory this app
 * has ever stored, built directly on the existing `memory.rs` fact store
 * (see that module's docs) rather than a second memory system. Lists every
 * memory across the two scopes the backend can actually represent —
 * `"global"` (applies to every project) and `"project"` (one workspace root)
 * — filtered by scope, by lifecycle state, and by text, with the full
 * provenance the store records (source, source turn, what it was merged
 * from, when a prompt was last built from it).
 *
 * Every mutation here takes effect on the very next prompt, because they all
 * route through `memory.rs` impls that `list_impl` reads — and `list_impl`
 * is the one function both `memory_list` (desktop, once per turn via
 * `rulesStore.refresh()`) and `monkey-cli`'s `compose_system_prompt_impl`
 * call. Disabled, expired and merge-retired memories are excluded there, and
 * pinned ones ordered first; see that module's
 * `disabled_and_deleted_facts_are_excluded_from_list_impl`,
 * `expired_and_merge_retired_facts_are_excluded_from_list_impl` and
 * `pinned_facts_are_listed_first_by_list_impl` for the direct proofs. This
 * panel adds no filter of its own beyond what it displays.
 *
 * "The very next prompt" means the next one *built*: a turn already queued
 * to the background daemon replays the system prompt frozen when it was
 * queued (`task.rs`) and re-reads no memory, so a memory disabled, expired
 * or merged while that turn waits still reaches that turn's model. The
 * honesty note says so in the panel itself.
 */
export function MemoryStudioPanel() {
  const { t } = useT();
  const [entries, setEntries] = useState<MemoryEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [scopeFilter, setScopeFilter] = useState<ScopeFilter>("all");
  const [stateFilter, setStateFilter] = useState<StateFilter>("active");
  const [search, setSearch] = useState("");
  const [selectedIds, setSelectedIds] = useState<Set<string>>(new Set());
  const [mergeText, setMergeText] = useState("");
  const [mergeOpen, setMergeOpen] = useState(false);
  const [confirmingPurge, setConfirmingPurge] = useState(false);
  const [redactOnExport, setRedactOnExport] = useState(true);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [success, setSuccess] = useState<string | null>(null);

  const refresh = async () => {
    try {
      setEntries(await listAllMemories());
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    void refresh();
  }, []);

  const run = async (name: string, operation: () => Promise<void>) => {
    setBusy(name);
    setError(null);
    setSuccess(null);
    try {
      await operation();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(null);
    }
  };

  // One `now` per render pass, so every row's expiry verdict agrees.
  const now = useMemo(() => new Date().toISOString(), [entries]);

  const filtered = useMemo(() => {
    const query = search.trim().toLowerCase();
    return entries.filter((entry) => {
      if (scopeFilter !== "all" && entry.scope !== scopeFilter) return false;
      if (query && !entry.text.toLowerCase().includes(query)) return false;
      switch (stateFilter) {
        case "active":
          return entry.retired_at === null;
        case "pinned":
          return entry.pinned;
        case "expired":
          return isExpired(entry, now);
        case "merged":
          return entry.merged_from.length > 0;
        case "retired":
          return entry.retired_at !== null;
      }
    });
  }, [entries, scopeFilter, stateFilter, search, now]);

  const selected = useMemo(
    () => entries.filter((entry) => selectedIds.has(entry.id)),
    [entries, selectedIds],
  );
  /** Merging is single-scope by construction backend-side (one storage root
   * per call), so the button refuses a mixed selection here rather than
   * letting the user discover it as an error. */
  const mergeScopeKey = (entry: MemoryEntry) => `${entry.scope}:${entry.project_root ?? ""}`;
  const sameScope = selected.length > 1 && selected.every((e) => mergeScopeKey(e) === mergeScopeKey(selected[0]));

  const toggleSelected = (id: string) =>
    setSelectedIds((prev) => {
      const next = new Set(prev);
      if (!next.delete(id)) next.add(id);
      return next;
    });

  const runMerge = () => run("merge", async () => {
    const merged = await mergeMemories(
      selected.map((entry) => entry.id),
      selected[0].project_root,
      mergeText.trim() || null,
    );
    await refresh();
    setSelectedIds(new Set());
    setMergeText("");
    setMergeOpen(false);
    setSuccess(t("MemoryStudioPanel.mergeComplete", { count: merged.merged_from.length }));
  });

  const purge = () => run("purge", async () => {
    const removed = await purgeExpiredMemories();
    await refresh();
    setConfirmingPurge(false);
    setSuccess(
      removed > 0
        ? t("MemoryStudioPanel.purgeComplete", { count: removed })
        : t("MemoryStudioPanel.purgeNone"),
    );
  });

  const globalEntries = filtered.filter((entry) => entry.scope === "global");
  const projectGroups = useMemo(() => {
    const groups = new Map<string, MemoryEntry[]>();
    for (const entry of filtered) {
      if (entry.scope !== "project" || !entry.project_root) continue;
      const list = groups.get(entry.project_root) ?? [];
      list.push(entry);
      groups.set(entry.project_root, list);
    }
    return [...groups.entries()].sort((a, b) => a[0].localeCompare(b[0]));
  }, [filtered]);

  const exportAll = () => run("export", async () => {
    const path = await save({
      defaultPath: "little-monkey-memories.json",
      filters: [{ name: "JSON", extensions: ["json"] }],
    });
    if (!path) return;
    const summary = await exportMemories(path, redactOnExport);
    setSuccess(
      summary.redacted_count > 0
        ? t("MemoryStudioPanel.exportCompleteRedacted", { count: summary.count, redacted: summary.redacted_count })
        : t("MemoryStudioPanel.exportComplete", { count: summary.count }),
    );
  });

  const importFile = () => run("import", async () => {
    const path = await open({ multiple: false, directory: false, filters: [{ name: "JSON", extensions: ["json"] }] });
    if (!path) return;
    const summary = await importMemories(path);
    await refresh();
    if (summary.errors.length > 0) {
      setError(t("MemoryStudioPanel.importPartial", { added: summary.added, skipped: summary.skipped_duplicate, errors: summary.errors.join("; ") }));
    } else {
      setSuccess(t("MemoryStudioPanel.importComplete", { added: summary.added, skipped: summary.skipped_duplicate }));
    }
  });

  const scopeButtons: { id: ScopeFilter; label: string }[] = [
    { id: "all", label: t("MemoryStudioPanel.filterAll") },
    { id: "global", label: t("MemoryStudioPanel.filterGlobal") },
    { id: "project", label: t("MemoryStudioPanel.filterProject") },
  ];

  const stateButtons: { id: StateFilter; label: string }[] = [
    { id: "active", label: t("MemoryStudioPanel.filterStateActive") },
    { id: "pinned", label: t("MemoryStudioPanel.filterPinned") },
    { id: "expired", label: t("MemoryStudioPanel.filterExpired") },
    { id: "merged", label: t("MemoryStudioPanel.filterMerged") },
    { id: "retired", label: t("MemoryStudioPanel.filterRetired") },
  ];

  return (
    <div className="flex flex-col gap-4 py-2">
      <p className="text-xs text-muted">{t("MemoryStudioPanel.intro")}</p>

      <div className="rounded-lg border border-dashed border-border bg-surface-2/40 p-2.5 text-[11px] leading-relaxed text-faint">
        {t("MemoryStudioPanel.honestyNote")}
      </div>

      {statusBanner(error, success)}

      <div className="flex flex-wrap items-center gap-2">
        <div className="flex gap-1 rounded-lg border border-border bg-surface-2 p-0.5">
          {scopeButtons.map((btn) => (
            <button
              key={btn.id}
              type="button"
              onClick={() => setScopeFilter(btn.id)}
              className={`rounded-md px-2.5 py-1 text-xs font-medium transition-colors ${
                scopeFilter === btn.id ? "bg-surface text-foreground shadow-sm" : "text-muted hover:text-foreground"
              }`}
            >
              {btn.label}
            </button>
          ))}
        </div>
        <div className="flex gap-1 rounded-lg border border-border bg-surface-2 p-0.5">
          {stateButtons.map((btn) => (
            <button
              key={btn.id}
              type="button"
              onClick={() => setStateFilter(btn.id)}
              className={`rounded-md px-2.5 py-1 text-xs font-medium transition-colors ${
                stateFilter === btn.id ? "bg-surface text-foreground shadow-sm" : "text-muted hover:text-foreground"
              }`}
            >
              {btn.label}
            </button>
          ))}
        </div>
        <input
          type="text"
          value={search}
          onChange={(e) => setSearch(e.target.value)}
          placeholder={t("MemoryStudioPanel.searchPlaceholder")}
          className="min-w-[10rem] flex-1 rounded-lg border border-border bg-surface-2 px-2.5 py-1.5 text-xs text-foreground placeholder:text-faint focus:outline-none focus:ring-1 focus:ring-accent"
        />
        <button
          type="button"
          onClick={() => void refresh()}
          className="flex cursor-pointer items-center gap-1.5 rounded-lg border border-border px-2.5 py-1.5 text-xs text-muted hover:text-foreground"
        >
          <RefreshCw size={12} />
          {t("MemoryStudioPanel.refreshButton")}
        </button>
      </div>

      <div className="flex flex-wrap items-center gap-3 rounded-lg border border-border bg-background p-2.5">
        <label className="flex items-center gap-1.5 text-xs text-muted">
          <input
            type="checkbox"
            checked={redactOnExport}
            onChange={(e) => setRedactOnExport(e.target.checked)}
            className="h-3.5 w-3.5"
          />
          {t("MemoryStudioPanel.redactToggleLabel")}
        </label>
        {!redactOnExport && <span className="text-[11px] text-warning">{t("MemoryStudioPanel.redactWarning")}</span>}
        <div className="ml-auto flex gap-1.5">
          <Button size="sm" variant="secondary" onClick={() => void exportAll()} disabled={busy !== null}>
            <Download size={12} />
            {busy === "export" ? t("MemoryStudioPanel.exportingButton") : t("MemoryStudioPanel.exportButton")}
          </Button>
          <Button size="sm" variant="secondary" onClick={() => void importFile()} disabled={busy !== null}>
            <Upload size={12} />
            {busy === "import" ? t("MemoryStudioPanel.importingButton") : t("MemoryStudioPanel.importButton")}
          </Button>
          {confirmingPurge ? (
            <>
              <Button size="sm" variant="ghost" onClick={() => setConfirmingPurge(false)} disabled={busy !== null}>
                {t("MemoryStudioPanel.cancelButton")}
              </Button>
              <Button size="sm" variant="danger" onClick={() => void purge()} disabled={busy !== null}>
                {busy === "purge" ? t("MemoryStudioPanel.purgingButton") : t("MemoryStudioPanel.purgeConfirmButton")}
              </Button>
            </>
          ) : (
            <Button size="sm" variant="secondary" onClick={() => setConfirmingPurge(true)} disabled={busy !== null}>
              <Trash2 size={12} />
              {t("MemoryStudioPanel.purgeButton")}
            </Button>
          )}
        </div>
      </div>

      {selected.length > 0 && (
        <div className="flex flex-col gap-1.5 rounded-lg border border-border bg-background p-2.5">
          <div className="flex flex-wrap items-center gap-2">
            <Button
              size="sm"
              variant="secondary"
              onClick={() => setMergeOpen((open) => !open)}
              disabled={!sameScope || busy !== null}
            >
              <Merge size={12} />
              {t("MemoryStudioPanel.mergeSelectedButton", { count: selected.length })}
            </Button>
            <Button size="sm" variant="ghost" onClick={() => setSelectedIds(new Set())} disabled={busy !== null}>
              {t("MemoryStudioPanel.cancelButton")}
            </Button>
            {!sameScope && selected.length > 1 && (
              <span className="text-[11px] text-warning">{t("MemoryStudioPanel.mergeScopeWarning")}</span>
            )}
          </div>
          {mergeOpen && sameScope && (
            <div className="flex flex-col gap-1.5">
              <textarea
                value={mergeText}
                onChange={(e) => setMergeText(e.target.value)}
                rows={2}
                spellCheck={false}
                placeholder={t("MemoryStudioPanel.mergeTextPlaceholder")}
                className="w-full resize-y rounded-md border border-border bg-surface px-2 py-1.5 text-xs text-foreground focus:outline-none focus:ring-2 focus:ring-accent"
              />
              <div className="flex justify-end">
                <Button size="sm" variant="primary" onClick={() => void runMerge()} disabled={busy !== null}>
                  {busy === "merge" ? t("MemoryStudioPanel.mergingButton") : t("MemoryStudioPanel.mergeConfirmButton")}
                </Button>
              </div>
            </div>
          )}
        </div>
      )}

      {loading ? (
        <p className="text-xs text-faint">{t("MemoryStudioPanel.loading")}</p>
      ) : filtered.length === 0 ? (
        <p className="text-xs text-faint">{entries.length === 0 ? t("MemoryStudioPanel.empty") : t("MemoryStudioPanel.noMatches")}</p>
      ) : (
        <div className="flex flex-col gap-4">
          {globalEntries.length > 0 && (
            <section>
              <h3 className="mb-2 text-xs font-semibold uppercase tracking-wide text-faint">
                {t("MemoryStudioPanel.globalSectionHeading", { count: globalEntries.length })}
              </h3>
              <div className="flex flex-col gap-2">
                {globalEntries.map((entry) => (
                  <MemoryRow
                    key={entry.id}
                    entry={entry}
                    now={now}
                    selected={selectedIds.has(entry.id)}
                    onToggleSelected={() => toggleSelected(entry.id)}
                    onChanged={refresh}
                  />
                ))}
              </div>
            </section>
          )}

          {projectGroups.map(([root, group]) => (
            <section key={root}>
              <h3 className="mb-2 flex items-baseline gap-2 text-xs font-semibold uppercase tracking-wide text-faint">
                <span>{t("MemoryStudioPanel.projectSectionHeading", { name: projectBasename(root), count: group.length })}</span>
              </h3>
              <p className="mb-2 truncate text-[10px] text-faint" title={root}>{root}</p>
              <div className="flex flex-col gap-2">
                {group.map((entry) => (
                  <MemoryRow
                    key={entry.id}
                    entry={entry}
                    now={now}
                    selected={selectedIds.has(entry.id)}
                    onToggleSelected={() => toggleSelected(entry.id)}
                    onChanged={refresh}
                  />
                ))}
              </div>
            </section>
          ))}
        </div>
      )}
    </div>
  );
}
