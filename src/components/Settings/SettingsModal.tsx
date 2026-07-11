import { useEffect, useState } from "react";
import { X } from "lucide-react";
import { IconButton, Tabs } from "../ui";
import { useModelStore } from "../../store/modelStore";
import { ProviderCard } from "./ProviderCard";
import { AddCustomProviderForm } from "./AddCustomProviderForm";
import { AutomationPanel } from "./AutomationPanel";
import { OpenRouterModelsPanel } from "./OpenRouterModelsPanel";
import { RulesMemoryPanel } from "./RulesMemoryPanel";
import { McpPanel } from "./McpPanel";
import { ModelManager } from "../Models";
import { OllamaPanel } from "../Ollama";
import { useT } from "../../lib/i18n";

interface SettingsModalProps {
  open: boolean;
  onClose: () => void;
}

type SettingsTab = "local" | "ollama" | "providers" | "openrouter" | "automation" | "rules" | "mcp";

const TAB_KEYS: { id: Exclude<SettingsTab, "openrouter">; labelKey: string }[] = [
  { id: "local", labelKey: "SettingsModal.tabLocalModels" },
  { id: "ollama", labelKey: "SettingsModal.tabOllama" },
  { id: "providers", labelKey: "SettingsModal.tabAiProviders" },
  { id: "automation", labelKey: "SettingsModal.tabAutomation" },
  { id: "rules", labelKey: "SettingsModal.tabRules" },
  { id: "mcp", labelKey: "SettingsModal.tabMcp" },
];

/**
 * App-wide Settings: one model-provider surface per tab (local llama.cpp,
 * Ollama, cloud AI providers) instead of stacking all three into one long
 * scroll — each tab manages its own reachability/refresh state internally
 * (`ModelManager`, `OllamaPanel`) so switching tabs is just a render swap,
 * no extra fetching logic here.
 */
export function SettingsModal({ open, onClose }: SettingsModalProps) {
  const refreshProviders = useModelStore((s) => s.refreshProviders);
  const providers = useModelStore((s) => s.providers);

  const [tab, setTab] = useState<SettingsTab>("local");
  const { t } = useT();

  const openrouterProvider = providers.find((p) => p.id === "openrouter");
  const openrouterConnected = openrouterProvider?.has_key ?? false;

  // A dedicated tab named after the provider, inserted right before "AI
  // Providers" — only while OpenRouter is connected. OpenRouter alone
  // returns 400+ models (see `ProviderCard.tsx`'s `FILTER_THRESHOLD`), so it
  // gets its own curation surface instead of dumping everything into the
  // chat toolbar's model switcher unfiltered.
  const TABS: { id: SettingsTab; label: string }[] = [];
  for (const { id, labelKey } of TAB_KEYS) {
    if (id === "providers" && openrouterConnected) {
      TABS.push({ id: "openrouter", label: openrouterProvider?.label ?? "OpenRouter" });
    }
    TABS.push({ id, label: t(labelKey) });
  }

  useEffect(() => {
    if (!open) return;
    void refreshProviders();
  }, [open, refreshProviders]);

  useEffect(() => {
    if (tab === "openrouter" && !openrouterConnected) setTab("providers");
  }, [tab, openrouterConnected]);

  useEffect(() => {
    if (!open) return;
    function handleKeyDown(e: KeyboardEvent) {
      if (e.key === "Escape") onClose();
    }
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [open, onClose]);

  if (!open) return null;

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4 backdrop-blur-[2px]"
      role="dialog"
      aria-modal="true"
      aria-labelledby="settings-modal-title"
      onClick={onClose}
    >
      <div
        className="flex h-[85vh] w-full max-w-2xl flex-col rounded-xl border border-border bg-background shadow-xl"
        onClick={(event) => event.stopPropagation()}
      >
        <div className="flex shrink-0 items-center justify-between border-b border-border px-5 py-3.5">
          <h2 id="settings-modal-title" className="text-sm font-semibold text-foreground">
            {t("SettingsModal.title")}
          </h2>
          <IconButton size="sm" onClick={onClose} aria-label={t("SettingsModal.closeSettingsAriaLabel")}>
            <X size={16} />
          </IconButton>
        </div>

        <div className="shrink-0 px-3">
          <Tabs tabs={TABS} active={tab} onChange={(id) => setTab(id as SettingsTab)} />
        </div>

        <div className="min-h-0 flex-1 overflow-y-auto p-3">
          {tab === "local" && <ModelManager />}
          {tab === "ollama" && <OllamaPanel />}
          {tab === "openrouter" && <OpenRouterModelsPanel />}
          {tab === "providers" && (
            <div className="flex flex-col gap-2 p-2">
              {providers.map((provider) => (
                <ProviderCard key={provider.id} provider={provider} />
              ))}
              <AddCustomProviderForm />
            </div>
          )}
          {tab === "automation" && <AutomationPanel />}
          {tab === "rules" && <RulesMemoryPanel />}
          {tab === "mcp" && <McpPanel />}
        </div>
      </div>
    </div>
  );
}
