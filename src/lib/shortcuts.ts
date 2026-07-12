export type ShortcutScope = "global" | "composer" | "suggestions" | "sessionMenu";

export interface ShortcutBinding {
  key: string;
  code?: string;
  primary?: boolean;
  /** Explicit Control modifier, used for Ctrl chords on macOS where Command is primary. */
  control?: boolean;
  /** Explicit Meta/Super modifier, used on non-macOS platforms where Control is primary. */
  meta?: boolean;
  shift?: boolean;
  alt?: boolean;
}

export interface ShortcutDefinition {
  id: string;
  scope: ShortcutScope;
  labelKey: string;
  descriptionKey: string;
  bindings: readonly ShortcutBinding[];
}

export interface ShortcutGroup {
  id: ShortcutScope;
  labelKey: string;
}

/**
 * The single source of truth for every shortcut surfaced in Settings.
 * Global and session-menu handlers resolve events through this table, while
 * composer/suggestion entries document the same local key handling used by
 * ChatWindow. Keeping the bindings here prevents UI labels from drifting
 * away from the keys users can actually press.
 */
export const SHORTCUTS = [
  {
    id: "newSession",
    scope: "global",
    labelKey: "ChatSessionList.newSession",
    descriptionKey: "KeyboardShortcutsPanel.newSessionDescription",
    bindings: [{ key: "n", code: "KeyN", primary: true }],
  },
  {
    id: "openSettings",
    scope: "global",
    labelKey: "AppMenu.settings",
    descriptionKey: "KeyboardShortcutsPanel.openSettingsDescription",
    bindings: [{ key: ",", code: "Comma", primary: true }],
  },
  {
    id: "openShortcuts",
    scope: "global",
    labelKey: "SettingsModal.tabKeyboardShortcuts",
    descriptionKey: "KeyboardShortcutsPanel.openShortcutsDescription",
    bindings: [{ key: "?", code: "Slash", primary: true }],
  },
  {
    id: "toggleWorkspacePanel",
    scope: "global",
    labelKey: "KeyboardShortcutsPanel.toggleWorkspacePanel",
    descriptionKey: "KeyboardShortcutsPanel.toggleWorkspacePanelDescription",
    bindings: [{ key: "e", code: "KeyE", primary: true, shift: true }],
  },
  {
    id: "sendMessage",
    scope: "composer",
    labelKey: "KeyboardShortcutsPanel.sendMessage",
    descriptionKey: "KeyboardShortcutsPanel.sendMessageDescription",
    bindings: [{ key: "Enter" }],
  },
  {
    id: "insertLineBreak",
    scope: "composer",
    labelKey: "KeyboardShortcutsPanel.insertLineBreak",
    descriptionKey: "KeyboardShortcutsPanel.insertLineBreakDescription",
    bindings: [{ key: "Enter", shift: true }],
  },
  {
    id: "nextSuggestion",
    scope: "suggestions",
    labelKey: "KeyboardShortcutsPanel.nextSuggestion",
    descriptionKey: "KeyboardShortcutsPanel.nextSuggestionDescription",
    bindings: [{ key: "ArrowDown" }],
  },
  {
    id: "previousSuggestion",
    scope: "suggestions",
    labelKey: "KeyboardShortcutsPanel.previousSuggestion",
    descriptionKey: "KeyboardShortcutsPanel.previousSuggestionDescription",
    bindings: [{ key: "ArrowUp" }],
  },
  {
    id: "chooseSuggestion",
    scope: "suggestions",
    labelKey: "KeyboardShortcutsPanel.chooseSuggestion",
    descriptionKey: "KeyboardShortcutsPanel.chooseSuggestionDescription",
    bindings: [{ key: "Enter" }, { key: "Tab" }],
  },
  {
    id: "closeSuggestions",
    scope: "suggestions",
    labelKey: "KeyboardShortcutsPanel.closeSuggestions",
    descriptionKey: "KeyboardShortcutsPanel.closeSuggestionsDescription",
    bindings: [{ key: "Escape" }],
  },
  {
    id: "sessionOpenSplit",
    scope: "sessionMenu",
    labelKey: "SessionMenu.splitView",
    descriptionKey: "KeyboardShortcutsPanel.sessionOpenSplitDescription",
    bindings: [{ key: "1", code: "Digit1" }],
  },
  {
    id: "sessionOpenWindow",
    scope: "global",
    labelKey: "SessionMenu.newWindow",
    descriptionKey: "KeyboardShortcutsPanel.sessionOpenWindowDescription",
    bindings: [{ key: "2", code: "Digit2", primary: true, alt: true }],
  },
  {
    id: "sessionOpenCursor",
    scope: "global",
    labelKey: "SessionMenu.cursor",
    descriptionKey: "KeyboardShortcutsPanel.sessionOpenCursorDescription",
    bindings: [{ key: "3", code: "Digit3", primary: true, alt: true }],
  },
  {
    id: "sessionOpenVsCode",
    scope: "global",
    labelKey: "SessionMenu.vscode",
    descriptionKey: "KeyboardShortcutsPanel.sessionOpenVsCodeDescription",
    bindings: [{ key: "4", code: "Digit4", primary: true, alt: true }],
  },
  {
    id: "sessionRevealFinder",
    scope: "global",
    labelKey: "KeyboardShortcutsPanel.revealInFileManager",
    descriptionKey: "KeyboardShortcutsPanel.sessionRevealFinderDescription",
    bindings: [{ key: "5", code: "Digit5", primary: true, alt: true }],
  },
  {
    id: "sessionTogglePin",
    scope: "global",
    labelKey: "KeyboardShortcutsPanel.togglePin",
    descriptionKey: "KeyboardShortcutsPanel.togglePinDescription",
    bindings: [{ key: "p", code: "KeyP", primary: true, alt: true }],
  },
  {
    id: "sessionToggleUnread",
    scope: "global",
    labelKey: "KeyboardShortcutsPanel.toggleUnread",
    descriptionKey: "KeyboardShortcutsPanel.toggleUnreadDescription",
    bindings: [{ key: "u", code: "KeyU", primary: true, shift: true }],
  },
  {
    id: "sessionRename",
    scope: "global",
    labelKey: "SessionMenu.rename",
    descriptionKey: "KeyboardShortcutsPanel.sessionRenameDescription",
    bindings: [{ key: "r", code: "KeyR", primary: true, alt: true }],
  },
  {
    id: "sessionFork",
    scope: "global",
    labelKey: "SessionMenu.fork",
    descriptionKey: "KeyboardShortcutsPanel.sessionForkDescription",
    bindings: [{ key: "f", code: "KeyF", primary: true, alt: true }],
  },
  {
    id: "sessionArchive",
    scope: "global",
    labelKey: "KeyboardShortcutsPanel.archiveOrRestore",
    descriptionKey: "KeyboardShortcutsPanel.sessionArchiveDescription",
    bindings: [{ key: "a", code: "KeyA", primary: true, shift: true }],
  },
  {
    id: "sessionDelete",
    scope: "sessionMenu",
    labelKey: "SessionMenu.delete",
    descriptionKey: "KeyboardShortcutsPanel.sessionDeleteDescription",
    bindings: [{ key: "d", code: "KeyD" }],
  },
  {
    id: "sessionCloseMenu",
    scope: "sessionMenu",
    labelKey: "KeyboardShortcutsPanel.closeSessionMenu",
    descriptionKey: "KeyboardShortcutsPanel.closeSessionMenuDescription",
    bindings: [{ key: "Escape" }],
  },
] as const satisfies readonly ShortcutDefinition[];

