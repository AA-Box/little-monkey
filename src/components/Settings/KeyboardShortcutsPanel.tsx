import { useEffect, useMemo, useRef, useState } from "react";
import { Monitor, Pencil, Plus, RotateCcw, Search, Trash2 } from "lucide-react";

import { useT } from "../../lib/i18n";
import { usePermissionStore } from "../../store/permissionStore";
import {
  defaultShortcutBindings,
  detectShortcutPlatform,
  effectiveShortcutBindings,
  findShortcutConflict,
  formatShortcutAriaLabel,
  formatShortcutBinding,
  shortcutBindingFromEvent,
  shortcutBindingSeparator,
  shortcutBindingsConflict,
  shortcutById,
  shortcutMatchesQuery,
  SHORTCUT_GROUPS,
  SHORTCUTS,
  validateShortcutBinding,
  type ShortcutBinding,
  type ShortcutId,
  type ShortcutPlatform,
  type ShortcutValidationError,
} from "../../lib/shortcuts";
import { MAX_SHORTCUT_BINDINGS, useShortcutStore } from "../../store/shortcutStore";
import { Button } from "../ui";

function ShortcutKeys({ binding, platform }: { binding: ShortcutBinding; platform: ShortcutPlatform }) {
  const parts = formatShortcutBinding(binding, platform);
  return (
    <span
      className="inline-flex items-center gap-1"
      role="img"
      aria-label={formatShortcutAriaLabel(binding, platform)}
    >
      {parts.map((part, index) => (
        <kbd
          key={`${part}-${index}`}
          aria-hidden="true"
          className="inline-flex min-h-7 min-w-7 items-center justify-center rounded-md border border-border-strong bg-surface-2 px-2 font-mono text-xs font-medium text-foreground shadow-sm"
        >
          {part}
        </kbd>
      ))}
    </span>
  );
}

interface EditorTarget {
  id: ShortcutId;
  index: number;
  originId: string;
}

const hasOwnShortcut = (overrides: object, id: ShortcutId) =>
  Object.prototype.hasOwnProperty.call(overrides, id);

const RECORDER_MODIFIER_KEYS = new Set(["Alt", "AltGraph", "Control", "Meta", "Shift"]);

