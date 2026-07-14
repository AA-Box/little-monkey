import { useMemo, useState } from "react";
import {
  Activity,
  Bot,
  Boxes,
  Plug,
  RefreshCw,
  Search,
  ShieldAlert,
  ShieldCheck,
  Workflow,
} from "lucide-react";
import type {
  PluginComponentDescriptor,
  PluginComponentKind,
  PluginRuntimeDescriptor,
  PluginRuntimeHealth,
} from "../../lib/ecosystemClient";
import { useT } from "../../lib/i18n";
import { useEcosystemStore } from "../../store/ecosystemStore";
import { Button, StatusPill } from "../ui";

function healthTone(health: PluginRuntimeHealth): "success" | "warning" | "neutral" | "danger" {
  if (health === "healthy") return "success";
  if (health === "needs_setup") return "warning";
  if (health === "disabled") return "neutral";
  return "danger";
}

function componentIcon(kind: PluginComponentKind) {
  if (kind === "assistant") return Bot;
  if (kind === "connector" || kind === "mcp_requirement") return Plug;
  if (kind === "workflow") return Workflow;
  return Boxes;
}

function pluginSearchText(plugin: PluginRuntimeDescriptor): string {
  return [
    plugin.name,
    plugin.package_id,
    plugin.description,
    plugin.kind,
    plugin.health,
    ...plugin.components.flatMap((component) => [component.kind, component.label, component.detail]),
  ].join(" ").toLowerCase();
}

function ComponentRow({
  plugin,
  component,
}: {
  plugin: PluginRuntimeDescriptor;
  component: PluginComponentDescriptor;
}) {
  const { t } = useT();
  const { busy, activatePluginWorkflow, deactivatePluginWorkflow } = useEcosystemStore();
  const Icon = componentIcon(component.kind);
  const workflowBusy = busy[`plugin-workflow-${plugin.package_id}-${component.source_path ?? ""}`];
  const canManageWorkflow = component.kind === "workflow"
    && Boolean(component.source_path)
    && plugin.enabled
    && !["blocked", "corrupt"].includes(plugin.health);

  return (
    <li className="flex flex-col gap-3 rounded-lg border border-border bg-surface-2 p-3 sm:flex-row sm:items-start sm:justify-between">
      <div className="flex min-w-0 gap-2.5">
        <span className="mt-0.5 rounded-md bg-surface p-1.5 text-muted" aria-hidden="true"><Icon size={14} /></span>
        <div className="min-w-0">
          <div className="flex flex-wrap items-center gap-2">
            <span className="break-all text-xs font-medium text-foreground">{component.label}</span>
            <StatusPill tone={component.state === "active" ? "success" : component.state === "needs_setup" ? "warning" : component.state === "blocked" ? "danger" : "neutral"}>
              {component.state.replace("_", " ")}
            </StatusPill>
            <span className="rounded bg-surface px-1.5 py-0.5 text-[11px] text-faint">{component.kind.replace("_", " ")}</span>
          </div>
          <p className="mt-1 text-xs leading-5 text-muted">{component.detail}</p>
          {component.activation_id && (
            <p className="mt-1 truncate font-mono text-[10px] text-faint" title={component.activation_id}>{component.activation_id}</p>
          )}
        </div>
      </div>
      {canManageWorkflow && component.source_path && (
        <Button
          size="sm"
          variant={component.state === "active" ? "secondary" : "primary"}
          disabled={workflowBusy}
          onClick={() => void (component.state === "active"
            ? deactivatePluginWorkflow(plugin.package_id, component.source_path!)
            : activatePluginWorkflow(plugin.package_id, component.source_path!))}
        >
          {workflowBusy && <RefreshCw size={13} className="motion-safe:animate-spin" />}
          {component.state === "active"
            ? t("EcosystemPlugins.deactivateWorkflow")
            : t("EcosystemPlugins.activateWorkflow")}
        </Button>
      )}
    </li>
  );
}