export type ShortcutId = (typeof SHORTCUTS)[number]["id"];
export type ShortcutOverrides = Partial<Record<ShortcutId, readonly ShortcutBinding[]>>;
export type ShortcutIdForScope<Scope extends ShortcutScope> = Extract<
  (typeof SHORTCUTS)[number],
  { readonly scope: Scope }
>["id"];

export const SHORTCUT_GROUPS: readonly ShortcutGroup[] = [
  { id: "global", labelKey: "KeyboardShortcutsPanel.groupApplication" },
  { id: "composer", labelKey: "KeyboardShortcutsPanel.groupComposer" },
  { id: "suggestions", labelKey: "KeyboardShortcutsPanel.groupSuggestions" },
  { id: "sessionMenu", labelKey: "KeyboardShortcutsPanel.groupSessionMenu" },
];

export type ShortcutEvent = Pick<
  KeyboardEvent,
  "key" | "code" | "metaKey" | "ctrlKey" | "altKey" | "shiftKey"
> & Partial<Pick<KeyboardEvent, "getModifierState">>;

type GlobalShortcutEvent = ShortcutEvent &
  Pick<KeyboardEvent, "defaultPrevented" | "repeat" | "isComposing">;

export function usesMacShortcuts(platform?: string): boolean {
  const currentPlatform =
    platform ?? (typeof navigator === "undefined" ? "" : navigator.platform);
  return /Mac|iPhone|iPad|iPod/i.test(currentPlatform);
}

