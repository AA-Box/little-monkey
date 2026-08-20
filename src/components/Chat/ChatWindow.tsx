import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import type { FormEvent, KeyboardEvent } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { CornerDownLeft, Square } from "lucide-react";

import { compactSessionNow, runAgentTurn, stopTurn } from "../../lib/agentLoop";
import type { AttachmentRef } from "../../lib/agentLoop";
import { startComparison } from "../../lib/compareRunner";
import { startCrew } from "../../lib/crewRunner";
import type { ModelTargetSnapshot } from "../../lib/modelTargets";
import { isImagePath, readImageAsDataUrl } from "../../lib/imageAttachment";
import { textContent } from "../../lib/llamaClient";
import { selectSessionMessages, selectTurnRunning, sessionMessages, useSessionStore } from "../../store/sessionStore";
import { useWorkspaceStore } from "../../store/workspaceStore";
import { usePromptStore } from "../../store/promptStore";
import { useShortcutStore } from "../../store/shortcutStore";
import MessageList from "./MessageList";
import RunningTasksChip from "./RunningTasksChip";
import TaskSuggestionChips from "./TaskSuggestionChips";
import { MentionAutocomplete } from "./MentionAutocomplete";
import type { MentionEntry } from "./MentionAutocomplete";
import { SlashCommandAutocomplete } from "./SlashCommandAutocomplete";
import type { SlashCatalogEntry } from "./SlashCommandAutocomplete";
import { ModeSelector } from "./ModeSelector";
import { EffortSelector } from "./EffortSelector";
import { PersonaSelector } from "./PersonaSelector";
import { StackPicker } from "./StackPicker";
import { ModelSwitcher } from "./ModelSwitcher";
import { CompareTargetPicker } from "./CompareTargetPicker";
import { CrewPicker } from "./CrewPicker";
import { ContextUsageIndicator } from "./ContextUsageIndicator";
import { CheckpointTimeline } from "./CheckpointTimeline";
import { AttachMenu } from "./AttachMenu";
import { AttachmentChip } from "./AttachmentChip";
import { DictationButton, type DictationButtonHandle } from "./DictationButton";
import { Tooltip } from "./MessageActions";
import { WorkspaceBar } from "../Workspace/WorkspaceBar";
import { useT } from "../../lib/i18n";
import { detectShortcutPlatform, shortcutIdForEvent } from "../../lib/shortcuts";
import {
  ecosystemClient,
  type ActivePluginRuntimeSnapshot,
  type ActiveSkillDescriptor,
  type PluginRuntimeDescriptor,
} from "../../lib/ecosystemClient";
import {
  localPromptSkills,
  nativeSkills,
  packageAssistantSkills,
  packageRuleInvocations,
  packageSkills,
  parseSkillTurn,
  type SkillInvocationSnapshot,
} from "../../lib/skills";
import { useEcosystemStore } from "../../store/ecosystemStore";
import { useNativeSkillsStore } from "../../store/nativeSkillsStore";
import { useSkillActivationPolicyStore } from "../../store/skillActivationPolicyStore";
import { companionClient } from "../../lib/companionClient";
import { loadGeneratedImage, loadWorkspaceImage } from "../../lib/imageGeneration";
import {
  BUILT_IN_SLASH_COMMANDS,
  formatCommandNotice,
  parseBuiltInSlashCommand,
  type BuiltInSlashCommandName,
} from "../../lib/slashCommands";
import { runSideQuestion, stopSideQuestion } from "../../lib/sideQuestion";
import { usePmCopilotStore } from "../../store/pmCopilotStore";
import { useSideChatStore } from "../../store/sideChatStore";
import SideChatPanel from "./SideChatPanel";
import { TASK_TOOL, PRESENT_PLAN_TOOL, buildTools, toolsForWorkspace } from "../../lib/tools";
import { mcpToolDefs } from "../../lib/mcpTools";
import {
  DEFAULT_PROVIDER_MODEL_FILTER,
  useSettingsStore,
} from "../../store/settingsStore";
import { usePermissionStore } from "../../store/permissionStore";
import { useModelStore } from "../../store/modelStore";
import { useUsageStore } from "../../store/usageStore";
import { useStackStore } from "../../store/stackStore";
import { useSkillProposalStore } from "../../store/skillProposalStore";
import { useBrowserWorkbenchStore } from "../../store/browserWorkbenchStore";
import { useSideTaskStore } from "../../store/sideTaskStore";
import { useMcpStore } from "../../store/mcpStore";
import { useTerminalStore } from "../../store/terminalStore";
import { nativeSkillsClient, type NativeSkillDescriptor } from "../../lib/nativeSkillsClient";
import { useCustomAgentStore } from "../../store/customAgentStore";
import type { SettingsTab } from "../Settings";
import { visibleProviderModelsForProvider } from "../../lib/providerModelSelection";
import { errorMessage } from "../../lib/errors";
import { daemonEnsure } from "../../lib/daemonClient";
import { isExecutionServiceUnavailable } from "../../lib/daemonDesktopTurn";

const MAX_TEXTAREA_HEIGHT_PX = 160;

/** Shape returned by the Rust `list_workspace_paths` command. */
interface WorkspacePathsResult {
  entries: MentionEntry[];
  truncated: boolean;
}

/** Cap on how many filtered rows are handed to <MentionAutocomplete>. */
const MAX_MENTION_RESULTS = 50;

/**
 * Looks backward from `cursor` in `text` for an active "@"-mention trigger:
 * the nearest "@" that is either at the start of the text or preceded by
 * whitespace, with no whitespace between that "@" and the cursor. Returns
 * the trigger's start index (the position of "@" itself) and the query text
 * typed after it, or `null` if the cursor isn't inside a mention trigger.
 */
function findMentionRange(text: string, cursor: number): { start: number; query: string } | null {
  const upToCursor = text.slice(0, cursor);
  const at = upToCursor.lastIndexOf("@");
  if (at === -1) return null;

  const query = upToCursor.slice(at + 1);
  if (/\s/.test(query)) return null; // whitespace between "@" and cursor — not an active trigger

  const before = at === 0 ? "" : upToCursor[at - 1];
  if (at !== 0 && !/\s/.test(before)) return null; // "@" isn't at start-of-text or after whitespace

  return { start: at, query };
}

/**
 * Filters/ranks the cached workspace path list against `query`: a
 * case-insensitive substring match on the full path, with basename-starts-
 * with ranked above basename-contains, ranked above full-path-contains, then
 * capped to MAX_MENTION_RESULTS.
 */
function filterMentionEntries(all: MentionEntry[], query: string): MentionEntry[] {
  const needle = query.toLowerCase();

  const ranked: { entry: MentionEntry; rank: number }[] = [];
  for (const entry of all) {
    const path = entry.path.toLowerCase();
    const lastSlash = path.lastIndexOf("/");
    const basename = lastSlash >= 0 ? path.slice(lastSlash + 1) : path;

    let rank: number;
    if (needle === "" || basename.startsWith(needle)) {
      rank = 0;
    } else if (basename.includes(needle)) {
      rank = 1;
    } else if (path.includes(needle)) {
      rank = 2;
    } else {
      continue;
    }
    ranked.push({ entry, rank });
  }

  ranked.sort((a, b) => a.rank - b.rank || a.entry.path.length - b.entry.path.length);
  return ranked.slice(0, MAX_MENTION_RESULTS).map((r) => r.entry);
}

/**
 * Looks for an active "/"-command trigger: unlike `findMentionRange`, this
 * only fires when "/" is the FIRST non-whitespace character of the whole
 * input and the cursor hasn't moved past that leading token — "/" mid-text
 * is almost always a path (e.g. "src/lib"), so no popup there. Returns the
 * trigger's start index (the position of "/" itself) and the query text
 * typed after it, or `null` if the cursor isn't inside an active trigger.
 */
function findSlashRange(text: string, cursor: number): { start: number; query: string } | null {
  const start = text.search(/\S/);
  if (start === -1 || text[start] !== "/") return null;
  if (cursor <= start) return null; // cursor is at or before the "/" itself — not an active trigger yet

  const query = text.slice(start + 1, cursor);
  if (/\s/.test(query)) return null; // whitespace between "/" and cursor — the leading token ended

  return { start, query };
}

/**
 * Splits the composer text into segments so recognized leading slash-command
 * tokens can be tinted in the input. Mirrors the send-path parsers exactly:
 * a built-in only counts as the whole first token (`parseBuiltInSlashCommand`)
 * while installed skills may stack (`parseSkillTurn`). Unknown leading
 * "/text" stays untinted — it is probably a path — so the tint doubles as
 * feedback that the token actually resolved. Returns `null` when nothing
 * highlights so the textarea can skip the overlay entirely.
 */
