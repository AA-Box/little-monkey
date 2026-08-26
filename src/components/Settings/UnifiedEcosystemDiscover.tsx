import { useEffect, useMemo, useState } from "react";
import { Boxes, ExternalLink, RefreshCw, Search, ShieldCheck } from "lucide-react";
import { useEcosystemStore } from "../../store/ecosystemStore";
import { useExtensionMarketplaceStore } from "../../store/extensionMarketplaceStore";
import {
  buildUnifiedCatalog,
  filterUnifiedCatalog,
  type UnifiedCatalogEntry,
  type UnifiedCatalogKind,
} from "../../lib/unifiedEcosystemCatalog";
import { Button, StatusPill } from "../ui";

interface UnifiedEcosystemDiscoverProps {
  onOpenPackages: () => void;
  onOpenExtensions: () => void;
  onOpenMcp: () => void;
}

const KIND_LABEL: Record<UnifiedCatalogKind, string> = {
  package: "Package",
  wasm: "WASM",
  mcp: "MCP",
};

function routeLabel(entry: UnifiedCatalogEntry): string {
  if (entry.kind === "package") return entry.updateState === "installed" ? "Manage package" : "Review package";
  if (entry.kind === "wasm") return entry.updateState === "installed" ? "Manage extension" : "Review verified release";
  return "Configure MCP";
}