export function EcosystemPlugins() {
  const { t } = useT();
  const {
    plugins,
    busy,
    refreshPluginRuntime,
    setPackageEnabled,
    rollbackPackage,
  } = useEcosystemStore();
  const [query, setQuery] = useState("");
  const normalizedQuery = query.trim().toLowerCase();
  const visible = useMemo(
    () => plugins.filter((plugin) => !normalizedQuery || pluginSearchText(plugin).includes(normalizedQuery)),
    [normalizedQuery, plugins],
  );
  const healthy = plugins.filter((plugin) => plugin.health === "healthy").length;
  const attention = plugins.filter((plugin) => plugin.health === "needs_setup").length;
  const blocked = plugins.filter((plugin) => ["blocked", "corrupt"].includes(plugin.health)).length;

  return (
    <div className="space-y-4">
      <section aria-label={t("EcosystemPlugins.summaryLabel")} className="grid gap-2 sm:grid-cols-3">
        <div className="rounded-xl border border-border bg-surface p-3">
          <p className="flex items-center gap-2 text-xs text-muted"><ShieldCheck size={14} className="text-success" />{t("EcosystemPlugins.healthy")}</p>
          <p className="mt-1 text-xl font-semibold text-foreground">{healthy}</p>
        </div>
        <div className="rounded-xl border border-border bg-surface p-3">
          <p className="flex items-center gap-2 text-xs text-muted"><Activity size={14} className="text-warning" />{t("EcosystemPlugins.needsSetup")}</p>
          <p className="mt-1 text-xl font-semibold text-foreground">{attention}</p>
        </div>
        <div className="rounded-xl border border-border bg-surface p-3">
          <p className="flex items-center gap-2 text-xs text-muted"><ShieldAlert size={14} className="text-danger" />{t("EcosystemPlugins.blocked")}</p>
          <p className="mt-1 text-xl font-semibold text-foreground">{blocked}</p>
        </div>
      </section>

      <div className="flex flex-wrap items-center gap-2">
        <label className="relative min-w-64 flex-1">
          <span className="sr-only">{t("EcosystemPlugins.searchLabel")}</span>
          <Search size={15} className="pointer-events-none absolute left-3 top-1/2 -translate-y-1/2 text-faint" />
          <input
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            placeholder={t("EcosystemPlugins.searchPlaceholder")}
            className="h-9 w-full rounded-lg border border-border bg-surface pl-9 pr-3 text-sm text-foreground placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
          />
        </label>
        <Button size="sm" disabled={busy.plugins} onClick={() => void refreshPluginRuntime()}>
          <RefreshCw size={14} className={busy.plugins ? "motion-safe:animate-spin" : ""} />
          {t("EcosystemPlugins.refresh")}
        </Button>
      </div>

      <div aria-live="polite" className="space-y-3">
        {visible.map((plugin) => (
          <article key={plugin.package_id} className="rounded-xl border border-border bg-surface p-4">
            <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
              <div className="min-w-0">
                <div className="flex flex-wrap items-center gap-2">
                  <h4 className="text-sm font-semibold text-foreground">{plugin.name}</h4>
                  <StatusPill tone={healthTone(plugin.health)}>{plugin.health.replace("_", " ")}</StatusPill>
                  <StatusPill tone={plugin.signed ? "success" : "warning"}>{plugin.signed ? t("EcosystemPlugins.signed") : t("EcosystemPlugins.localUnsigned")}</StatusPill>
                </div>
                <p className="mt-1 break-all font-mono text-[11px] text-faint">{plugin.package_id} · v{plugin.version ?? "—"}</p>
                <p className="mt-2 max-w-3xl text-xs leading-5 text-muted">{plugin.description}</p>
              </div>
              <div className="flex shrink-0 flex-wrap gap-2">
                <Button
                  size="sm"
                  variant={plugin.enabled ? "secondary" : "primary"}
                  disabled={plugin.health === "blocked" || plugin.health === "corrupt" || busy[`package-enable-${plugin.package_id}`]}
                  onClick={() => void setPackageEnabled(plugin.package_id, !plugin.enabled)}
                >
                  {plugin.enabled ? t("EcosystemPlugins.deactivate") : t("EcosystemPlugins.activate")}
                </Button>
                {plugin.rollback_target && (
                  <Button
                    size="sm"
                    disabled={!plugin.rollback_healthy || busy[`package-rollback-${plugin.package_id}`]}
                    onClick={() => void rollbackPackage(plugin.package_id)}
                  >
                    {t("EcosystemPlugins.rollbackTo", { version: plugin.rollback_target })}
                  </Button>
                )}
              </div>
            </div>

            {plugin.issues.length > 0 && (
              <ul className="mt-3 space-y-1 rounded-lg border border-warning/30 bg-warning-soft p-3 text-xs text-warning">
                {plugin.issues.map((issue) => <li key={issue} className="flex gap-2"><ShieldAlert size={13} className="mt-0.5 shrink-0" /><span>{issue}</span></li>)}
              </ul>
            )}

            <details className="mt-4 rounded-lg border border-border bg-background/40 open:bg-background/70">
              <summary className="cursor-pointer select-none px-3 py-2.5 text-xs font-medium text-foreground focus:outline-none focus-visible:ring-2 focus-visible:ring-accent">
                {t("EcosystemPlugins.components", { count: plugin.components.length })}
              </summary>
              <div className="border-t border-border p-3">
                <ul className="space-y-2">
                  {plugin.components.map((component) => (
                    <ComponentRow key={component.component_id} plugin={plugin} component={component} />
                  ))}
                </ul>
                <dl className="mt-3 grid gap-2 text-[11px] text-muted sm:grid-cols-3">
                  <div><dt className="text-faint">{t("EcosystemPlugins.bundleDigest")}</dt><dd className="mt-0.5 truncate font-mono" title={plugin.bundle_sha256 ?? undefined}>{plugin.bundle_sha256 ?? "—"}</dd></div>
                  <div><dt className="text-faint">{t("EcosystemPlugins.permissions")}</dt><dd className="mt-0.5">{plugin.permissions.length}</dd></div>
                  <div><dt className="text-faint">{t("EcosystemPlugins.pin")}</dt><dd className="mt-0.5">{plugin.pinned_version ?? t("EcosystemPlugins.followLatest")}</dd></div>
                </dl>
              </div>
            </details>
          </article>
        ))}
      </div>

      {visible.length === 0 && (
        <div className="rounded-xl border border-dashed border-border p-8 text-center text-sm text-muted">
          {plugins.length === 0 ? t("EcosystemPlugins.empty") : t("EcosystemPlugins.noResults")}
        </div>
      )}
    </div>
  );
}