function splitCommandSegments(
  text: string,
  skillCommands: ReadonlySet<string>,
): { text: string; command: boolean }[] | null {
  const first = text.search(/\S/);
  if (first === -1 || text[first] !== "/") return null;

  const spans: { start: number; end: number }[] = [];
  if (parseBuiltInSlashCommand(text)) {
    const rel = text.slice(first).search(/\s/);
    spans.push({ start: first, end: rel < 0 ? text.length : first + rel });
  } else {
    let cursor = first;
    while (text[cursor] === "/") {
      const rel = text.slice(cursor).search(/\s/);
      const end = rel < 0 ? text.length : cursor + rel;
      if (!skillCommands.has(text.slice(cursor + 1, end).toLowerCase())) break;
      spans.push({ start: cursor, end });
      cursor = end;
      while (cursor < text.length && /\s/.test(text[cursor])) cursor += 1;
    }
  }
  if (spans.length === 0) return null;

  const segments: { text: string; command: boolean }[] = [];
  let pos = 0;
  for (const span of spans) {
    if (span.start > pos) segments.push({ text: text.slice(pos, span.start), command: false });
    segments.push({ text: text.slice(span.start, span.end), command: true });
    pos = span.end;
  }
  if (pos < text.length) segments.push({ text: text.slice(pos), command: false });
  return segments;
}

/**
 * Filters/ranks prompt-library entries against `query`: command-starts-with
 * ranked above name-starts-with, ranked above either containing the query —
 * fully client-side against `promptStore` entries, unlike mentions there's
 * no Rust round trip since the whole library is already in memory.
 */
function filterSlashEntries(all: SlashCatalogEntry[], query: string): SlashCatalogEntry[] {
  const needle = query.toLowerCase();

  const ranked: { entry: SlashCatalogEntry; rank: number }[] = [];
  for (const entry of all) {
    const command = entry.command.toLowerCase();
    const name = entry.name.toLowerCase();

    let rank: number;
    if (needle === "" || command.startsWith(needle)) {
      rank = 0;
    } else if (name.startsWith(needle)) {
      rank = 1;
    } else if (command.includes(needle) || name.includes(needle)) {
      rank = 2;
    } else {
      continue;
    }
    ranked.push({ entry, rank });
  }

  ranked.sort((a, b) => a.rank - b.rank || a.entry.command.length - b.entry.command.length);
  return ranked.map((r) => r.entry);
}

function activeModelDescription(): string {
  const state = useModelStore.getState();
  if (state.activeProvider === "provider") {
    return state.activeProviderId && state.activeProviderModel
      ? `${state.activeProviderId}:${state.activeProviderModel}`
      : "cloud provider (no model selected)";
  }
  if (state.activeProvider === "ollama") {
    return state.activeOllamaModel ? `ollama:${state.activeOllamaModel}` : "Ollama (no model selected)";
  }
  return state.active ? `local:${state.active.name} (${state.llamaStatus})` : `local runtime (${state.llamaStatus}; no model selected)`;
}

/** How much of a session's transcript exists right now — the evidence for
 * whether a failed send ever became a turn. */
function messageCount(sessionId: string): number {
  return useSessionStore.getState().sessions.find((entry) => entry.id === sessionId)?.messages.length ?? 0;
}

export async function switchModelFromSlash(selector: string): Promise<string> {
  const state = useModelStore.getState();
  const providerModelFilters = useSettingsStore.getState().providerModelFilters;
  const requested = selector.trim().toLowerCase();
  if (!requested) return activeModelDescription();
  const candidates: Array<{ canonical: string; aliases: string[]; activate: () => Promise<void> }> = [];
  for (const model of state.installed.filter((entry) => entry.kind === "chat")) {
    candidates.push({
      canonical: `local:${model.id}`,
      aliases: [model.id, model.name].map((value) => value.toLowerCase()),
      activate: async () => state.start(model),
    });
  }
  for (const model of state.ollamaModels) {
    candidates.push({
      canonical: `ollama:${model.name}`,
      aliases: [model.name.toLowerCase()],
      activate: async () => useModelStore.getState().useOllamaModel(model.name),
    });
  }
  for (const provider of state.providers) {
    if (!provider.has_key) continue;
    const providerModels = visibleProviderModelsForProvider(
      provider.id,
      state.providerModels[provider.id] ?? [],
      providerModelFilters[provider.id] ?? DEFAULT_PROVIDER_MODEL_FILTER,
      state,
    );
    for (const model of providerModels) {
      candidates.push({
        canonical: `${provider.id}:${model.id}`,
        aliases: [model.id.toLowerCase()],
        activate: async () => useModelStore.getState().useProviderModel(provider.id, model.id),
      });
    }
  }
  const matches = candidates.filter((candidate) =>
    candidate.canonical.toLowerCase() === requested || candidate.aliases.includes(requested),
  );
  if (matches.length === 0) {
    const available = candidates.slice(0, 30).map((candidate) => candidate.canonical).join("\n");
    throw new Error(`No configured model matches “${selector}”.${available ? `\n\nAvailable:\n${available}` : ""}`);
  }
  if (matches.length > 1) {
    throw new Error(`“${selector}” is available from more than one runtime/provider. Choose one explicitly:\n${matches.map((entry) => entry.canonical).join("\n")}`);
  }
  await matches[0].activate();
  return matches[0].canonical;
}

interface ChatWindowProps {
  /** The session this pane renders and sends turns into. Each pane (primary
   * and split — see App.tsx) owns one; they operate independently. */
  sessionId: string;
  /** Opens Settings on the Prompts tab — passed down to `PersonaSelector`'s
   * "Manage prompts…" row (see App.tsx's deep-link hook). */
  onManagePrompts: () => void;
  onOpenSettingsTab: (tab: SettingsTab) => void;
  /** Optional host element in the window's title-bar strip (see App.tsx).
   * When provided, the Compare/Crew pickers portal there instead of the
   * composer footer — only the primary pane gets one; the split pane keeps
   * its footer placement. */
  headerActionsSlot?: HTMLElement | null;
  /** Opens the Background-tasks drawer — the "N running tasks" chip's click
   * target (see App.tsx, which reuses its own top-bar tasks-toggle open
   * branch). Optional so hosts without the right-sidebar region (none
   * today) simply get a non-clickable chip. */
  onOpenBackgroundTasks?: () => void;
  /** Opens the Product Manager Copilot panel — `/pm-plan`'s target surface,
   * where the plain-text goal typed here becomes an editable, savable plan.
   * Optional for the same reason as `onOpenBackgroundTasks`: a host without
   * the feature-panel region still runs the command, it just can't reveal the
   * panel. */
  onOpenPmCopilot?: () => void;
  /** Switches the app to the Studio section — where a running generation the
   * chip is counting actually lives. Optional like the two above. */
  onOpenStudio?: () => void;
}