export function shouldHandleGlobalShortcut(
  event: GlobalShortcutEvent,
  permissionPending: boolean,
  shortcutRecording = false,
): boolean {
  return !permissionPending && !shortcutRecording && !event.defaultPrevented && !event.repeat && !event.isComposing;
}

export function effectiveShortcutBindings(
  shortcut: ShortcutDefinition,
  overrides: ShortcutOverrides = {},
): readonly ShortcutBinding[] {
  const override = overrides[shortcut.id as ShortcutId];
  // A shortcut must always remain usable. Treat an empty/corrupt override as
  // absent here as a final line of defence; the store also refuses to persist
  // empty binding arrays.
  return Array.isArray(override) && override.length > 0 ? override : shortcut.bindings;
}

function keyMatches(event: ShortcutEvent, binding: ShortcutBinding): boolean {
  // Prefer the logical key so the letter printed in Settings is also the
  // letter users press on AZERTY, Dvorak, and other non-QWERTY layouts.
  // `code` is only a fallback for engines that cannot identify the key.
  if (event.key && event.key !== "Unidentified") {
    return event.key.toLowerCase() === binding.key.toLowerCase();
  }
  return Boolean(binding.code && event.code === binding.code);
}

export function matchesShortcut(
  event: ShortcutEvent,
  binding: ShortcutBinding,
  isMac = usesMacShortcuts(),
): boolean {
  // On many Windows/Linux layouts AltGr reports as Control+Alt. It produces
  // text and must never be mistaken for a command chord.
  if (event.getModifierState?.("AltGraph")) return false;
  const expectsPrimary = binding.primary === true;
  const expectsMeta = (expectsPrimary && isMac) || binding.meta === true;
  const expectsCtrl = (expectsPrimary && !isMac) || binding.control === true;
  // Some layouts require Shift to produce a displayed digit or punctuation
  // character (for example, "1" on AZERTY). Treat that as part of producing
  // the logical key, while still rejecting Shift for letter commands.
  const layoutShiftProducesKey =
    binding.shift !== true &&
    event.shiftKey &&
    binding.key.length === 1 &&
    !isLetterKey(binding.key) &&
    event.key === binding.key;

  return (
    keyMatches(event, binding) &&
    event.metaKey === expectsMeta &&
    event.ctrlKey === expectsCtrl &&
    event.altKey === (binding.alt === true) &&
    (event.shiftKey === (binding.shift === true) || layoutShiftProducesKey)
  );
}

export function shortcutIdForEvent<Scope extends ShortcutScope>(
  event: ShortcutEvent,
  scope: Scope,
  isMac = usesMacShortcuts(),
  overrides: ShortcutOverrides = {},
): ShortcutIdForScope<Scope> | null {
  const shortcut = SHORTCUTS.find(
    (candidate) =>
      candidate.scope === scope &&
      effectiveShortcutBindings(candidate, overrides).some((binding) => matchesShortcut(event, binding, isMac)),
  );
  return (shortcut?.id ?? null) as ShortcutIdForScope<Scope> | null;
}