/** Searchable, editable catalog backed by the same registry as every handler. */
export function KeyboardShortcutsPanel() {
  const { t } = useT();
  const [query, setQuery] = useState("");
  const [editor, setEditor] = useState<EditorTarget | null>(null);
  const [draft, setDraft] = useState<ShortcutBinding | null>(null);
  const [editorError, setEditorError] = useState<string | null>(null);
  const [status, setStatus] = useState("");
  const [confirmResetAll, setConfirmResetAll] = useState(false);
  const recorderRef = useRef<HTMLButtonElement>(null);
  const reviewRef = useRef<HTMLDivElement>(null);
  const searchRef = useRef<HTMLInputElement>(null);
  const platform = useMemo(() => detectShortcutPlatform(), []);
  const separator = shortcutBindingSeparator(platform);
  const platformName = t(
    platform === "macos"
      ? "KeyboardShortcutsPanel.platformMacos"
      : platform === "windows"
        ? "KeyboardShortcutsPanel.platformWindows"
        : "KeyboardShortcutsPanel.platformLinux",
  );
  const permissionPending = usePermissionStore((state) => state.pending !== null);

  const overrides = useShortcutStore((state) => state.overrides);
  const recordingId = useShortcutStore((state) => state.recordingId);
  const startRecording = useShortcutStore((state) => state.startRecording);
  const stopRecording = useShortcutStore((state) => state.stopRecording);
  const replaceBinding = useShortcutStore((state) => state.replaceBinding);
  const addBinding = useShortcutStore((state) => state.addBinding);
  const removeBinding = useShortcutStore((state) => state.removeBinding);
  const resetShortcut = useShortcutStore((state) => state.resetShortcut);
  const resetAll = useShortcutStore((state) => state.resetAll);

  const groups = useMemo(
    () =>
      SHORTCUT_GROUPS.map((group) => {
        const shortcuts = SHORTCUTS.filter((shortcut) => shortcut.scope === group.id).filter(
          (shortcut) => shortcutMatchesQuery(shortcut, query, t, platform, overrides),
        );
        return { ...group, shortcuts };
      }).filter((group) => group.shortcuts.length > 0),
    [overrides, platform, query, t],
  );
  const resultCount = groups.reduce((count, group) => count + group.shortcuts.length, 0);
  const hasCustomizations = Object.keys(overrides).length > 0;

  const currentEditorBindings = editor
    ? effectiveShortcutBindings(shortcutById(editor.id), overrides, platform)
    : [];
  const draftValidation: ShortcutValidationError | null =
    editor && draft ? validateShortcutBinding(editor.id, draft, platform) : null;
  const draftDuplicate = Boolean(
    editor &&
      draft &&
      currentEditorBindings.some(
        (binding, index) =>
          index !== editor.index && shortcutBindingsConflict(binding, draft, platform),
      ),
  );
  const draftConflictId =
    editor && draft && !draftValidation && !draftDuplicate
      ? findShortcutConflict(editor.id, draft, overrides, platform)
      : null;
  const draftLabel = draft ? formatShortcutBinding(draft, platform).join(separator) : "";

  const restoreFocus = (elementId: string) => {
    requestAnimationFrame(() => {
      const target = document.getElementById(elementId) ?? searchRef.current;
      target?.focus();
    });
  };

  const closeEditor = (restore = true) => {
    const originId = editor?.originId;
    stopRecording();
    setEditor(null);
    setDraft(null);
    setEditorError(null);
    if (restore && originId) restoreFocus(originId);
  };

  const beginRecording = (target: EditorTarget) => {
    setEditor(target);
    setDraft(null);
    setEditorError(null);
    setStatus("");
    // This synchronous store flag must be set before the next keydown so the
    // app-level capture listener cannot execute a chord being re-recorded.
    startRecording(target.id);
  };

  const tryAgain = () => {
    if (!editor) return;
    setDraft(null);
    setEditorError(null);
    startRecording(editor.id);
  };

  useEffect(() => {
    if (!editor || recordingId !== editor.id) return;

    function handleRecordedKey(event: KeyboardEvent) {
      // Capture Tab/Escape/Enter too: they are valid assignments and must not
      // move focus, close Settings, or trigger an existing command.
      event.preventDefault();
      event.stopImmediatePropagation();
      if (event.repeat || event.isComposing) return;

      const binding = shortcutBindingFromEvent(event, platform);
      if (!binding) {
        setEditorError(
          event.key === "Dead" || event.key === "Process" || event.key === "Unidentified"
            ? t("KeyboardShortcutsPanel.invalidKeyError")
            : null,
        );
        return;
      }
      setDraft(binding);
      setEditorError(null);
      stopRecording();
    }

    function handleRecordedKeyUp(event: KeyboardEvent) {
      if (
        RECORDER_MODIFIER_KEYS.has(event.key) &&
        !event.metaKey &&
        !event.ctrlKey &&
        !event.altKey &&
        !event.shiftKey
      ) {
        setEditorError(t("KeyboardShortcutsPanel.modifierOnlyError"));
      }
    }

    window.addEventListener("keydown", handleRecordedKey, true);
    window.addEventListener("keyup", handleRecordedKeyUp, true);
    return () => {
      window.removeEventListener("keydown", handleRecordedKey, true);
      window.removeEventListener("keyup", handleRecordedKeyUp, true);
    };
  }, [editor, platform, recordingId, stopRecording, t]);

  useEffect(() => {
    if (editor && recordingId === editor.id) recorderRef.current?.focus();
  }, [editor, recordingId]);

  useEffect(() => {
    if (draft && recordingId === null) reviewRef.current?.focus();
  }, [draft, recordingId]);

  // A visible permission prompt owns keyboard decisions. If one appears
  // while the recorder is open, discard the unsaved draft and yield focus.
  useEffect(() => {
    if (!permissionPending || !editor) return;
    stopRecording();
    setEditor(null);
    setDraft(null);
    setEditorError(null);
  }, [editor, permissionPending, stopRecording]);

  useEffect(
    () => () => {
      useShortcutStore.getState().stopRecording();
    },
    [],
  );

  const validationMessage = () => {
    if (!draft) return null;
    if (draftDuplicate) return t("KeyboardShortcutsPanel.duplicateError");
    if (draftConflictId) {
      return t("KeyboardShortcutsPanel.conflictError", {
        shortcut: draftLabel,
        action: t(shortcutById(draftConflictId).labelKey),
      });
    }
    if (!draftValidation) return null;
    const keys: Record<ShortcutValidationError, string> = {
      invalidKey: "KeyboardShortcutsPanel.invalidKeyError",
      globalNeedsModifier: "KeyboardShortcutsPanel.globalNeedsModifierError",
      typingKey: "KeyboardShortcutsPanel.typingKeyError",
      reserved: "KeyboardShortcutsPanel.reservedShortcutError",
    };
    return t(keys[draftValidation], { shortcut: draftLabel });
  };
  const validationError = editorError ?? validationMessage();

  const saveDraft = () => {
    if (!editor || !draft || validationError) return;
    const result = editor.index < currentEditorBindings.length
      ? replaceBinding(editor.id, editor.index, draft, platform)
      : addBinding(editor.id, draft, platform);
    if (!result.ok) {
      if (result.reason === "conflict" && result.conflictId) {
        setEditorError(
          t("KeyboardShortcutsPanel.conflictError", {
            shortcut: draftLabel,
            action: t(shortcutById(result.conflictId).labelKey),
          }),
        );
      } else {
        const errors: Record<string, string> = {
          duplicate: "KeyboardShortcutsPanel.duplicateError",
          maxBindings: "KeyboardShortcutsPanel.maxBindingsError",
          invalidKey: "KeyboardShortcutsPanel.invalidKeyError",
          globalNeedsModifier: "KeyboardShortcutsPanel.globalNeedsModifierError",
          typingKey: "KeyboardShortcutsPanel.typingKeyError",
          reserved: "KeyboardShortcutsPanel.reservedShortcutError",
        };
        setEditorError(t(errors[result.reason] ?? "KeyboardShortcutsPanel.invalidKeyError", { shortcut: draftLabel }));
      }
      return;
    }

    const action = t(shortcutById(editor.id).labelKey);
    const originId = editor.originId;
    const savedFocusId = editor.index < currentEditorBindings.length
      ? originId
      : `shortcut-${editor.id}-binding-${editor.index}`;
    setStatus(t("KeyboardShortcutsPanel.savedStatus", { shortcut: draftLabel, action }));
    closeEditor(false);
    restoreFocus(savedFocusId);
  };

  const handleRemove = (id: ShortcutId, index: number) => {
    const shortcut = shortcutById(id);
    const bindings = effectiveShortcutBindings(shortcut, overrides, platform);
    const binding = bindings[index];
    if (!binding) {
      setStatus(t("KeyboardShortcutsPanel.lastBindingError"));
      return;
    }
    const result = removeBinding(id, index, platform);
    if (!result.ok) {
      setStatus(t("KeyboardShortcutsPanel.lastBindingError"));
      return;
    }
    if (editor?.id === id) closeEditor(false);
    setStatus(
      t("KeyboardShortcutsPanel.removedStatus", {
        shortcut: formatShortcutBinding(binding, platform).join(separator),
        action: t(shortcut.labelKey),
      }),
    );
    restoreFocus(`shortcut-${id}-binding-${Math.min(index, bindings.length - 2)}`);
  };

  const handleResetShortcut = (id: ShortcutId) => {
    const action = t(shortcutById(id).labelKey);
    const result = resetShortcut(id, platform);
    if (!result.ok) {
      if (result.reason === "conflict" && result.conflictId) {
        const resetPreview = { ...overrides };
        delete resetPreview[id];
        const defaultBindings = defaultShortcutBindings(shortcutById(id), platform);
        const defaultBinding = defaultBindings.find(
          (binding) =>
            findShortcutConflict(id, binding, resetPreview, platform) === result.conflictId,
        ) ?? defaultBindings[0];
        const defaultLabel = formatShortcutBinding(defaultBinding, platform).join(separator);
        setStatus(
          t("KeyboardShortcutsPanel.conflictError", {
            shortcut: defaultLabel,
            action: t(shortcutById(result.conflictId).labelKey),
          }),
        );
      }
      return;
    }
    if (editor?.id === id) closeEditor(false);
    setStatus(t("KeyboardShortcutsPanel.resetStatus", { action }));
    restoreFocus(`shortcut-${id}-binding-0`);
  };

  const handleResetAll = () => {
    closeEditor(false);
    resetAll();
    setConfirmResetAll(false);
    setStatus(t("KeyboardShortcutsPanel.resetAllStatus"));
    requestAnimationFrame(() => searchRef.current?.focus());
  };

  const cancelResetAll = () => {
    setConfirmResetAll(false);
    restoreFocus("keyboard-shortcuts-reset-all");
  };

  const openResetAll = () => {
    setConfirmResetAll(true);
    restoreFocus("keyboard-shortcuts-reset-all-cancel");
  };

  return (
    <div className="mx-auto flex w-full max-w-3xl flex-col gap-5">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="flex max-w-2xl flex-col items-start gap-2">
          <p className="text-sm leading-6 text-muted">
            {t("KeyboardShortcutsPanel.description")}
          </p>
          <span className="inline-flex items-center gap-1.5 rounded-full border border-border bg-surface-2 px-2.5 py-1 text-xs font-medium text-foreground">
            <Monitor size={13} aria-hidden="true" />
            {t("KeyboardShortcutsPanel.platformContext", { platform: platformName })}
          </span>
        </div>
        {hasCustomizations && !confirmResetAll && (
          <Button
            id="keyboard-shortcuts-reset-all"
            type="button"
            size="sm"
            variant="ghost"
            onClick={openResetAll}
          >
            <RotateCcw size={14} aria-hidden="true" />
            {t("KeyboardShortcutsPanel.resetAllButton")}
          </Button>
        )}
      </div>

      {confirmResetAll && (
        <div className="flex flex-wrap items-center justify-between gap-3 rounded-lg border border-warning bg-warning-soft px-3 py-2">
          <p className="text-sm text-foreground">{t("KeyboardShortcutsPanel.resetAllConfirm")}</p>
          <div className="flex gap-2">
            <Button id="keyboard-shortcuts-reset-all-cancel" type="button" size="sm" onClick={cancelResetAll}>
              {t("KeyboardShortcutsPanel.cancelButton")}
            </Button>
            <Button type="button" size="sm" variant="danger" onClick={handleResetAll}>
              {t("KeyboardShortcutsPanel.resetAllButton")}
            </Button>
          </div>
        </div>
      )}

      <div className="relative">
        <Search
          size={16}
          className="pointer-events-none absolute left-3 top-1/2 -translate-y-1/2 text-faint"
          aria-hidden="true"
        />
        <input
          ref={searchRef}
          type="search"
          value={query}
          onChange={(event) => {
            setQuery(event.target.value);
            setStatus("");
          }}
          placeholder={t("KeyboardShortcutsPanel.searchPlaceholder")}
          aria-label={t("KeyboardShortcutsPanel.searchAriaLabel")}
          data-settings-autofocus
          autoComplete="off"
          className="h-10 w-full rounded-lg border border-border bg-surface pl-9 pr-3 text-sm text-foreground placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
        />
      </div>
      {status && (
        <p
          className="rounded-lg border border-border bg-surface-2 px-3 py-2 text-sm text-foreground"
          role="status"
          aria-live="polite"
        >
          {status}
        </p>
      )}
      <p className="sr-only" role="status" aria-live="polite">
        {resultCount === 0
          ? t("KeyboardShortcutsPanel.noResults")
          : t("KeyboardShortcutsPanel.resultCount", { count: resultCount })}
      </p>

      {groups.length === 0 ? (
        <div className="rounded-lg border border-dashed border-border px-4 py-10 text-center text-sm text-muted">
          {t("KeyboardShortcutsPanel.noResults")}
        </div>
      ) : (
        groups.map((group) => (
          <section key={group.id} aria-labelledby={`shortcut-group-${group.id}`}>
            <h3
              id={`shortcut-group-${group.id}`}
              className="mb-2 text-xs font-semibold uppercase tracking-wide text-muted"
            >
              {t(group.labelKey)}
            </h3>
            <div className="overflow-hidden rounded-xl border border-border bg-surface">
              {group.shortcuts.map((shortcut, shortcutIndex) => {
                const bindings = effectiveShortcutBindings(shortcut, overrides, platform);
                const customized = hasOwnShortcut(overrides, shortcut.id);
                const editing = editor?.id === shortcut.id;
                return (
                  <div
                    key={shortcut.id}
                    className={`grid min-h-16 grid-cols-1 gap-3 px-4 py-3 lg:grid-cols-[minmax(0,1fr)_auto] lg:items-center lg:gap-x-6 ${
                      shortcutIndex === 0 ? "" : "border-t border-border"
                    }`}
                  >
                    <div className="min-w-0">
                      <div className="flex flex-wrap items-center gap-2">
                        <p className="text-sm font-medium text-foreground">{t(shortcut.labelKey)}</p>
                        {customized && (
                          <span className="rounded-full bg-accent-soft px-2 py-0.5 text-[11px] font-medium text-accent">
                            {t("KeyboardShortcutsPanel.modifiedBadge")}
                          </span>
                        )}
                      </div>
                      <p className="mt-0.5 text-xs leading-5 text-muted">{t(shortcut.descriptionKey)}</p>
                    </div>

                    <div className="flex max-w-full flex-wrap items-center gap-2 lg:justify-end">
                      {bindings.map((binding, bindingIndex) => {
                        const bindingLabel = formatShortcutBinding(binding, platform).join(separator);
                        const bindingId = `shortcut-${shortcut.id}-binding-${bindingIndex}`;
                        return (
                          <div key={`${shortcut.id}-${bindingIndex}`} className="flex items-center gap-1">
                            {bindingIndex > 0 && (
                              <span className="mr-1 text-xs text-muted">
                                {t("KeyboardShortcutsPanel.orSeparator")}
                              </span>
                            )}
                            <button
                              id={bindingId}
                              type="button"
                              onClick={() => beginRecording({ id: shortcut.id, index: bindingIndex, originId: bindingId })}
                              aria-label={t("KeyboardShortcutsPanel.editBindingAriaLabel", {
                                shortcut: bindingLabel,
                                action: t(shortcut.labelKey),
                              })}
                              className="flex min-h-11 items-center gap-2 rounded-lg border border-transparent px-1.5 transition-colors hover:border-border-strong hover:bg-surface-2 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent"
                            >
                              <ShortcutKeys binding={binding} platform={platform} />
                              <Pencil size={13} className="text-faint" aria-hidden="true" />
                            </button>
                            {bindings.length > 1 && (
                              <button
                                type="button"
                                onClick={() => handleRemove(shortcut.id, bindingIndex)}
                                aria-label={t("KeyboardShortcutsPanel.removeBindingAriaLabel", {
                                  shortcut: bindingLabel,
                                  action: t(shortcut.labelKey),
                                })}
                                className="inline-flex size-11 items-center justify-center rounded-lg text-faint hover:bg-danger-soft hover:text-danger focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent"
                              >
                                <Trash2 size={14} aria-hidden="true" />
                              </button>
                            )}
                          </div>
                        );
                      })}
                      {bindings.length < MAX_SHORTCUT_BINDINGS && (
                        <button
                          id={`shortcut-${shortcut.id}-add`}
                          type="button"
                          onClick={() =>
                            beginRecording({
                              id: shortcut.id,
                              index: bindings.length,
                              originId: `shortcut-${shortcut.id}-add`,
                            })
                          }
                          aria-label={t("KeyboardShortcutsPanel.addBindingAriaLabel", {
                            action: t(shortcut.labelKey),
                          })}
                          className="inline-flex min-h-11 items-center gap-1.5 rounded-lg px-2 text-xs font-medium text-muted hover:bg-surface-2 hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent"
                        >
                          <Plus size={14} aria-hidden="true" />
                          {t("KeyboardShortcutsPanel.addBindingButton")}
                        </button>
                      )}
                      {customized && (
                        <button
                          type="button"
                          onClick={() => handleResetShortcut(shortcut.id)}
                          aria-label={t("KeyboardShortcutsPanel.resetShortcutAriaLabel", {
                            action: t(shortcut.labelKey),
                          })}
                          className="inline-flex min-h-11 items-center gap-1.5 rounded-lg px-2 text-xs font-medium text-muted hover:bg-surface-2 hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent"
                        >
                          <RotateCcw size={13} aria-hidden="true" />
                          {t("KeyboardShortcutsPanel.resetShortcutButton")}
                        </button>
                      )}
                    </div>

                    {editing && (
                      <fieldset className="rounded-lg border border-accent/40 bg-accent-soft/40 p-3 lg:col-span-2">
                        <legend className="px-1 text-xs font-semibold text-foreground">
                          {t("KeyboardShortcutsPanel.recordButton")}
                        </legend>
                        {recordingId === shortcut.id ? (
                          <div className="flex flex-wrap items-center gap-3">
                            <button
                              ref={recorderRef}
                              type="button"
                              aria-label={t("KeyboardShortcutsPanel.recorderAriaLabel", {
                                action: t(shortcut.labelKey),
                              })}
                              aria-describedby={`shortcut-recorder-help-${shortcut.id}`}
                              className="min-h-11 min-w-52 animate-pulse rounded-lg border-2 border-accent bg-background px-4 text-sm font-medium text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2"
                            >
                              {t("KeyboardShortcutsPanel.listeningStatus")}
                            </button>
                            <Button type="button" size="sm" onClick={() => closeEditor()}>
                              {t("KeyboardShortcutsPanel.cancelButton")}
                            </Button>
                            <p id={`shortcut-recorder-help-${shortcut.id}`} className="basis-full text-xs leading-5 text-muted">
                              {t("KeyboardShortcutsPanel.listeningInstructions")}
                            </p>
                            {editorError && <p className="basis-full text-xs text-danger" role="alert">{editorError}</p>}
                          </div>
                        ) : draft ? (
                          <div
                            ref={reviewRef}
                            tabIndex={-1}
                            role="group"
                            aria-label={t("KeyboardShortcutsPanel.draftAriaLabel", { shortcut: draftLabel })}
                            className="flex flex-wrap items-center gap-3 rounded-md focus:outline-none focus:ring-2 focus:ring-accent focus:ring-offset-2 focus:ring-offset-background"
                          >
                            <span className="text-xs font-medium text-muted">
                              {t("KeyboardShortcutsPanel.draftLabel")}
                            </span>
                            <ShortcutKeys binding={draft} platform={platform} />
                            <button
                              type="button"
                              onClick={saveDraft}
                              disabled={Boolean(validationError)}
                              aria-invalid={Boolean(validationError)}
                              className="inline-flex h-8 cursor-pointer items-center justify-center rounded-md bg-accent px-2.5 text-xs font-medium text-accent-foreground transition-colors hover:bg-accent-hover disabled:cursor-not-allowed disabled:opacity-50 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2 focus-visible:ring-offset-background"
                            >
                              {t("KeyboardShortcutsPanel.saveButton")}
                            </button>
                            <Button type="button" size="sm" onClick={tryAgain}>
                              {t("KeyboardShortcutsPanel.tryAgainButton")}
                            </Button>
                            <Button type="button" size="sm" variant="ghost" onClick={() => closeEditor()}>
                              {t("KeyboardShortcutsPanel.cancelButton")}
                            </Button>
                            {validationError && (
                              <p className="basis-full text-xs text-danger" role="alert">
                                {validationError}
                              </p>
                            )}
                          </div>
                        ) : null}
                      </fieldset>
                    )}
                  </div>
                );
              })}
            </div>
          </section>
        ))
      )}
    </div>
  );
}

export default KeyboardShortcutsPanel;
