import { useEffect, useMemo, useState } from "react";
import { Download, RefreshCw, Search, ShieldCheck, Store, Trash2 } from "lucide-react";

import type { PermissionGrant, PermissionRisk, PermissionView } from "../../lib/executableExtensionsClient";
import type { ExtensionRegistryEntry } from "../../lib/extensionMarketplace";
import { useExtensionMarketplaceStore, type ExtensionUpdatePolicy } from "../../store/extensionMarketplaceStore";
import { Button, StatusPill, type PillTone } from "../ui";

const RISK_TONE: Record<PermissionRisk, PillTone> = {
  low: "neutral",
  medium: "warning",
  high: "danger",
  critical: "danger",
};

type Tab = "discover" | "registries" | "updates";

function registryLabel(entry: ExtensionRegistryEntry, sources: ReturnType<typeof useExtensionMarketplaceStore.getState>["sources"], registries: ReturnType<typeof useExtensionMarketplaceStore.getState>["registries"]): string {
  const registry = registries.find((item) => item.snapshot.entries.some((candidate) => candidate.extension_id === entry.extension_id && candidate.version === entry.version));
  return registry ? registry.source.display_name : sources[0]?.display_name ?? "Verified registry";
}

function PermissionRow({ permission, granted, binding, onGranted, onBinding }: {
  permission: PermissionView;
  granted: boolean;
  binding: string;
  onGranted: (value: boolean) => void;
  onBinding: (value: string) => void;
}) {
  const needsBinding = permission.kind === "workspace_read" || permission.kind === "workspace_write";
  return (
    <div className="rounded-md border border-border bg-background p-2 text-xs">
      <div className="flex items-start gap-2">
        <input type="checkbox" checked={granted} onChange={(event) => onGranted(event.target.checked)} aria-label={`Grant ${permission.permission_id}`} className="mt-0.5" />
        <div className="min-w-0 flex-1">
          <div className="flex flex-wrap items-center gap-2">
            <span className="font-mono text-foreground">{permission.kind}</span>
            <StatusPill tone={RISK_TONE[permission.risk]}>{permission.risk}</StatusPill>
            <span className="break-all text-muted">{permission.scope}</span>
          </div>
          <p className="mt-1 text-muted">{permission.reason}</p>
          {needsBinding && granted && (
            <input value={binding} onChange={(event) => onBinding(event.target.value)} placeholder="Workspace path to bind" className="mt-2 w-full rounded-md border border-border bg-surface px-2 py-1.5 text-xs text-foreground" />
          )}
        </div>
      </div>
    </div>
  );
}