export function shortcutById(id: ShortcutId): (typeof SHORTCUTS)[number] {
  const shortcut = SHORTCUTS.find((candidate) => candidate.id === id);
  if (!shortcut) throw new Error(`Unknown shortcut: ${id}`);
  return shortcut;
}

const KEY_LABELS: Readonly<Record<string, string>> = {
  ArrowDown: "↓",
  ArrowUp: "↑",
  Enter: "Enter",
  Escape: "Esc",
  Tab: "Tab",
  " ": "Space",
};

const ACCESSIBLE_KEY_LABELS: Readonly<Record<string, string>> = {
  ArrowDown: "Down arrow",
  ArrowUp: "Up arrow",
  Enter: "Enter",
  Escape: "Escape",
  Tab: "Tab",
  " ": "Space",
  "?": "Question mark",
  ",": "Comma",
};

export function formatShortcutBinding(binding: ShortcutBinding, isMac: boolean): string[] {
  const parts: string[] = [];
  if (binding.primary) parts.push(isMac ? "⌘" : "Ctrl");
  if (binding.control && !(binding.primary && !isMac)) parts.push(isMac ? "⌃" : "Ctrl");
  if (binding.meta && !(binding.primary && isMac)) parts.push(isMac ? "⌘" : "Meta");
  if (binding.alt) parts.push(isMac ? "⌥" : "Alt");
  // A question mark already communicates the Shift+/ keystroke and matches
  // how native application menus render this conventional shortcut.
  if (binding.shift && binding.key !== "?") parts.push(isMac ? "⇧" : "Shift");

  const keyLabel = KEY_LABELS[binding.key] ??
    (binding.key.length === 1 ? binding.key.toUpperCase() : binding.key);
  parts.push(keyLabel);
  return parts;
}

export function shortcutDisplayLabel(
  id: ShortcutId,
  isMac = usesMacShortcuts(),
  overrides: ShortcutOverrides = {},
): string {
  const [binding] = effectiveShortcutBindings(shortcutById(id), overrides);
  return binding ? formatShortcutBinding(binding, isMac).join(isMac ? "" : "+") : "";
}

export function formatShortcutAriaLabel(binding: ShortcutBinding, isMac: boolean): string {
  const parts: string[] = [];
  if (binding.primary) parts.push(isMac ? "Command" : "Control");
  if (binding.control && !(binding.primary && !isMac)) parts.push("Control");
  if (binding.meta && !(binding.primary && isMac)) parts.push(isMac ? "Command" : "Meta");
  if (binding.alt) parts.push(isMac ? "Option" : "Alt");
  if (binding.shift) parts.push("Shift");
  parts.push(
    ACCESSIBLE_KEY_LABELS[binding.key] ??
      (binding.key.length === 1 ? binding.key.toUpperCase() : binding.key),
  );
  return parts.join(" plus ");
}

export function shortcutMatchesQuery(
  shortcut: ShortcutDefinition,
  query: string,
  translate: (key: string) => string,
  isMac: boolean,
  overrides: ShortcutOverrides = {},
): boolean {
  const normalizedQuery = query.trim().toLocaleLowerCase();
  if (!normalizedQuery) return true;

  const bindingText = effectiveShortcutBindings(shortcut, overrides)
    .flatMap((binding) => [
      ...formatShortcutBinding(binding, isMac),
      binding.primary ? (isMac ? "command cmd" : "control ctrl") : "",
      binding.control ? "control ctrl" : "",
      binding.meta ? "meta super command" : "",
      binding.alt ? (isMac ? "option alt" : "alt") : "",
      binding.shift ? "shift" : "",
      binding.key === "Escape" ? "escape" : "",
      binding.key === "?" ? "question mark slash" : "",
    ])
    .join(" ");
  return [translate(shortcut.labelKey), translate(shortcut.descriptionKey), bindingText]
    .join(" ")
    .toLocaleLowerCase()
    .includes(normalizedQuery);
}

