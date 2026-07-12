import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { CheckCircle2, ChevronRight, Clock, FolderOpen, Pencil, Play, Plus, Trash2, XCircle } from "lucide-react";

import { Button, IconButton, StatusPill } from "../ui";
import type { PillTone } from "../ui";
import { useT } from "../../lib/i18n";
import { useRecipeStore, type DiscoveredRecipe, type Recipe } from "../../store/recipeStore";
import { useAutomationsStore } from "../../store/automationsStore";
import { useSessionStore } from "../../store/sessionStore";
import { runRecipeNow } from "../../lib/recipeRunner";

const SOURCE_TONE: Record<string, PillTone> = {
  workspace: "success",
  global: "neutral",
};

const SAMPLE_RECIPE_YAML = `version: 1
name: my-recipe
description: What this recipe does
target:
  ollama: qwen2.5:14b
permission_mode: acceptEdits
prompt: |
  Do the thing.
params: {}
`;

/** One recipe's row: name, source/permission badges, path, and its actions
 * (Run now / Edit / Delete). Broken files (failed to parse) render their
 * error instead of the usual row, matching `recipes.rs`'s
 * "surface the file, don't drop it" stance. */
function RecipeRow({
  entry,
  onEdit,
  onRun,
  onDelete,
  busy,
}: {
  entry: DiscoveredRecipe;
  onEdit: () => void;
  onRun: () => void;
  onDelete: () => void;
  busy: boolean;
}) {
  const { t } = useT();

  if (!entry.recipe) {
    return (
      <div className="flex items-start gap-2 rounded-lg border border-danger/40 bg-danger/5 px-3 py-2 text-xs">
        <XCircle size={14} className="mt-0.5 shrink-0 text-danger" />
        <div className="min-w-0">
          <p className="truncate font-medium text-foreground">{entry.path}</p>
          <p className="text-danger">{entry.error}</p>
        </div>
      </div>
    );
  }

  const recipe = entry.recipe;
  return (
    <div className="flex flex-col rounded-lg border border-border bg-surface">
      <div className="flex items-center gap-3 px-3 py-2">
        <CheckCircle2 size={14} className="shrink-0 text-success" />
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <span className="truncate text-sm font-medium text-foreground">{recipe.name}</span>
            <StatusPill tone={SOURCE_TONE[entry.source] ?? "neutral"}>{entry.source}</StatusPill>
            <StatusPill tone="neutral">{recipe.permission_mode}</StatusPill>
          </div>
          <p className="truncate text-xs text-faint">{recipe.description ?? entry.path}</p>
        </div>
        <div className="flex shrink-0 items-center gap-1">
          <IconButton size="sm" onClick={onRun} disabled={busy} aria-label={t("ScheduledTasksPanel.runNow")}>
            <Play size={14} />
          </IconButton>
          <IconButton size="sm" onClick={onEdit} disabled={busy} aria-label={t("ScheduledTasksPanel.edit")}>
            <Pencil size={14} />
          </IconButton>
          {entry.source === "global" && (
            <IconButton size="sm" onClick={onDelete} disabled={busy} aria-label={t("ScheduledTasksPanel.delete")}>
              <Trash2 size={14} />
            </IconButton>
          )}
        </div>
      </div>
      <ScheduleControls recipeName={recipe.name} />
    </div>
  );
}

const RUN_STATUS_TONE: Record<string, PillTone> = {
  ok: "success",
  error: "danger",
  denied: "warning",
};

/**
 * A recipe's schedule (design doc slice 3): finds (or lazily creates, on
 * first enable) the single `AutomationEntry` for this recipe, and lets the
 * user edit its cron expression — validated live via `cron_validate`,
 * showing croner's own human-readable description so a typo or an
 * unexpected-day mismatch (POSIX vs. Quartz weekday numbering) is caught
 * before saving — its enabled toggle, and its last-run status with a
 * jump-to-session link.
 */
