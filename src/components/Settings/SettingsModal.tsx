import { useEffect, useRef, useState } from "react";
import {
  BarChart3,
  BookOpen,
  Bot,
  Boxes,
  Cloud,
  Cpu,
  Keyboard,
  HardDrive,
  Gauge,
  GitPullRequest,
  ListChecks,
  Lock,
  MessageSquare,
  MonitorCheck,
  Palette,
  Plug,
  ScrollText,
  Search,
  Server,
  ShieldCheck,
  Sparkles,
  Terminal,
  X,
  Zap,
  type LucideIcon,
} from "lucide-react";
import { IconButton } from "../ui";
import { useModelStore } from "../../store/modelStore";
import { ProviderCard } from "./ProviderCard";
import { AddCustomProviderForm } from "./AddCustomProviderForm";
import { AutomationPanel } from "./AutomationPanel";
import { OpenRouterModelsPanel } from "./OpenRouterModelsPanel";
import { RulesMemoryPanel } from "./RulesMemoryPanel";
import { McpPanel } from "./McpPanel";
import { PromptLibraryPanel } from "./PromptLibraryPanel";
import { ApiServerPanel } from "./ApiServerPanel";
import { KnowledgePanel } from "./KnowledgePanel";
import { KeyboardShortcutsPanel } from "./KeyboardShortcutsPanel";
import { ScheduledTasksPanel } from "./ScheduledTasksPanel";
import { UsagePanel } from "./UsagePanel";
import { PortabilityPanel } from "./PortabilityPanel";
import { EcosystemPanel } from "./EcosystemPanel";
import { RuntimeHubPanel } from "./RuntimeHubPanel";
import { BrowserVerificationPanel } from "./BrowserVerificationPanel";
import { BackgroundAgentsPanel } from "./BackgroundAgentsPanel";
import { GitDeliveryPanel } from "./GitDeliveryPanel";
import { CompanionPanel } from "./CompanionPanel";
import { SecurityDoctorPanel } from "./SecurityDoctorPanel";
import { PrivacyFirewallPanel } from "./PrivacyFirewallPanel";
import { AppearancePanel } from "./AppearancePanel";
import { ModelManager } from "../Models";
import { OllamaPanel } from "../Ollama";
import { useT } from "../../lib/i18n";

interface SettingsModalProps {
  open: boolean;
  onClose: () => void;
  /** Tab to select the moment the modal transitions from closed to open —
   * the deep-link hook `PersonaSelector`'s "Manage prompts…" row (and
   * anything else that wants to jump straight to a tab) uses, via App.tsx.
   * Left unset for the normal "open on whatever tab was last active" case. */
  initialTab?: SettingsTab;
  /** Changes for every deep-link request, including repeated requests for
   * the same tab while Settings is already open. */
  initialTabRequest?: number;
}

export type SettingsTab = "local" | "ollama" | "providers" | "openrouter" | "automation" | "rules" | "mcp" | "prompts" | "apiserver" | "knowledge" | "shortcuts" | "usage" | "tasks" | "portability" | "ecosystem" | "runtimehub" | "browser" | "gitdelivery" | "background" | "companion" | "security" | "privacy" | "appearance";

const ICONS: Record<Exclude<SettingsTab, "openrouter">, LucideIcon> = {
  local: Cpu,
  ollama: Server,
  providers: Cloud,
  knowledge: BookOpen,
  automation: Zap,
  rules: ScrollText,
  mcp: Plug,
  prompts: MessageSquare,
  apiserver: Terminal,
  shortcuts: Keyboard,
  usage: BarChart3,
  tasks: ListChecks,
  portability: HardDrive,
  ecosystem: Boxes,
  runtimehub: Gauge,
  browser: MonitorCheck,
  gitdelivery: GitPullRequest,
  background: Bot,
  companion: Sparkles,
  security: ShieldCheck,
  privacy: Lock,
  appearance: Palette,
};

const GROUPS: { labelKey: string; ids: Exclude<SettingsTab, "openrouter">[] }[] = [
  { labelKey: "SettingsModal.groupApplication", ids: ["appearance", "security", "privacy", "companion", "shortcuts", "usage", "portability"] },
  { labelKey: "SettingsModal.groupModels", ids: ["runtimehub", "local", "ollama", "providers"] },
  { labelKey: "SettingsModal.groupWorkspace", ids: ["knowledge", "automation", "rules", "tasks"] },
  { labelKey: "SettingsModal.groupIntegrations", ids: ["ecosystem", "browser", "gitdelivery", "background", "mcp", "prompts", "apiserver"] },
];