const MODIFIER_KEYS = new Set([
  "Alt",
  "AltGraph",
  "CapsLock",
  "Control",
  "Fn",
  "FnLock",
  "Hyper",
  "Meta",
  "NumLock",
  "ScrollLock",
  "Shift",
  "Super",
  "Symbol",
  "SymbolLock",
]);

const RESERVED_PRIMARY_KEYS = new Set(["a", "c", "q", "r", "v", "w", "x", "z"]);

export type ShortcutValidationError =
  | "invalidKey"
  | "globalNeedsModifier"
  | "typingKey"
  | "reserved";

function normalizeShortcutKey(key: string): string {
  if (key === "Spacebar") return " ";
  if (key === "Esc") return "Escape";
  return isLetterKey(key) ? key.toLowerCase() : key;
}

function isLetterKey(key: string): boolean {
  return /^\p{L}$/u.test(key);
}

/** Converts a real keydown into a layout-aware, platform-portable binding. */
export function shortcutBindingFromEvent(
  event: ShortcutEvent,
  isMac = usesMacShortcuts(),
): ShortcutBinding | null {
  if (event.getModifierState?.("AltGraph")) return null;
  const key = normalizeShortcutKey(event.key);
  if (!key || key === "Dead" || key === "Process" || key === "Unidentified" || MODIFIER_KEYS.has(key)) {
    return null;
  }

  // Shift is part of the command for letters/named keys (Shift+K,
  // Shift+Enter). For punctuation/digits it commonly exists only to produce
  // the logical character on the current layout, so the character itself is
  // the portable identity (e.g. ? or 1 on AZERTY).
  const semanticShift =
    event.shiftKey && (key === " " || key.length > 1 || isLetterKey(key));
  const binding: ShortcutBinding = { key };
  if (event.code) binding.code = event.code;
  if (isMac ? event.metaKey : event.ctrlKey) binding.primary = true;
  if (isMac && event.ctrlKey) binding.control = true;
  if (!isMac && event.metaKey) binding.meta = true;
  if (event.altKey) binding.alt = true;
  if (semanticShift) binding.shift = true;
  return binding;
}

/** Defensive shape validation for hand-edited/corrupt persisted bindings. */
export function sanitizeShortcutBinding(raw: unknown): ShortcutBinding | null {
  if (!raw || typeof raw !== "object") return null;
  const candidate = raw as Partial<ShortcutBinding>;
  if (typeof candidate.key !== "string") return null;
  const key = normalizeShortcutKey(candidate.key === " " ? " " : candidate.key.trim());
  if (
    !key ||
    key.length > 32 ||
    key === "Dead" ||
    key === "Process" ||
    key === "Unidentified" ||
    MODIFIER_KEYS.has(key)
  ) return null;

  const binding: ShortcutBinding = { key };
  if (typeof candidate.code === "string" && candidate.code.length > 0 && candidate.code.length <= 48) {
    binding.code = candidate.code;
  }
  if (candidate.primary === true) binding.primary = true;
  if (candidate.control === true) binding.control = true;
  if (candidate.meta === true) binding.meta = true;
  if (candidate.alt === true) binding.alt = true;
  // A printable punctuation/digit already encodes the layout-dependent Shift
  // needed to produce it. Persisting Shift as well would make the same chord
  // impossible to compare reliably across keyboard layouts.
  if (
    candidate.shift === true &&
    (key === " " || key.length > 1 || isLetterKey(key))
  ) binding.shift = true;
  return binding;
}

