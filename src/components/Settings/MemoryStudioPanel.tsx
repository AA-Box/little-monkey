import { useEffect, useMemo, useState } from "react";
import { open, save } from "@tauri-apps/plugin-dialog";
import { Download, Pencil, Power, RefreshCw, Trash2, Upload } from "lucide-react";

import { Button, StatusPill } from "../ui";
import { useT } from "../../lib/i18n";
import {
  deleteMemory,
  exportMemories,
  importMemories,
  listAllMemories,
  setMemoryEnabled,
  updateMemory,
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

/** One memory row: metadata display (source, scope, created date, source
 * turn when known, enabled/disabled), inline edit, enable/disable toggle,
 * and delete — the "edit, pin, ..., delete, disable" actions from
 * ROADMAP.md's Memory Studio spec that this system can actually back (see
 * the panel-level info note for which of that list — pin/expiry,
 * confidence, source file/connector, last-used, "why do you know this"
 * chat linking — are honestly not implemented and why). */
function MemoryRow({ entry, onChanged }: { entry: MemoryEntry; onChanged: () => Promise<void> }) {
  const { t } = useT();
  const [editing, setEditing] = useState(false);
  const [text, setText] = useState(entry.text);
  const [saving, setSaving] = useState(false);
  const [togglingEnabled, setTogglingEnabled] = useState(false);
  const [confirmingDelete, setConfirmingDelete] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const [rowError, setRowError] = useState<string | null>(null);

  const overLimit = charCount(text) > MAX_FACT_CHARS;

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
      className={`rounded-lg border p-2.5 ${entry.enabled ? "border-border bg-background" : "border-border/60 bg-surface-2/50"}`}
    >
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="flex flex-wrap items-center gap-1.5">
          <StatusPill tone="neutral">
            {entry.source === "agent" ? t("MemoryStudioPanel.sourceAgent") : t("MemoryStudioPanel.sourceUser")}
          </StatusPill>
          <StatusPill tone={entry.enabled ? "success" : "warning"}>
            {entry.enabled ? t("MemoryStudioPanel.statusEnabled") : t("MemoryStudioPanel.statusDisabled")}
          </StatusPill>
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
              {confirmingDelete ? (
                <span className="flex items-center gap-1">
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

      <p className="mt-1.5 text-[10px] text-faint">{t("MemoryStudioPanel.createdAt", { date: formatDate(entry.created_at) })}</p>
      {rowError && <p className="mt-1.5 text-xs text-danger">{t("MemoryStudioPanel.errorPrefix", { error: rowError })}</p>}
    </div>
  );
}

/**
 * Memory Studio (ROADMAP.md Phase 1): the full-control surface over every
 * durable memory this app has ever stored, built directly on the existing
 * `memory.rs` fact store (see that module's docs) rather than a second
 * memory system. Lists every memory across every scope the backend can
 * actually represent — `"global"` (applies to every project) and
 * `"project"` (one workspace root) — with filter/search, per-memory
 * metadata (source, scope, created date, source turn when known, enabled
 * state), and edit/enable-disable/delete/import/export actions.
 *
 * Deleting or disabling a memory here takes effect immediately: both routes
 * go through `memory.rs`'s `delete_fact_impl`/`set_enabled_impl`, and the
 * very next `memory_list` call (once per turn, via `rulesStore.refresh()`)
 * excludes it — see `memory.rs`'s `list_impl` and its
 * `disabled_and_deleted_facts_are_excluded_from_list_impl` test for the
 * direct proof.
 */
export function MemoryStudioPanel() {
  const { t } = useT();
  const [entries, setEntries] = useState<MemoryEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [scopeFilter, setScopeFilter] = useState<ScopeFilter>("all");
  const [search, setSearch] = useState("");
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

  const filtered = useMemo(() => {
    const query = search.trim().toLowerCase();
    return entries.filter((entry) => {
      if (scopeFilter !== "all" && entry.scope !== scopeFilter) return false;
      if (query && !entry.text.toLowerCase().includes(query)) return false;
      return true;
    });
  }, [entries, scopeFilter, search]);

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
        </div>
      </div>

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
                  <MemoryRow key={entry.id} entry={entry} onChanged={refresh} />
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
                  <MemoryRow key={entry.id} entry={entry} onChanged={refresh} />
                ))}
              </div>
            </section>
          ))}
        </div>
      )}
    </div>
  );
}
