import { Suspense, useCallback, useEffect, useReducer, useRef, useState } from "react";
import { invoke, isTauri } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { Columns2, FileDiff, FolderTree, GitPullRequest, Globe2, ListTodo, Maximize2, Minimize2, PanelRight, Plus, SquareTerminal, X } from "lucide-react";

import ChatSessionList from "./components/Chat/ChatSessionList";
import ChatWindow from "./components/Chat/ChatWindow";
import { PrivacyFirewallGate } from "./components/Chat/PrivacyFirewallGate";
import { AppMenu } from "./components/AppMenu";
import { ErrorBoundary } from "./components/ErrorBoundary";
import { useBrowserPaneStore } from "./store/browserPaneStore";
import { useApprovalChainStore } from "./store/approvalChainStore";
import { useTerminalStore } from "./store/terminalStore";
import type { SettingsTab } from "./components/Settings/SettingsModal";
import { useRunStore } from "./store/runStore";
import {
  SIDE_TASK_PANEL_OPEN_REQUEST_EVENT,
  selectRunningSideTaskCount,
  useSideTaskStore,
} from "./store/sideTaskStore";
import { selectRunningShellTaskCount, useBackgroundShellStore } from "./store/backgroundShellStore";
import { useSubagentStore, selectRunningSubagentCount } from "./store/subagentStore";
import { SessionGrantBanner } from "./components/Workspace/SessionGrantBanner";
import { IconButton, Button } from "./components/ui";
import { useSessionStore } from "./store/sessionStore";
import { primaryRoot, useWorkspaceStore } from "./store/workspaceStore";
import { useModelStore } from "./store/modelStore";
import { useMcpStore } from "./store/mcpStore";
import { useArtifactStore } from "./store/artifactStore";
import { usePermissionStore } from "./store/permissionStore";
import { useShortcutStore } from "./store/shortcutStore";
import { useRecipeStore, subscribeToRecipeChanges } from "./store/recipeStore";
import { subscribeToLocalAppsChanges, subscribeToLocalAppRunRequests } from "./store/localAppsStore";
import { hydrateAutomations } from "./store/automationsStore";
import { useOnboardingStore } from "./store/onboardingStore";
import { startScheduler } from "./lib/scheduler";
import { startBackupScheduler } from "./lib/backupScheduler";
import { startSyntheticMonitoringScheduler } from "./store/syntheticMonitoringStore";
import { useT } from "./lib/i18n";
import {
  detectShortcutPlatform,
  shortcutDisplayLabel,
  shortcutIdForEvent,
  shouldHandleGlobalShortcut,
  type ShortcutIdForScope,
} from "./lib/shortcuts";
import { onRunCancellationRequested } from "./lib/runProtocol";
import { cancelRegisteredRun } from "./lib/runCancellationRegistry";
import { onProcessesChanged } from "./lib/processTable";
import {
  PENDING_SIGNAL_SWEEP_INTERVAL_MS,
  deliverProcessSignal,
  sweepPendingProcessSignals,
} from "./lib/processSignalDelivery";
import { recoverDaemonDesktopTurns } from "./lib/agentLoop";
import { paletteClient } from "./lib/paletteClient";
import { featurePanelReducer, type FeaturePanelId } from "./lib/appShellPanels";
import {
  AgentInbox,
  ApiContractDiffLabPanel,
  ApprovalChainModal,
  ArtifactPane,
  BackgroundTasksPanel,
  BriefStudioPanel,
  BrowserPane,
  BrowserWorkbench,
  CommandPalette,
  CompareView,
  ConnectorBuilderPanel,
  CrewView,
  CrossRepoChangePlannerPanel,
  CrossRepoIntelligencePanel,
  DailyBriefPanel,
  DatabaseAdminGuardrailsPanel,
  DataNotebookPanel,
  DebatePanel,
  DeepResearchWorkspacePanel,
  DesignToAppPanel,
  DiffPanel,
  DiffViewer,
  EvalHarnessPanel,
  EvidenceBoardPanel,
  FileTree,
  GlobalSearch,
  GoldenDatasetBuilderPanel,
  IncidentCommanderPanel,
  IssueToPrPanel,
  KnowledgeGraphExplorerPanel,
  McpGeneratorPanel,
  MigrationAgentPanel,
  OnboardingWizard,
  PermissionModal,
  PmCopilotPanel,
  ProductionDebuggingPanel,
  RedTeamLabPanel,
  ReviewPanel,
  RunCenter,
  SecurityAutofixPanel,
  SettingsModal,
  SideTaskPane,
  SopCompilerPanel,
  SpreadsheetCopilotPanel,
  SyntheticMonitoringPanel,
  TerminalPanel,
  TrustScorecardsPanel,
  VisualEditModePanel,
  WorkCanvasPanel,
} from "./app/lazyComponents";

/** A file currently previewed in the Workspace panel, with a baseline snapshot
 * (captured the moment it was opened) so edits made by the agent afterwards
 * can be diffed against it. */
interface SelectedFile {
  path: string;
  original: string;
  current: string;
}

function formatError(err: unknown): string {
  if (err instanceof Error) return err.message;
  if (typeof err === "string") return err;
  try {
    return JSON.stringify(err);
  } catch {
    return String(err);
  }
}

/** The panels the right sidebar can host as tabs. EVERY secondary surface
 * lives here — review, working-tree diff, terminal, browser, side tasks,
 * files, background tasks — rather than some of them taking over their own
 * column beside the chat. One region, one tab strip, one width: opening the
 * browser no longer hides the diff, and a side task no longer squeezes the
 * chat into a third of the window. */
type RightTabKind =
  | "review"
  | "diff"
  | "terminal"
  | "files"
  | "backgroundTasks"
  | "browser"
  | "sideTasks";

const RIGHT_TAB_KINDS: readonly RightTabKind[] = [
  "review",
  "diff",
  "terminal",
  "browser",
  "sideTasks",
  "files",
  "backgroundTasks",
];

const RIGHT_TAB_LABEL_KEYS: Record<RightTabKind, string> = {
  review: "App.rightPanelReview",
  diff: "App.diffPanelTitle",
  terminal: "App.rightPanelTerminal",
  browser: "App.rightPanelBrowser",
  sideTasks: "App.sideTaskPaneTitle",
  files: "App.rightPanelWorkspace",
  backgroundTasks: "App.rightPanelBackgroundTasks",
};

/** Null for a tab with no dedicated accelerator — the picker just omits the
 * key hint for it rather than inventing a binding. */
const RIGHT_TAB_SHORTCUT_IDS: Record<
  RightTabKind,
  "openReview" | "openTerminal" | "openBrowserTab" | "openFiles" | "openBackgroundTasksPanel" | "openSideTaskPane" | null
> = {
  review: "openReview",
  diff: null,
  terminal: "openTerminal",
  browser: "openBrowserTab",
  sideTasks: "openSideTaskPane",
  files: "openFiles",
  backgroundTasks: "openBackgroundTasksPanel",
};

function RightTabIcon({ kind, size }: { kind: RightTabKind; size: number }) {
  const className = "shrink-0 text-faint";
  switch (kind) {
    case "review":
      return <GitPullRequest size={size} className={className} />;
    case "diff":
      return <FileDiff size={size} className={className} />;
    case "terminal":
      return <SquareTerminal size={size} className={className} />;
    case "browser":
      return <Globe2 size={size} className={className} />;
    case "sideTasks":
      return <Columns2 size={size} className={className} />;
    case "files":
      return <FolderTree size={size} className={className} />;
    default:
      return <ListTodo size={size} className={className} />;
  }
}

function LazyPanelFallback() {
  return (
    <div className="flex h-full min-h-0 w-full items-center justify-center" aria-busy="true">
      <div className="h-5 w-5 animate-spin rounded-full border-2 border-border border-t-accent" />
    </div>
  );
}

/** Clamps a user-dragged right-menu width to a sane range, capped relative
 * to the viewport so a wide drag on a big display can't be dragged into an
 * unreasonably narrow chat column. `minWidth` rises above the 220 floor once
 * the fixed dock-toggle cluster's width is known: the sidebar's tab strip
 * reserves that cluster's footprint at its right end, so a sidebar narrower
 * than cluster + one chip would leave the strip with no usable viewport. */
function clampRightMenuWidth(value: number, minWidth = 220): number {
  const floor = Math.max(220, Math.round(minWidth));
  const viewportCap = Math.max(floor, Math.floor((typeof window === "undefined" ? 1200 : window.innerWidth) * 0.6));
  return Math.min(viewportCap, Math.max(floor, Math.round(value)));
}

/** Minimum right-sidebar width for a given dock-cluster reserve: the strip
 * keeps at least ~140px of scrollable chip viewport beside the reserve. */
function rightMenuMinWidth(dockReserve: number): number {
  return dockReserve > 0 ? dockReserve + 140 : 220;
}

