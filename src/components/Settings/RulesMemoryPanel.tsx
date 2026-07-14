import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Pencil, Plus, Trash2 } from "lucide-react";
import { Button } from "../ui";
import { useT } from "../../lib/i18n";
import { useRulesStore, type MemoryFact } from "../../store/rulesStore";
import { useWorkspaceStore } from "../../store/workspaceStore";
import { useSettingsStore } from "../../store/settingsStore";

/**
 * Filename `rules_write`'s `"project"` scope expects to find at the top of a
 * workspace root — must match `RULE_FILE_NAME` in `src-tauri/src/rules.rs`.
 */
const RULE_FILE_NAME = "MONKEY.md";

/** Must match `MAX_RULE_CHARS` in `src-tauri/src/rules.rs`. */
const MAX_RULE_CHARS = 16_000;

/** Must match `MAX_FACT_CHARS` in `src-tauri/src/memory.rs`. */
const MAX_FACT_CHARS = 500;

/**
 * Counts Unicode scalar values (code points), matching Rust's
 * `str::chars().count()` — which is what `rules.rs`/`memory.rs` actually cap
 * against. JS's `.length` counts UTF-16 code *units* instead, so text with
 * astral-plane characters (e.g. many emoji) reports roughly double the
 * backend's count — enough to wrongly trip these client-side "over limit"
 * checks (and disable Save/Add) for content the backend would accept fine.
 * `Array.from` iterates by code point, same as Rust's `chars()`.
 */
function charCount(text: string): number {
  return Array.from(text).length;
}

/** No shared toggle-switch component exists in `ui/` yet — cloned from
 * `AutomationPanel.tsx`'s local `Toggle` rather than promoted prematurely. */
function Toggle({
  checked,
  onChange,
  label,
  description,
}: {
  checked: boolean;
  onChange: (value: boolean) => void;
  label: string;
  description?: string;
}) {
  return (
    <label className="flex flex-col gap-0.5 py-2.5">
      <span className="flex items-center justify-between gap-3">
        <span className="text-sm font-medium text-foreground">{label}</span>
        <button
          type="button"
          role="switch"
          aria-checked={checked}
          aria-label={label}
          onClick={() => onChange(!checked)}
          className={`relative h-5 w-9 shrink-0 cursor-pointer rounded-full transition-colors ${
            checked ? "bg-accent" : "border border-border bg-surface-2"
          }`}
        >
          <span
            className={`absolute top-0.5 h-4 w-4 rounded-full bg-white shadow transition-[left] ${
              checked ? "left-[18px]" : "left-0.5"
            }`}
          />
        </button>
      </span>
      {description && <p className="pr-12 text-xs text-muted">{description}</p>}
    </label>
  );
}

interface RuleEditorProps {
  heading: string;
  description: string;
  placeholder: string;
  initialContent: string;
  truncated: boolean;
  /** Whether a file already exists on disk for this scope/root — drives the
   * "Save" vs "Create MONKEY.md" button label. */
  exists: boolean;
  onSave: (content: string) => Promise<void>;
}

/** One MONKEY.md editor — reused for the global file and for each attached
 * project root. Local edit state only; the caller owns persistence. */
