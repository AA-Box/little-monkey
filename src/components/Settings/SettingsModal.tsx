import { Suspense, lazy, useEffect, useRef, useState } from "react";
import {
  AppWindow,
  BarChart3,
  BookOpen,
  Bot,
  Blocks,
  Boxes,
  Brain,
  Cloud,
  Cpu,
  FlaskConical,
  Keyboard,
  HardDrive,
  Gauge,
  GitPullRequest,
  Inbox,
  ListChecks,
  ListOrdered,
  Lock,
  MessageSquare,
  MessagesSquare,
  Network,
  PhoneCall,
  MonitorCheck,
  MousePointerClick,
  Palette,
  PlugZap,
  ScrollText,
  Search,
  Server,
  ShieldCheck,
  Sparkles,
  Stethoscope,
  Terminal,
  RefreshCw,
  Users,
  UsersRound,
  X,
  Zap,
  type LucideIcon,
} from "lucide-react";
import { IconButton } from "../ui";
import { useModelStore } from "../../store/modelStore";
import { ProviderCard } from "./ProviderCard";
import { AddCustomProviderForm } from "./AddCustomProviderForm";
/** The largest panel in this modal by a wide margin, and one behind a tab most
 * sessions never open. Loading it with the rest of Settings put the whole of it
 * in the modal's chunk, which is what finally pushed that chunk past its
 * budget. Split out here rather than by raising the budget: the budget is doing
 * its job. */
/** Shown while a lazily-loaded panel's chunk arrives. Mirrors the app's own
 * lazy-panel fallback rather than inventing a second spinner. */
function PanelFallback() {
  return (
    <div className="flex min-h-32 w-full items-center justify-center" aria-busy="true">
      <div className="h-5 w-5 animate-spin rounded-full border-2 border-border border-t-accent" />
    </div>
  );
}

const AutomationPanel = lazy(() =>
  import("./AutomationPanel").then((module) => ({ default: module.AutomationPanel })),
);

import { ProviderModelsPanel } from "./OpenRouterModelsPanel";
import {
  connectedProviderNavigationItems,
  isOllamaConfigured,
  providerIdFromSettingsTab,
  type ProviderSettingsTab,
} from "./providerSettingsNavigation";
import { RulesMemoryPanel } from "./RulesMemoryPanel";
import { MemoryStudioPanel } from "./MemoryStudioPanel";
import { ConnectorsPanel } from "./ConnectorsPanel";
import { PromptLibraryPanel } from "./PromptLibraryPanel";
import { ApiServerPanel } from "./ApiServerPanel";
import { KnowledgePanel } from "./KnowledgePanel";
import { KeyboardShortcutsPanel } from "./KeyboardShortcutsPanel";
import { ScheduledTasksPanel } from "./ScheduledTasksPanel";
import { UsagePanel } from "./UsagePanel";
import { PortabilityPanel } from "./PortabilityPanel";
import { EcosystemPanel } from "./EcosystemPanel";
import { ExecutableExtensionsPanel } from "./ExecutableExtensionsPanel";
import { RuntimeHubPanel } from "./RuntimeHubPanel";
import { BrowserVerificationPanel } from "./BrowserVerificationPanel";
import { BackgroundAgentsPanel } from "./BackgroundAgentsPanel";
import { ChannelsPanel } from "./ChannelsPanel";
import { PeersPanel } from "./PeersPanel";
import { TelephonyPanel } from "./TelephonyPanel";
import { ResourceLedgerPanel } from "./ResourceLedgerPanel";
import { GitDeliveryPanel } from "./GitDeliveryPanel";
import { TriagePanel } from "../Triage/TriagePanel";
import { ApprovalChainsPanel } from "./ApprovalChainsPanel";
import { LocalAppsPanel } from "./LocalAppsPanel";
import { CompanionPanel } from "./CompanionPanel";
import { SecurityDoctorPanel } from "./SecurityDoctorPanel";
import { UpdatesPanel } from "./UpdatesPanel";
import { PrivacyFirewallPanel } from "./PrivacyFirewallPanel";
import { DesktopControlPanel } from "./DesktopControlPanel";
import { DiagnosticsPanel } from "./DiagnosticsPanel";
import { ExecutionTargetsPanel } from "./ExecutionTargetsPanel";
import { AppearancePanel } from "./AppearancePanel";
import { ProfilesPanel } from "./ProfilesPanel";
import { TeamModePanel } from "./TeamModePanel";
import { CompareLabPanel } from "./CompareLabPanel";
import { ModelManager } from "../Models";
import { OllamaPanel } from "../Ollama";
import { useT } from "../../lib/i18n";
import { useRuntimeHubStore } from "../../store/runtimeHubStore";