export function UnifiedEcosystemDiscover({
  onOpenPackages,
  onOpenExtensions,
  onOpenMcp,
}: UnifiedEcosystemDiscoverProps) {
  const packages = useEcosystemStore((state) => state.catalog);
  const installedPackages = useEcosystemStore((state) => state.installed);
  const refreshPackages = useEcosystemStore((state) => state.refreshPackages);
  const extensionCatalog = useExtensionMarketplaceStore((state) => state.catalog);
  const installedExtensions = useExtensionMarketplaceStore((state) => state.installed);
  const extensionLoading = useExtensionMarketplaceStore((state) => state.loading);
  const hydrateExtensions = useExtensionMarketplaceStore((state) => state.hydrate);
  const refreshExtensions = useExtensionMarketplaceStore((state) => state.refreshAll);
  const previewExtension = useExtensionMarketplaceStore((state) => state.previewEntry);
  const [query, setQuery] = useState("");
  const [kind, setKind] = useState<UnifiedCatalogKind | "all">("all");
  const [refreshing, setRefreshing] = useState(false);
  const [openingId, setOpeningId] = useState<string | null>(null);

  useEffect(() => {
    void hydrateExtensions();
  }, [hydrateExtensions]);

  const catalog = useMemo(
    () => buildUnifiedCatalog({
      packages,
      installedPackages,
      extensions: extensionCatalog,
      installedExtensions,
    }),
    [packages, installedPackages, extensionCatalog, installedExtensions],
  );
  const visible = useMemo(() => filterUnifiedCatalog(catalog, query, kind), [catalog, query, kind]);

  async function refresh() {
    setRefreshing(true);
    try {
      await Promise.allSettled([refreshPackages(), refreshExtensions()]);
    } finally {
      setRefreshing(false);
    }
  }

  async function open(entry: UnifiedCatalogEntry) {
    if (entry.kind === "package") {
      onOpenPackages();
      return;
    }
    if (entry.kind === "mcp") {
      onOpenMcp();
      return;
    }

    const release = extensionCatalog.find((candidate) =>
      `wasm:${candidate.registry_source_id}:${candidate.extension_id}@${candidate.version}` === entry.id
    );
    // Installed entries remain manageable even when their registry release was
    // removed. New/update review, however, must always start from the exact
    // verified M4 identity represented by this catalog row.
    if (!release) {
      onOpenExtensions();
      return;
    }
    setOpeningId(entry.id);
    try {
      await previewExtension(release);
      onOpenExtensions();
    } finally {
      setOpeningId(null);
    }
  }

  return (
    <div className="space-y-4">
      <section className="rounded-xl border border-border bg-surface p-4">
        <div className="flex flex-wrap items-start justify-between gap-3">
          <div className="flex items-start gap-3">
            <div className="rounded-lg bg-accent-soft p-2 text-accent"><Boxes size={18} /></div>
            <div>
              <h3 className="text-sm font-semibold text-foreground">Unified catalog</h3>
              <p className="mt-1 max-w-3xl text-xs leading-5 text-muted">
                Browse declarative packages, sandboxed WASM extensions, and MCP integrations together. Discovery is unified; install and configuration authority is not. Every result routes into its existing security boundary.
              </p>
            </div>
          </div>
          <Button size="sm" disabled={refreshing || extensionLoading} onClick={() => void refresh()}>
            <RefreshCw size={14} className={refreshing || extensionLoading ? "animate-spin" : ""} /> Refresh
          </Button>
        </div>
      </section>

      <div className="flex flex-wrap gap-2">
        <div className="relative min-w-64 flex-1">
          <Search size={14} className="pointer-events-none absolute left-2.5 top-1/2 -translate-y-1/2 text-faint" />
          <input
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            placeholder="Search name, publisher, capability, permission, registry…"
            className="h-9 w-full rounded-md border border-border bg-surface pl-8 pr-3 text-xs text-foreground focus:outline-none focus:ring-2 focus:ring-accent"
          />
        </div>
        <select
          aria-label="Catalog type"
          value={kind}
          onChange={(event) => setKind(event.target.value as UnifiedCatalogKind | "all")}
          className="h-9 rounded-md border border-border bg-surface px-3 text-xs text-foreground"
        >
          <option value="all">All types</option>
          <option value="package">Packages</option>
          <option value="wasm">WASM extensions</option>
          <option value="mcp">MCP integrations</option>
        </select>
      </div>

      {visible.length === 0 ? (
        <div className="rounded-xl border border-dashed border-border p-8 text-center text-sm text-muted">
          No catalog entries match the current search.
        </div>
      ) : (
        <div className="grid gap-3 xl:grid-cols-2">
          {visible.map((entry) => (
            <article key={entry.id} className="flex min-h-64 flex-col rounded-xl border border-border bg-surface p-4">
              <div className="flex items-start justify-between gap-3">
                <div className="min-w-0">
                  <div className="flex flex-wrap items-center gap-2">
                    <h4 className="truncate text-sm font-semibold text-foreground">{entry.name}</h4>
                    <StatusPill tone={entry.kind === "wasm" ? "warning" : entry.kind === "mcp" ? "neutral" : "success"}>
                      {KIND_LABEL[entry.kind]}
                    </StatusPill>
                    {entry.updateState === "revoked" && <StatusPill tone="danger">revoked</StatusPill>}
                    {entry.updateState === "update_available" && <StatusPill tone="warning">update</StatusPill>}
                    {entry.updateState === "installed" && <StatusPill tone="success">installed</StatusPill>}
                  </div>
                  <p className="mt-1 text-[11px] text-faint">
                    {entry.publisher ?? "Publisher resolved during verified runtime review"} · {entry.version}
                    {entry.registryName ? ` · ${entry.registryName}` : ""}
                  </p>
                </div>
                <ShieldCheck size={15} className="shrink-0 text-muted" />
              </div>

              <p className="mt-3 text-xs leading-5 text-muted">{entry.description}</p>

              <dl className="mt-3 space-y-2 text-[11px]">
                <div><dt className="font-medium text-foreground">Trust</dt><dd className="mt-0.5 text-muted">{entry.trust}</dd></div>
                <div><dt className="font-medium text-foreground">Compatibility</dt><dd className="mt-0.5 text-muted">{entry.compatibility}</dd></div>
                <div><dt className="font-medium text-foreground">Capabilities</dt><dd className="mt-0.5 break-words text-muted">{entry.capabilities.join(" · ") || "None declared"}</dd></div>
                <div><dt className="font-medium text-foreground">Permissions / setup</dt><dd className="mt-0.5 break-words text-muted">{entry.permissions.join(" · ") || "No extra authority declared"}</dd></div>
              </dl>

              <div className="mt-auto pt-4">
                <div className="rounded-md bg-surface-2 px-2 py-1.5 text-[11px] text-muted">{entry.securityBoundary}</div>
                {!entry.metadataComplete && (
                  <p className="mt-2 text-[11px] text-faint">
                    Full executable metadata is fetched and verified natively when you review this release. The catalog never trusts unsigned renderer metadata to fill publisher, capability, or permission fields.
                  </p>
                )}
                <Button
                  className="mt-3"
                  size="sm"
                  disabled={entry.updateState === "revoked" || openingId !== null || extensionLoading}
                  onClick={() => void open(entry)}
                >
                  {openingId === entry.id ? <RefreshCw size={13} className="animate-spin" /> : <ExternalLink size={13} />}
                  {openingId === entry.id ? "Verifying release…" : routeLabel(entry)}
                </Button>
              </div>
            </article>
          ))}
        </div>
      )}
    </div>
  );
}