export function validateShortcutBinding(
  id: ShortcutId,
  binding: ShortcutBinding,
  isMac = usesMacShortcuts(),
): ShortcutValidationError | null {
  const shortcut = shortcutById(id);
  if (!binding.key || MODIFIER_KEYS.has(binding.key)) return "invalidKey";
  const hasCommandModifier =
    binding.primary === true || binding.control === true || binding.meta === true || binding.alt === true;
  if (shortcut.scope === "global" && !hasCommandModifier) return "globalNeedsModifier";

  if (
    (shortcut.scope === "composer" || shortcut.scope === "suggestions") &&
    binding.key.length === 1 &&
    !hasCommandModifier
  ) {
    return "typingKey";
  }

  const key = binding.key.toLowerCase();
  const onlyPrimary = binding.primary && !binding.control && !binding.meta && !binding.alt;
  const reservedPrimary =
    onlyPrimary &&
    ((!binding.shift && (RESERVED_PRIMARY_KEYS.has(key) || key === "tab" || key === " ")) ||
      (binding.shift && key === "z"));
  const reservedMac =
    isMac &&
    ((onlyPrimary && !binding.shift && (key === "h" || key === "m")) ||
      (binding.primary && binding.alt && !binding.control && !binding.meta && !binding.shift && key === "escape"));
  const effectiveControl = binding.primary || binding.control;
  const reservedNonMac =
    !isMac &&
    (binding.meta === true ||
      (binding.alt === true && !binding.primary && !binding.control && key === "tab") ||
      (binding.alt === true && !binding.primary && !binding.control && key === "f4") ||
      (binding.alt === true && effectiveControl && key === "delete"));
  if (reservedPrimary || reservedMac || reservedNonMac) return "reserved";
  return null;
}

export function shortcutBindingIdentity(binding: ShortcutBinding): string {
  return [
    binding.primary === true ? "primary" : "",
    binding.control === true ? "control" : "",
    binding.meta === true ? "meta" : "",
    binding.alt === true ? "alt" : "",
    binding.shift === true ? "shift" : "",
    binding.key.toLocaleLowerCase(),
  ].join("|");
}

function platformShortcutBindingIdentity(binding: ShortcutBinding, isMac: boolean): string {
  const meta = (binding.primary === true && isMac) || binding.meta === true;
  const control = (binding.primary === true && !isMac) || binding.control === true;
  return [
    meta ? "meta" : "",
    control ? "control" : "",
    binding.alt === true ? "alt" : "",
    binding.shift === true ? "shift" : "",
    binding.key.toLocaleLowerCase(),
  ].join("|");
}

/** True when two portable bindings collide on macOS or Windows/Linux. */
export function shortcutBindingsConflict(
  left: ShortcutBinding,
  right: ShortcutBinding,
): boolean {
  return (
    platformShortcutBindingIdentity(left, true) === platformShortcutBindingIdentity(right, true) ||
    platformShortcutBindingIdentity(left, false) === platformShortcutBindingIdentity(right, false)
  );
}

export function shortcutBindingsEqual(
  left: readonly ShortcutBinding[],
  right: readonly ShortcutBinding[],
): boolean {
  return left.length === right.length &&
    left.every((binding, index) => shortcutBindingIdentity(binding) === shortcutBindingIdentity(right[index]));
}

function scopesCanConflict(left: ShortcutScope, right: ShortcutScope): boolean {
  // Contextual scopes have deterministic precedence and are not active in
  // the same interaction surface. The registry itself intentionally uses
  // Escape for both suggestions and the session menu. Only same-scope
  // collisions (unreachable later action) and global collisions (the global
  // window-capture listener always wins) are true conflicts.
  return left === right || left === "global" || right === "global";
}

export function findShortcutConflict(
  id: ShortcutId,
  binding: ShortcutBinding,
  overrides: ShortcutOverrides = {},
): ShortcutId | null {
  const target = shortcutById(id);
  const conflict = SHORTCUTS.find(
    (candidate) =>
      candidate.id !== id &&
      scopesCanConflict(target.scope, candidate.scope) &&
      effectiveShortcutBindings(candidate, overrides).some(
        (otherBinding) => shortcutBindingsConflict(otherBinding, binding),
      ),
  );
  return (conflict?.id ?? null) as ShortcutId | null;
}
