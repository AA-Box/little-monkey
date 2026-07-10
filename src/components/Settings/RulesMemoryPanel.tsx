import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Button } from "../ui";
import { useT } from "../../lib/i18n";
import { useRulesStore } from "../../store/rulesStore";
import { useWorkspaceStore } from "../../store/workspaceStore";

/**
 * Filename `rules_write`'s `"project"` scope expects to find at the top of a
 * workspace root — must match `RULE_FILE_NAME` in `src-tauri/src/rules.rs`.
 */
const RULE_FILE_NAME = "MONKEY.md";

/** Must match `MAX_RULE_CHARS` in `src-tauri/src/rules.rs`. */
const MAX_RULE_CHARS = 16_000;

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

  const overLimit = content.length > MAX_RULE_CHARS;

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
          {t("RulesMemoryPanel.charCount", { count: content.length, max: MAX_RULE_CHARS })}
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

/**
 * Settings "Rules" tab: editors for every MONKEY.md file currently in
 * effect — the global app-data file (applies to every project) plus one per
 * attached workspace root (primary and every secondary), mirroring how
 * `rules.rs`'s `rules_read` assembles them for the system prompt.
 *
 * Fact-memory management (slice 4) intentionally isn't here yet — this tab
 * is rules-only for now.
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

  async function saveGlobal(content: string) {
    await invoke("rules_write", { scope: "global", rootPath: null, content });
    await refreshRules();
  }

  async function saveProject(rootPath: string, content: string) {
    await invoke("rules_write", { scope: "project", rootPath, content });
    await refreshRules();
  }

  return (
    <div className="flex flex-col gap-4 p-2">
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
    </div>
  );
}