const LABEL_KEYS: Record<Exclude<SettingsTab, "openrouter">, string> = {
  local: "SettingsModal.tabLocalModels",
  ollama: "SettingsModal.tabOllama",
  providers: "SettingsModal.tabAiProviders",
  knowledge: "SettingsModal.tabKnowledge",
  automation: "SettingsModal.tabAutomation",
  rules: "SettingsModal.tabRules",
  mcp: "SettingsModal.tabMcp",
  prompts: "SettingsModal.tabPrompts",
  apiserver: "SettingsModal.tabApiServer",
  shortcuts: "SettingsModal.tabKeyboardShortcuts",
  usage: "SettingsModal.tabUsage",
  tasks: "SettingsModal.tabTasks",
  portability: "SettingsModal.tabPortability",
  ecosystem: "SettingsModal.tabEcosystem",
  runtimehub: "SettingsModal.tabRuntimeHub",
  browser: "SettingsModal.tabBrowserVerification",
  gitdelivery: "SettingsModal.tabGitDelivery",
  background: "SettingsModal.tabBackgroundAgents",
  companion: "SettingsModal.tabCompanion",
  security: "SettingsModal.tabSecurityDoctor",
  privacy: "SettingsModal.tabPrivacyFirewall",
  appearance: "SettingsModal.tabAppearance",
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
  const providers = useModelStore((s) => s.providers);

  const [tab, setTab] = useState<SettingsTab>("local");
  const [query, setQuery] = useState("");
  const { t } = useT();
  const dialogRef = useRef<HTMLDivElement>(null);
  const previouslyFocusedRef = useRef<HTMLElement | null>(null);
  const onCloseRef = useRef(onClose);
  onCloseRef.current = onClose;

  const openrouterProvider = providers.find((p) => p.id === "openrouter");
  const openrouterConnected = openrouterProvider?.has_key ?? false;

  // A dedicated nav item named after the provider, inserted right before "AI
  // Providers" — only while OpenRouter is connected. OpenRouter alone
  // returns 400+ models (see `ProviderCard.tsx`'s `FILTER_THRESHOLD`), so it
  // gets its own curation surface instead of dumping everything into the
  // chat toolbar's model switcher unfiltered.
  const navGroups = GROUPS.map((group) => ({
    label: t(group.labelKey),
    items: group.ids.flatMap((id) => {
      const items: { id: SettingsTab; label: string; icon: LucideIcon }[] = [];
      if (id === "providers" && openrouterConnected) {
        items.push({ id: "openrouter", label: openrouterProvider?.label ?? "OpenRouter", icon: Sparkles });
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
    navGroups.flatMap((group) => group.items).find((item) => item.id === tab)?.label ?? t(LABEL_KEYS[tab === "openrouter" ? "providers" : tab]);

  useEffect(() => {
    if (!open) return;
    void refreshProviders();
  }, [open, refreshProviders]);

  useEffect(() => {
    if (tab === "openrouter" && !openrouterConnected) setTab("providers");
  }, [tab, openrouterConnected]);

  // Jump to the requested tab whenever the modal opens with one specified
  // (e.g. PersonaSelector's "Manage prompts…" row) — re-applied on every
  // open, not just the first, so opening it a second time from a different
  // deep link still lands on the right tab even if the modal already
  // remembers a different one from last time.
  useEffect(() => {
    if (open && initialTab) {
      setTab(initialTab);
      setQuery("");
    }
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
            <div className="h-full overflow-y-auto px-6 pb-6 pt-4 [overscroll-behavior:contain]">
              {tab === "local" && <ModelManager />}
              {tab === "ollama" && <OllamaPanel />}
              {tab === "openrouter" && <OpenRouterModelsPanel />}
              {tab === "providers" && (
                <div className="flex flex-col gap-2">
                  {providers.map((provider) => (
                    <ProviderCard key={provider.id} provider={provider} />
                  ))}
                  <AddCustomProviderForm />
                </div>
              )}
              {tab === "knowledge" && <KnowledgePanel />}
              {tab === "automation" && <AutomationPanel />}
              {tab === "rules" && <RulesMemoryPanel />}
              {tab === "mcp" && <McpPanel />}
              {tab === "prompts" && <PromptLibraryPanel />}
              {tab === "apiserver" && <ApiServerPanel />}
              {tab === "shortcuts" && <KeyboardShortcutsPanel />}
              {tab === "usage" && <UsagePanel />}
              {tab === "tasks" && <ScheduledTasksPanel />}
              {tab === "portability" && <PortabilityPanel />}
              {tab === "ecosystem" && <EcosystemPanel />}
              {tab === "runtimehub" && <RuntimeHubPanel />}
              {tab === "browser" && <BrowserVerificationPanel />}
              {tab === "gitdelivery" && <GitDeliveryPanel />}
              {tab === "background" && <BackgroundAgentsPanel />}
              {tab === "companion" && <CompanionPanel />}
              {tab === "security" && <SecurityDoctorPanel />}
              {tab === "privacy" && <PrivacyFirewallPanel />}
              {tab === "appearance" && <AppearancePanel />}
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}
