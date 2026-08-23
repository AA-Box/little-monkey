import { useEffect, useState } from "react";
import { AlertTriangle, RefreshCw, ShieldCheck } from "lucide-react";
import { Button, Tabs } from "../ui";
import { useT } from "../../lib/i18n";
import { useEcosystemStore } from "../../store/ecosystemStore";
import { EcosystemDiscover } from "./EcosystemDiscover";
import { EcosystemPackages } from "./EcosystemPackages";
import { EcosystemPlugins } from "./EcosystemPlugins";
import { EcosystemMcpApps, EcosystemOAuth } from "./EcosystemConnections";
import { EcosystemWorkflowDesigner, EcosystemWorkflowRuns } from "./EcosystemWorkflows";
import { ExtensionMarketplacePanel } from "./ExtensionMarketplacePanel";

type EcosystemTab = "marketplace" | "extensions" | "installed" | "plugins" | "connections" | "apps" | "workflows" | "runs";

export function EcosystemPanel() {
  const { t } = useT();
  const [tab, setTab] = useState<EcosystemTab>("marketplace");
  const { error, clearError, busy, refreshPackages, refreshWorkflows } = useEcosystemStore();

  useEffect(() => {
    void Promise.allSettled([refreshPackages(), refreshWorkflows()]);
  }, [refreshPackages, refreshWorkflows]);

  const tabs = [
    { id: "marketplace", label: t("EcosystemPanel.marketplace") },
    { id: "extensions", label: "Extensions" },
    { id: "installed", label: t("EcosystemPanel.installed") },
    { id: "plugins", label: t("EcosystemPanel.plugins") },
    { id: "connections", label: t("EcosystemPanel.connections") },
    { id: "apps", label: t("EcosystemPanel.apps") },
    { id: "workflows", label: t("EcosystemPanel.workflows") },
    { id: "runs", label: t("EcosystemPanel.runs") },
  ];

  return (
    <div className="space-y-5">
      <header className="flex flex-wrap items-start justify-between gap-3 rounded-xl border border-border bg-surface p-4">
        <div className="flex items-start gap-3">
          <div className="rounded-lg bg-accent-soft p-2 text-accent"><ShieldCheck size={18} /></div>
          <div>
            <h3 className="text-sm font-semibold text-foreground">{t("EcosystemPanel.title")}</h3>
            <p className="mt-1 max-w-2xl text-xs leading-5 text-muted">{t("EcosystemPanel.description")}</p>
          </div>
        </div>
        <Button
          size="sm"
          disabled={busy.packages || busy.workflows}
          onClick={() => void Promise.allSettled([refreshPackages(), refreshWorkflows()])}
        >
          <RefreshCw size={14} className={busy.packages || busy.workflows ? "animate-spin" : ""} />
          {t("EcosystemPanel.refresh")}
        </Button>
      </header>

      <div className="overflow-x-auto [overscroll-behavior:contain]">
        <Tabs tabs={tabs} active={tab} onChange={(next) => { clearError(); setTab(next as EcosystemTab); }} />
      </div>

      {error && (
        <div role="alert" className="flex items-start justify-between gap-3 rounded-lg border border-danger/30 bg-danger-soft p-3 text-xs text-danger">
          <span className="flex min-w-0 items-start gap-2"><AlertTriangle size={15} className="mt-0.5 shrink-0" /><span className="whitespace-pre-wrap break-words">{error}</span></span>
          <button type="button" className="shrink-0 font-medium hover:underline focus:outline-none focus:ring-2 focus:ring-danger" onClick={clearError}>{t("EcosystemPanel.dismiss")}</button>
        </div>
      )}

      {tab === "marketplace" && <EcosystemDiscover />}
      {tab === "extensions" && <ExtensionMarketplacePanel />}
      {tab === "installed" && <EcosystemPackages view="installed" />}
      {tab === "plugins" && <EcosystemPlugins />}
      {tab === "connections" && <EcosystemOAuth />}
      {tab === "apps" && <EcosystemMcpApps />}
      {tab === "workflows" && <EcosystemWorkflowDesigner />}
      {tab === "runs" && <EcosystemWorkflowRuns />}
    </div>
  );
}