interface SettingsModalProps {
  open: boolean;
  onClose: () => void;
  /** Tab to select the moment the modal transitions from closed to open —
   * the deep-link hook `PersonaSelector`'s "Manage prompts…" row (and
   * anything else that wants to jump straight to a tab) uses, via App.tsx.
   * Left unset for the normal open, which starts on Appearance. */
  initialTab?: SettingsTab;
  /** Changes for every deep-link request, including repeated requests for
   * the same tab while Settings is already open. */
  initialTabRequest?: number;
}

type StaticSettingsTab = "local" | "ollama" | "providers" | "automation" | "rules" | "memorystudio" | "connectors" | "prompts" | "apiserver" | "knowledge" | "shortcuts" | "usage" | "tasks" | "portability" | "extensions" | "ecosystem" | "runtimehub" | "browser" | "gitdelivery" | "triage" | "background" | "channels" | "telephony" | "peers" | "companion" | "security" | "privacy" | "diagnostics" | "appearance" | "desktopcontrol" | "team" | "profiles" | "approvalchains" | "localapps" | "comparelab" | "resources" | "updates" | "execution";
export type SettingsTab = StaticSettingsTab | ProviderSettingsTab;

const ICONS: Record<StaticSettingsTab, LucideIcon> = {
  local: Cpu,
  ollama: Server,
  providers: Cloud,
  knowledge: BookOpen,
  automation: Zap,
  rules: ScrollText,
  memorystudio: Brain,
  connectors: PlugZap,
  prompts: MessageSquare,
  apiserver: Terminal,
  shortcuts: Keyboard,
  usage: BarChart3,
  tasks: ListChecks,
  portability: HardDrive,
  extensions: Blocks,
  ecosystem: Boxes,
  runtimehub: Gauge,
  browser: MonitorCheck,
  gitdelivery: GitPullRequest,
  background: Bot,
  channels: MessagesSquare,
  peers: Network,
  telephony: PhoneCall,
  companion: Sparkles,
  security: ShieldCheck,
  privacy: Lock,
  diagnostics: Stethoscope,
  appearance: Palette,
  triage: Inbox,
  approvalchains: ListOrdered,
  localapps: AppWindow,
  desktopcontrol: MousePointerClick,
  team: Users,
  profiles: UsersRound,
  comparelab: FlaskConical,
  resources: Gauge,
  updates: RefreshCw,
  execution: Server,
};

const GROUPS: { labelKey: string; ids: StaticSettingsTab[] }[] = [
  { labelKey: "SettingsModal.groupApplication", ids: ["appearance", "updates", "security", "privacy", "diagnostics", "approvalchains", "profiles", "team", "companion", "desktopcontrol", "shortcuts", "usage", "resources", "portability"] },
  { labelKey: "SettingsModal.groupModels", ids: ["runtimehub", "local", "ollama", "providers", "comparelab"] },
  { labelKey: "SettingsModal.groupWorkspace", ids: ["knowledge", "automation", "rules", "memorystudio", "tasks", "localapps"] },
  { labelKey: "SettingsModal.groupIntegrations", ids: ["extensions", "ecosystem", "browser", "gitdelivery", "triage", "background", "execution", "connectors", "channels", "telephony", "peers", "prompts", "apiserver"] },
];