export default function ChatWindow({ sessionId, onManagePrompts, onOpenSettingsTab, headerActionsSlot, onOpenBackgroundTasks, onOpenPmCopilot, onOpenStudio }: ChatWindowProps) {
  const messages = useSessionStore(selectSessionMessages(sessionId));
  const persistError = useSessionStore((state) => state.persistError);
  const roots = useWorkspaceStore((state) => state.roots);
  const { t } = useT();

  const [input, setInput] = useState("");
  // Whether THIS pane's session has a turn in flight — session-scoped store
  // state, not component state: the turn survives pane switches (keeping
  // its Stop affordance wherever its session is shown), and a session
  // already running a turn stays locked in whichever pane displays it.
  const sending = useSessionStore(selectTurnRunning(sessionId));
  const activeProvider = useModelStore((state) => state.activeProvider);
  const llamaStatus = useModelStore((state) => state.llamaStatus);
  const localModelStarting = activeProvider === "local" && llamaStatus === "starting";
  const [preparingTurn, setPreparingTurn] = useState(false);
  const preparingTurnRef = useRef(false);
  const [startingComparison, setStartingComparison] = useState(false);
  const [compareTargets, setCompareTargets] = useState<ModelTargetSnapshot[]>([]);
  const [startingCrew, setStartingCrew] = useState(false);
  const [crewId, setCrewId] = useState<string | null>(null);
  const [ultracodeMode, setUltracodeMode] = useState(false);
  // The caught value, not its text: what the banner offers depends on what
  // failed. A refused turn whose only fault is that the execution service is
  // down gets a Repair action instead of a Retry that would refuse again.
  const [error, setError] = useState<unknown>(null);
  const [repairingService, setRepairingService] = useState(false);
  const [attachments, setAttachments] = useState<AttachmentRef[]>([]);
  const pendingBrowserEvidence = useBrowserWorkbenchStore((state) => state.pendingBySession[sessionId] ?? null);
  const consumeBrowserEvidence = useBrowserWorkbenchStore((state) => state.consumeForChat);
  const pendingTerminalEvidence = useTerminalStore((state) => state.pendingEvidenceByChat[sessionId] ?? null);
  const consumeTerminalEvidence = useTerminalStore((state) => state.consumeEvidence);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const dictationButtonRef = useRef<DictationButtonHandle>(null);

  // "@"-mention autocomplete state. `mentionQuery` being non-null is what
  // controls whether the popup is rendered at all.
  const [mentionQuery, setMentionQuery] = useState<string | null>(null);
  const [mentionEntries, setMentionEntries] = useState<MentionEntry[]>([]);
  const [mentionActiveIndex, setMentionActiveIndex] = useState(0);

  // Index of the "@" that opened the current mention trigger, so selection
  // knows exactly what span of the textarea to replace.
  const mentionStartRef = useRef<number | null>(null);
  // Cache of the full workspace path list, fetched at most once per mount
  // (or re-fetched after a prior failure, e.g. no workspace was open yet).
  const workspacePathsRef = useRef<MentionEntry[] | null>(null);
  const workspacePathsPromiseRef = useRef<Promise<MentionEntry[]> | null>(null);
  // Guards against a slow/late `list_workspace_paths` response clobbering
  // the results of a more recent keystroke.
  const mentionRequestIdRef = useRef(0);

  // Invalidate the cached mention path list whenever the attached folders
  // change (primary swapped, or a secondary attached/removed) — otherwise a
  // newly attached folder's files would never show up in "@"-mentions until
  // ChatWindow happens to remount.
  const rootsKey = roots.map((r) => r.id).join("|");
  useEffect(() => {
    workspacePathsRef.current = null;
    workspacePathsPromiseRef.current = null;
  }, [rootsKey]);

  // "/"-command autocomplete state — mirrors the "@"-mention state above.
  // `slashQuery` being non-null is what controls whether the popup renders.
  const promptEntries = usePromptStore((state) => state.entries);
  const installedPackageKey = useEcosystemStore((state) =>
    state.installed
      .map((entry) => `${entry.package_id}:${entry.sequence}:${entry.enabled}:${entry.revoked}:${entry.tombstoned}`)
      .join("|"),
  );
  const [activePackageSkills, setActivePackageSkills] = useState<ActiveSkillDescriptor[]>([]);
  const [activePluginSnapshots, setActivePluginSnapshots] = useState<ActivePluginRuntimeSnapshot[]>([]);
  const [pluginRuntimes, setPluginRuntimes] = useState<PluginRuntimeDescriptor[]>([]);
  const [activeNativeSkills, setActiveNativeSkills] = useState<NativeSkillDescriptor[]>([]);
  const nativeSkillsRevision = useNativeSkillsStore((state) => state.revision);
  const skillActivationPolicies = useSkillActivationPolicyStore((state) => state.policies);
  useEffect(() => {
    let cancelled = false;
    void Promise.all([
      ecosystemClient.activeSkills().catch(() => [] as ActiveSkillDescriptor[]),
      ecosystemClient.activePluginSnapshots().catch(() => [] as ActivePluginRuntimeSnapshot[]),
      ecosystemClient.pluginRuntime().catch(() => [] as PluginRuntimeDescriptor[]),
      nativeSkillsClient.discover().catch(() => [] as NativeSkillDescriptor[]),
      // Custom agent defs ride the same discovery pass (and the same
      // workspace-change deps) as skills; the store keeps its own state, so
      // nothing is destructured from this slot.
      useCustomAgentStore.getState().refresh(),
    ])
      .then(([packageEntries, pluginSnapshots, runtimes, nativeEntries]) => {
        if (!cancelled) {
          setActivePackageSkills(packageEntries);
          setActivePluginSnapshots(pluginSnapshots);
          setPluginRuntimes(runtimes);
          setActiveNativeSkills(nativeEntries);
        }
      })
      .catch(() => {
        if (!cancelled) {
          setActivePackageSkills([]);
          setActivePluginSnapshots([]);
          setPluginRuntimes([]);
          setActiveNativeSkills([]);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [installedPackageKey, rootsKey, nativeSkillsRevision]);
  const baseAvailableSkills = useMemo(
    () => [...localPromptSkills(promptEntries), ...nativeSkills(activeNativeSkills), ...packageSkills(activePackageSkills)],
    [promptEntries, activeNativeSkills, activePackageSkills, skillActivationPolicies],
  );
  const activePackageAssistants = useMemo(
    () => packageAssistantSkills(
      activePluginSnapshots,
      new Set(pluginRuntimes.filter((runtime) => runtime.health === "healthy").map((runtime) => runtime.package_id)),
    ),
    [activePluginSnapshots, pluginRuntimes],
  );
  const availableSkills = useMemo(
    () => [...baseAvailableSkills, ...activePackageAssistants],
    [baseAvailableSkills, activePackageAssistants],
  );
  const availableSkillsRef = useRef(availableSkills);
  useEffect(() => {
    availableSkillsRef.current = availableSkills;
  }, [availableSkills]);
  const slashCatalog = useMemo<SlashCatalogEntry[]>(
    () => [
      ...BUILT_IN_SLASH_COMMANDS.map((command) => ({
        id: `builtin:${command.command}`,
        kind: "snippet" as const,
        name: command.name,
        command: command.command,
        content: command.usage,
        description: command.description,
        createdAt: 0,
        updatedAt: 0,
        builtin: true,
      })),
      ...promptEntries,
      ...activeNativeSkills
        .filter((skill) => skill.source.kind !== "signed_package" && skill.enabled && skill.eligibility.eligible)
        .map((skill) => ({
          id: `native:${skill.source.kind}:${skill.command}:${skill.sha256}`,
          kind: "skill" as const,
          name: skill.name,
          command: skill.command,
          content: skill.instructions,
          description: skill.description,
          createdAt: 0,
          updatedAt: 0,
        })),
      ...activePackageSkills.map((skill) => ({
        id: `package:${skill.package_id}`,
        kind: "skill" as const,
        name: skill.name,
        command: skill.command,
        content: skill.instructions,
        description: skill.description,
        createdAt: 0,
        updatedAt: 0,
      })),
      ...activePackageAssistants.map((assistant) => ({
        id: assistant.id,
        kind: "skill" as const,
        name: assistant.name,
        command: assistant.command,
        content: assistant.instructions,
        description: assistant.description,
        createdAt: 0,
        updatedAt: 0,
      })),
    ],
    [promptEntries, activeNativeSkills, activePackageSkills, activePackageAssistants],
  );
  const [slashQuery, setSlashQuery] = useState<string | null>(null);
  const [slashEntries, setSlashEntries] = useState<SlashCatalogEntry[]>([]);
  const [slashActiveIndex, setSlashActiveIndex] = useState(0);
  // Index of the "/" that opened the current slash trigger, so selection
  // knows exactly what span of the textarea to replace.
  const slashStartRef = useRef<number | null>(null);
  const invokedSkillPreview = useMemo(() => {
    try {
      return parseSkillTurn(input, availableSkills)?.invocations ?? [];
    } catch {
      return [];
    }
  }, [input, availableSkills]);
  const skillCommandSet = useMemo(
    () => new Set(availableSkills.map((skill) => skill.command.toLowerCase())),
    [availableSkills],
  );
  // When set, the textarea renders its own text transparent and this overlay
  // paints the same text on top with recognized command tokens in accent.
  const commandSegments = useMemo(
    () => splitCommandSegments(input, skillCommandSet),
    [input, skillCommandSet],
  );
  const commandOverlayRef = useRef<HTMLDivElement>(null);
  const syncCommandOverlayScroll = useCallback(() => {
    const overlay = commandOverlayRef.current;
    const el = textareaRef.current;
    if (overlay && el) overlay.scrollTop = el.scrollTop;
  }, []);
  useEffect(() => {
    syncCommandOverlayScroll();
  }, [input, commandSegments, syncCommandOverlayScroll]);
  const activePackageRuleCount = useMemo(
    () => activePluginSnapshots.reduce(
      (total, snapshot) => total + (snapshot.manifest.kind === "assistant"
        ? 0
        : snapshot.manifest.content.filter((reference) => reference.kind === "rule").length),
      0,
    ),
    [activePluginSnapshots],
  );

  const prepareTurnInstructions = useCallback(async (text: string): Promise<SkillInvocationSnapshot[]> => {
    const pluginSnapshots = await ecosystemClient.activePluginSnapshots();
    const runtimes = pluginSnapshots.some((snapshot) => snapshot.manifest.kind === "assistant")
      ? await ecosystemClient.pluginRuntime()
      : [];
    const readyAssistantIds = new Set(
      runtimes.filter((runtime) => runtime.health === "healthy").map((runtime) => runtime.package_id),
    );
    // Native .agents/skills roots are user-editable and may change while this
    // composer stays open. Freeze the exact current discovery immediately
    // before parsing the turn so hashes, instructions, and resource paths
    // all belong to one snapshot.
    const freshNativeEntries = await nativeSkillsClient.discover();
    const freshAvailableSkills = [
      ...localPromptSkills(promptEntries),
      ...nativeSkills(freshNativeEntries),
      ...packageSkills(activePackageSkills),
      ...packageAssistantSkills(pluginSnapshots, readyAssistantIds),
    ];
    const parsed = parseSkillTurn(text, [
      ...freshAvailableSkills,
    ]);
    availableSkillsRef.current = freshAvailableSkills;
    setActiveNativeSkills(freshNativeEntries);
    setActivePluginSnapshots(pluginSnapshots);
    setPluginRuntimes(runtimes);
    return [
      ...packageRuleInvocations(pluginSnapshots, parsed?.request ?? text.trim()),
      ...(parsed?.invocations ?? []),
    ];
  }, [activePackageSkills, promptEntries]);

  const resizeTextarea = useCallback(() => {
    const el = textareaRef.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, MAX_TEXTAREA_HEIGHT_PX)}px`;
  }, []);

  // Composer state (draft text, attachments, error banner, @/ popups) is
  // pane-local, not session-local — nothing else keys it to `sessionId`. A
  // brand new `sessionId` (switching panes, or `newSession` handing this
  // pane a freshly reset session) means a blank compose slate.
  useEffect(() => {
    setInput("");
    setError(null);
    setAttachments([]);
    setCompareTargets([]);
    setStartingComparison(false);
    setCrewId(null);
    setStartingCrew(false);
    setMentionQuery(null);
    mentionStartRef.current = null;
    setSlashQuery(null);
    slashStartRef.current = null;
    requestAnimationFrame(resizeTextarea);
  }, [sessionId, resizeTextarea]);

  // Browser evidence is never sent directly from the workbench. The user
  // explicitly stages it here first, where the bounded untrusted summary and
  // screenshot remain visible and removable before Send.
  useEffect(() => {
    if (!pendingBrowserEvidence) return;
    setInput((current) => [current.trim(), pendingBrowserEvidence.summary].filter(Boolean).join("\n\n"));
    if (pendingBrowserEvidence.screenshot) {
      const screenshot = pendingBrowserEvidence.screenshot;
      setAttachments((current) => [
        ...current.filter((attachment) => attachment.path !== screenshot.path),
        {
          path: screenshot.path,
          isDir: false,
          kind: "image",
          dataUrl: screenshot.dataUrl,
        },
      ]);
    }
    consumeBrowserEvidence(sessionId, pendingBrowserEvidence.id);
    requestAnimationFrame(() => {
      resizeTextarea();
      textareaRef.current?.focus();
    });
  }, [consumeBrowserEvidence, pendingBrowserEvidence, resizeTextarea, sessionId]);

  // Terminal evidence crosses into a model turn only after TerminalPanel's
  // explicit review confirmation. It then appears as a normal removable
  // composer attachment and still waits for the user's final Send action.
  useEffect(() => {
    if (!pendingTerminalEvidence?.length) return;
    const evidence = consumeTerminalEvidence(sessionId);
    if (evidence.length === 0) return;
    setAttachments((current) => {
      const existing = new Set(current.map((attachment) => attachment.path));
      const additions: AttachmentRef[] = evidence
        .filter((entry) => !existing.has(entry.path))
        .map((entry) => ({
          path: entry.path,
          isDir: false,
          kind: "inline_text",
          content: entry.content,
          label: entry.label,
        }));
      return additions.length > 0 ? [...current, ...additions] : current;
    });
    setInput((current) => current.trim() ? current : t("TerminalPanel.defaultPrompt"));
    requestAnimationFrame(() => {
      resizeTextarea();
      textareaRef.current?.focus();
    });
  }, [consumeTerminalEvidence, pendingTerminalEvidence, resizeTextarea, sessionId, t]);

  // One finalized spoken utterance, sent as its own turn.
  //
  // `utteranceId` is the recognition job's id, minted before the audio was
  // transcribed, and it travels all the way to the durable ingress row as the
  // turn's dedupe identity: a submission retried after a timeout lands on the
  // run the first attempt made instead of starting a second one. Only the
  // final transcript gets here — partial recognition never becomes a turn.
  const sendVoiceTurn = useCallback((text: string, utteranceId: string) => {
    setError(null);
    void runAgentTurn(sessionId, text, [], undefined, utteranceId, [], [], false, null, "voice")
      .catch((err: unknown) => {
        setError(err);
      });
  }, [sessionId]);

  // The separately-capability-scoped companion overlay never writes session
  // state directly. Rust emits its explicit context only to the main window;
  // the currently active primary composer accepts it here for user review
  // before Send. Screen context stays an in-memory vision attachment.
  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | null = null;
    void companionClient.onCompose((payload) => {
      if (useSessionStore.getState().activeSessionId !== sessionId) return;
      // A finalized hands-free utterance is a turn the operator already made,
      // out loud. It becomes a durable `voice` ingress turn under the
      // recognition job's own id, rather than text waiting in the box — see
      // `sendVoiceTurn`. Everything else still waits for Send.
      if (payload.utteranceId) {
        sendVoiceTurn(payload.text, payload.utteranceId);
        return;
      }
      setInput(payload.text);
      if (payload.imageDataUrl) {
        setAttachments((current) => [
          ...current.filter((attachment) => !attachment.path.startsWith("companion://")),
          {
            path: `companion://${payload.source}/${Date.now()}.png`,
            isDir: false,
            kind: "image",
            dataUrl: payload.imageDataUrl ?? undefined,
          },
        ]);
      }
      requestAnimationFrame(() => {
        resizeTextarea();
        textareaRef.current?.focus();
      });
    }).then((cleanup) => {
      if (disposed) cleanup();
      else unlisten = cleanup;
    }).catch(() => undefined);
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [resizeTextarea, sendVoiceTurn, sessionId]);

  const loadWorkspacePaths = useCallback((): Promise<MentionEntry[]> => {
    if (workspacePathsRef.current) return Promise.resolve(workspacePathsRef.current);
    if (!workspacePathsPromiseRef.current) {
      workspacePathsPromiseRef.current = invoke<WorkspacePathsResult>("list_workspace_paths")
        .then((result) => {
          workspacePathsRef.current = result.entries;
          return result.entries;
        })
        .catch(() => {
          // No workspace open (or some other failure) — never let this throw
          // into the chat's error banner. Reset so a later "@" can retry,
          // e.g. once a workspace has been opened.
          workspacePathsPromiseRef.current = null;
          return [];
        });
    }
    return workspacePathsPromiseRef.current;
  }, []);

  const handleAddFiles = useCallback(async () => {
    try {
      const selected = await open({ multiple: true, directory: false });
      if (!selected) return; // user cancelled
      const paths = Array.isArray(selected) ? selected : [selected];
      if (paths.length === 0) return;
      setAttachments((prev) => {
        const existing = new Set(prev.map((a) => a.path));
        const additions = paths.filter((path) => !existing.has(path)).map((path) => ({ path, isDir: false }));
        return additions.length > 0 ? [...prev, ...additions] : prev;
      });

      // Image files get their bytes read + base64-encoded once, right now,
      // rather than on send — the resulting data URL doubles as both the
      // chip's thumbnail preview and the content actually wired to the
      // model later (see `agentLoop.ts`'s `resolveReferences`), so the file
      // is never read twice. Chips still appear immediately above; each
      // image one just upgrades in place once its read finishes.
      for (const path of paths) {
        if (!isImagePath(path)) continue;
        try {
          const dataUrl = await readImageAsDataUrl(path);
          setAttachments((prev) => prev.map((a) => (a.path === path ? { ...a, kind: "image", dataUrl } : a)));
        } catch (err) {
          console.error(`Failed to read image "${path}"`, err);
        }
      }
    } catch (err) {
      console.error("Failed to open file picker", err);
    }
  }, []);

  const handleAddFolder = useCallback(async () => {
    try {
      const selected = await open({ multiple: true, directory: true });
      if (!selected) return; // user cancelled
      const paths = Array.isArray(selected) ? selected : [selected];
      if (paths.length === 0) return;
      setAttachments((prev) => {
        const existing = new Set(prev.map((a) => a.path));
        const additions = paths.filter((path) => !existing.has(path)).map((path) => ({ path, isDir: true }));
        return additions.length > 0 ? [...prev, ...additions] : prev;
      });
    } catch (err) {
      console.error("Failed to open folder picker", err);
    }
  }, []);

  const handleRemoveAttachment = useCallback((path: string) => {
    setAttachments((prev) => prev.filter((a) => a.path !== path));
  }, []);

  const handleEditGeneratedImage = useCallback(async (path: string, _prompt: string, artifactId?: string) => {
    setError(null);
    try {
      const dataUrl = artifactId ? await loadGeneratedImage(artifactId) : await loadWorkspaceImage(path);
      if (!dataUrl) throw new Error(t("GeneratedImage.desktopOnly"));
      const name = path.split(/[\\/]/).filter(Boolean).pop() || "generated-image.png";
      setInput(t("GeneratedImage.editPrompt"));
      setAttachments((current) => [
        ...current.filter((attachment) => !attachment.path.startsWith("generated://")),
        {
          path: `generated://${artifactId ?? path}`,
          isDir: false,
          kind: "image",
          dataUrl,
          label: name,
        },
      ]);
      requestAnimationFrame(() => {
        resizeTextarea();
        textareaRef.current?.focus();
      });
    } catch (caught) {
      setError(caught);
    }
  }, [resizeTextarea, t]);

  const sendTurn = useCallback((
    text: string,
    pendingAttachments: AttachmentRef[],
    skillInvocations: SkillInvocationSnapshot[] = [],
    // Ultracode (see `EffortSelector.tsx`): the SAME single-model turn, with
    // the agent loop layering its multi-agent-orchestration system section on
    // top and force-offering the `task` tool — never a multi-model fan-out.
    ultracode = false,
  ) => {
    setError(null);

    // The agent loop owns the turn's abort handle (keyed by session — see
    // stopTurn) and flips the per-session running flag `sending` above
    // subscribes to, so nothing pane-local tracks the turn.
    // `availableSkills` (the same full list `prepareTurnInstructions` already
    // draws `skillInvocations` from) lets the turn's `skill` tool auto-invoke
    // any skill not already explicitly invoked above — see
    // `settingsStore.skillAutoInvokeEnabled`.
    const messagesBefore = messageCount(sessionId);
    void runAgentTurn(sessionId, text, pendingAttachments, undefined, undefined, skillInvocations, availableSkillsRef.current, ultracode)
      .catch((err: unknown) => {
        setError(err);
        // A send refused before it was accepted — an unavailable resident
        // runner is the usual reason — leaves nothing behind: no transcript
        // entry, no durable turn, no run. The typed message is the only thing
        // that would be lost, so it goes back in the box, and pressing Send
        // again once the runner is up is the same send rather than a retyped
        // one. A turn that got far enough to write to the transcript owns its
        // own failure and the composer stays as the user left it.
        if (messageCount(sessionId) !== messagesBefore) return;
        setInput((current) => current || text);
        setAttachments((current) => (current.length > 0 ? current : pendingAttachments));
      })
      .finally(() => {
        textareaRef.current?.focus();
      });
  }, [sessionId]);

  const appendCommandNotice = useCallback((command: BuiltInSlashCommandName, text: string, ok = true, targetSessionId = sessionId) => {
    useSessionStore.getState().addMessage(targetSessionId, {
      role: "system",
      content: formatCommandNotice({ command, text, ok }),
    });
  }, [sessionId]);

  const executeBuiltIn = useCallback(async (
    command: BuiltInSlashCommandName,
    commandArguments: string,
  ): Promise<void> => {
    const requireNoArguments = () => {
      if (commandArguments) throw new Error(`/${command} does not accept arguments.`);
    };
    if (command === "stop") {
      requireNoArguments();
      const stoppedSideQuestion = stopSideQuestion(sessionId);
      if (!useSessionStore.getState().runningTurns[sessionId]) {
        appendCommandNotice(
          command,
          stoppedSideQuestion ? "Cancelled the running side question." : "No model turn is currently running.",
        );
        return;
      }
      stopTurn(sessionId);
      appendCommandNotice(command, "Cancellation requested. The run will finish only after the active model/tool acknowledges cancellation.");
      return;
    }
    if (command === "btw") {
      useSideChatStore.getState().open(sessionId);
      if (!commandArguments) return;
      await runSideQuestion(sessionId, commandArguments);
      return;
    }
    if (command === "pm-plan") {
      if (!commandArguments) throw new Error("Use /pm-plan <product goal>.");
      // Generation runs against the same active chat target this composer's
      // ModelSwitcher selects (`pmCopilot.ts`'s `activeTarget`), so the goal
      // is typed here and only edited/saved in the panel.
      onOpenPmCopilot?.();
      await usePmCopilotStore.getState().startFromGoal(commandArguments);
      const drafted = usePmCopilotStore.getState();
      // `generate()` records failures in its own state rather than throwing,
      // so surface them here as a failed command notice.
      if (drafted.status === "error") throw new Error(drafted.error ?? "Plan generation failed.");
      const plan = drafted.plan;
      appendCommandNotice(
        command,
        plan
          ? `Drafted a plan with ${plan.userStories.length} user stories and ${plan.milestones.length} milestones. Review, edit, and save it in Product Manager Copilot.`
          : "Product Manager Copilot is open. No plan was drafted.",
      );
      return;
    }
    if (command === "new") {
      requireNoArguments();
      useSessionStore.getState().newSession();
      const newSessionId = useSessionStore.getState().activeSessionId;
      appendCommandNotice(command, "Started a fresh local chat. No model request was made.", true, newSessionId);
      return;
    }
    if (command === "compact") {
      requireNoArguments();
      const result = await compactSessionNow(sessionId);
      appendCommandNotice(
        command,
        result.changed
          ? `Compacted ${result.removedMessages} older message${result.removedMessages === 1 ? "" : "s"}.`
          : "Nothing was compacted. Keep at least two complete user turns before using /compact.",
      );
      return;
    }
    if (command === "model") {
      const selected = await switchModelFromSlash(commandArguments);
      appendCommandNotice(command, commandArguments ? `Active model switched to ${selected}.` : `Active model: ${selected}`);
      return;
    }
    if (command === "usage") {
      requireNoArguments();
      const usageState = useUsageStore.getState();
      const usage = usageState.usageBySession[sessionId];
      appendCommandNotice(
        command,
        usage
          ? [
              `Last completed turn: ${usage.promptTokens.toLocaleString()} prompt + ${usage.completionTokens.toLocaleString()} completion = ${usage.totalTokens.toLocaleString()} tokens`,
              `Context limit: ${usageState.contextLimit?.toLocaleString() ?? "not reported by the active runtime"}`,
            ].join("\n")
          : "The active runtime has not reported token usage for a completed turn in this chat yet.",
      );
      return;
    }
    if (command === "skills") {
      requireNoArguments();
      const lines = availableSkills
        .slice()
        .sort((left, right) => left.command.localeCompare(right.command))
        .map((skill) => `/${skill.command} — ${skill.name} [${skill.source} ${skill.version}]`);
      appendCommandNotice(command, lines.length > 0 ? lines.join("\n") : "No enabled skills are currently available.");
      return;
    }
    if (command === "plugins") {
      requireNoArguments();
      const ecosystem = useEcosystemStore.getState();
      const catalogById = new Map(ecosystem.catalog.map((entry) => [entry.manifest.package_id, entry.manifest.display_name]));
      const lines = ecosystem.installed.map((plugin) => {
        const health = plugin.revoked
          ? "revoked"
          : plugin.tombstoned
            ? "uninstalled"
            : plugin.enabled
              ? "enabled"
              : "disabled";
        return `${catalogById.get(plugin.package_id) ?? plugin.package_id} ${plugin.active_version ?? ""} — ${health}`.trim();
      });
      appendCommandNotice(command, lines.length > 0 ? lines.join("\n") : "No declarative plugins are installed. Open Settings → Ecosystem to review signed packages.");
      return;
    }
    if (command === "tools") {
      requireNoArguments();
      const settings = useSettingsStore.getState();
      const permissionMode = usePermissionStore.getState().mode;
      const attachedIds = useSessionStore.getState().sessions.find((entry) => entry.id === sessionId)?.attachedStackIds ?? [];
      const stackNames = useStackStore.getState().stacks.filter((stack) => attachedIds.includes(stack.id)).map((stack) => stack.name);
      const hasWorkspace = useWorkspaceStore.getState().roots.some((root) => root.is_primary);
      const builtIns = toolsForWorkspace(buildTools(stackNames), hasWorkspace).filter((tool) => {
        const name = tool.function.name;
        if (!settings.memoryEnabled && name === "remember") return false;
        if (!settings.webToolsEnabled && (name === "web_fetch" || name === "web_search")) return false;
        return true;
      });
      if (settings.subagentsEnabled && hasWorkspace) builtIns.push(TASK_TOOL);
      if (permissionMode === "plan") builtIns.push(PRESENT_PLAN_TOOL);
      const mcp = mcpToolDefs().defs;
      const lines = [...builtIns, ...mcp]
        .map((tool) => tool.function.name)
        .sort()
        .map((name) => `${name.startsWith("mcp__") ? "MCP" : "host"} · ${name}`);
      appendCommandNotice(command, `${lines.join("\n")}\n\nPermission mode: ${permissionMode}. Tool permissions remain authoritative.`);
      return;
    }
    if (command === "status") {
      requireNoArguments();
      const roots = useWorkspaceStore.getState().roots;
      const mcp = useMcpStore.getState().servers;
      const pluginCount = useEcosystemStore.getState().installed.filter((entry) => entry.enabled && !entry.revoked && !entry.tombstoned).length;
      appendCommandNotice(command, [
        `Model: ${activeModelDescription()}`,
        `Turn: ${useSessionStore.getState().runningTurns[sessionId] ? "running" : "idle"}`,
        `Permission mode: ${usePermissionStore.getState().mode}`,
        `Workspace: ${roots.length > 0 ? roots.map((root) => root.path).join(", ") : "none attached"}`,
        `MCP: ${mcp.filter((entry) => entry.status === "connected").length}/${mcp.length} connected`,
        `Skills: ${availableSkills.length} enabled`,
        `Plugins: ${pluginCount} enabled`,
      ].join("\n"));
      return;
    }
    if (command === "learn") {
      const separator = commandArguments.indexOf("|");
      if (separator < 1) throw new Error("Use /learn command | instructions. The proposal will remain quarantined until you review its digest.");
      const proposedCommand = commandArguments.slice(0, separator).trim();
      const instructions = commandArguments.slice(separator + 1).trim();
      if (availableSkills.some((skill) => skill.command.toLowerCase() === proposedCommand.toLowerCase())) {
        throw new Error(`/${proposedCommand} already exists as an enabled skill.`);
      }
      const proposal = await useSkillProposalStore.getState().createProposal(proposedCommand, instructions);
      appendCommandNotice(
        command,
        `Created quarantined /${proposal.command} proposal. Review the full instructions, warnings, and sha256:${proposal.contentSha256} in Settings → Prompts before approval.`,
      );
      onOpenSettingsTab("prompts");
    }
  }, [appendCommandNotice, availableSkills, onOpenPmCopilot, onOpenSettingsTab, sessionId]);

  const handleSend = useCallback(async () => {
    let settledInput = input;
    if (dictationButtonRef.current?.isActive()) {
      try {
        const finalValue = await dictationButtonRef.current.settleForSend();
        if (finalValue !== null) settledInput = finalValue;
      } catch (reason) {
        setError(reason);
        return;
      }
    }
    const text = settledInput.trim();
    if (!text) return;
    const builtIn = parseBuiltInSlashCommand(text);
    if (builtIn) {
      setInput("");
      setAttachments([]);
      requestAnimationFrame(resizeTextarea);
      void executeBuiltIn(builtIn.definition.command, builtIn.arguments).catch((commandError: unknown) => {
        appendCommandNotice(
          builtIn.definition.command,
          errorMessage(commandError),
          false,
        );
      });
      return;
    }
    if (sending || startingComparison || startingCrew || preparingTurnRef.current || localModelStarting) return;

    preparingTurnRef.current = true;
    setPreparingTurn(true);
    let skillInvocations: SkillInvocationSnapshot[];
    try {
      skillInvocations = await prepareTurnInstructions(text);
    } catch (skillError) {
      setError(`No turn was sent because enabled plugin instructions could not be verified: ${errorMessage(skillError)}`);
      preparingTurnRef.current = false;
      setPreparingTurn(false);
      return;
    }
    preparingTurnRef.current = false;
    setPreparingTurn(false);

    const pendingAttachments = attachments;
    if (crewId) {
      setError(null);
      setStartingCrew(true);
      try {
        await startCrew(sessionId, text, pendingAttachments, crewId, skillInvocations);
        setInput("");
        setAttachments([]);
        requestAnimationFrame(resizeTextarea);
      } catch (err) {
        setError(err);
      } finally {
        setStartingCrew(false);
        textareaRef.current?.focus();
      }
      return;
    }
    if (compareTargets.length >= 2) {
      setError(null);
      setStartingComparison(true);
      try {
        await startComparison(sessionId, text, pendingAttachments, compareTargets, skillInvocations);
        setInput("");
        setAttachments([]);
        requestAnimationFrame(resizeTextarea);
      } catch (err) {
        setError(err);
      } finally {
        setStartingComparison(false);
        textareaRef.current?.focus();
      }
      return;
    }

    setInput("");
    setAttachments([]);
    requestAnimationFrame(resizeTextarea);
    // Ultracode rides the normal single-turn path — the flag only changes
    // what the agent loop layers into the system prompt and tool list.
    sendTurn(text, pendingAttachments, skillInvocations, ultracodeMode);
  }, [input, sending, startingComparison, startingCrew, ultracodeMode, attachments, crewId, compareTargets, resizeTextarea, sendTurn, sessionId, prepareTurnInstructions, executeBuiltIn, appendCommandNotice, localModelStarting]);

  const handleStop = useCallback(() => {
    stopTurn(sessionId);
  }, [sessionId]);

  // A turn refused for want of the execution service leaves nothing behind
  // (see `sendTurn`): the typed message went back in the composer, so the
  // repair only has to clear the banner — the send is the user's again, with
  // exactly the text they wrote.
  const serviceRepairNeeded = isExecutionServiceUnavailable(error);
  const handleRepairService = useCallback(() => {
    setRepairingService(true);
    daemonEnsure()
      .then(() => { setError(null); })
      .catch((repairError: unknown) => { setError(repairError); })
      .finally(() => {
        setRepairingService(false);
        textareaRef.current?.focus();
      });
  }, []);

  const handleEditMessage = useCallback(
    (index: number, newText: string) => {
      if (sending || preparingTurnRef.current) return;
      preparingTurnRef.current = true;
      setPreparingTurn(true);
      void prepareTurnInstructions(newText)
        .then((skillInvocations) => {
          if (useSessionStore.getState().runningTurns[sessionId]) {
            throw new Error("This chat started another turn while plugin instructions were being verified.");
          }
          useSessionStore.getState().truncateFromIndex(sessionId, index);
          sendTurn(newText, [], skillInvocations);
        })
        .catch((turnError: unknown) => {
          setError(turnError);
        })
        .finally(() => {
          preparingTurnRef.current = false;
          setPreparingTurn(false);
        });
    },
    [sending, prepareTurnInstructions, sendTurn, sessionId]
  );

  // Entry point for ROADMAP.md's "Side Tasks" item: "start a side task from
  // selected chat context" — the `Split` hover button `MessageBubble.tsx`
  // renders on every user/assistant bubble calls this with the message's
  // own transcript index. Opens the side-task composer prefilled with that
  // message's text as the seed prompt; nothing runs until the user reviews
  // and clicks "Start side task" there (`SideTaskComposer.tsx`) — this
  // handler itself never starts anything.
  const handleStartSideTask = useCallback(
    (index: number) => {
      const source = sessionMessages(sessionId)[index];
      if (!source) return;
      const text = textContent(source.content).trim();
      if (!text) return;
      const roleLabel = source.role === "user" ? "Your message" : "Assistant message";
      useSideTaskStore.getState().openComposer({
        title: text.length > 60 ? `${text.slice(0, 60)}…` : text,
        prompt: text,
        profile: "explore",
        source: { kind: "chat_message", label: roleLabel, excerpt: text.length > 240 ? `${text.slice(0, 240)}…` : text },
        sessionId,
      });
    },
    [sessionId]
  );

  // Regenerate the last turn: drop everything from the last user message
  // onward (its whole downstream reply included) and resubmit that message —
  // the same mechanics as editing a past message, just without changing the
  // text. Image attachments are rebuilt from the stored message's content
  // parts (they carry the already-encoded data URL), so a retried turn keeps
  // its images.
  const handleRetry = useCallback(() => {
    if (sending || preparingTurnRef.current) return;
    const currentMessages = sessionMessages(sessionId);
    let lastUserIndex = -1;
    for (let i = currentMessages.length - 1; i >= 0; i--) {
      if (currentMessages[i].role === "user") {
        lastUserIndex = i;
        break;
      }
    }
    if (lastUserIndex === -1) return;

    const userMessage = currentMessages[lastUserIndex];
    const text = textContent(userMessage.content);
    const imageAttachments: AttachmentRef[] =
      typeof userMessage.content === "string"
        ? []
        : userMessage.content
            .filter((part) => part.type === "image_url")
            .map((part, i) => ({ path: `retried-image-${i}`, isDir: false, kind: "image" as const, dataUrl: part.image_url.url }));
    if (!text.trim() && imageAttachments.length === 0) return;

    preparingTurnRef.current = true;
    setPreparingTurn(true);
    void prepareTurnInstructions(text)
      .then((skillInvocations) => {
        if (useSessionStore.getState().runningTurns[sessionId]) {
          throw new Error("This chat started another turn while plugin instructions were being verified.");
        }
        useSessionStore.getState().truncateFromIndex(sessionId, lastUserIndex);
        // Ultracode is sticky for the chat (see EffortSelector), so a retry
        // keeps it — same flag the original send carried.
        sendTurn(text, imageAttachments, skillInvocations, ultracodeMode);
      })
      .catch((turnError: unknown) => {
        setError(turnError);
      })
      .finally(() => {
        preparingTurnRef.current = false;
        setPreparingTurn(false);
      });
  }, [sending, prepareTurnInstructions, sendTurn, sessionId, ultracodeMode]);

  const closeMentionPopup = useCallback(() => {
    setMentionQuery(null);
    mentionStartRef.current = null;
  }, []);

  const selectMentionEntry = useCallback(
    (entry: MentionEntry) => {
      const start = mentionStartRef.current;
      const el = textareaRef.current;
      if (start === null) {
        closeMentionPopup();
        return;
      }

      const currentValue = el ? el.value : input;
      const end = el?.selectionStart ?? currentValue.length;
      const insertion = `@${entry.path} `;
      const nextValue = currentValue.slice(0, start) + insertion + currentValue.slice(end);
      const cursorPos = start + insertion.length;

      setInput(nextValue);
      closeMentionPopup();

      requestAnimationFrame(() => {
        const node = textareaRef.current;
        if (node) {
          node.focus();
          node.setSelectionRange(cursorPos, cursorPos);
        }
        resizeTextarea();
      });
    },
    [input, closeMentionPopup, resizeTextarea]
  );

  const closeSlashPopup = useCallback(() => {
    setSlashQuery(null);
    slashStartRef.current = null;
  }, []);

  const selectSlashEntry = useCallback(
    (entry: SlashCatalogEntry) => {
      const start = slashStartRef.current;
      const el = textareaRef.current;
      if (start === null) {
        closeSlashPopup();
        return;
      }

      const currentValue = el ? el.value : input;
      const end = el?.selectionStart ?? currentValue.length;

      // Picking a persona sets it as this session's active persona (composed
      // into the system prompt every turn — see systemPrompt.ts) and clears
      // the "/command" the user typed, the Cherry-Studio-style "switch
      // assistant from the composer" gesture; it never inserts text into the
      // composer the way a snippet does.
      if (entry.kind === "persona") {
        useSessionStore.getState().setSessionPersona(sessionId, entry.id);
        const nextValue = currentValue.slice(0, start) + currentValue.slice(end);
        setInput(nextValue);
        closeSlashPopup();

        requestAnimationFrame(() => {
          const node = textareaRef.current;
          if (node) {
            node.focus();
            node.setSelectionRange(start, start);
          }
          resizeTextarea();
        });
        return;
      }

      const insertion = entry.builtin || entry.kind === "skill" ? `/${entry.command} ` : entry.content;
      const nextValue = currentValue.slice(0, start) + insertion + currentValue.slice(end);
      const cursorPos = start + insertion.length;

      setInput(nextValue);
      closeSlashPopup();

      requestAnimationFrame(() => {
        const node = textareaRef.current;
        if (node) {
          node.focus();
          node.setSelectionRange(cursorPos, cursorPos);
        }
        resizeTextarea();
      });
    },
    [input, closeSlashPopup, resizeTextarea, sessionId]
  );

  const handleInput = (event: FormEvent<HTMLTextAreaElement>) => {
    const value = event.currentTarget.value;
    const cursor = event.currentTarget.selectionStart;
    setInput(value);
    resizeTextarea();

    // Mention and slash triggers are mutually exclusive: one anchors at an
    // "@" that can appear anywhere, the other only at index 0 — but both
    // popups are still separate pieces of state, so an inactive trigger's
    // popup must be explicitly closed rather than left stale.
    const mentionRange = findMentionRange(value, cursor);
    if (mentionRange) {
      if (slashQuery !== null) closeSlashPopup();

      mentionStartRef.current = mentionRange.start;
      setMentionQuery(mentionRange.query);
      setMentionActiveIndex(0);

      const requestId = ++mentionRequestIdRef.current;
      void loadWorkspacePaths().then((all) => {
        if (mentionRequestIdRef.current !== requestId) return; // a newer keystroke superseded this fetch
        setMentionEntries(filterMentionEntries(all, mentionRange.query));
      });
      return;
    }
    if (mentionQuery !== null) closeMentionPopup();

    const slashRange = findSlashRange(value, cursor);
    if (slashRange) {
      slashStartRef.current = slashRange.start;
      setSlashQuery(slashRange.query);
      setSlashActiveIndex(0);
      setSlashEntries(filterSlashEntries(slashCatalog, slashRange.query));
      return;
    }
    if (slashQuery !== null) closeSlashPopup();
  };

  const handleKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    // Enter confirms an active IME composition on CJK and other input
    // methods; it must not choose a suggestion or send the message too.
    if (event.nativeEvent.isComposing) return;
    // Resolve from the store on every event so an edit in Settings is live
    // without remounting the composer (including in a secondary window).
    const { overrides } = useShortcutStore.getState();
    const platform = detectShortcutPlatform();
    const suggestionShortcut = shortcutIdForEvent(event, "suggestions", platform, overrides);
    if (mentionQuery !== null) {
      if (suggestionShortcut === "nextSuggestion") {
        event.preventDefault();
        setMentionActiveIndex((prev) => (mentionEntries.length === 0 ? 0 : (prev + 1) % mentionEntries.length));
        return;
      }
      if (suggestionShortcut === "previousSuggestion") {
        event.preventDefault();
        setMentionActiveIndex((prev) =>
          mentionEntries.length === 0 ? 0 : (prev - 1 + mentionEntries.length) % mentionEntries.length
        );
        return;
      }
      if (suggestionShortcut === "chooseSuggestion") {
        event.preventDefault();
        const entry = mentionEntries[mentionActiveIndex];
        if (entry) {
          selectMentionEntry(entry);
        } else {
          closeMentionPopup();
        }
        return;
      }
      if (suggestionShortcut === "closeSuggestions") {
        event.preventDefault();
        closeMentionPopup();
        return;
      }
    }

    if (slashQuery !== null) {
      if (suggestionShortcut === "nextSuggestion") {
        event.preventDefault();
        setSlashActiveIndex((prev) => (slashEntries.length === 0 ? 0 : (prev + 1) % slashEntries.length));
        return;
      }
      if (suggestionShortcut === "previousSuggestion") {
        event.preventDefault();
        setSlashActiveIndex((prev) => (slashEntries.length === 0 ? 0 : (prev - 1 + slashEntries.length) % slashEntries.length));
        return;
      }
      if (suggestionShortcut === "chooseSuggestion") {
        event.preventDefault();
        const entry = slashEntries[slashActiveIndex];
        if (entry) {
          selectSlashEntry(entry);
        } else {
          closeSlashPopup();
        }
        return;
      }
      if (suggestionShortcut === "closeSuggestions") {
        event.preventDefault();
        closeSlashPopup();
        return;
      }
    }

    const composerShortcut = shortcutIdForEvent(event, "composer", platform, overrides);
    if (composerShortcut === "sendMessage") {
      event.preventDefault();
      handleSend();
      return;
    }

    if (composerShortcut === "insertLineBreak") {
      event.preventDefault();

      const textarea = event.currentTarget;
      const currentValue = textarea.value;
      const selectionStart = textarea.selectionStart ?? currentValue.length;
      const selectionEnd = textarea.selectionEnd ?? selectionStart;
      const nextValue =
        currentValue.slice(0, selectionStart) + "\n" + currentValue.slice(selectionEnd);
      const nextCaret = selectionStart + 1;

      setInput(nextValue);
      closeMentionPopup();
      closeSlashPopup();
      requestAnimationFrame(() => {
        const node = textareaRef.current;
        if (!node) return;
        node.focus();
        node.setSelectionRange(nextCaret, nextCaret);
        resizeTextarea();
      });
      return;
    }

    // Once Enter/Shift+Enter is reassigned, do not let the textarea's native
    // newline behavior keep the old binding alive behind the registry's back.
    if (event.key === "Enter") event.preventDefault();
  };

  // Rendered either in the composer footer or portaled into the title-bar
  // strip (`headerActionsSlot`) — same elements and state either way.
  const comparisonPickers = (
    <>
      <CompareTargetPicker
        value={compareTargets}
        onChange={(targets) => {
          setCompareTargets(targets);
          if (targets.length > 0) {
            setCrewId(null);
            setUltracodeMode(false);
          }
        }}
        disabled={sending || preparingTurn || startingComparison || startingCrew}
        placement={headerActionsSlot ? "down" : "up"}
      />
      <CrewPicker
        value={crewId}
        onChange={(nextCrewId) => {
          setCrewId(nextCrewId);
          if (nextCrewId) {
            setCompareTargets([]);
            setUltracodeMode(false);
          }
        }}
        disabled={sending || preparingTurn || startingComparison || startingCrew}
        placement={headerActionsSlot ? "down" : "up"}
      />
    </>
  );

  // Knowledge (StackPicker) portals alongside Compare/Crew when a header
  // slot exists; otherwise it stays in its original composer-footer spot
  // (see the split pane's fallback render below) rather than jumping rows.
  const stackPicker = <StackPicker sessionId={sessionId} placement={headerActionsSlot ? "down" : "up"} />;

  return (
    <div className="flex h-full min-h-0 min-w-0 flex-1 flex-col bg-background">
      <MessageList
        sessionId={sessionId}
        messages={messages}
        onEditUserMessage={handleEditMessage}
        editingDisabled={sending}
        onRetry={handleRetry}
        onStartSideTask={handleStartSideTask}
        onEditGeneratedImage={handleEditGeneratedImage}
        onOpenBackgroundTasks={onOpenBackgroundTasks}
        onOpenSettingsTab={onOpenSettingsTab}
      />

      {error !== null && (
        <div className="mx-4 mb-2">
          <div className="mx-auto flex max-w-3xl items-center justify-between gap-3 rounded-lg border border-danger bg-danger-soft px-3 py-2 text-sm text-danger">
            <span className="min-w-0 break-words">
              {serviceRepairNeeded ? t("ChatWindow.executionServiceDown") : errorMessage(error)}
            </span>
            {/* Retrying a turn the execution service refused just refuses
                again, so the same slot repairs the service instead. */}
            <button
              type="button"
              onClick={serviceRepairNeeded
                ? handleRepairService
                : compareTargets.length >= 2 || crewId ? handleSend : handleRetry}
              disabled={sending || preparingTurn || startingComparison || startingCrew || repairingService}
              className="shrink-0 cursor-pointer rounded-md border border-danger px-2 py-0.5 text-xs transition-colors hover:bg-danger hover:text-danger-foreground disabled:cursor-not-allowed disabled:opacity-50"
            >
              {serviceRepairNeeded
                ? repairingService ? t("ChatWindow.repairingService") : t("ChatWindow.repairServiceButton")
                : t("ChatWindow.retryButton")}
            </button>
          </div>
        </div>
      )}

      {persistError && (
        <div className="mx-4 mb-2">
          <div className="mx-auto max-w-3xl rounded-lg border border-warning bg-warning-soft px-3 py-2 text-sm text-warning">
            {t("ChatWindow.persistErrorBanner", { error: persistError })}
          </div>
        </div>
      )}

      <TaskSuggestionChips sessionId={sessionId} />

      <RunningTasksChip onClick={onOpenBackgroundTasks} onOpenStudio={onOpenStudio} />

      <div className="relative shrink-0 bg-background px-4 py-3">
        <SideChatPanel sessionId={sessionId} />
        <WorkspaceBar sessionId={sessionId} />
        <div className="relative mx-auto max-w-3xl">
          {mentionQuery !== null && (
            <MentionAutocomplete
              query={mentionQuery}
              entries={mentionEntries}
              activeIndex={mentionActiveIndex}
              onSelect={selectMentionEntry}
              onHoverIndex={setMentionActiveIndex}
            />
          )}
          {slashQuery !== null && (
            <SlashCommandAutocomplete
              query={slashQuery}
              entries={slashEntries}
              activeIndex={slashActiveIndex}
              onSelect={selectSlashEntry}
              onHoverIndex={setSlashActiveIndex}
            />
          )}
          <div className="flex flex-col rounded-3xl border border-border bg-surface px-4 py-2 transition-colors focus-within:border-accent focus-within:ring-1 focus-within:ring-accent">
            {(activePackageRuleCount > 0 || invokedSkillPreview.length > 0) && (
              <div className="mb-1.5 flex flex-wrap gap-1.5" aria-label={t("ChatWindow.activeTurnInstructionsLabel")}>
                {activePackageRuleCount > 0 && (
                  <span
                    title={t("ChatWindow.activePackageRulesTitle", { count: activePackageRuleCount })}
                    className="rounded-full border border-accent/30 bg-accent-soft px-2 py-0.5 text-[11px] text-accent"
                  >
                    {t("ChatWindow.activePackageRules", { count: activePackageRuleCount })}
                  </span>
                )}
                {invokedSkillPreview.map(({ skill }) => (
                  <span
                    key={skill.id}
                    title={`${skill.source} · ${skill.version}`}
                    className="rounded-full border border-success/30 bg-success-soft px-2 py-0.5 text-[11px] text-success"
                  >
                    /{skill.command} · {skill.name}
                  </span>
                ))}
              </div>
            )}
            {attachments.length > 0 && (
              <div className="mb-1.5 flex flex-wrap gap-1.5">
                {attachments.map((attachment) => {
                  const segments = attachment.path.split(/[\\/]/).filter(Boolean);
                  const name = attachment.label ?? segments[segments.length - 1] ?? attachment.path;
                  return (
                    <AttachmentChip
                      key={attachment.path}
                      name={name}
                      isDir={attachment.isDir}
                      previewUrl={attachment.kind === "image" ? attachment.dataUrl : undefined}
                      // Inline-content attachments (terminal evidence) carry a
                      // synthetic path — nothing to reveal on disk.
                      revealPath={attachment.content === undefined ? attachment.path : undefined}
                      onRemove={() => handleRemoveAttachment(attachment.path)}
                    />
                  );
                })}
              </div>
            )}
            <div className="flex items-end gap-2">
              <div className="relative min-w-0 flex-1">
                <textarea
                  ref={textareaRef}
                  value={input}
                  onChange={handleInput}
                  onKeyDown={handleKeyDown}
                  onScroll={syncCommandOverlayScroll}
                  placeholder={t("ChatWindow.inputPlaceholder")}
                  rows={1}
                  disabled={preparingTurn || startingComparison || startingCrew}
                  data-focus-ring="custom"
                  className={`block max-h-40 min-h-[1.75rem] w-full resize-none bg-transparent py-1 text-sm leading-relaxed outline-none placeholder:text-faint ${
                    commandSegments ? "text-transparent caret-foreground" : "text-foreground"
                  }`}
                />
                {commandSegments && (
                  <div
                    ref={commandOverlayRef}
                    aria-hidden
                    className="pointer-events-none absolute inset-0 overflow-hidden whitespace-pre-wrap break-words py-1 text-sm leading-relaxed text-foreground"
                  >
                    {commandSegments.map((segment, index) =>
                      segment.command ? (
                        <span key={index} className="text-accent">
                          {segment.text}
                        </span>
                      ) : (
                        <span key={index}>{segment.text}</span>
                      ),
                    )}
                  </div>
                )}
              </div>
              <DictationButton
                ref={dictationButtonRef}
                sessionId={sessionId}
                value={input}
                onChange={setInput}
                textareaRef={textareaRef}
                resizeTextarea={resizeTextarea}
                disabled={sending || preparingTurn || startingComparison || startingCrew}
              />
              <span className="group/action relative shrink-0">
                <button
                  type="button"
                  onClick={sending ? handleStop : handleSend}
                  disabled={preparingTurn || startingComparison || startingCrew || localModelStarting || (!sending && !input.trim())}
                  aria-label={sending ? t("ChatWindow.stopResponseAriaLabel") : t("ChatWindow.sendMessageAriaLabel")}
                  className="flex h-7 w-7 shrink-0 cursor-pointer items-center justify-center rounded-full text-faint transition-colors duration-150 hover:bg-surface-2 hover:text-foreground disabled:cursor-not-allowed disabled:opacity-40 disabled:hover:bg-transparent focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2 focus-visible:ring-offset-background"
                >
                  {sending ? <Square size={13} className="fill-current" /> : <CornerDownLeft size={16} />}
                </button>
                {/* The one control in the composer whose effect is worth
                    spelling out: stopping mid-turn keeps what already
                    streamed rather than discarding the exchange. */}
                {sending && (
                  <Tooltip
                    text={t("ChatWindow.stopResponseAriaLabel")}
                    hint={t("ChatWindow.stopResponseHint")}
                  />
                )}
              </span>
            </div>
          </div>
        </div>
        <div className="mx-auto mt-1.5 flex max-w-3xl flex-wrap items-center justify-between gap-x-3 gap-y-1.5">
          <div className="flex flex-wrap items-center gap-1.5">
            <ModeSelector />
            <PersonaSelector sessionId={sessionId} onManagePrompts={onManagePrompts} />
            <AttachMenu onAddFiles={() => void handleAddFiles()} onAddFolder={() => void handleAddFolder()} />
            {!headerActionsSlot && comparisonPickers}
          </div>
          <div className="flex flex-wrap items-center gap-3">
            <ModelSwitcher />
            {!headerActionsSlot && stackPicker}
            <EffortSelector
              ultracodeActive={ultracodeMode}
              onUltracodeChange={(active) => {
                setUltracodeMode(active);
                if (active) {
                  setCompareTargets([]);
                  setCrewId(null);
                }
              }}
              disabled={sending || preparingTurn || startingComparison || startingCrew}
            />
            <CheckpointTimeline sessionId={sessionId} />
            <ContextUsageIndicator sessionId={sessionId} />
          </div>
        </div>
      </div>
      {headerActionsSlot &&
        createPortal(
          <>
            {comparisonPickers}
            {stackPicker}
          </>,
          headerActionsSlot,
        )}
    </div>
  );
}
