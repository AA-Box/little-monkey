import { useMemo, useState } from "react";
import { open, save } from "@tauri-apps/plugin-dialog";
import { invoke } from "@tauri-apps/api/core";
import { Download, Pencil, Plus, RotateCcw, ShieldAlert, Star, Trash2, Upload, X } from "lucide-react";
import { Button } from "../ui";
import {
  findByCommand,
  parseImportPayload,
  slugify,
  usePromptStore,
  type PromptEntry,
  type PromptKind,
} from "../../store/promptStore";
import { useT } from "../../lib/i18n";
import { BUILT_IN_SLASH_COMMANDS } from "../../lib/slashCommands";
import { useSkillProposalStore } from "../../store/skillProposalStore";
import { NativeSkillsManager } from "./NativeSkillsManager";
import { errorMessage } from "../../lib/errors";

/** Same slash-trigger slug shape the design doc pins for `PromptEntry.command`. */
const COMMAND_PATTERN = /^[a-z0-9-]{1,32}$/;
const RESERVED_COMMANDS = new Set<string>(BUILT_IN_SLASH_COMMANDS.map((entry) => entry.command));

/** `@tauri-apps/plugin-dialog` file-type filter shared by the Import/Export
 * pickers — the library only ever round-trips JSON. */
const JSON_FILTERS = [{ name: "JSON", extensions: ["json"] }];

/** First non-blank line of `content`, truncated for the list row's preview —
 * the create/edit form shows the full text. */
function firstLine(content: string): string {
  const line = content.split("\n").find((l) => l.trim().length > 0) ?? "";
  return line.length > 80 ? `${line.slice(0, 80)}…` : line;
}

/** Local draft state for the inline create/edit form. `id: null` means
 * "creating a new entry"; otherwise it's the id of the entry being edited. */
interface DraftState {
  id: string | null;
  kind: PromptKind;
  name: string;
  command: string;
  /** Once the user hand-edits the command field, stop auto-deriving it from
   * the name on every keystroke. */
  commandTouched: boolean;
  content: string;
  description: string;
}

const EMPTY_DRAFT: DraftState = {
  id: null,
  kind: "snippet",
  name: "",
  command: "",
  commandTouched: false,
  content: "",
  description: "",
};

/**
 * Settings "Prompts" tab: the saved persona/snippet list (kind badge, name,
 * `/command`, first-line preview, edit/delete) plus an inline create/edit
 * form — modeled on `McpPanel.tsx`/`AddMcpServerForm.tsx`'s shape. Personas
 * selected via `PersonaSelector` flow into the system prompt per-turn (see
 * `agentLoop.ts`); this tab manages the library and the kind radio, and
 * snippets are immediately usable via the "/"-command popup in the chat
 * input (see `SlashCommandAutocomplete.tsx`).
 */