function ScheduleControls({ recipeName }: { recipeName: string }) {
  const { t } = useT();
  const entries = useAutomationsStore((s) => s.entries);
  const addEntry = useAutomationsStore((s) => s.addEntry);
  const updateEntry = useAutomationsStore((s) => s.updateEntry);
  const switchSession = useSessionStore((s) => s.switchSession);
  const entry = entries.find((e) => e.recipeName === recipeName);

  const [cronDraft, setCronDraft] = useState(entry?.cron ?? "0 3 * * *");
  const [cronDescription, setCronDescription] = useState<{ ok: boolean; message: string } | null>(null);

  useEffect(() => {
    setCronDraft(entry?.cron ?? "0 3 * * *");
  }, [entry?.cron]);

  const validateCron = async (expr: string) => {
    try {
      const description = await invoke<string>("cron_validate", { expr });
      setCronDescription({ ok: true, message: description });
    } catch (err) {
      setCronDescription({ ok: false, message: err instanceof Error ? err.message : String(err) });
    }
  };

  const handleToggle = (enabled: boolean) => {
    if (entry) {
      updateEntry(entry.id, { enabled });
    } else if (enabled) {
      addEntry({ recipeName, cron: cronDraft, enabled: true, catchUpIfMissed: false });
      void validateCron(cronDraft);
    }
  };

  const handleCronBlur = () => {
    if (entry) updateEntry(entry.id, { cron: cronDraft });
    void validateCron(cronDraft);
  };

  return (
    <div className="flex flex-col gap-1.5 border-t border-border px-3 py-2 text-xs">
      <div className="flex items-center gap-2">
        <input
          type="checkbox"
          checked={entry?.enabled ?? false}
          onChange={(e) => handleToggle(e.target.checked)}
          aria-label={t("ScheduledTasksPanel.scheduleEnabled")}
        />
        <Clock size={12} className="shrink-0 text-faint" />
        <input
          type="text"
          value={cronDraft}
          onChange={(e) => setCronDraft(e.target.value)}
          onBlur={handleCronBlur}
          placeholder="0 3 * * *"
          className="w-32 rounded border border-border bg-background px-1.5 py-0.5 font-mono text-xs text-foreground focus:outline-none focus:ring-1 focus:ring-accent"
        />
        {entry?.lastStatus && (
          <StatusPill tone={RUN_STATUS_TONE[entry.lastStatus] ?? "neutral"}>{entry.lastStatus}</StatusPill>
        )}
        {entry?.lastRunAt && (
          <span className="text-faint">{t("ScheduledTasksPanel.lastRun", { time: new Date(entry.lastRunAt).toLocaleString() })}</span>
        )}
        {entry?.lastSessionId && (
          <button
            type="button"
            onClick={() => entry.lastSessionId && switchSession(entry.lastSessionId)}
            className="cursor-pointer text-accent hover:underline"
          >
            {t("ScheduledTasksPanel.viewSession")}
          </button>
        )}
      </div>
      {cronDescription && (
        <p className={cronDescription.ok ? "text-faint" : "text-danger"}>{cronDescription.message}</p>
      )}
    </div>
  );
}

/**
 * "Tasks" Settings tab (design doc slice 2): lists every recipe visible from
 * the current workspace (`.littlemonkey/recipes/`) plus the global recipes
 * directory, with an inline YAML editor (validate-on-demand via
 * `recipes_validate`) for creating/editing global recipes, and a "Run now"
 * that starts an ordinary tagged chat session via `recipeRunner.ts`.
 * Workspace-committed recipes are read-only here by design — they're meant
 * to be edited in a text editor and checked in, not mutated from the app.
 */