function RuleEditor({ heading, description, placeholder, initialContent, truncated, exists, onSave }: RuleEditorProps) {
  const { t } = useT();
  const [content, setContent] = useState(initialContent);
  const [dirty, setDirty] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [justSaved, setJustSaved] = useState(false);

  // Picks up external edits to the file (or the freshly-created file after a
  // successful save) as long as the user isn't mid-edit locally.
  useEffect(() => {
    if (!dirty) setContent(initialContent);
  }, [initialContent, dirty]);

  const overLimit = charCount(content) > MAX_RULE_CHARS;

  async function handleSave() {
    setSaving(true);
    setError(null);
    try {
      await onSave(content);
      setDirty(false);
      setJustSaved(true);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  }

  return (
    <div className="rounded-lg border border-border bg-background p-3">
      <div className="mb-1 flex items-center justify-between gap-2">
        <h4 className="truncate text-sm font-medium text-foreground">{heading}</h4>
        <span className={`shrink-0 text-xs ${overLimit ? "text-danger" : "text-faint"}`}>
          {t("RulesMemoryPanel.charCount", { count: charCount(content), max: MAX_RULE_CHARS })}
        </span>
      </div>
      <p className="mb-2 text-xs text-muted">{description}</p>
      {truncated && (
        <p className="mb-2 rounded-md bg-warning-soft px-2 py-1 text-xs text-warning">
          {t("RulesMemoryPanel.truncatedWarning")}
        </p>
      )}
      <textarea
        value={content}
        onChange={(e) => {
          setContent(e.target.value);
          setDirty(true);
          setJustSaved(false);
        }}
        placeholder={placeholder}
        rows={8}
        spellCheck={false}
        className="w-full resize-y rounded-md border border-border bg-surface px-2.5 py-2 font-mono text-xs text-foreground focus:outline-none focus:ring-2 focus:ring-accent"
      />
      <div className="mt-2 flex items-center justify-between gap-2">
        <span className="text-xs">
          {error ? (
            <span className="text-danger">{t("RulesMemoryPanel.saveErrorPrefix", { error })}</span>
          ) : justSaved && !dirty ? (
            <span className="text-faint">{t("RulesMemoryPanel.savedStatus")}</span>
          ) : null}
        </span>
        <Button size="sm" variant="primary" onClick={() => void handleSave()} disabled={saving || (!dirty && exists)}>
          {saving
            ? t("RulesMemoryPanel.savingButton")
            : exists
              ? t("RulesMemoryPanel.saveButton")
              : t("RulesMemoryPanel.createButton")}
        </Button>
      </div>
    </div>
  );
}

/** One remembered fact in the Settings list: source badge, inline edit
 * (`memory_update`), and delete (`memory_delete`) — the transcript's
 * `MemoryRow` "Forget" button covers the same delete action from the chat
 * side, this is the equivalent from Settings. */
function FactRow({ fact, onChanged }: { fact: MemoryFact; onChanged: () => Promise<void> }) {
  const { t } = useT();
  const [editing, setEditing] = useState(false);
  const [text, setText] = useState(fact.text);
  const [saving, setSaving] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const overLimit = charCount(text) > MAX_FACT_CHARS;

  async function handleSave() {
    setSaving(true);
    setError(null);
    try {
      await invoke("memory_update", { id: fact.id, text });
      await onChanged();
      setEditing(false);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  }

  async function handleDelete() {
    setDeleting(true);
    setError(null);
    try {
      await invoke("memory_delete", { id: fact.id });
      await onChanged();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setDeleting(false);
    }
  }

  return (
    <div className="rounded-lg border border-border bg-background p-2.5">
      <div className="flex items-start justify-between gap-2">
        <span
          className={`shrink-0 rounded-full px-1.5 py-0.5 text-[10px] font-medium uppercase tracking-wide ${
            fact.source === "agent" ? "bg-accent-soft text-accent" : "bg-surface-2 text-muted"
          }`}
        >
          {fact.source === "agent" ? t("RulesMemoryPanel.memorySourceAgent") : t("RulesMemoryPanel.memorySourceUser")}
        </span>
        <div className="flex shrink-0 items-center gap-1">
          {!editing && (
            <>
              <button
                type="button"
                onClick={() => {
                  setText(fact.text);
                  setEditing(true);
                  setError(null);
                }}
                className="flex cursor-pointer items-center gap-1 rounded-md border border-border px-1.5 py-0.5 text-xs text-muted transition-colors hover:text-foreground"
              >
                <Pencil size={11} />
                {t("RulesMemoryPanel.memoryEditButton")}
              </button>
              <button
                type="button"
                onClick={() => void handleDelete()}
                disabled={deleting}
                className="flex cursor-pointer items-center gap-1 rounded-md border border-border px-1.5 py-0.5 text-xs text-muted transition-colors hover:text-danger disabled:cursor-not-allowed disabled:opacity-50"
              >
                <Trash2 size={11} />
                {deleting ? t("RulesMemoryPanel.memoryDeletingButton") : t("RulesMemoryPanel.memoryDeleteButton")}
              </button>
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
              {t("RulesMemoryPanel.charCount", { count: charCount(text), max: MAX_FACT_CHARS })}
            </span>
            <div className="flex gap-1.5">
              <Button size="sm" variant="ghost" onClick={() => setEditing(false)} disabled={saving}>
                {t("RulesMemoryPanel.memoryCancelButton")}
              </Button>
              <Button size="sm" variant="primary" onClick={() => void handleSave()} disabled={saving || overLimit || text.trim().length === 0}>
                {saving ? t("RulesMemoryPanel.savingButton") : t("RulesMemoryPanel.memorySaveButton")}
              </Button>
            </div>
          </div>
        </div>
      ) : (
        <p className="mt-1.5 whitespace-pre-wrap break-words text-xs text-foreground">{fact.text}</p>
      )}
      {error && <p className="mt-1.5 text-xs text-danger">{t("RulesMemoryPanel.memoryErrorPrefix", { error })}</p>}
    </div>
  );
}

/**
 * Fact-memory management section: every remembered fact for the current
 * primary workspace root, with inline edit/delete, a confirm-gated "Clear
 * all", and a manual "Add fact" affordance (recorded with `source: "user"`
 * via `memory_add`, same as `tool_remember` records `source: "agent"`).
 */
function MemorySection({ hasWorkspace }: { hasWorkspace: boolean }) {
  const { t } = useT();
  const facts = useRulesStore((s) => s.facts);
  const refresh = useRulesStore((s) => s.refresh);
  const memoryEnabled = useSettingsStore((s) => s.memoryEnabled);
  const setMemoryEnabled = useSettingsStore((s) => s.setMemoryEnabled);

  const [newFactText, setNewFactText] = useState("");
  const [adding, setAdding] = useState(false);
  const [addError, setAddError] = useState<string | null>(null);
  const [confirmingClear, setConfirmingClear] = useState(false);
  const [clearing, setClearing] = useState(false);
  const [clearError, setClearError] = useState<string | null>(null);

  async function handleAdd() {
    setAdding(true);
    setAddError(null);
    try {
      await invoke("memory_add", { text: newFactText });
      await refresh();
      setNewFactText("");
    } catch (e) {
      setAddError(e instanceof Error ? e.message : String(e));
    } finally {
      setAdding(false);
    }
  }

  async function handleClearAll() {
    setClearing(true);
    setClearError(null);
    try {
      await invoke("memory_clear");
      await refresh();
      setConfirmingClear(false);
    } catch (e) {
      setClearError(e instanceof Error ? e.message : String(e));
    } finally {
      setClearing(false);
    }
  }

  const overLimit = charCount(newFactText) > MAX_FACT_CHARS;

  return (
    <section>
      <h3 className="mb-2 text-xs font-semibold uppercase tracking-wide text-faint">
        {t("RulesMemoryPanel.memoryHeading")}
      </h3>
      <Toggle
        checked={memoryEnabled}
        onChange={setMemoryEnabled}
        label={t("RulesMemoryPanel.memoryToggleLabel")}
        description={t("RulesMemoryPanel.memoryToggleDescription")}
      />
      <div className="flex flex-col gap-3">
        <div className="flex flex-col gap-2">
          {facts.length === 0 ? (
            <p className="text-xs text-faint">{t("RulesMemoryPanel.memoryEmpty")}</p>
          ) : (
            facts.map((fact) => <FactRow key={fact.id} fact={fact} onChanged={refresh} />)
          )}
        </div>

        {facts.length > 0 && hasWorkspace && (
          <div className="flex items-center justify-between gap-2 border-t border-border pt-2">
            {confirmingClear ? (
              <div className="flex flex-1 items-center justify-between gap-2">
                <span className="text-xs text-warning">
                  {t("RulesMemoryPanel.memoryClearConfirmPrompt", { count: facts.length })}
                </span>
                <div className="flex shrink-0 gap-1.5">
                  <Button size="sm" variant="ghost" onClick={() => setConfirmingClear(false)} disabled={clearing}>
                    {t("RulesMemoryPanel.memoryClearCancelButton")}
                  </Button>
                  <Button size="sm" variant="danger" onClick={() => void handleClearAll()} disabled={clearing}>
                    {clearing ? t("RulesMemoryPanel.memoryClearingButton") : t("RulesMemoryPanel.memoryClearConfirmButton")}
                  </Button>
                </div>
              </div>
            ) : (
              <Button size="sm" variant="ghost" onClick={() => setConfirmingClear(true)}>
                <Trash2 size={12} />
                {t("RulesMemoryPanel.memoryClearAllButton")}
              </Button>
            )}
          </div>
        )}
        {clearError && <p className="text-xs text-danger">{t("RulesMemoryPanel.memoryErrorPrefix", { error: clearError })}</p>}

        {hasWorkspace ? (
          <div className="flex flex-col gap-1.5 rounded-lg border border-dashed border-border p-2.5">
            <textarea
              value={newFactText}
              onChange={(e) => setNewFactText(e.target.value)}
              placeholder={t("RulesMemoryPanel.memoryAddPlaceholder")}
              rows={2}
              spellCheck={false}
              className="w-full resize-y rounded-md border border-border bg-surface px-2 py-1.5 text-xs text-foreground focus:outline-none focus:ring-2 focus:ring-accent"
            />
            <div className="flex items-center justify-between gap-2">
              <span className={`text-xs ${overLimit ? "text-danger" : "text-faint"}`}>
                {t("RulesMemoryPanel.charCount", { count: charCount(newFactText), max: MAX_FACT_CHARS })}
              </span>
              <Button
                size="sm"
                variant="primary"
                onClick={() => void handleAdd()}
                disabled={adding || overLimit || newFactText.trim().length === 0}
              >
                <Plus size={12} />
                {adding ? t("RulesMemoryPanel.memoryAddingButton") : t("RulesMemoryPanel.memoryAddButton")}
              </Button>
            </div>
            {addError && <p className="text-xs text-danger">{t("RulesMemoryPanel.memoryErrorPrefix", { error: addError })}</p>}
          </div>
        ) : (
          <p className="text-xs text-faint">{t("RulesMemoryPanel.memoryNoWorkspaceOpen")}</p>
        )}
      </div>
    </section>
  );
}

/**
 * Settings "Rules" tab: editors for every MONKEY.md file currently in
 * effect — the global app-data file (applies to every project) plus one per
 * attached workspace root (primary and every secondary), mirroring how
 * `rules.rs`'s `rules_read` assembles them for the system prompt — plus
 * (slice 4) fact-memory management for the current primary root.
 */
export function RulesMemoryPanel() {
  const { t } = useT();
  const rules = useRulesStore((s) => s.rules);
  const refreshRules = useRulesStore((s) => s.refresh);
  const roots = useWorkspaceStore((s) => s.roots);
  const refreshRoots = useWorkspaceStore((s) => s.refreshRoots);

  useEffect(() => {
    void refreshRules();
    void refreshRoots();
  }, [refreshRules, refreshRoots]);

  const globalRule = rules.find((r) => r.scope === "global") ?? null;
  const hasPrimaryRoot = roots.some((r) => r.is_primary);

  async function saveGlobal(content: string) {
    await invoke("rules_write", { scope: "global", rootPath: null, content });
    await refreshRules();
  }

  async function saveProject(rootPath: string, content: string) {
    await invoke("rules_write", { scope: "project", rootPath, content });
    await refreshRules();
  }

  return (
    <div className="flex flex-col gap-4 py-2">
      <section>
        <h3 className="mb-2 text-xs font-semibold uppercase tracking-wide text-faint">
          {t("RulesMemoryPanel.globalHeading")}
        </h3>
        <RuleEditor
          heading={t("RulesMemoryPanel.globalFileLabel")}
          description={t("RulesMemoryPanel.globalDescription")}
          placeholder={t("RulesMemoryPanel.globalPlaceholder")}
          initialContent={globalRule?.content ?? ""}
          truncated={globalRule?.truncated ?? false}
          exists={globalRule != null}
          onSave={saveGlobal}
        />
      </section>

      <section>
        <h3 className="mb-2 text-xs font-semibold uppercase tracking-wide text-faint">
          {t("RulesMemoryPanel.projectHeading")}
        </h3>
        {roots.length === 0 ? (
          <p className="text-xs text-faint">{t("RulesMemoryPanel.noWorkspaceOpen")}</p>
        ) : (
          <div className="flex flex-col gap-3">
            {roots.map((root) => {
              // Same resolvable-path convention `resolve_path_and_root` uses
              // for tool calls: plain filename for the primary root, label
              // prefix for an attached secondary.
              const rootPath = root.is_primary ? RULE_FILE_NAME : `${root.label}/${RULE_FILE_NAME}`;
              const rule = rules.find((r) => r.scope === "project" && r.label === root.label) ?? null;
              return (
                <RuleEditor
                  key={root.id}
                  heading={root.label}
                  description={t("RulesMemoryPanel.projectDescription")}
                  placeholder={t("RulesMemoryPanel.projectPlaceholder")}
                  initialContent={rule?.content ?? ""}
                  truncated={rule?.truncated ?? false}
                  exists={rule != null}
                  onSave={(content) => saveProject(rootPath, content)}
                />
              );
            })}
          </div>
        )}
      </section>

      <MemorySection hasWorkspace={hasPrimaryRoot} />
    </div>
  );
}