export function PromptLibraryPanel() {
  const { t } = useT();
  const entries = usePromptStore((s) => s.entries);
  const addEntry = usePromptStore((s) => s.addEntry);
  const updateEntry = usePromptStore((s) => s.updateEntry);
  const removeEntry = usePromptStore((s) => s.removeEntry);
  const importEntries = usePromptStore((s) => s.importEntries);
  const exportPayload = usePromptStore((s) => s.exportPayload);
  const defaultPersonaId = usePromptStore((s) => s.defaultPersonaId);
  const setDefaultPersona = usePromptStore((s) => s.setDefaultPersona);
  const proposals = useSkillProposalStore((s) => s.proposals);
  const approveProposal = useSkillProposalStore((s) => s.approveProposal);
  const rejectProposal = useSkillProposalStore((s) => s.rejectProposal);
  const rollbackProposal = useSkillProposalStore((s) => s.rollbackProposal);

  const [draft, setDraft] = useState<DraftState | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [confirmingRemoveId, setConfirmingRemoveId] = useState<string | null>(null);

  const [importError, setImportError] = useState<string | null>(null);
  const [importPreview, setImportPreview] = useState<PromptEntry[] | null>(null);
  const [importBusy, setImportBusy] = useState(false);
  const [exportError, setExportError] = useState<string | null>(null);
  const [exportBusy, setExportBusy] = useState(false);
  const [importedCount, setImportedCount] = useState<number | null>(null);
  const [proposalBusy, setProposalBusy] = useState<string | null>(null);
  const [proposalError, setProposalError] = useState<string | null>(null);

  const commandError = useMemo(() => {
    if (!draft) return null;
    const command = draft.command.trim();
    if (command.length === 0) return t("PromptLibraryPanel.commandRequiredError");
    if (!COMMAND_PATTERN.test(command)) return t("PromptLibraryPanel.commandFormatError");
    if (RESERVED_COMMANDS.has(command)) return `/${command} is reserved by a built-in command.`;
    const collision = findByCommand(entries, command);
    if (collision && collision.id !== draft.id) return t("PromptLibraryPanel.commandTakenError");
    return null;
  }, [draft, entries, t]);

  function startCreate() {
    setDraft({ ...EMPTY_DRAFT });
    setError(null);
  }

  function startEdit(entry: PromptEntry) {
    setDraft({
      id: entry.id,
      kind: entry.kind,
      name: entry.name,
      command: entry.command,
      commandTouched: true,
      content: entry.content,
      description: entry.description ?? "",
    });
    setError(null);
  }

  function cancelDraft() {
    setDraft(null);
    setError(null);
  }

  function handleNameChange(name: string) {
    setDraft((prev) => {
      if (!prev) return prev;
      const command = prev.commandTouched ? prev.command : slugify(name);
      return { ...prev, name, command };
    });
  }

  function handleCommandChange(rawCommand: string) {
    setDraft((prev) => (prev ? { ...prev, command: slugify(rawCommand), commandTouched: true } : prev));
  }

  function handleSave() {
    if (!draft) return;
    const name = draft.name.trim();
    const command = draft.command.trim();
    const description = draft.description.trim();

    if (name.length === 0) {
      setError(t("PromptLibraryPanel.nameRequiredError"));
      return;
    }
    if (commandError) {
      setError(commandError);
      return;
    }
    if (draft.content.trim().length === 0) {
      setError(t("PromptLibraryPanel.contentRequiredError"));
      return;
    }

    if (draft.id === null) {
      addEntry({ kind: draft.kind, name, command, content: draft.content, description: description || undefined });
    } else {
      updateEntry(draft.id, { kind: draft.kind, name, command, content: draft.content, description: description || undefined });
    }
    setDraft(null);
    setError(null);
  }

  async function handleExport() {
    setExportError(null);
    setExportBusy(true);
    try {
      const path = await save({ defaultPath: "prompts.json", filters: JSON_FILTERS });
      if (!path) return;
      await invoke("prompts_write_external", { path, payload: exportPayload() });
    } catch (err) {
      setExportError(errorMessage(err));
    } finally {
      setExportBusy(false);
    }
  }

  async function handlePickImport() {
    setImportError(null);
    setImportPreview(null);
    setImportedCount(null);
    setImportBusy(true);
    try {
      const path = await open({ multiple: false, filters: JSON_FILTERS });
      if (!path || Array.isArray(path)) return;
      const raw = await invoke<string>("prompts_read_external", { path });
      const parsed = parseImportPayload(raw);
      if (parsed.length === 0) {
        setImportError(t("PromptLibraryPanel.importEmptyError"));
        return;
      }
      setImportPreview(parsed);
    } catch (err) {
      setImportError(errorMessage(err));
    } finally {
      setImportBusy(false);
    }
  }

  function confirmImport() {
    if (!importPreview) return;
    const count = importEntries(importPreview);
    setImportPreview(null);
    setImportedCount(count);
  }

  function cancelImport() {
    setImportPreview(null);
  }

  return (
    <div className="flex flex-col gap-3 py-2">
      <p className="text-xs text-muted">{t("PromptLibraryPanel.description")}</p>

      <NativeSkillsManager />

      <div className="flex flex-wrap items-center gap-2">
        <Button variant="secondary" size="sm" onClick={() => void handleExport()} disabled={exportBusy || entries.length === 0}>
          <Download size={12} />
          {t("PromptLibraryPanel.exportButton")}
        </Button>
        <Button variant="secondary" size="sm" onClick={() => void handlePickImport()} disabled={importBusy}>
          <Upload size={12} />
          {t("PromptLibraryPanel.importButton")}
        </Button>
      </div>
      {exportError && <p className="text-xs text-danger">{exportError}</p>}
      {importError && <p className="text-xs text-danger">{importError}</p>}
      {importedCount !== null && (
        <p className="text-xs text-muted">{t("PromptLibraryPanel.importSuccess", { count: importedCount })}</p>
      )}

      {importPreview && (
        <div className="flex flex-col gap-2 rounded-lg border border-dashed border-border p-3">
          <p className="text-xs text-foreground">
            {t("PromptLibraryPanel.importPreviewMessage", { count: importPreview.length })}
          </p>
          <div className="flex items-center justify-end gap-2">
            <Button variant="ghost" size="sm" onClick={cancelImport}>
              {t("PromptLibraryPanel.cancelButton")}
            </Button>
            <Button variant="secondary" size="sm" onClick={confirmImport}>
              {t("PromptLibraryPanel.importConfirmButton")}
            </Button>
          </div>
        </div>
      )}

      {proposals.length > 0 && (
        <section className="flex flex-col gap-2 rounded-lg border border-border bg-surface p-3">
          <div>
            <h3 className="text-sm font-medium text-foreground">Learned skill proposals</h3>
            <p className="text-xs text-faint">
              /learn drafts stay quarantined until you inspect the exact instructions and approve their SHA-256 digest.
            </p>
          </div>
          {proposalError && <p className="text-xs text-danger">{proposalError}</p>}
          {proposals.map((proposal) => (
            <div key={proposal.id} className="rounded-md border border-border bg-background p-2.5">
              <div className="flex flex-wrap items-center gap-2">
                <span className="font-mono text-xs text-foreground">/{proposal.command}</span>
                <span className={`rounded px-1.5 py-0.5 text-[10px] font-medium uppercase ${
                  proposal.status === "quarantined"
                    ? "bg-warning-soft text-warning"
                    : proposal.status === "applied"
                      ? "bg-success-soft text-success"
                      : "bg-surface-2 text-faint"
                }`}>{proposal.status.replace("_", " ")}</span>
                <span className="ml-auto font-mono text-[10px] text-faint" title={proposal.contentSha256}>
                  sha256:{proposal.contentSha256.slice(0, 12)}…
                </span>
              </div>
              <pre className="mt-2 max-h-40 overflow-auto whitespace-pre-wrap break-words rounded bg-surface-2 p-2 font-sans text-xs text-muted">
                {proposal.instructions}
              </pre>
              {proposal.riskFlags.length > 0 && (
                <div className="mt-2 flex items-start gap-1.5 text-xs text-warning">
                  <ShieldAlert size={13} className="mt-0.5 shrink-0" />
                  <span>{proposal.riskFlags.join("; ")}</span>
                </div>
              )}
              <div className="mt-2 flex justify-end gap-1.5">
                {proposal.status === "quarantined" && (
                  <>
                    <Button variant="ghost" size="sm" onClick={() => rejectProposal(proposal.id)}>
                      <X size={12} /> Reject
                    </Button>
                    <Button
                      variant="secondary"
                      size="sm"
                      disabled={proposalBusy === proposal.id}
                      onClick={() => {
                        if (
                          proposal.riskFlags.length > 0 &&
                          !window.confirm(`This skill has ${proposal.riskFlags.length} risk warning(s). Approve the reviewed digest anyway?`)
                        ) return;
                        setProposalBusy(proposal.id);
                        setProposalError(null);
                        void approveProposal(proposal.id, proposal.contentSha256)
                          .catch((reason: unknown) => setProposalError(errorMessage(reason)))
                          .finally(() => setProposalBusy(null));
                      }}
                    >
                      Approve exact digest
                    </Button>
                  </>
                )}
                {proposal.status === "applied" && (
                  <Button variant="ghost" size="sm" onClick={() => rollbackProposal(proposal.id)}>
                    <RotateCcw size={12} /> Roll back
                  </Button>
                )}
              </div>
            </div>
          ))}
        </section>
      )}

      {entries.length === 0 ? (
        <p className="px-1 text-xs text-faint">{t("PromptLibraryPanel.emptyState")}</p>
      ) : (
        <div className="flex flex-col gap-2">
          {entries.map((entry) => (
            <div key={entry.id} className="rounded-lg border border-border bg-background p-3">
              <div className="flex items-center gap-2">
                <span
                  className={`shrink-0 rounded px-1.5 py-0.5 text-[10px] font-medium uppercase ${
                    entry.kind === "persona" ? "bg-accent-soft text-accent" : "bg-surface-2 text-muted"
                  }`}
                >
                  {entry.kind === "persona"
                    ? t("PromptLibraryPanel.personaBadge")
                    : entry.kind === "skill"
                      ? t("PromptLibraryPanel.skillBadge")
                      : t("PromptLibraryPanel.snippetBadge")}
                </span>
                <span className="truncate text-sm font-medium text-foreground">{entry.name}</span>
                <span className="truncate font-mono text-xs text-faint">/{entry.command}</span>
                {entry.kind === "persona" && entry.id === defaultPersonaId && (
                  <span className="shrink-0 rounded px-1.5 py-0.5 text-[10px] font-medium uppercase bg-accent-soft text-accent">
                    {t("PromptLibraryPanel.defaultBadge")}
                  </span>
                )}
                <div className="ml-auto flex shrink-0 items-center gap-1">
                  {entry.kind === "persona" &&
                    (entry.id === defaultPersonaId ? (
                      <Button variant="ghost" size="sm" onClick={() => setDefaultPersona(null)}>
                        <Star size={12} />
                        {t("PromptLibraryPanel.unsetDefaultButton")}
                      </Button>
                    ) : (
                      <Button variant="ghost" size="sm" onClick={() => setDefaultPersona(entry.id)}>
                        <Star size={12} />
                        {t("PromptLibraryPanel.setDefaultButton")}
                      </Button>
                    ))}
                  <Button variant="ghost" size="sm" onClick={() => startEdit(entry)}>
                    <Pencil size={12} />
                    {t("PromptLibraryPanel.editButton")}
                  </Button>
                  {confirmingRemoveId === entry.id ? (
                    <span className="flex items-center gap-1">
                      <Button variant="ghost" size="sm" onClick={() => setConfirmingRemoveId(null)}>
                        {t("PromptLibraryPanel.removeCancelButton")}
                      </Button>
                      <Button
                        variant="danger"
                        size="sm"
                        onClick={() => {
                          removeEntry(entry.id);
                          setConfirmingRemoveId(null);
                          setDraft((prev) => (prev?.id === entry.id ? null : prev));
                        }}
                      >
                        {t("PromptLibraryPanel.removeConfirmButton")}
                      </Button>
                    </span>
                  ) : (
                    <Button variant="ghost" size="sm" onClick={() => setConfirmingRemoveId(entry.id)}>
                      <Trash2 size={12} />
                      {t("PromptLibraryPanel.removeButton")}
                    </Button>
                  )}
                </div>
              </div>
              {(entry.description || entry.content) && (
                <p className="mt-1 truncate text-xs text-faint">{entry.description || firstLine(entry.content)}</p>
              )}
            </div>
          ))}
        </div>
      )}

      {draft ? (
        <div className="flex flex-col gap-2 rounded-lg border border-dashed border-border p-3">
          <p className="text-xs font-semibold uppercase tracking-wider text-faint">
            {draft.id === null ? t("PromptLibraryPanel.createHeading") : t("PromptLibraryPanel.editHeading")}
          </p>

          <div className="flex items-center gap-3 text-xs text-muted">
            <label className="flex cursor-pointer items-center gap-1.5">
              <input
                type="radio"
                name="prompt-kind"
                checked={draft.kind === "snippet"}
                onChange={() => setDraft((prev) => (prev ? { ...prev, kind: "snippet" } : prev))}
                className="accent-accent"
              />
              {t("PromptLibraryPanel.kindSnippetLabel")}
            </label>
            <label className="flex cursor-pointer items-center gap-1.5">
              <input
                type="radio"
                name="prompt-kind"
                checked={draft.kind === "persona"}
                onChange={() => setDraft((prev) => (prev ? { ...prev, kind: "persona" } : prev))}
                className="accent-accent"
              />
              {t("PromptLibraryPanel.kindPersonaLabel")}
            </label>
            <label className="flex cursor-pointer items-center gap-1.5">
              <input
                type="radio"
                name="prompt-kind"
                checked={draft.kind === "skill"}
                onChange={() => setDraft((prev) => (prev ? { ...prev, kind: "skill" } : prev))}
                className="accent-accent"
              />
              {t("PromptLibraryPanel.kindSkillLabel")}
            </label>
          </div>

          <div className="flex flex-col gap-2 sm:flex-row">
            <input
              type="text"
              value={draft.name}
              onChange={(event) => handleNameChange(event.target.value)}
              placeholder={t("PromptLibraryPanel.namePlaceholder")}
              className="h-8 min-w-0 flex-1 rounded-md border border-border bg-surface px-2.5 text-sm text-foreground placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
            />
            <div className="flex min-w-0 flex-1 items-center gap-1">
              <span className="shrink-0 font-mono text-sm text-faint">/</span>
              <input
                type="text"
                value={draft.command}
                onChange={(event) => handleCommandChange(event.target.value)}
                placeholder={t("PromptLibraryPanel.commandPlaceholder")}
                className="h-8 min-w-0 flex-1 rounded-md border border-border bg-surface px-2.5 font-mono text-sm text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
              />
            </div>
          </div>
          {commandError && <p className="text-xs text-danger">{commandError}</p>}

          <textarea
            value={draft.content}
            onChange={(event) => setDraft((prev) => (prev ? { ...prev, content: event.target.value } : prev))}
            placeholder={t("PromptLibraryPanel.contentPlaceholder")}
            rows={5}
            className="w-full resize-y rounded-md border border-border bg-surface px-2.5 py-1.5 text-sm text-foreground placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
          />

          <input
            type="text"
            value={draft.description}
            onChange={(event) => setDraft((prev) => (prev ? { ...prev, description: event.target.value } : prev))}
            placeholder={t("PromptLibraryPanel.descriptionPlaceholder")}
            className="h-8 w-full rounded-md border border-border bg-surface px-2.5 text-sm text-foreground placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
          />

          {error && <p className="text-xs text-danger">{error}</p>}

          <div className="flex items-center justify-end gap-2">
            <Button variant="ghost" size="sm" onClick={cancelDraft}>
              {t("PromptLibraryPanel.cancelButton")}
            </Button>
            <Button variant="secondary" size="sm" onClick={handleSave}>
              {draft.id === null ? t("PromptLibraryPanel.createButton") : t("PromptLibraryPanel.saveButton")}
            </Button>
          </div>
        </div>
      ) : (
        <Button variant="ghost" size="sm" onClick={startCreate} className="self-start">
          <Plus size={12} />
          {t("PromptLibraryPanel.addButton")}
        </Button>
      )}
    </div>
  );
}

export default PromptLibraryPanel;