export function ExtensionMarketplacePanel() {
  const sources = useExtensionMarketplaceStore((state) => state.sources);
  const registries = useExtensionMarketplaceStore((state) => state.registries);
  const catalog = useExtensionMarketplaceStore((state) => state.catalog);
  const installed = useExtensionMarketplaceStore((state) => state.installed);
  const updates = useExtensionMarketplaceStore((state) => state.updates);
  const updatePolicy = useExtensionMarketplaceStore((state) => state.updatePolicy);
  const pending = useExtensionMarketplaceStore((state) => state.pendingPreview);
  const loading = useExtensionMarketplaceStore((state) => state.loading);
  const error = useExtensionMarketplaceStore((state) => state.error);
  const notice = useExtensionMarketplaceStore((state) => state.notice);
  const hydrate = useExtensionMarketplaceStore((state) => state.hydrate);
  const addSource = useExtensionMarketplaceStore((state) => state.addSource);
  const removeSource = useExtensionMarketplaceStore((state) => state.removeSource);
  const refreshSource = useExtensionMarketplaceStore((state) => state.refreshSource);
  const refreshAll = useExtensionMarketplaceStore((state) => state.refreshAll);
  const previewInstall = useExtensionMarketplaceStore((state) => state.previewInstall);
  const clearPreview = useExtensionMarketplaceStore((state) => state.clearPreview);
  const installPending = useExtensionMarketplaceStore((state) => state.installPending);
  const setUpdatePolicy = useExtensionMarketplaceStore((state) => state.setUpdatePolicy);
  const applySafeUpdates = useExtensionMarketplaceStore((state) => state.applySafeUpdates);

  const [tab, setTab] = useState<Tab>("discover");
  const [query, setQuery] = useState("");
  const [sourceId, setSourceId] = useState("");
  const [sourceName, setSourceName] = useState("");
  const [sourceUrl, setSourceUrl] = useState("");
  const [sourceKeyId, setSourceKeyId] = useState("");
  const [sourcePublicKey, setSourcePublicKey] = useState("");
  const [grantedIds, setGrantedIds] = useState<Set<string>>(new Set());
  const [bindings, setBindings] = useState<Record<string, string>>({});
  const [reviewHighRisk, setReviewHighRisk] = useState(false);
  const [reviewUntrusted, setReviewUntrusted] = useState(false);

  useEffect(() => { void hydrate(); }, [hydrate]);
  useEffect(() => {
    if (!pending) {
      setGrantedIds(new Set());
      setBindings({});
      setReviewHighRisk(false);
      setReviewUntrusted(false);
      return;
    }
    setGrantedIds(new Set(pending.runtime_preview.permissions.filter((permission) => permission.granted).map((permission) => permission.permission_id)));
    setBindings({});
    setReviewHighRisk(false);
    setReviewUntrusted(false);
  }, [pending]);

  const visible = useMemo(() => {
    const needle = query.trim().toLowerCase();
    return catalog.filter((entry) => !needle || [entry.display_name, entry.description, entry.publisher, entry.category, ...entry.capabilities].join(" ").toLowerCase().includes(needle));
  }, [catalog, query]);

  const submitRegistry = async () => {
    await addSource({
      source_id: sourceId,
      display_name: sourceName,
      index_url: sourceUrl,
      public_key_base64: sourcePublicKey,
      key_id: sourceKeyId,
      enabled: true,
    });
    setSourceId("");
    setSourceName("");
    setSourceUrl("");
    setSourceKeyId("");
    setSourcePublicKey("");
  };

  const applyInstall = async () => {
    if (!pending) return;
    const grants: PermissionGrant[] = [];
    for (const permission of pending.runtime_preview.permissions) {
      if (!grantedIds.has(permission.permission_id)) continue;
      const needsBinding = permission.kind === "workspace_read" || permission.kind === "workspace_write";
      const binding = needsBinding ? bindings[permission.permission_id]?.trim() ?? "" : "";
      if (needsBinding && !binding) throw new Error(`Workspace binding is required for ${permission.permission_id}.`);
      grants.push({ permission_id: permission.permission_id, binding: binding || null });
    }
    await installPending(grants, reviewHighRisk, reviewUntrusted);
  };

  return (
    <div className="space-y-4">
      <section className="rounded-lg border border-border bg-surface p-4">
        <div className="flex flex-wrap items-start justify-between gap-3">
          <div className="max-w-2xl">
            <h3 className="flex items-center gap-2 text-sm font-semibold text-foreground"><Store size={16} /> Extension Marketplace</h3>
            <p className="mt-1 text-xs leading-5 text-muted">
              Discover executable WASM extensions from user-trusted signed registries. Registry verification proves discovery metadata and package bytes; Little Monkey's existing Wasmtime runtime still independently validates the extension and enforces its permission grants.
            </p>
          </div>
          <Button size="sm" disabled={loading} onClick={() => void refreshAll()}><RefreshCw size={13} /> Refresh</Button>
        </div>
        {error && <pre role="alert" className="mt-3 whitespace-pre-wrap rounded-md border border-danger/40 bg-danger-soft p-2 text-xs text-danger">{error}</pre>}
        {notice && <p className="mt-3 rounded-md border border-success/40 bg-success-soft p-2 text-xs text-success">{notice}</p>}
      </section>

      <div className="flex flex-wrap gap-2">
        {(["discover", "registries", "updates"] as Tab[]).map((entry) => (
          <Button key={entry} size="sm" variant={tab === entry ? "primary" : "secondary"} onClick={() => setTab(entry)}>
            {entry === "updates" ? `Updates (${updates.length})` : entry[0].toUpperCase() + entry.slice(1)}
          </Button>
        ))}
      </div>

      {tab === "discover" && (
        <section className="space-y-3">
          <div className="relative">
            <Search size={14} className="pointer-events-none absolute left-2.5 top-1/2 -translate-y-1/2 text-faint" />
            <input value={query} onChange={(event) => setQuery(event.target.value)} placeholder="Search extensions, capabilities, publishers…" className="w-full rounded-md border border-border bg-surface py-2 pl-8 pr-3 text-xs text-foreground" />
          </div>
          {registries.length === 0 ? (
            <p className="rounded-lg border border-border bg-surface p-5 text-center text-xs text-muted">Add and verify a signed registry first. Little Monkey intentionally ships no hard-coded network executable catalog.</p>
          ) : visible.length === 0 ? (
            <p className="py-5 text-center text-xs text-muted">No matching extensions.</p>
          ) : (
            <div className="grid gap-3 xl:grid-cols-2">
              {visible.map((entry) => {
                const existing = installed.find((item) => item.manifest.extension_id === entry.extension_id);
                return (
                  <article key={`${entry.extension_id}@${entry.version}`} className="rounded-lg border border-border bg-surface p-3">
                    <div className="flex items-start justify-between gap-3">
                      <div className="min-w-0">
                        <div className="flex flex-wrap items-center gap-2">
                          <h4 className="text-sm font-semibold text-foreground">{entry.display_name}</h4>
                          <StatusPill tone="success">signed registry</StatusPill>
                          {entry.deprecated && <StatusPill tone="warning">deprecated</StatusPill>}
                        </div>
                        <p className="mt-1 text-[11px] text-faint">{entry.publisher} · {entry.version} · {entry.category.replaceAll("_", " ")} · {registryLabel(entry, sources, registries)}</p>
                        <p className="mt-2 text-xs text-muted">{entry.description}</p>
                        <div className="mt-2 flex flex-wrap gap-1">{entry.capabilities.map((capability) => <StatusPill key={capability}>{capability}</StatusPill>)}</div>
                      </div>
                      <Button size="sm" variant="primary" disabled={loading || entry.revoked || existing?.active_version === entry.version} onClick={() => void previewInstall(entry)}>
                        <Download size={13} /> {existing ? "Review update" : "Review install"}
                      </Button>
                    </div>
                  </article>
                );
              })}
            </div>
          )}
        </section>
      )}

      {tab === "registries" && (
        <section className="space-y-3">
          <div className="rounded-lg border border-border bg-surface p-4">
            <h4 className="text-xs font-semibold text-foreground">Add signed static registry</h4>
            <p className="mt-1 text-xs text-muted">The public key is configured here by the user/admin. A downloaded registry cannot add or replace its own trust root.</p>
            <div className="mt-3 grid gap-2 md:grid-cols-2">
              <input value={sourceId} onChange={(event) => setSourceId(event.target.value)} placeholder="source id" className="rounded-md border border-border bg-background px-2 py-1.5 text-xs" />
              <input value={sourceName} onChange={(event) => setSourceName(event.target.value)} placeholder="display name" className="rounded-md border border-border bg-background px-2 py-1.5 text-xs" />
              <input value={sourceUrl} onChange={(event) => setSourceUrl(event.target.value)} placeholder="https://example.com/extensions/index.json" className="rounded-md border border-border bg-background px-2 py-1.5 text-xs md:col-span-2" />
              <input value={sourceKeyId} onChange={(event) => setSourceKeyId(event.target.value)} placeholder="key id" className="rounded-md border border-border bg-background px-2 py-1.5 text-xs" />
              <input value={sourcePublicKey} onChange={(event) => setSourcePublicKey(event.target.value)} placeholder="Ed25519 public key (base64)" className="rounded-md border border-border bg-background px-2 py-1.5 font-mono text-xs" />
            </div>
            <Button className="mt-3" size="sm" variant="primary" disabled={loading || !sourceId.trim() || !sourceUrl.trim() || !sourceKeyId.trim() || !sourcePublicKey.trim()} onClick={() => void submitRegistry()}>
              <ShieldCheck size={13} /> Verify and add
            </Button>
          </div>
          {sources.map((source) => (
            <article key={source.source_id} className="rounded-lg border border-border bg-surface p-3">
              <div className="flex flex-wrap items-start justify-between gap-3">
                <div className="min-w-0">
                  <div className="flex items-center gap-2"><h4 className="text-sm font-medium text-foreground">{source.display_name}</h4><StatusPill tone={registries.some((registry) => registry.source.source_id === source.source_id) ? "success" : "warning"}>{registries.some((registry) => registry.source.source_id === source.source_id) ? "verified" : "not verified"}</StatusPill></div>
                  <p className="mt-1 break-all font-mono text-[11px] text-faint">{source.index_url}</p>
                  <p className="mt-1 text-[11px] text-muted">key {source.key_id} · trusted sequence {source.last_sequence}{source.last_snapshot_sha256 ? ` · ${source.last_snapshot_sha256.slice(0, 12)}…` : ""}</p>
                </div>
                <div className="flex gap-2">
                  <Button size="sm" disabled={loading} onClick={() => void refreshSource(source.source_id)}><RefreshCw size={13} /> Verify</Button>
                  <Button size="sm" variant="danger" onClick={() => removeSource(source.source_id)}><Trash2 size={13} /> Remove</Button>
                </div>
              </div>
            </article>
          ))}
        </section>
      )}

      {tab === "updates" && (
        <section className="space-y-3">
          <div className="rounded-lg border border-border bg-surface p-3">
            <label className="flex flex-wrap items-center gap-2 text-xs text-foreground">
              Update policy
              <select value={updatePolicy} onChange={(event) => void setUpdatePolicy(event.target.value as ExtensionUpdatePolicy)} className="rounded-md border border-border bg-background px-2 py-1.5">
                <option value="off">Off</option>
                <option value="notify">Notify</option>
                <option value="automatic_safe">Automatic safe updates</option>
              </select>
              {updatePolicy === "automatic_safe" && <Button size="sm" disabled={loading} onClick={() => void applySafeUpdates()}>Apply safe updates now</Button>}
            </label>
            <p className="mt-2 text-[11px] text-muted">Automatic updates are permitted only when publisher/runtime trust remains verified, host compatibility is preserved, and authority does not expand. Any new or widened permission pauses for review.</p>
          </div>
          {updates.length === 0 ? <p className="py-5 text-center text-xs text-muted">No marketplace updates are currently available.</p> : updates.map((update) => (
            <article key={`${update.entry.extension_id}@${update.entry.version}`} className="rounded-lg border border-border bg-surface p-3 text-xs">
              <div className="flex flex-wrap items-center gap-2"><span className="font-semibold text-foreground">{update.entry.display_name}</span><span className="text-muted">{update.installed.active_version} → {update.entry.version}</span><StatusPill tone={update.safe_auto_update ? "success" : "warning"}>{update.safe_auto_update ? "safe automatic" : "review required"}</StatusPill></div>
              {update.reasons.length > 0 && <ul className="mt-2 list-disc space-y-1 pl-4 text-muted">{update.reasons.map((reason) => <li key={reason}>{reason}</li>)}</ul>}
            </article>
          ))}
        </section>
      )}

      {pending && (
        <div className="fixed inset-0 z-[70] flex items-center justify-center bg-black/50 p-5" onClick={clearPreview}>
          <div className="max-h-[85vh] w-full max-w-2xl overflow-y-auto rounded-xl border border-border bg-background p-4 shadow-2xl" onClick={(event) => event.stopPropagation()} role="dialog" aria-modal="true" aria-label="Extension install review">
            <div className="flex items-start justify-between gap-3">
              <div><h3 className="text-base font-semibold text-foreground">Review {pending.entry.display_name} {pending.entry.version}</h3><p className="mt-1 text-xs text-muted">Signed registry: {pending.registry.source.display_name} · runtime trust: {pending.runtime_preview.trust.state}</p></div>
              <Button size="sm" onClick={clearPreview}>Close</Button>
            </div>
            {pending.runtime_preview.blockers.length > 0 && <div className="mt-3 rounded-md border border-danger/40 bg-danger-soft p-2 text-xs text-danger">{pending.runtime_preview.blockers.join(" · ")}</div>}
            <div className="mt-4 space-y-2">
              <h4 className="text-xs font-semibold text-foreground">Exact permissions</h4>
              {pending.runtime_preview.permissions.length === 0 ? <p className="text-xs text-muted">This extension requests no host permissions.</p> : pending.runtime_preview.permissions.map((permission) => (
                <PermissionRow key={permission.permission_id} permission={permission} granted={grantedIds.has(permission.permission_id)} binding={bindings[permission.permission_id] ?? ""} onGranted={(value) => setGrantedIds((current) => { const next = new Set(current); if (value) next.add(permission.permission_id); else next.delete(permission.permission_id); return next; })} onBinding={(value) => setBindings((current) => ({ ...current, [permission.permission_id]: value }))} />
              ))}
            </div>
            {pending.runtime_preview.permission_diff && (
              <div className="mt-4 rounded-md border border-border bg-surface p-2 text-xs text-muted">
                Permission diff: +{pending.runtime_preview.permission_diff.added.length} / -{pending.runtime_preview.permission_diff.removed.length} / {pending.runtime_preview.permission_diff.unchanged.length} unchanged{pending.runtime_preview.permission_diff.expands_authority ? " · authority expands" : ""}
              </div>
            )}
            {pending.runtime_preview.requires_high_risk_approval && <label className="mt-4 flex gap-2 text-xs text-foreground"><input type="checkbox" checked={reviewHighRisk} onChange={(event) => setReviewHighRisk(event.target.checked)} /> I reviewed the high/critical-risk permissions above.</label>}
            {pending.runtime_preview.requires_untrusted_approval && <label className="mt-3 flex gap-2 text-xs text-foreground"><input type="checkbox" checked={reviewUntrusted} onChange={(event) => setReviewUntrusted(event.target.checked)} /> I understand the registry signature verifies distribution metadata, but this publisher is not trusted by my local executable-extension trust store.</label>}
            {pending.runtime_preview.requires_unsigned_approval && <p className="mt-3 rounded-md border border-danger/40 bg-danger-soft p-2 text-xs text-danger">Marketplace installation refuses unsigned network-delivered executable extensions. Install an unsigned local development extension only through the existing local-folder development flow.</p>}
            <div className="mt-4 flex justify-end gap-2"><Button onClick={clearPreview}>Cancel</Button><Button variant="primary" disabled={loading || pending.runtime_preview.blockers.length > 0 || pending.runtime_preview.requires_unsigned_approval || (pending.runtime_preview.requires_high_risk_approval && !reviewHighRisk) || (pending.runtime_preview.requires_untrusted_approval && !reviewUntrusted)} onClick={() => void applyInstall()}>Install with these grants</Button></div>
          </div>
        </div>
      )}
    </div>
  );
}