const LABEL_KEYS: Record<StaticSettingsTab, string> = {
  local: "SettingsModal.tabLocalModels",
  ollama: "SettingsModal.tabOllama",
  providers: "SettingsModal.tabAiProviders",
  knowledge: "SettingsModal.tabKnowledge",
  automation: "SettingsModal.tabAutomation",
  rules: "SettingsModal.tabRules",
  memorystudio: "SettingsModal.tabMemoryStudio",
  connectors: "SettingsModal.tabConnectors",
  prompts: "SettingsModal.tabPrompts",
  apiserver: "SettingsModal.tabApiServer",
  shortcuts: "SettingsModal.tabKeyboardShortcuts",
  usage: "SettingsModal.tabUsage",
  tasks: "SettingsModal.tabTasks",
  portability: "SettingsModal.tabPortability",
  extensions: "SettingsModal.tabExecutableExtensions",
  ecosystem: "SettingsModal.tabEcosystem",
  runtimehub: "SettingsModal.tabRuntimeHub",
  browser: "SettingsModal.tabBrowserVerification",
  gitdelivery: "SettingsModal.tabGitDelivery",
  background: "SettingsModal.tabBackgroundAgents",
  channels: "SettingsModal.tabChannels",
  peers: "SettingsModal.tabPeers",
  telephony: "SettingsModal.tabTelephony",
  companion: "SettingsModal.tabCompanion",
  security: "SettingsModal.tabSecurityDoctor",
  privacy: "SettingsModal.tabPrivacyFirewall",
  diagnostics: "SettingsModal.tabDiagnostics",
  appearance: "SettingsModal.tabAppearance",
  triage: "SettingsModal.tabTriage",
  approvalchains: "SettingsModal.tabApprovalChains",
  localapps: "SettingsModal.tabLocalApps",
  desktopcontrol: "SettingsModal.tabDesktopControl",
  team: "SettingsModal.tabTeamMode",
  profiles: "SettingsModal.tabProfiles",
  comparelab: "SettingsModal.tabCompareLab",
  resources: "SettingsModal.tabResourceLedger",
  updates: "SettingsModal.tabUpdates",
  execution: "SettingsModal.tabExecutionTargets",
};

const FOCUSABLE_SELECTOR = [
  "button:not([disabled])",
  "a[href]",
  'input:not([type="hidden"]):not([disabled])',
  "select:not([disabled])",
  "textarea:not([disabled])",
  '[tabindex]:not([tabindex="-1"])',
].join(",");

/**
 * App-wide Settings: one model-provider surface per tab (local llama.cpp,
 * Ollama, cloud AI providers) instead of stacking all three into one long
 * scroll — each tab manages its own reachability/refresh state internally
 * (`ModelManager`, `OllamaPanel`) so switching tabs is just a render swap,
 * no extra fetching logic here.
 */