export function ScheduledTasksPanel() {
  const { t } = useT();
  const recipes = useRecipeStore((s) => s.recipes);
  const loading = useRecipeStore((s) => s.loading);
  const listError = useRecipeStore((s) => s.error);
  const refresh = useRecipeStore((s) => s.refresh);
  const save = useRecipeStore((s) => s.save);
  const remove = useRecipeStore((s) => s.remove);
  const validate = useRecipeStore((s) => s.validate);
  const readRaw = useRecipeStore((s) => s.readRaw);

  const [editing, setEditing] = useState<{ name: string; content: string; isNew: boolean } | null>(null);
  const [validation, setValidation] = useState<{ ok: boolean; message: string } | null>(null);
  const [busyName, setBusyName] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const startNew = () => {
    setEditing({ name: "", content: SAMPLE_RECIPE_YAML, isNew: true });
    setValidation(null);
    setActionError(null);
  };

  const startEdit = async (recipe: Recipe) => {
    setActionError(null);
    try {
      const content = await readRaw(recipe.name);
      setEditing({ name: recipe.name, content, isNew: false });
      setValidation(null);
    } catch (err) {
      setActionError(err instanceof Error ? err.message : String(err));
    }
  };

  const handleValidate = async () => {
    if (!editing) return;
    try {
      const recipe = await validate(editing.content);
      setValidation({ ok: true, message: t("ScheduledTasksPanel.validateOk", { name: recipe.name }) });
    } catch (err) {
      setValidation({ ok: false, message: err instanceof Error ? err.message : String(err) });
    }
  };

  const handleSave = async () => {
    if (!editing) return;
    try {
      const recipe = await validate(editing.content);
      await save(recipe.name, editing.content);
      setEditing(null);
      setValidation(null);
    } catch (err) {
      setValidation({ ok: false, message: err instanceof Error ? err.message : String(err) });
    }
  };

  const handleDelete = async (name: string) => {
    setActionError(null);
    setBusyName(name);
    try {
      await remove(name);
    } catch (err) {
      setActionError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusyName(null);
    }
  };

  const handleRun = async (recipe: Recipe) => {
    setActionError(null);
    setBusyName(recipe.name);
    try {
      await runRecipeNow(recipe);
    } catch (err) {
      setActionError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusyName(null);
    }
  };

  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <p className="min-w-0 flex-1 text-xs text-muted">{t("ScheduledTasksPanel.description")}</p>
        <Button size="sm" onClick={startNew} className="shrink-0 whitespace-nowrap">
          <Plus size={14} />
          {t("ScheduledTasksPanel.newRecipe")}
        </Button>
      </div>

      {listError && <p className="text-xs text-danger">{listError}</p>}
      {actionError && <p className="text-xs text-danger">{actionError}</p>}

      {editing && (
        <div className="flex flex-col gap-2 rounded-lg border border-border bg-surface-2 p-3">
          <div className="flex items-center gap-2 text-xs font-medium text-foreground">
            <FolderOpen size={14} />
            {editing.isNew ? t("ScheduledTasksPanel.newRecipeTitle") : t("ScheduledTasksPanel.editRecipeTitle", { name: editing.name })}
          </div>
          <textarea
            value={editing.content}
            onChange={(e) => setEditing({ ...editing, content: e.target.value })}
            spellCheck={false}
            rows={14}
            className="w-full rounded-md border border-border bg-background px-3 py-2 font-mono text-xs text-foreground focus:outline-none focus:ring-1 focus:ring-accent"
          />
          {validation && (
            <p className={`text-xs ${validation.ok ? "text-success" : "text-danger"}`}>{validation.message}</p>
          )}
          <div className="flex items-center gap-2">
            <Button size="sm" variant="secondary" onClick={() => void handleValidate()}>
              {t("ScheduledTasksPanel.validate")}
            </Button>
            <Button size="sm" variant="primary" onClick={() => void handleSave()}>
              {t("ScheduledTasksPanel.save")}
            </Button>
            <Button size="sm" variant="ghost" onClick={() => setEditing(null)}>
              {t("ScheduledTasksPanel.cancel")}
            </Button>
          </div>
        </div>
      )}

      <div className="flex flex-col gap-2">
        {loading && recipes.length === 0 && <p className="text-xs text-faint">{t("ScheduledTasksPanel.loading")}</p>}
        {!loading && recipes.length === 0 && (
          <div className="flex items-center gap-2 rounded-lg border border-dashed border-border px-3 py-4 text-xs text-faint">
            <ChevronRight size={14} />
            {t("ScheduledTasksPanel.empty")}
          </div>
        )}
        {recipes.map((entry) => (
          <RecipeRow
            key={entry.path}
            entry={entry}
            busy={busyName === entry.recipe?.name}
            onEdit={() => entry.recipe && void startEdit(entry.recipe)}
            onRun={() => entry.recipe && void handleRun(entry.recipe)}
            onDelete={() => entry.recipe && void handleDelete(entry.recipe.name)}
          />
        ))}
      </div>
    </div>
  );
}