function App() {
  const { t } = useT();
  const activeSessionId = useSessionStore((s) => s.activeSessionId);
  const activeComparisonId = useSessionStore((s) =>
    s.sessions.find((session) => session.id === s.activeSessionId)?.comparisonBranch?.comparisonId ?? null
  );
  const activeCrewSessionId = useSessionStore((s) =>
    s.sessions.find((session) => session.id === s.activeSessionId)?.crewRun ? s.activeSessionId : null
  );
  const newSession = useSessionStore((s) => s.newSession);
  const switchSession = useSessionStore((s) => s.switchSession);
  const splitSessionId = useSessionStore((s) => s.splitSessionId);
  const splitTitle = useSessionStore((s) =>
    s.splitSessionId === null ? null : s.sessions.find((x) => x.id === s.splitSessionId)?.title ?? null
  );
  const splitCrewSessionId = useSessionStore((s) =>
    s.splitSessionId !== null && s.sessions.find((session) => session.id === s.splitSessionId)?.crewRun
      ? s.splitSessionId
      : null
  );
  const closeSplit = useSessionStore((s) => s.closeSplit);
  const rootsVersion = useWorkspaceStore((s) => s.rootsVersion);
  const refreshRoots = useWorkspaceStore((s) => s.refreshRoots);
  const refreshRecent = useWorkspaceStore((s) => s.refreshRecent);
  const refreshModels = useModelStore((s) => s.refresh);
  const refreshOllama = useModelStore((s) => s.refreshOllama);
  const refreshProviders = useModelStore((s) => s.refreshProviders);
  const refreshRecipes = useRecipeStore((s) => s.refresh);
  const refreshMcp = useMcpStore((s) => s.refresh);
  const connectMcp = useMcpStore((s) => s.connect);
  const activeArtifact = useArtifactStore((s) => s.active);
  const permissionPending = usePermissionStore((s) => s.pending !== null);
  const hasCompletedOnboarding = useOnboardingStore((s) => s.hasCompletedOnboarding);
  const restartOnboarding = useOnboardingStore((s) => s.restartOnboarding);

  const [workspacePanelOpen, setWorkspacePanelOpen] = useState(true);
  const [selectedFile, setSelectedFile] = useState<SelectedFile | null>(null);
  const [diffLoading, setDiffLoading] = useState(false);
  const [diffError, setDiffError] = useState<string | null>(null);
  const [activeFeaturePanel, dispatchFeaturePanel] = useReducer(featurePanelReducer, null);
  const [commandPaletteOpen, setCommandPaletteOpen] = useState(false);
  const [terminalOpen, setTerminalOpen] = useState(false);
  const [settingsMounted, setSettingsMounted] = useState(false);
  const settingsOpen = activeFeaturePanel === "settings";
  const runCenterOpen = activeFeaturePanel === "run-center";
  const browserWorkbenchOpen = activeFeaturePanel === "browser-workbench";
  const designToAppOpen = activeFeaturePanel === "design-to-app";
  const issueToPrOpen = activeFeaturePanel === "issue-to-pr";
  const productionDebuggingOpen = activeFeaturePanel === "production-debugging";
  const incidentCommanderOpen = activeFeaturePanel === "incident-commander";
  const securityAutofixOpen = activeFeaturePanel === "security-autofix";
  const trustScorecardsOpen = activeFeaturePanel === "trust-scorecards";
  const sopCompilerOpen = activeFeaturePanel === "sop-compiler";
  const mcpGeneratorOpen = activeFeaturePanel === "mcp-generator";
  const connectorBuilderOpen = activeFeaturePanel === "connector-builder";
  const migrationAgentOpen = activeFeaturePanel === "migration-agent";
  const globalSearchOpen = activeFeaturePanel === "global-search";
  const agentInboxOpen = activeFeaturePanel === "agent-inbox";
  const redTeamLabOpen = activeFeaturePanel === "red-team-lab";
  const knowledgeGraphOpen = activeFeaturePanel === "knowledge-graph";
  const spreadsheetCopilotOpen = activeFeaturePanel === "spreadsheet-copilot";
  const evidenceBoardOpen = activeFeaturePanel === "evidence-board";
  const goldenDatasetBuilderOpen = activeFeaturePanel === "golden-dataset-builder";
  const dailyBriefOpen = activeFeaturePanel === "daily-brief";
  const dataNotebookOpen = activeFeaturePanel === "data-notebook";
  const syntheticMonitoringOpen = activeFeaturePanel === "synthetic-monitoring";
  const crossRepoIntelligenceOpen = activeFeaturePanel === "cross-repo-intelligence";
  const workCanvasOpen = activeFeaturePanel === "work-canvas";
  const pmCopilotOpen = activeFeaturePanel === "pm-copilot";
  const deepResearchOpen = activeFeaturePanel === "deep-research";
  const briefStudioOpen = activeFeaturePanel === "brief-studio";
  const crossRepoPlannerOpen = activeFeaturePanel === "cross-repo-planner";
  const visualEditModeOpen = activeFeaturePanel === "visual-edit-mode";
  const debateOpen = activeFeaturePanel === "debate";
  const dbAdminGuardrailsOpen = activeFeaturePanel === "db-admin-guardrails";
  const apiContractDiffLabOpen = activeFeaturePanel === "api-contract-diff-lab";
  const workflowTestHarnessOpen = activeFeaturePanel === "workflow-test-harness";
  useEffect(() => {
    if (settingsOpen) setSettingsMounted(true);
  }, [settingsOpen]);

  // Title-bar slot the primary ChatWindow portals its Compare/Crew pickers
  // into — callback-ref state (not a plain ref) so ChatWindow re-renders
  // once the element mounts and the portal can attach.
  const [chatHeaderActionsEl, setChatHeaderActionsEl] = useState<HTMLDivElement | null>(null);
  const terminalDock = useTerminalStore((state) => state.dock);
  const browserPaneOpen = useBrowserPaneStore((state) => state.open);
  const approvalChainPending = useApprovalChainStore((s) => s.pending !== null);
  /** Changed-file count behind the top-bar Diff badge; polled, best-effort. */
  const [changedFileCount, setChangedFileCount] = useState(0);
  const runningSideTaskCount = useSideTaskStore(selectRunningSideTaskCount);
  const runningSubagentCount = useSubagentStore(selectRunningSubagentCount);
  const runningShellCount = useBackgroundShellStore(selectRunningShellTaskCount);
  // Headless work only — background shell commands plus `task` subagents.
  // Side tasks are counted separately (they have their own sidebar tab and
  // their own badge) precisely because they are not this: they are
  // conversations the user opened, not work the app is doing behind the chat.
  const runningBackgroundTaskCount = runningShellCount + runningSubagentCount;

  /** The browser pane paints a NATIVE webview over its content rect, which
   * always sits above the app's DOM — so it must be told to hide whenever
   * something would cover it (settings, palette, permission prompts). */
  const browserPaneObscured =
    settingsOpen || commandPaletteOpen || permissionPending || approvalChainPending;

  // The fixed dock-toggle cluster (top-right, below) floats ABOVE whatever
  // column happens to be rightmost, so that column's h-11 tab strip must not
  // lay chips underneath it — a strip that scrolls "under" transparent icons
  // reads as broken, and the last tab would be permanently obscured. The
  // cluster's width is dynamic (badges, the conditional fullscreen toggle),
  // so it is measured for real and handed to the strips as a trailing inset.
  const [dockEl, setDockEl] = useState<HTMLDivElement | null>(null);
  const [dockReserve, setDockReserve] = useState(0);
  useEffect(() => {
    if (!dockEl) return;
    // 12px `right-3` offset + 8px breathing room past the cluster's left edge.
    const update = () => setDockReserve(Math.ceil(dockEl.getBoundingClientRect().width) + 20);
    update();
    const observer = new ResizeObserver(update);
    observer.observe(dockEl);
    return () => observer.disconnect();
  }, [dockEl]);

  // Background shell commands are owned by Rust and can be started by any
  // window's turn, so the shell subscribes once at boot — otherwise the
  // top-bar badge would only ever know about commands started while the
  // Background Tasks panel happened to be mounted.
  useEffect(() => {
    void useBackgroundShellStore.getState().initialize();
  }, []);

  /** The right sidebar hosts real TABS: several panels open at once, one
   * active, the rest kept mounted-but-hidden so their state (a running
   * terminal, review scroll position, a browser page) survives switching —
   * choosing one never closes another. */
  const [rightTabs, setRightTabs] = useState<RightTabKind[]>([]);
  const [activeRightTab, setActiveRightTab] = useState<RightTabKind | null>(null);
  /** Whether the region is visible at all; with no tabs open it shows the
   * centered picker menu. */
  const [rightOpen, setRightOpen] = useState(false);
  const [plusMenuOpen, setPlusMenuOpen] = useState(false);
  /** Right-region fullscreen — covers the whole region (tab strip + active
   * tab). Reset when the region closes so it can't linger stale. */
  const [rightFullscreen, setRightFullscreen] = useState(false);
  useEffect(() => {
    if (!rightOpen) setRightFullscreen(false);
  }, [rightOpen]);

  // Top-bar Diff badge: slow poll of the changed-file count, refreshed
  // immediately whenever the diff tab comes forward. Badge only — the panel
  // itself fetches its own list.
  useEffect(() => {
    if (!isTauri()) return;
    let cancelled = false;
    const refresh = async () => {
      if (!primaryRoot(useWorkspaceStore.getState().roots)) return;
      try {
        const files = await invoke<unknown[]>("git_changed_files");
        if (!cancelled) setChangedFileCount(files.length);
      } catch {
        // Badge is cosmetic — ignore failures.
      }
    };
    void refresh();
    const timer = window.setInterval(() => void refresh(), 15_000);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [activeRightTab]);

  // Tabs whose visibility ALSO lives in a store, because feature code far
  // from this shell asks for them (a browser automation step, a message's
  // "start side task" action). Opening/closing the tab and flipping the
  // store's flag are kept in lockstep here, in both directions, so neither
  // side can end up showing a panel the other thinks is closed.
  const openRightTab = useCallback((kind: RightTabKind) => {
    setRightOpen(true);
    setPlusMenuOpen(false);
    setRightTabs((tabs) => (tabs.includes(kind) ? tabs : [...tabs, kind]));
    setActiveRightTab(kind);
    if (kind === "terminal") {
      useTerminalStore.getState().setDock("right");
      setTerminalOpen(true);
    }
    if (kind === "browser") useBrowserPaneStore.getState().setOpen(true);
    if (kind === "sideTasks") useSideTaskStore.getState().openPane();
  }, []);

  const closeRightTab = useCallback((kind: RightTabKind) => {
    setRightTabs((tabs) => {
      const next = tabs.filter((tab) => tab !== kind);
      setActiveRightTab((active) => (active === kind ? next[next.length - 1] ?? null : active));
      return next;
    });
    if (kind === "terminal" && useTerminalStore.getState().dock === "right") {
      setTerminalOpen(false);
    }
    if (kind === "browser") useBrowserPaneStore.getState().setOpen(false);
    if (kind === "sideTasks") useSideTaskStore.getState().closePane();
  }, []);

  // With every surface living in this one strip, the active chip can easily
  // sit outside the scrolled viewport (the strip also gives up its right end
  // to the dock cluster) — so bring it into view whenever it changes, rather
  // than leaving the user looking at a panel whose tab they cannot see.
  const rightTabStripRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!activeRightTab) return;
    const reveal = () =>
      rightTabStripRef.current
        ?.querySelector(`[data-right-tab="${activeRightTab}"]`)
        ?.scrollIntoView({ block: "nearest", inline: "nearest" });
    reveal();
    // Again once the region's open/resize transition has settled: the first
    // call can land while the sidebar is still animating out from width 0,
    // where there is nothing to scroll yet.
    const timer = window.setTimeout(reveal, 240);
    return () => window.clearTimeout(timer);
  }, [activeRightTab, rightTabs]);

  /** True when `kind` is the tab currently on screen — what the dock toggles
   * use to decide between "bring this forward" and "close it". */
  const rightTabShowing = useCallback(
    (kind: RightTabKind) => rightOpen && activeRightTab === kind,
    [rightOpen, activeRightTab],
  );

  const toggleRightTab = useCallback(
    (kind: RightTabKind) => {
      if (rightOpen && activeRightTab === kind) closeRightTab(kind);
      else openRightTab(kind);
    },
    [rightOpen, activeRightTab, closeRightTab, openRightTab],
  );

  // Source-aware side-task actions can originate anywhere in the app (a
  // message action, a terminal selection, browser evidence). Their store
  // emits a typed, payload-free request; the shell owns showing the side-task
  // TAB without coupling feature code to this layout.
  useEffect(() => {
    const revealSideTasks = () => openRightTab("sideTasks");
    window.addEventListener(SIDE_TASK_PANEL_OPEN_REQUEST_EVENT, revealSideTasks);
    return () => window.removeEventListener(SIDE_TASK_PANEL_OPEN_REQUEST_EVENT, revealSideTasks);
  }, [openRightTab]);

  // Dock moves migrate the terminal between its two homes: re-docked to the
  // bottom (via the panel's dock button) it leaves the tab strip; re-docked
  // to the right (same button on the bottom panel) it must gain a sidebar
  // tab, otherwise an open right-docked terminal would have nowhere to
  // render at all.
  useEffect(() => {
    if (terminalDock === "bottom") {
      setRightTabs((tabs) => {
        if (!tabs.includes("terminal")) return tabs;
        const next = tabs.filter((tab) => tab !== "terminal");
        setActiveRightTab((active) => (active === "terminal" ? next[next.length - 1] ?? null : active));
        return next;
      });
    } else if (terminalOpen) {
      openRightTab("terminal");
    }
  }, [terminalDock, terminalOpen, openRightTab]);

  // Store-driven reveal for the browser: anything that flips
  // `browserPaneStore.open` (a workbench handoff, a shortcut) gets its
  // sidebar tab opened here. Guarded on the tab being absent so it never
  // fights `openRightTab`, which sets that same flag itself. The side-task
  // twin of this effect lives beside `sideTaskPaneOpen` further down.
  useEffect(() => {
    if (browserPaneOpen && !rightTabs.includes("browser")) openRightTab("browser");
  }, [browserPaneOpen, rightTabs, openRightTab]);

  // User-resizable width for the right sidebar region, shared by every tab
  // and persisted across restarts — mirrors the terminal's own drag-to-resize
  // (TerminalPanel.tsx).
  const RIGHT_MENU_WIDTH_KEY = "little-monkey-right-menu-width";
  const [rightMenuWidth, setRightMenuWidthState] = useState(() => {
    try {
      const stored = Number(localStorage.getItem(RIGHT_MENU_WIDTH_KEY));
      return Number.isFinite(stored) && stored > 0 ? clampRightMenuWidth(stored) : 288;
    } catch {
      return 288;
    }
  });
  const setRightMenuWidth = useCallback((value: number) => {
    const clamped = clampRightMenuWidth(value, rightMenuMinWidth(dockReserve));
    setRightMenuWidthState(clamped);
    try {
      localStorage.setItem(RIGHT_MENU_WIDTH_KEY, String(clamped));
    } catch {
      // Best-effort persistence only.
    }
  }, [dockReserve]);
  const onMenuResizeStart = useCallback((event: React.PointerEvent<HTMLDivElement>) => {
    event.preventDefault();
    const startX = event.clientX;
    const startWidth = rightMenuWidth;
    const target = event.currentTarget;
    target.setPointerCapture(event.pointerId);
    // Live moves only update state; localStorage is written once on release
    // — a synchronous storage write per pointermove makes the drag stutter.
    let latest = startWidth;
    const move = (moveEvent: PointerEvent) => {
      latest = clampRightMenuWidth(startWidth + (startX - moveEvent.clientX), rightMenuMinWidth(dockReserve));
      setRightMenuWidthState(latest);
    };
    const up = () => {
      target.removeEventListener("pointermove", move);
      target.removeEventListener("pointerup", up);
      target.removeEventListener("pointercancel", up);
      setRightMenuWidth(latest);
    };
    target.addEventListener("pointermove", move);
    target.addEventListener("pointerup", up);
    target.addEventListener("pointercancel", up);
  }, [rightMenuWidth, setRightMenuWidth, dockReserve]);

  // A persisted width from before the dock cluster was measured (or from a
  // wider window) can be narrower than the strip's reserve + viewport
  // minimum; widen the live state once the real reserve is known. Storage is
  // left alone — the correction is a display concern, not a new preference.
  useEffect(() => {
    if (!rightOpen || rightFullscreen || dockReserve === 0) return;
    const min = rightMenuMinWidth(dockReserve);
    setRightMenuWidthState((width) => (width < min ? clampRightMenuWidth(width, min) : width));
  }, [rightOpen, rightFullscreen, dockReserve]);
  const shortcutOverrides = useShortcutStore((s) => s.overrides);
  const shortcutLabel = useCallback(
    (id: Parameters<typeof shortcutDisplayLabel>[0]) =>
      shortcutDisplayLabel(id, detectShortcutPlatform(), shortcutOverrides),
    [shortcutOverrides],
  );
  // Tab Settings should jump to the moment it opens — set alongside the
  // active feature panel by anything that deep-links into a specific tab (right
  // now just `PersonaSelector`'s "Manage prompts…" row); left `undefined`
  // for the normal "open on whatever tab was last active" path (AppMenu).
  // Reset back to `undefined` on close (see the `SettingsModal` below) so a
  // one-off deep link doesn't stick around and hijack every later normal
  // open too.
  const [settingsInitialTab, setSettingsInitialTab] = useState<SettingsTab | undefined>(undefined);
  // A sequence number makes repeated requests for the same tab observable
  // even while Settings is already open and the user has navigated away.
  const [settingsTabRequest, setSettingsTabRequest] = useState(0);

  const activateFeaturePanel = useCallback((panel: FeaturePanelId) => {
    setCommandPaletteOpen(false);
    dispatchFeaturePanel({ type: "open", panel });
  }, []);

  /** Normal navigation forgets a one-off Settings deep link before opening. */
  const openFeaturePanel = useCallback((panel: FeaturePanelId) => {
    setSettingsInitialTab(undefined);
    activateFeaturePanel(panel);
  }, [activateFeaturePanel]);

  const closeFeaturePanel = useCallback((panel: FeaturePanelId) => {
    dispatchFeaturePanel({ type: "close", panel });
  }, []);

  const resetFeaturePanels = useCallback(() => {
    dispatchFeaturePanel({ type: "reset" });
  }, []);

  const openSettingsTab = useCallback((tab: SettingsTab) => {
    setSettingsInitialTab(tab);
    setSettingsTabRequest((request) => request + 1);
    activateFeaturePanel("settings");
  }, [activateFeaturePanel]);

  // The chat's "N running tasks" chip target — same body as the shortcut
  // action `openBackgroundTasksPanel` and the top-bar tasks toggle's open
  // branch.
  const openBackgroundTasksPanel = useCallback(() => {
    openRightTab("backgroundTasks");
  }, [openRightTab]);

  // Staging a composer seed (a message bubble's "Start side task" fork
  // button, a work-canvas node's delegate action) must also show the tab
  // that hosts the composer. The store's own `openComposer` already flips
  // `paneOpen`, so this is only the belt-and-braces path for a seed staged
  // through some other route; `openComposer` stages a fresh seed object on
  // every call, so it fires per click even when a previous seed was never
  // consumed.
  const sideTaskComposerSeed = useSideTaskStore((state) => state.composerSeed);
  useEffect(() => {
    if (sideTaskComposerSeed) openRightTab("sideTasks");
  }, [sideTaskComposerSeed, openRightTab]);

  const sideTaskPaneOpen = useSideTaskStore((state) => state.paneOpen);
  // Store-driven reveal for side tasks — the twin of the browser effect above.
  useEffect(() => {
    if (sideTaskPaneOpen && !rightTabs.includes("sideTasks")) openRightTab("sideTasks");
  }, [sideTaskPaneOpen, rightTabs, openRightTab]);

  const handleManagePrompts = useCallback(() => {
    openSettingsTab("prompts");
  }, [openSettingsTab]);

  // Opens the Global Command Palette over whatever's currently shown —
  // triggered by the in-window shortcut below (Cmd/Ctrl+Shift+K, only while
  // focused) and by the OS-level global shortcut (works even unfocused; see
  // `src-tauri/src/command_palette.rs`, which shows/focuses this window and
  // emits `palette://open` for the listener further down).
  const openCommandPalette = useCallback(() => {
    // Settings is the only feature surface above the palette in the z-stack;
    // dismiss it so the globally invoked palette cannot open invisibly.
    closeFeaturePanel("settings");
    setSettingsInitialTab(undefined);
    setCommandPaletteOpen(true);
  }, [closeFeaturePanel]);

  // App-wide accelerators. The same definitions are rendered by the
  // Keyboard Shortcuts Settings panel, so a displayed binding always has a
  // live handler behind it with native modifiers on macOS, Windows, and Linux.
  useEffect(() => {
    function handleKeyDown(event: KeyboardEvent) {
      // Read at event time rather than closing over a render-time snapshot:
      // edits made in Settings take effect on the very next keydown. Recording
      // must suspend dispatch because this capture listener runs before the
      // recorder's target-level key handler can cancel an existing shortcut.
      const { overrides, recordingId } = useShortcutStore.getState();
      if (!shouldHandleGlobalShortcut(event, permissionPending, recordingId !== null)) return;
      const shortcut = shortcutIdForEvent(event, "global", detectShortcutPlatform(), overrides);
      if (!shortcut) return;

      // Session-scoped commands (pin/rename/fork/archive/open-in-X) act on
      // whichever session is active — read fresh rather than via a selector
      // so a session switch made without a re-render of this effect is still
      // picked up on the very next keydown.
      const session = useSessionStore.getState();
      const activeSession = session.sessions.find((s) => s.id === session.activeSessionId) ?? null;
      const resolveWorkspacePath = () =>
        activeSession?.workspacePath ?? primaryRoot(useWorkspaceStore.getState().roots)?.path ?? null;
      const openInEditor = (editor: "cursor" | "vscode") => {
        const path = resolveWorkspacePath();
        if (!path) return;
        void invoke("open_in_editor", { path, editor }).catch((err) => console.error(err));
      };

      const actions: Record<ShortcutIdForScope<"global">, () => void> = {
        newSession: () => {
          resetFeaturePanels();
          setSettingsInitialTab(undefined);
          newSession();
        },
        openSettings: () => openFeaturePanel("settings"),
        openShortcuts: () => openSettingsTab("shortcuts"),
        toggleWorkspacePanel: () => setWorkspacePanelOpen((open) => !open),
        openCommandPalette: () => openCommandPalette(),
        toggleRightSidebar: () => setRightOpen((open) => !open),
        openTerminal: () => {
          if (useTerminalStore.getState().dock === "right") {
            toggleRightTab("terminal");
          } else {
            setTerminalOpen((open) => !open);
          }
        },
        openBrowser: () => toggleRightTab("browser"),
        openBrowserTab: () => openRightTab("browser"),
        openReview: () => openRightTab("review"),
        openFiles: () => openRightTab("files"),
        openBackgroundTasksPanel: () => openRightTab("backgroundTasks"),
        openSideTaskPane: () => toggleRightTab("sideTasks"),
        sessionTogglePin: () => activeSession && session.togglePin(activeSession.id),
        sessionToggleUnread: () => activeSession && session.toggleUnread(activeSession.id),
        sessionRename: () => activeSession && session.requestRename(activeSession.id),
        sessionFork: () => activeSession && session.forkSession(activeSession.id),
        sessionArchive: () =>
          activeSession &&
          (activeSession.archived ? session.unarchiveSession : session.archiveSession)(activeSession.id),
        sessionOpenWindow: () => {
          if (!activeSession) return;
          void invoke("open_session_window", { sessionId: activeSession.id }).catch((err) => console.error(err));
        },
        sessionOpenCursor: () => openInEditor("cursor"),
        sessionOpenVsCode: () => openInEditor("vscode"),
        sessionRevealFinder: () => {
          const path = resolveWorkspacePath();
          if (!path) return;
          void invoke("reveal_in_finder", { path }).catch((err) => console.error(err));
        },
      };

      event.preventDefault();
      actions[shortcut]();
    }

    // Capture keeps native-style app commands available even inside inputs
    // that stop bubbling for their own local Enter/Escape handling.
    window.addEventListener("keydown", handleKeyDown, true);
    return () => window.removeEventListener("keydown", handleKeyDown, true);
  }, [newSession, openCommandPalette, openFeaturePanel, openSettingsTab, permissionPending, resetFeaturePanels]);

  // The OS-level global shortcut (works even when Little Monkey isn't the
  // focused app) is registered in Rust and, on press, shows/focuses this
  // window and emits this event — see `command_palette::show_palette`.
  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | null = null;
    void paletteClient.onOpen(openCommandPalette).then((cleanup) => {
      if (disposed) cleanup();
      else unlisten = cleanup;
    }).catch(() => undefined);
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [openCommandPalette]);

  useEffect(() => {
    void refreshRoots();
    void refreshRecent();
    void refreshModels();
    void refreshOllama();
    void refreshProviders();
  }, [refreshRoots, refreshRecent, refreshModels, refreshOllama, refreshProviders]);

  useEffect(() => {
    void refreshRecipes();
    void subscribeToRecipeChanges();
  }, [refreshRecipes]);

  // The scheduler tick loop (design doc slice 3) must run in exactly one
  // place — every window shares the same `automations.json`/recipes, so
  // running it in a secondary session window too (see
  // `system::open_session_window`) would fire each due entry twice. Tauri
  // defaults the first window's label to "main" (see tauri.conf.json's
  // unlabeled window entry); secondary windows are labeled `session-<id>`.
  // `getCurrentWindow()` throws outside the Tauri shell (plain-browser dev
  // has no `window.__TAURI_INTERNALS__`) — guarded the same way every other
  // store's boot hydration already guards its own Tauri-only calls.
  useEffect(() => {
    void hydrateAutomations();
    if (isTauri() && getCurrentWindow().label === "main") {
      startScheduler();
      const stopBackupScheduler = startBackupScheduler();
      const stopSyntheticMonitoringScheduler = startSyntheticMonitoringScheduler();
      return () => {
        stopBackupScheduler();
        stopSyntheticMonitoringScheduler();
      };
    }
    return undefined;
  }, []);

  // A stop requested from Run Center (including another app window) is
  // recorded by Rust first, then bridged back to the exact active desktop
  // AbortController. The turn finalizer records Cancelling/Cancelled only
  // after the model/tool cancellation path has actually been triggered.
  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | null = null;
    void onRunCancellationRequested(({ runId }) => cancelRegisteredRun(runId)).then((cleanup) => {
      if (disposed) cleanup();
      else unlisten = cleanup;
    });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  // Durable signal intent (`process_signal`) reaches the desktop's own loops.
  // Recording the intent and delivering it are deliberately separate — that is
  // what lets a stop survive a restart and cross a process boundary — so
  // something has to read the latch. The daemon does it once per tick for its
  // jobs; this is the same read for everything the desktop owns.
  //
  // Every window subscribes, not just main: a chat turn's AbortController lives
  // in the one WebView that started it, so only that window can deliver, and a
  // miss elsewhere is a map lookup with no IPC behind it. The main window
  // additionally owns the Rust-side kinds (background shells, workflow runs),
  // which any window could reach and therefore exactly one should.
  //
  // The interval exists because `monkey processes signal` writes from a different
  // OS process and cannot emit a Tauri event, so no listener will ever hear it —
  // see `processSignalDelivery.ts` for why that is one indexed query rather than
  // per-round polling.
  useEffect(() => {
    if (!isTauri()) return undefined;
    const options = { ownsGlobalKinds: getCurrentWindow().label === "main" };
    let disposed = false;
    let unlisten: (() => void) | null = null;
    void onProcessesChanged((record) => {
      void deliverProcessSignal(record, options);
    }).then((cleanup) => {
      if (disposed) cleanup();
      else unlisten = cleanup;
    });
    void sweepPendingProcessSignals(options);
    const timer = setInterval(() => {
      void sweepPendingProcessSignals(options);
    }, PENDING_SIGNAL_SWEEP_INTERVAL_MS);
    return () => {
      disposed = true;
      unlisten?.();
      clearInterval(timer);
    };
  }, []);

  // Rebind persisted chat placeholders to the daemon's shared run ledger
  // after a WebView/app restart. Only the main window owns recovery so two
  // session windows never race to render the same durable event stream.
  useEffect(() => {
    if (isTauri() && getCurrentWindow().label === "main") {
      recoverDaemonDesktopTurns();
    }
  }, []);

  // Local App Builder (ROADMAP.md, Phase 3): a published app's static page
  // triggers a run over HTTP, but only the desktop app's own frontend loop
  // can actually execute a recipe (`recipeRunner.ts`'s `runRecipeNow`) — see
  // `local_apps.rs`'s module doc. Main-window-only, same reasoning as the
  // scheduler and daemon-recovery effects above: every window shares the
  // same local API server, so running this in a secondary window too would
  // race to handle the same run request twice.
  useEffect(() => {
    void subscribeToLocalAppsChanges();
    if (isTauri() && getCurrentWindow().label === "main") {
      void subscribeToLocalAppRunRequests();
    }
  }, []);

  // Eager connect-on-startup for MCP servers: load the configured list, then
  // kick off `mcp_connect` for every enabled-but-not-yet-connected one so
  // their tools are ready before the user's first turn, without any Rust
  // setup-hook plumbing (mirrors the design doc's "frontend triggers
  // mcp_connect_all on mount" note). Failures surface per-server via the
  // McpPanel status pill (mcp://status -> "error"), not here.
  useEffect(() => {
    void (async () => {
      await refreshMcp();
      const toConnect = useMcpStore.getState().servers.filter((s) => s.enabled && s.status !== "connected");
      await Promise.all(toConnect.map((s) => connectMcp(s.id).catch(() => {})));
    })();
  }, [refreshMcp, connectMcp]);

  // The primary root changing (attach/detach of the whole workspace, not
  // secondary folders) invalidates whatever file was being previewed.
  useEffect(() => {
    setSelectedFile(null);
  }, [rootsVersion]);

  // Clicking "Preview" on a code fence should actually reveal the pane, even
  // if the user had collapsed it — otherwise `artifactStore.open()` would
  // silently do nothing visible.
  useEffect(() => {
    if (activeArtifact) setWorkspacePanelOpen(true);
  }, [activeArtifact]);

  const handleSelectFile = useCallback((path: string, content: string) => {
    setSelectedFile({ path, original: content, current: content });
  }, []);

  const handleRefreshDiff = useCallback(async () => {
    if (!selectedFile) return;
    setDiffLoading(true);
    setDiffError(null);
    try {
      const content = await invoke<string>("tool_read_file", { path: selectedFile.path });
      setSelectedFile((prev) => (prev ? { ...prev, current: content } : prev));
    } catch (err) {
      setDiffError(formatError(err));
    } finally {
      setDiffLoading(false);
    }
  }, [selectedFile]);

  // Work Canvas "Open" action for a file-reference node: re-reads the file
  // fresh (same `tool_read_file` path `FileTree.tsx`'s own click handler
  // uses) and drops it into the same Workspace-panel preview/diff pane, so a
  // canvas node never carries a stale copy of the file — only ever a live
  // pointer back to it. Swallows a read failure into the diff pane's own
  // error slot rather than throwing, since the referenced file may have
  // since moved or been deleted.
  const handleOpenFileFromCanvas = useCallback(async (path: string) => {
    setWorkspacePanelOpen(true);
    setDiffError(null);
    try {
      const content = await invoke<string>("tool_read_file", { path });
      setSelectedFile({ path, original: content, current: content });
    } catch (err) {
      setDiffError(formatError(err));
    }
  }, []);

  // First-run onboarding (ROADMAP.md Phase 6): a Tauri-only, full-screen
  // wizard that replaces the entire shell below until it's finished or
  // explicitly skipped. Plain-browser dev (`vite` without the Tauri shell)
  // never shows it — same `isTauri()` guard every other Tauri-only boot
  // effect above already uses — so it can't block local frontend-only
  // development. Placed after every hook above so hook call order never
  // depends on this condition.
  if (isTauri() && !hasCompletedOnboarding) {
    return (
      <Suspense fallback={<LazyPanelFallback />}>
        <OnboardingWizard />
      </Suspense>
    );
  }

  return (
    <div className="flex h-screen w-screen bg-background text-foreground">
      {/* Left sidebar: chat session list, extending to the very top of the
          window (the title bar is overlaid — see tauri.conf.json). The top
          strip stays empty as a drag region and clears the macOS traffic
          lights. Workspace folder picking now lives in the WorkspaceBar
          above the chat input (see ChatWindow). */}
      <aside className="app-session-sidebar flex shrink-0 flex-col border-r border-border bg-surface">
        <div data-tauri-drag-region className="h-11 shrink-0" />
        <div className="min-h-0 flex-1 overflow-y-auto [overscroll-behavior:contain]">
          <ChatSessionList />
        </div>
        <AppMenu
          onOpenSettings={() => openFeaturePanel("settings")}
          onOpenRunCenter={() => openFeaturePanel("run-center")}
          onOpenGlobalSearch={() => openFeaturePanel("global-search")}
          onOpenBrowserWorkbench={() => openFeaturePanel("browser-workbench")}
          onOpenCommandPalette={openCommandPalette}
          onOpenIssueToPr={() => openFeaturePanel("issue-to-pr")}
          onOpenDesignToApp={() => openFeaturePanel("design-to-app")}
          onOpenProductionDebugging={() => openFeaturePanel("production-debugging")}
          onOpenIncidentCommander={() => openFeaturePanel("incident-commander")}
          onOpenSecurityAutofix={() => openFeaturePanel("security-autofix")}
          onOpenTrustScorecards={() => openFeaturePanel("trust-scorecards")}
          onOpenSopCompiler={() => openFeaturePanel("sop-compiler")}
          onOpenMcpGenerator={() => openFeaturePanel("mcp-generator")}
          onOpenConnectorBuilder={() => openFeaturePanel("connector-builder")}
          onOpenMigrationAgent={() => openFeaturePanel("migration-agent")}
          onOpenSideTasks={() => useSideTaskStore.getState().openPane()}
          onOpenBackgroundTasks={openBackgroundTasksPanel}
          onOpenAgentInbox={() => openFeaturePanel("agent-inbox")}
          onOpenKnowledgeGraph={() => openFeaturePanel("knowledge-graph")}
          onOpenSpreadsheetCopilot={() => openFeaturePanel("spreadsheet-copilot")}
          onOpenTerminal={() => setTerminalOpen(true)}
          onOpenRedTeamLab={() => openFeaturePanel("red-team-lab")}
          onOpenEvidenceBoard={() => openFeaturePanel("evidence-board")}
          onOpenDebate={() => openFeaturePanel("debate")}
          onOpenDbAdminGuardrails={() => openFeaturePanel("db-admin-guardrails")}
          onRestartOnboarding={() => {
            resetFeaturePanels();
            setSettingsInitialTab(undefined);
            setTerminalOpen(false);
            restartOnboarding();
          }}
          onOpenDailyBrief={() => openFeaturePanel("daily-brief")}
          onOpenApiContractDiffLab={() => openFeaturePanel("api-contract-diff-lab")}
          onOpenGoldenDatasetBuilder={() => openFeaturePanel("golden-dataset-builder")}
          onOpenDataNotebook={() => openFeaturePanel("data-notebook")}
          onOpenSyntheticMonitoring={() => openFeaturePanel("synthetic-monitoring")}
          onOpenCrossRepoIntelligence={() => openFeaturePanel("cross-repo-intelligence")}
          onOpenWorkCanvas={() => openFeaturePanel("work-canvas")}
          onOpenPmCopilot={() => openFeaturePanel("pm-copilot")}
          onOpenDeepResearch={() => openFeaturePanel("deep-research")}
          onOpenBriefStudio={() => openFeaturePanel("brief-studio")}
          onOpenCrossRepoChangePlanner={() => openFeaturePanel("cross-repo-planner")}
          onOpenVisualEditMode={() => openFeaturePanel("visual-edit-mode")}
          onOpenWorkflowTestHarness={() => openFeaturePanel("workflow-test-harness")}
        />
      </aside>

      {/* Center: chat, with a drag-region strip standing in for the title
          bar. The dock-toggle icons used to live inline here too, but that
          put them inside this flex-1 column — every time the right region's
          width animated (open/close/resize/fullscreen), the column reflowed
          and the right-aligned icons visibly slid with it. They're fixed to
          the viewport's top-right corner instead (below), so their on-screen
          position never moves regardless of what the sidebar is doing. The
          Compare/Crew/Knowledge portal target stays right here, in-flow —
          it's unaffected by the sidebar and belongs at the strip's left
          edge, right of the session sidebar. */}
      <div className="flex min-h-0 min-w-0 flex-1 flex-col">
        <div data-tauri-drag-region className="flex h-11 shrink-0 items-center px-2">
          <div ref={setChatHeaderActionsEl} className="flex items-center gap-1.5" />
        </div>
        <SessionGrantBanner />
        {/* Per-pane boundary so one pane crashing doesn't take down the other
            (or the sidebar/workspace). `resetKey` clears a shown error on
            session switch — the replacement session gets a fresh render. */}
        <ErrorBoundary
          resetKey={
            browserWorkbenchOpen
              ? `browser-${activeSessionId}`
              : activeFeaturePanel && activeFeaturePanel !== "settings"
                ? activeFeaturePanel
                : activeComparisonId ?? activeCrewSessionId ?? activeSessionId
          }
        >
          <Suspense fallback={<LazyPanelFallback />}>
            {globalSearchOpen ? (
              <GlobalSearch
                onClose={() => closeFeaturePanel("global-search")}
                onOpenRun={(runId) => {
                  openFeaturePanel("run-center");
                  void useRunStore.getState().selectRun(runId);
                }}
              />
            ) : agentInboxOpen ? (
              <AgentInbox
                onClose={() => closeFeaturePanel("agent-inbox")}
                onOpenRunCenter={(runId) => {
                  openFeaturePanel("run-center");
                  void useRunStore.getState().selectRun(runId);
                }}
              />
            ) : redTeamLabOpen ? (
              <RedTeamLabPanel onClose={() => closeFeaturePanel("red-team-lab")} />
            ) : knowledgeGraphOpen ? (
              <KnowledgeGraphExplorerPanel onClose={() => closeFeaturePanel("knowledge-graph")} />
            ) : spreadsheetCopilotOpen ? (
              <SpreadsheetCopilotPanel onClose={() => closeFeaturePanel("spreadsheet-copilot")} />
            ) : evidenceBoardOpen ? (
              <EvidenceBoardPanel sessionId={activeSessionId} onClose={() => closeFeaturePanel("evidence-board")} />
            ) : goldenDatasetBuilderOpen ? (
              <GoldenDatasetBuilderPanel onClose={() => closeFeaturePanel("golden-dataset-builder")} />
            ) : dailyBriefOpen ? (
              <DailyBriefPanel
                onClose={() => closeFeaturePanel("daily-brief")}
                onOpenRunCenter={(runId) => {
                  openFeaturePanel("run-center");
                  void useRunStore.getState().selectRun(runId);
                }}
                onOpenAgentInbox={() => openFeaturePanel("agent-inbox")}
                onOpenSettingsTab={openSettingsTab}
              />
            ) : dataNotebookOpen ? (
              <DataNotebookPanel onClose={() => closeFeaturePanel("data-notebook")} />
            ) : syntheticMonitoringOpen ? (
              <SyntheticMonitoringPanel onClose={() => closeFeaturePanel("synthetic-monitoring")} />
            ) : workCanvasOpen ? (
              <WorkCanvasPanel
                onClose={() => closeFeaturePanel("work-canvas")}
                onOpenSession={(sessionId) => {
                  closeFeaturePanel("work-canvas");
                  switchSession(sessionId);
                }}
                onOpenRun={(runId) => {
                  openFeaturePanel("run-center");
                  void useRunStore.getState().selectRun(runId);
                }}
                onOpenFile={(path) => void handleOpenFileFromCanvas(path)}
              />
            ) : pmCopilotOpen ? (
              <PmCopilotPanel onClose={() => closeFeaturePanel("pm-copilot")} />
            ) : deepResearchOpen ? (
              <DeepResearchWorkspacePanel onClose={() => closeFeaturePanel("deep-research")} />
            ) : briefStudioOpen ? (
              <BriefStudioPanel onClose={() => closeFeaturePanel("brief-studio")} />
            ) : crossRepoPlannerOpen ? (
              <CrossRepoChangePlannerPanel onClose={() => closeFeaturePanel("cross-repo-planner")} />
            ) : crossRepoIntelligenceOpen ? (
              <CrossRepoIntelligencePanel onClose={() => closeFeaturePanel("cross-repo-intelligence")} />
            ) : visualEditModeOpen ? (
              <VisualEditModePanel onClose={() => closeFeaturePanel("visual-edit-mode")} />
            ) : designToAppOpen ? (
              <DesignToAppPanel
                onClose={() => closeFeaturePanel("design-to-app")}
                onOpenRunCapsule={(runId) => {
                  openFeaturePanel("run-center");
                  void useRunStore.getState().selectRun(runId);
                }}
              />
            ) : runCenterOpen ? (
              <RunCenter onClose={() => closeFeaturePanel("run-center")} />
            ) : debateOpen ? (
              <DebatePanel onClose={() => closeFeaturePanel("debate")} />
            ) : dbAdminGuardrailsOpen ? (
              <DatabaseAdminGuardrailsPanel onClose={() => closeFeaturePanel("db-admin-guardrails")} />
            ) : issueToPrOpen ? (
              <IssueToPrPanel
                onClose={() => closeFeaturePanel("issue-to-pr")}
                onOpenRunCapsule={(runId) => {
                  openFeaturePanel("run-center");
                  void useRunStore.getState().selectRun(runId);
                }}
              />
            ) : productionDebuggingOpen ? (
              <ProductionDebuggingPanel
                onClose={() => closeFeaturePanel("production-debugging")}
                onOpenRunCapsule={(runId) => {
                  openFeaturePanel("run-center");
                  void useRunStore.getState().selectRun(runId);
                }}
              />
            ) : incidentCommanderOpen ? (
              <IncidentCommanderPanel onClose={() => closeFeaturePanel("incident-commander")} />
            ) : securityAutofixOpen ? (
              <SecurityAutofixPanel
                onClose={() => closeFeaturePanel("security-autofix")}
                onOpenRunCapsule={(runId) => {
                  openFeaturePanel("run-center");
                  void useRunStore.getState().selectRun(runId);
                }}
              />
            ) : trustScorecardsOpen ? (
              <TrustScorecardsPanel onClose={() => closeFeaturePanel("trust-scorecards")} />
            ) : sopCompilerOpen ? (
              <SopCompilerPanel
                onClose={() => closeFeaturePanel("sop-compiler")}
                onOpenSkillProposals={() => openSettingsTab("prompts")}
              />
            ) : mcpGeneratorOpen ? (
              <McpGeneratorPanel onClose={() => closeFeaturePanel("mcp-generator")} />
            ) : connectorBuilderOpen ? (
              <ConnectorBuilderPanel onClose={() => closeFeaturePanel("connector-builder")} />
            ) : migrationAgentOpen ? (
              <MigrationAgentPanel
                onClose={() => closeFeaturePanel("migration-agent")}
                onOpenRunCapsule={(runId) => {
                  openFeaturePanel("run-center");
                  void useRunStore.getState().selectRun(runId);
                }}
              />
            ) : apiContractDiffLabOpen ? (
              <ApiContractDiffLabPanel onClose={() => closeFeaturePanel("api-contract-diff-lab")} />
            ) : workflowTestHarnessOpen ? (
              <EvalHarnessPanel onClose={() => closeFeaturePanel("workflow-test-harness")} />
            ) : browserWorkbenchOpen ? (
              <BrowserWorkbench
                key={activeSessionId}
                taskId={activeSessionId}
                chatSessionId={activeSessionId}
                onClose={() => closeFeaturePanel("browser-workbench")}
              />
            ) : activeComparisonId ? (
              <CompareView groupId={activeComparisonId} />
            ) : activeCrewSessionId ? (
              <CrewView sessionId={activeCrewSessionId} />
            ) : (
              <ChatWindow
                sessionId={activeSessionId}
                onManagePrompts={handleManagePrompts}
                onOpenSettingsTab={openSettingsTab}
                headerActionsSlot={chatHeaderActionsEl}
                onOpenBackgroundTasks={openBackgroundTasksPanel}
              />
            )}
          </Suspense>
        </ErrorBoundary>
        {terminalOpen && terminalDock === "bottom" && (
          <Suspense fallback={null}>
            <TerminalPanel chatSessionId={activeSessionId} onClose={() => setTerminalOpen(false)} />
          </Suspense>
        )}
      </div>


      {/* Split pane: a second, fully independent chat opened via the session
          menu's "Open in > Split view" — Claude-Desktop-style, inside the
          same window. Its top strip doubles as the pane header: session
          title + close, still draggable like the other title-bar strips. */}
      {(activeFeaturePanel === null || activeFeaturePanel === "settings") && activeComparisonId === null && activeCrewSessionId === null && splitSessionId !== null && (
        <div className="flex min-h-0 min-w-0 flex-1 flex-col border-l border-border">
          <div data-tauri-drag-region className="flex h-11 shrink-0 items-center justify-between gap-2 border-b border-border px-3">
            <span className="pointer-events-none min-w-0 truncate text-sm font-medium text-foreground">
              {splitTitle}
            </span>
            <IconButton size="sm" onClick={closeSplit} aria-label={t("App.closeSplitPane")}>
              <X size={16} />
            </IconButton>
          </div>
          <ErrorBoundary resetKey={splitSessionId}>
            <Suspense fallback={<LazyPanelFallback />}>
              {splitCrewSessionId ? (
                <CrewView sessionId={splitCrewSessionId} />
              ) : (
                <ChatWindow
                  sessionId={splitSessionId}
                  onManagePrompts={handleManagePrompts}
                  onOpenSettingsTab={openSettingsTab}
                  onOpenBackgroundTasks={openBackgroundTasksPanel}
                />
              )}
            </Suspense>
          </ErrorBoundary>
        </div>
      )}

      {/* Right region: hidden by default, and the ONLY home for every
          secondary surface — review, diff, terminal, browser, side tasks,
          files, background tasks are all real TABS of this one sidebar.
          Several can be open at once, one active, the rest kept
          mounted-but-hidden so their state (a running terminal, a loaded
          review, a browser page, a side task mid-run) survives switching —
          choosing one never closes another. A shared, drag-resizable width
          animates open and closed; a fullscreen toggle covers the whole
          region. Nothing here takes over its own column beside the chat. */}
      <Suspense fallback={null}>
        <aside
          className={`relative flex h-full flex-col overflow-hidden border-border bg-surface ${
            rightOpen
              ? rightFullscreen
                ? "fixed inset-x-0 bottom-0 top-11 z-40 w-full border"
                : "shrink-0 border-l transition-[width] duration-200 ease-out"
              : "w-0 shrink-0 border-l-0 transition-[width] duration-200 ease-out"
          }`}
          style={rightOpen && !rightFullscreen ? { width: rightMenuWidth } : undefined}
        >
          {rightOpen && !rightFullscreen && (
            <div
              role="separator"
              aria-orientation="vertical"
              onPointerDown={onMenuResizeStart}
              className="absolute inset-y-0 left-0 z-20 w-1.5 cursor-ew-resize bg-transparent transition-colors hover:bg-accent/40 active:bg-accent/60"
            />
          )}

          {rightTabs.length === 0 ? (
            <div className="flex flex-1 flex-col items-center justify-center gap-1 p-3">
              <div className="flex w-full max-w-[272px] flex-col gap-1">
                {RIGHT_TAB_KINDS.map((kind) => (
                  <button
                    key={kind}
                    type="button"
                    disabled={
                      (kind === "terminal" || kind === "review" || kind === "diff") &&
                      !primaryRoot(useWorkspaceStore.getState().roots)
                    }
                    onClick={() => openRightTab(kind)}
                    className="flex items-center gap-2.5 rounded-lg px-3 py-2.5 text-left text-sm text-foreground hover:bg-surface-2 focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-accent disabled:opacity-50"
                  >
                    <RightTabIcon kind={kind} size={16} />
                    <span className="min-w-0 flex-1 truncate">{t(RIGHT_TAB_LABEL_KEYS[kind])}</span>
                    {RIGHT_TAB_SHORTCUT_IDS[kind] && (
                      <kbd className="ml-3 shrink-0 rounded-md bg-surface-2 px-1.5 py-0.5 font-mono text-[11px] text-faint">
                        {shortcutLabel(RIGHT_TAB_SHORTCUT_IDS[kind]!)}
                      </kbd>
                    )}
                  </button>
                ))}
              </div>
            </div>
          ) : (
            <>
              {/* Tab strip: one chip per open tab, "+" opens the rest. The
                  chips row scrolls horizontally in its own inner container —
                  the "+" dropdown must live OUTSIDE that scroll container,
                  because `overflow-x-auto` also clips absolutely-positioned
                  children to the 44px strip. The scroll viewport also stops
                  short of the fixed dock-toggle cluster (margin, not padding
                  — padding would still let mid-scroll chips slide under the
                  transparent icons), except in fullscreen where the panel
                  sits below the cluster and the full width is usable. */}
              <div className="relative shrink-0 border-b border-border">
                <div
                  ref={rightTabStripRef}
                  data-tauri-drag-region
                  className="flex h-11 items-center gap-1 overflow-x-auto px-3 [scrollbar-width:thin]"
                  style={rightFullscreen ? undefined : { marginRight: dockReserve }}
                >
                  {rightTabs.map((kind) => (
                    <div
                      key={kind}
                      data-right-tab={kind}
                      className={`group inline-flex max-w-44 shrink-0 items-center rounded-lg text-sm transition-colors ${
                        kind === activeRightTab ? "bg-surface-2 text-foreground" : "text-muted hover:bg-surface-2 hover:text-foreground"
                      }`}
                    >
                      <button
                        type="button"
                        onClick={() => setActiveRightTab(kind)}
                        className="inline-flex min-w-0 items-center gap-1.5 py-1.5 pl-2.5 focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-accent"
                      >
                        <RightTabIcon kind={kind} size={15} />
                        <span className="truncate">{t(RIGHT_TAB_LABEL_KEYS[kind])}</span>
                      </button>
                      <button
                        type="button"
                        aria-label={t("App.closeRightTab")}
                        onClick={() => closeRightTab(kind)}
                        className="ml-0.5 mr-1 rounded-sm p-0.5 text-faint opacity-0 hover:text-danger focus-visible:opacity-100 focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-accent group-hover:opacity-100"
                      >
                        <X size={11} />
                      </button>
                    </div>
                  ))}
                  {RIGHT_TAB_KINDS.some((kind) => !rightTabs.includes(kind)) && (
                    <IconButton
                      size="sm"
                      variant="ghost"
                      onClick={() => setPlusMenuOpen((open) => !open)}
                      aria-label={t("App.openRightSidebar")}
                    >
                      <Plus size={15} />
                    </IconButton>
                  )}
                </div>
                {plusMenuOpen && (
                  <>
                    <div className="fixed inset-0 z-30" onClick={() => setPlusMenuOpen(false)} />
                    <div className="absolute left-3 top-full z-40 mt-1 w-64 rounded-xl border border-border bg-background p-1.5 shadow-xl">
                      {RIGHT_TAB_KINDS.filter((kind) => !rightTabs.includes(kind)).map((kind) => (
                        <button
                          key={kind}
                          type="button"
                          disabled={
                            (kind === "terminal" || kind === "review" || kind === "diff") &&
                            !primaryRoot(useWorkspaceStore.getState().roots)
                          }
                          onClick={() => openRightTab(kind)}
                          className="flex w-full items-center gap-2.5 rounded-lg px-2.5 py-2 text-left text-sm text-foreground hover:bg-surface-2 disabled:opacity-50"
                        >
                          <RightTabIcon kind={kind} size={15} />
                          <span className="min-w-0 flex-1 truncate">{t(RIGHT_TAB_LABEL_KEYS[kind])}</span>
                          {RIGHT_TAB_SHORTCUT_IDS[kind] && (
                            <kbd className="shrink-0 font-mono text-[11px] text-faint">
                              {shortcutLabel(RIGHT_TAB_SHORTCUT_IDS[kind]!)}
                            </kbd>
                          )}
                        </button>
                      ))}
                    </div>
                  </>
                )}
              </div>

              <div className="relative min-h-0 flex-1">
                {rightTabs.map((kind) => (
                  <div key={kind} className={`absolute inset-0 ${kind === activeRightTab ? "flex flex-col" : "hidden"}`}>
                    {kind === "review" ? (
                      <ReviewPanel onClose={() => closeRightTab("review")} />
                    ) : kind === "diff" ? (
                      <DiffPanel onClose={() => closeRightTab("diff")} />
                    ) : kind === "sideTasks" ? (
                      // Side tasks are CONVERSATIONS, so this tab hosts the
                      // whole pane — its own task tab strip and its composer —
                      // rather than a status list. The pane never needs a
                      // trailing inset here: the sidebar's own strip already
                      // reserves the dock cluster's footprint above it.
                      <SideTaskPane sessionId={activeSessionId} onClose={() => closeRightTab("sideTasks")} />
                    ) : kind === "terminal" ? (
                      <TerminalPanel
                        chatSessionId={activeSessionId}
                        onClose={() => closeRightTab("terminal")}
                        embedded
                        hideFullscreenButton
                      />
                    ) : kind === "browser" ? (
                      // The real in-app browser — same pane the dock's globe
                      // toggle opens, sharing one tab list via the store. The
                      // native webview must stay hidden while this tab is not
                      // the active one, or it would paint over its neighbors.
                      <BrowserPane
                        embedded
                        obscured={
                          browserPaneObscured ||
                          !rightOpen ||
                          activeRightTab !== "browser"
                        }
                        onClose={() => closeRightTab("browser")}
                      />
                    ) : kind === "backgroundTasks" ? (
                      <BackgroundTasksPanel sessionId={activeSessionId} onClose={() => closeRightTab("backgroundTasks")} />
                    ) : (
                      <div className={`flex h-full flex-col ${workspacePanelOpen ? "w-full" : "w-12"}`}>
                        <div className="flex h-9 shrink-0 items-center justify-between gap-1 border-b border-border px-3">
                          {workspacePanelOpen && (
                            <span className="text-[11px] font-semibold uppercase tracking-wider text-faint">
                              {t("App.workspacePanelTitle")}
                            </span>
                          )}
                          <IconButton
                            size="sm"
                            onClick={() => closeRightTab("files")}
                            aria-label={t("App.closeRightTab")}
                            title={t("App.closeRightTab")}
                            className="ml-auto"
                          >
                            <X size={16} />
                          </IconButton>
                        </div>

                        {workspacePanelOpen && (activeArtifact ? (
                          <ArtifactPane />
                        ) : (
                          <div className="flex min-h-0 flex-1 flex-col">
                            <div className="min-h-0 flex-[3] border-b border-border">
                              <FileTree key={rootsVersion} onSelectFile={handleSelectFile} />
                            </div>
                            <div className="flex min-h-0 flex-[2] flex-col">
                              <div className="flex h-9 shrink-0 items-center justify-between border-b border-border px-3">
                                <span className="text-[11px] font-semibold uppercase tracking-wider text-faint">
                                  {t("App.diffPanelTitle")}
                                </span>
                                <Button
                                  variant="ghost"
                                  size="sm"
                                  onClick={() => void handleRefreshDiff()}
                                  disabled={!selectedFile || diffLoading}
                                >
                                  {diffLoading ? t("App.diffRefreshing") : t("App.diffRefresh")}
                                </Button>
                              </div>
                              {diffError && <p className="px-3 pt-1.5 text-xs text-danger">{diffError}</p>}
                              <div className="min-h-0 flex-1 overflow-auto p-2 [overscroll-behavior:contain]">
                                {selectedFile ? (
                                  <DiffViewer
                                    fileName={selectedFile.path}
                                    oldValue={selectedFile.original}
                                    newValue={selectedFile.current}
                                    oldTitle={t("App.diffOldTitleOpened")}
                                    newTitle={t("App.diffNewTitleCurrent")}
                                  />
                                ) : (
                                  <p className="p-3 text-sm text-faint">
                                    {t("App.diffEmptyStateHint")}
                                  </p>
                                )}
                              </div>
                            </div>
                          </div>
                        ))}
                      </div>
                    )}
                  </div>
                ))}
              </div>
            </>
          )}
        </aside>
      </Suspense>

      {/* Dock-toggle icons: fixed to the viewport's top-right corner (z-50,
          above the right sidebar's own z-40 fullscreen and the terminal's
          z-40 fullscreen) rather than laid out inside the shrinking chat
          column — their screen position must stay put as the sidebar
          animates open/closed or resizes, and they must stay reachable even
          while a right-region panel is fullscreen. */}
      <div ref={setDockEl} className="fixed right-3 top-2 z-50 flex items-center gap-1.5">
        {/* Every toggle below targets a TAB of the one right sidebar: it
            brings that tab forward, or closes it when it is already the one
            showing. None of them opens a column of its own any more. */}
        <IconButton
          size="sm"
          variant={rightTabShowing("diff") ? "active" : "ghost"}
          className="relative"
          onClick={() => toggleRightTab("diff")}
          disabled={!primaryRoot(useWorkspaceStore.getState().roots)}
          aria-label={rightTabShowing("diff") ? t("App.closeDiff") : t("App.openDiff")}
          title={rightTabShowing("diff") ? t("App.closeDiff") : t("App.openDiff")}
        >
          <FileDiff size={15} />
          {changedFileCount > 0 && (
            <span className="absolute -right-0.5 -top-0.5 flex h-3.5 min-w-3.5 items-center justify-center rounded-full bg-accent px-0.5 text-[9px] font-semibold leading-none text-accent-foreground">
              {changedFileCount > 9 ? "9+" : changedFileCount}
            </span>
          )}
        </IconButton>
        <IconButton
          size="sm"
          variant={rightTabShowing("terminal") ? "active" : "ghost"}
          // Always the sidebar tab — `openRightTab` re-docks a bottom-docked
          // terminal to the right on the way. The bottom dock is still
          // reachable from the terminal panel's own dock button.
          onClick={() => toggleRightTab("terminal")}
          disabled={!primaryRoot(useWorkspaceStore.getState().roots)}
          aria-label={rightTabShowing("terminal") ? t("App.closeTerminal") : t("App.openTerminal")}
          title={rightTabShowing("terminal") ? t("App.closeTerminal") : t("App.openTerminal")}
        >
          <SquareTerminal size={15} />
        </IconButton>
        <IconButton
          size="sm"
          variant={rightTabShowing("browser") ? "active" : "ghost"}
          onClick={() => toggleRightTab("browser")}
          aria-label={rightTabShowing("browser") ? t("App.closeBrowser") : t("App.openBrowser")}
          title={rightTabShowing("browser") ? t("App.closeBrowser") : t("App.openBrowser")}
        >
          <Globe2 size={15} />
        </IconButton>
        <IconButton
          size="sm"
          variant={rightTabShowing("sideTasks") ? "active" : "ghost"}
          className="relative"
          onClick={() => toggleRightTab("sideTasks")}
          aria-label={rightTabShowing("sideTasks") ? t("App.closeSideTaskPane") : t("App.openSideTaskPane")}
          title={rightTabShowing("sideTasks") ? t("App.closeSideTaskPane") : t("App.openSideTaskPane")}
        >
          <Columns2 size={15} />
          {runningSideTaskCount > 0 && (
            <span className="absolute -right-0.5 -top-0.5 flex h-3.5 min-w-3.5 items-center justify-center rounded-full bg-accent px-0.5 text-[9px] font-semibold leading-none text-accent-foreground">
              {runningSideTaskCount > 9 ? "9+" : runningSideTaskCount}
            </span>
          )}
        </IconButton>
        <IconButton
          size="sm"
          variant={rightTabShowing("backgroundTasks") ? "active" : "ghost"}
          className="relative"
          onClick={() => toggleRightTab("backgroundTasks")}
          aria-label={rightTabShowing("backgroundTasks") ? t("App.closeTasks") : t("App.openTasks")}
          title={rightTabShowing("backgroundTasks") ? t("App.closeTasks") : t("App.openTasks")}
        >
          <ListTodo size={15} />
          {runningBackgroundTaskCount > 0 && (
            <span className="absolute -right-0.5 -top-0.5 flex h-3.5 min-w-3.5 items-center justify-center rounded-full bg-accent px-0.5 text-[9px] font-semibold leading-none text-accent-foreground">
              {runningBackgroundTaskCount > 9 ? "9+" : runningBackgroundTaskCount}
            </span>
          )}
        </IconButton>
        {/* Only present while the right region has something to fullscreen
            — mirrors the reference layout, where this button appears
            alongside the other dock toggles the moment the side panel
            opens, rather than occupying a slot all the time. */}
        {rightOpen && (
          <IconButton
            size="sm"
            variant="ghost"
            onClick={() => setRightFullscreen((value) => !value)}
            aria-label={rightFullscreen ? t("App.collapseRightSidebar") : t("App.expandRightSidebar")}
            title={rightFullscreen ? t("App.collapseRightSidebar") : t("App.expandRightSidebar")}
          >
            {rightFullscreen ? <Minimize2 size={15} /> : <Maximize2 size={15} />}
          </IconButton>
        )}
        <IconButton
          size="sm"
          variant={rightOpen ? "active" : "ghost"}
          onClick={() => setRightOpen((open) => !open)}
          aria-label={rightOpen ? t("App.closeRightSidebar") : t("App.openRightSidebar")}
          title={rightOpen ? t("App.closeRightSidebar") : t("App.openRightSidebar")}
        >
          <PanelRight size={15} />
        </IconButton>
      </div>

      {commandPaletteOpen && (
        <Suspense fallback={null}>
          <CommandPalette
            onClose={() => setCommandPaletteOpen(false)}
            onOpenSettingsTab={openSettingsTab}
          />
        </Suspense>
      )}
      {permissionPending && (
        <Suspense fallback={null}>
          <PermissionModal />
        </Suspense>
      )}
      {approvalChainPending && (
        <Suspense fallback={null}>
          <ApprovalChainModal />
        </Suspense>
      )}
      <PrivacyFirewallGate />
      {(settingsOpen || settingsMounted) && (
        <Suspense fallback={null}>
          <SettingsModal
            open={settingsOpen}
            onClose={() => {
              closeFeaturePanel("settings");
              // Consume the deep-link tab on close so it only affects the
              // opening it was requested for — otherwise it would stick around
              // and force every later normal open (e.g. the plain gear icon)
              // back onto that tab instead of "whatever was last active".
              setSettingsInitialTab(undefined);
            }}
            initialTab={settingsInitialTab}
            initialTabRequest={settingsTabRequest}
          />
        </Suspense>
      )}
    </div>
  );
}

export default App;