export function SettingsModal({ open, onClose, initialTab, initialTabRequest = 0 }: SettingsModalProps) {
  const refreshProviders = useModelStore((s) => s.refreshProviders);
  const refreshOllama = useModelStore((s) => s.refreshOllama);
  const providers = useModelStore((s) => s.providers);
  const ollamaReachable = useModelStore((s) => s.ollamaReachable);
  const ollamaBinaryFound = useModelStore((s) => s.ollamaBinaryFound);
  const ollamaModels = useModelStore((s) => s.ollamaModels);
  const ollamaSignedInUser = useModelStore((s) => s.ollamaSignedInUser);

  const [tab, setTab] = useState<SettingsTab>("appearance");
  const [query, setQuery] = useState("");
  const { t } = useT();
  const dialogRef = useRef<HTMLDivElement>(null);
  const previouslyFocusedRef = useRef<HTMLElement | null>(null);
  const onCloseRef = useRef(onClose);
  onCloseRef.current = onClose;

  function openRuntimeHubPairing() {
    useRuntimeHubStore.getState().setSection("lan");
    setTab("runtimehub");
    setQuery("");
  }

  const connectedProviderItems = connectedProviderNavigationItems(providers);
  const ollamaConfigured = isOllamaConfigured({
    reachable: ollamaReachable,
    binaryFound: ollamaBinaryFound,
    installedModelCount: ollamaModels.length,
    signedInUser: ollamaSignedInUser,
  });
  const selectedProviderId = providerIdFromSettingsTab(tab);
  const selectedProvider = selectedProviderId
    ? providers.find((provider) => provider.id === selectedProviderId && provider.has_key)
    : undefined;

  // Connected providers get their own model-selection entry immediately
  // before the always-available provider configuration tab. This is derived
  // from the backend's live `has_key` probes, so built-ins and custom
  // providers follow the same rule without hardcoded provider names.
  const navGroups = GROUPS.map((group) => ({
    label: t(group.labelKey),
    items: group.ids.flatMap((id) => {
      const items: { id: SettingsTab; label: string; icon: LucideIcon }[] = [];
      if (id === "ollama" && !ollamaConfigured) {
        return items;
      }
      if (id === "providers") {
        items.push(
          ...connectedProviderItems.map((provider) => ({
            id: provider.tabId,
            label: provider.label,
            icon: Sparkles,
          })),
        );
      }
      items.push({ id, label: t(LABEL_KEYS[id]), icon: ICONS[id] });
      return items;
    }),
  }))
    .map((group) => ({
      ...group,
      items: group.items.filter((item) => item.label.toLowerCase().includes(query.trim().toLowerCase())),
    }))
    .filter((group) => group.items.length > 0);

  const activeLabel =
    selectedProvider?.label ??
    navGroups.flatMap((group) => group.items).find((item) => item.id === tab)?.label ??
    t(LABEL_KEYS[selectedProviderId ? "providers" : tab as StaticSettingsTab]);

  useEffect(() => {
    if (!open) return;
    void refreshProviders();
    void refreshOllama();
  }, [open, refreshOllama, refreshProviders]);

  useEffect(() => {
    if (tab === "ollama" && !ollamaConfigured) {
      setTab("providers");
      return;
    }
    if (selectedProviderId && !selectedProvider) {
      setTab("providers");
    }
  }, [ollamaConfigured, selectedProvider, selectedProviderId, tab]);

  // Select the requested tab for deep links, or reset to Appearance for every
  // normal open. The modal remains mounted after closing, so this must run on
  // each transition from closed to open rather than relying on useState's
  // initial value.
  useEffect(() => {
    if (!open) return;
    setTab(initialTab ?? "appearance");
    setQuery("");
  }, [open, initialTab, initialTabRequest]);

  useEffect(() => {
    if (!open) return;

    previouslyFocusedRef.current = document.activeElement instanceof HTMLElement
      ? document.activeElement
      : null;
    const focusFrame = requestAnimationFrame(() => {
      const dialog = dialogRef.current;
      if (!dialog || dialog.contains(document.activeElement)) return;
      const preferred = dialog.querySelector<HTMLElement>("[data-settings-autofocus]");
      (preferred ?? dialog).focus();
    });

    function handleKeyDown(e: KeyboardEvent) {
      if (e.key === "Escape" && !e.defaultPrevented) {
        e.preventDefault();
        onCloseRef.current();
        return;
      }
      if (e.key !== "Tab") return;

      const dialog = dialogRef.current;
      if (!dialog) return;
      const focusable = Array.from(dialog.querySelectorAll<HTMLElement>(FOCUSABLE_SELECTOR)).filter(
        (element) => element.tabIndex >= 0 && element.getClientRects().length > 0,
      );
      if (focusable.length === 0) {
        e.preventDefault();
        dialog.focus();
        return;
      }

      const first = focusable[0];
      const last = focusable[focusable.length - 1];
      const active = document.activeElement;
      if (e.shiftKey && (active === first || !dialog.contains(active))) {
        e.preventDefault();
        last.focus();
      } else if (!e.shiftKey && (active === last || active === dialog || !dialog.contains(active))) {
        e.preventDefault();
        first.focus();
      }
    }
    window.addEventListener("keydown", handleKeyDown);
    return () => {
      cancelAnimationFrame(focusFrame);
      window.removeEventListener("keydown", handleKeyDown);
      const previous = previouslyFocusedRef.current;
      if (previous?.isConnected) previous.focus();
      previouslyFocusedRef.current = null;
    };
  }, [open]);

  if (!open) return null;

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 backdrop-blur-[2px]"
      onClick={onClose}
    >
      <div
        ref={dialogRef}
        role="dialog"
        aria-modal="true"
        aria-labelledby="settings-modal-title"
        tabIndex={-1}
        className="flex h-[85vh] w-[90vw] overflow-hidden rounded-xl border border-border bg-background shadow-2xl"
        onClick={(event) => event.stopPropagation()}
      >
        <div className="flex w-64 shrink-0 flex-col border-r border-border bg-surface">
          <div className="flex min-h-0 flex-1 flex-col gap-4 overflow-y-auto p-3 [overscroll-behavior:contain]">
            <div className="relative">
              <Search size={14} className="pointer-events-none absolute left-2.5 top-1/2 -translate-y-1/2 text-faint" />
              <input
                type="text"
                value={query}
                onChange={(event) => setQuery(event.target.value)}
                placeholder={t("SettingsModal.searchPlaceholder")}
                className="w-full rounded-lg border border-border bg-surface-2 py-1.5 pl-8 pr-3 text-sm text-foreground placeholder:text-faint focus:outline-none focus:ring-1 focus:ring-accent"
              />
            </div>

            <p id="settings-modal-title" className="px-1 text-sm font-semibold text-foreground">
              {t("SettingsModal.title")}
            </p>

            {navGroups.map((group) => (
              <div key={group.label} className="flex flex-col gap-0.5">
                <p className="px-2 pb-1 text-xs font-medium uppercase tracking-wide text-faint">{group.label}</p>
                {group.items.map((item) => {
                  const Icon = item.icon;
                  const isActive = item.id === tab;
                  return (
                    <button
                      key={item.id}
                      type="button"
                      onClick={() => setTab(item.id)}
                      className={`flex items-center gap-2.5 rounded-lg px-2.5 py-1.5 text-left text-sm transition-colors ${
                        isActive
                          ? "bg-surface-2 font-medium text-foreground"
                          : "text-muted hover:bg-surface-2 hover:text-foreground"
                      }`}
                    >
                      <Icon size={16} className="shrink-0" />
                      {item.label}
                    </button>
                  );
                })}
              </div>
            ))}
          </div>
        </div>

        <div className="flex min-w-0 flex-1 flex-col">
          <div className="flex shrink-0 items-center justify-between px-6 pb-4 pt-6">
            <h2 className="text-lg font-semibold text-foreground">{activeLabel}</h2>
            <IconButton size="sm" onClick={onClose} aria-label={t("SettingsModal.closeSettingsAriaLabel")}>
              <X size={16} />
            </IconButton>
          </div>

          <div className="relative min-h-0 flex-1">
            <div className="pointer-events-none absolute inset-x-0 top-0 z-10 h-4 bg-gradient-to-b from-background to-transparent" />
            <div className="settings-controls h-full overflow-y-auto px-6 pb-6 pt-4 [overscroll-behavior:contain]">
              {tab === "local" && <ModelManager />}
              {tab === "ollama" && <OllamaPanel />}
              {selectedProvider && (
                <ProviderModelsPanel
                  key={selectedProvider.id}
                  providerId={selectedProvider.id}
                  providerLabel={selectedProvider.label}
                />
              )}
              {tab === "providers" && (
                <div className="flex flex-col gap-2">
                  {providers.map((provider) => (
                    <ProviderCard key={provider.id} provider={provider} />
                  ))}
                  <AddCustomProviderForm />
                </div>
              )}
              {tab === "knowledge" && <KnowledgePanel />}
              {tab === "automation" && (
                <Suspense fallback={<PanelFallback />}>
                  <AutomationPanel />
                </Suspense>
              )}
              {tab === "rules" && <RulesMemoryPanel />}
              {tab === "memorystudio" && <MemoryStudioPanel />}
              {tab === "connectors" && <ConnectorsPanel />}
              {tab === "prompts" && <PromptLibraryPanel />}
              {tab === "apiserver" && <ApiServerPanel onOpenRuntimeHubPairing={openRuntimeHubPairing} />}
              {tab === "shortcuts" && <KeyboardShortcutsPanel />}
              {tab === "usage" && <UsagePanel />}
              {tab === "tasks" && <ScheduledTasksPanel />}
              {tab === "localapps" && <LocalAppsPanel />}
              {tab === "portability" && <PortabilityPanel />}
              {tab === "extensions" && <ExecutableExtensionsPanel />}
              {tab === "ecosystem" && <EcosystemPanel />}
              {tab === "runtimehub" && <RuntimeHubPanel />}
              {tab === "browser" && <BrowserVerificationPanel />}
              {tab === "gitdelivery" && <GitDeliveryPanel />}
              {tab === "triage" && <TriagePanel />}
              {tab === "approvalchains" && <ApprovalChainsPanel />}
              {tab === "background" && <BackgroundAgentsPanel />}
              {tab === "channels" && <ChannelsPanel />}
              {tab === "peers" && <PeersPanel />}
              {tab === "telephony" && <TelephonyPanel />}
              {tab === "companion" && <CompanionPanel />}
              {tab === "updates" && <UpdatesPanel />}
              {tab === "security" && <SecurityDoctorPanel />}
              {tab === "privacy" && <PrivacyFirewallPanel />}
              {tab === "diagnostics" && <DiagnosticsPanel />}
              {tab === "appearance" && <AppearancePanel />}
              {tab === "desktopcontrol" && <DesktopControlPanel />}
              {tab === "team" && <TeamModePanel />}
              {tab === "profiles" && <ProfilesPanel />}
              {tab === "comparelab" && <CompareLabPanel />}
              {tab === "resources" && <ResourceLedgerPanel />}
              {tab === "execution" && <ExecutionTargetsPanel />}
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}
