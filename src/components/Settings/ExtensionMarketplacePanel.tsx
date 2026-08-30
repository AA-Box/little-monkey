import { useEffect, useMemo, useState } from "react";
import { Download, RefreshCw, Search, ShieldCheck, Store } from "lucide-react";

import type { PermissionGrant, PermissionRisk, PermissionView } from "../../lib/executableExtensionsClient";
import { useExtensionMarketplaceStore, type ExtensionUpdatePolicy } from "../../store/extensionMarketplaceStore";
import { Button, StatusPill, type PillTone } from "../ui";

const RISK_TONE: Record<PermissionRisk, PillTone> = {
  low: "neutral",
  medium: "warning",
  high: "danger",
  critical: "danger",
};

type Tab = "discover" | "registries" | "updates";

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
            <input value={binding} onChange={(event) => onBinding(event.target.value)} placeholder="Canonical workspace path to bind" className="mt-2 w-full rounded-md border border-border bg-surface px-2 py-1.5 text-xs text-foreground" />
          )}
        </div>
      </div>
    </div>
  );
}

export function ExtensionMarketplacePanel() {
  const registryRecords = useExtensionMarketplaceStore((state) => state.registryRecords);
  const catalog = useExtensionMarketplaceStore((state) => state.catalog);
  const installed = useExtensionMarketplaceStore((state) => state.installed);
  const updates = useExtensionMarketplaceStore((state) => state.updates);
  const updatePolicy = useExtensionMarketplaceStore((state) => state.updatePolicy);
  const pending = useExtensionMarketplaceStore((state) => state.pendingPreview);
  const loading = useExtensionMarketplaceStore((state) => state.loading);
  const error = useExtensionMarketplaceStore((state) => state.error);
  const notice = useExtensionMarketplaceStore((state) => state.notice);
  const hydrate = useExtensionMarketplaceStore((state) => state.hydrate);
  const refreshAll = useExtensionMarketplaceStore((state) => state.refreshAll);
  const previewEntry = useExtensionMarketplaceStore((state) => state.previewEntry);
  const clearPreview = useExtensionMarketplaceStore((state) => state.clearPreview);
  const applyPending = useExtensionMarketplaceStore((state) => state.applyPending);
  const setUpdatePolicy = useExtensionMarketplaceStore((state) => state.setUpdatePolicy);
  const applySafeUpdates = useExtensionMarketplaceStore((state) => state.applySafeUpdates);

  const [tab, setTab] = useState<Tab>("discover");
  const [query, setQuery] = useState("");
  const [grantedIds, setGrantedIds] = useState<Set<string>>(new Set());
  const [bindings, setBindings] = useState<Record<string, string>>({});
  const [reviewHighRisk, setReviewHighRisk] = useState(false);
  const [reviewUntrusted, setReviewUntrusted] = useState(false);
  const [modalError, setModalError] = useState<string | null>(null);

  useEffect(() => { void hydrate(); }, [hydrate]);
  useEffect(() => {
    setModalError(null);
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
    return catalog.filter((entry) => !needle || `${entry.extension_id} ${entry.registry_display_name} ${entry.version}`.toLowerCase().includes(needle));
  }, [catalog, query]);

  const submitApproval = async () => {
    if (!pending) return;
    const grants: PermissionGrant[] = [];
    for (const permission of pending.runtime_preview.permissions) {
      if (!grantedIds.has(permission.permission_id)) continue;
      const needsBinding = permission.kind === "workspace_read" || permission.kind === "workspace_write";
      const binding = needsBinding ? bindings[permission.permission_id]?.trim() ?? "" : "";
      if (needsBinding && !binding) {
        setModalError(`Workspace binding is required for ${permission.permission_id}.`);
        return;
      }
      grants.push({ permission_id: permission.permission_id, binding: binding || null });
    }
    setModalError(null);
    try {
      await applyPending(grants, reviewHighRisk, reviewUntrusted);
    } catch (caught) {
      setModalError(caught instanceof Error ? caught.message : String(caught));
    }
  };

  return (
    <div className="space-y-4">
      <section className="rounded-lg border border-border bg-surface p-4">
        <div className="flex flex-wrap items-start justify-between gap-3">
          <div className="max-w-2xl">
            <h3 className="flex items-center gap-2 text-sm font-semibold text-foreground"><Store size={16} /> Executable extensions</h3>
            <p className="mt-1 text-xs leading-5 text-muted">
              Executable WASM releases are indexed by the same signed M4 registries as the rest of Ecosystem. The registry binds immutable .lmx bytes; the existing executable-extension runtime independently verifies publisher trust, component checksums, compatibility and exact permission grants before install or update.
            </p>
          </div>
          <Button size="sm" disabled={loading} onClick={() => void refreshAll()}><RefreshCw size={13} className={loading ? "animate-spin" : ""} /> Refresh</Button>
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
            <input value={query} onChange={(event) => setQuery(event.target.value)} placeholder="Search extension id, registry or version…" className="w-full rounded-md border border-border bg-surface py-2 pl-8 pr-3 text-xs text-foreground" />
          </div>
          {registryRecords.length === 0 ? (
            <p className="rounded-lg border border-border bg-surface p-5 text-center text-xs text-muted">No M4 registry sources are configured. Add and verify a signed registry in Ecosystem first.</p>
          ) : visible.length === 0 ? (
            <p className="py-5 text-center text-xs text-muted">No executable extension releases were found in verified registry snapshots. Publishers opt in with the reserved <code>extension.&lt;id&gt;</code> package namespace.</p>
          ) : (
            <div className="grid gap-3 xl:grid-cols-2">
              {visible.map((entry) => {
                const existing = installed.find((item) => item.manifest.extension_id === entry.extension_id);
                return (
                  <article key={`${entry.registry_source_id}:${entry.extension_id}@${entry.version}`} className="rounded-lg border border-border bg-surface p-3">
                    <div className="flex items-start justify-between gap-3">
                      <div className="min-w-0">
                        <div className="flex flex-wrap items-center gap-2">
                          <h4 className="break-all text-sm font-semibold text-foreground">{entry.extension_id}</h4>
                          <StatusPill tone="success">M4 signed</StatusPill>
                          {entry.revoked && <StatusPill tone="danger">revoked</StatusPill>}
                        </div>
                        <p className="mt-1 text-[11px] text-faint">{entry.version} · {entry.registry_display_name} · {entry.package_sha256.slice(0, 12)}…</p>
                        {existing && <p className="mt-2 text-xs text-muted">Installed: {existing.manifest.display_name} {existing.active_version}</p>}
                        {entry.revocation_reason && <p className="mt-2 text-xs text-danger">{entry.revocation_reason}</p>}
                      </div>
                      <Button size="sm" variant="primary" disabled={loading || entry.revoked || existing?.active_version === entry.version} onClick={() => void previewEntry(entry)}>
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
          <div className="rounded-lg border border-border bg-surface p-3 text-xs text-muted">
            <div className="flex items-center gap-2 font-medium text-foreground"><ShieldCheck size={14} /> Shared M4 trust roots</div>
            <p className="mt-1">This view intentionally does not maintain a second executable-extension registry or key store. It consumes the same registry sources and Rust-verified snapshots used by Ecosystem packages.</p>
          </div>
          {registryRecords.length === 0 ? <p className="py-5 text-center text-xs text-muted">No registry sources configured.</p> : registryRecords.map((record) => (
            <article key={record.source.source_id} className="rounded-lg border border-border bg-surface p-3 text-xs">
              <div className="flex flex-wrap items-center gap-2">
                <span className="font-semibold text-foreground">{record.source.display_name}</span>
                <StatusPill tone={record.verified ? "success" : "warning"}>{record.verified ? "verified" : "not verified"}</StatusPill>
              </div>
              <p className="mt-1 break-all font-mono text-[11px] text-faint">{record.source.location}</p>
              {record.verified && <p className="mt-1 text-[11px] text-muted">sequence {record.verified.snapshot.sequence} · snapshot {record.verified.snapshot_sha256.slice(0, 16)}… · expires {new Date(record.verified.snapshot.expires_unix_ms).toLocaleString()}</p>}
              {record.last_verification_error && <p className="mt-2 text-danger">{record.last_verification_error}</p>}
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
            <p className="mt-2 text-[11px] text-muted">Automatic updates require the same verified publisher/key lineage, no authority expansion, no new risk acknowledgement, runtime compatibility, and no host-bound grants. Workspace-bound permissions always pause for manual review because the UI never reconstructs a canonical host path from its display label.</p>
          </div>
          {updates.length === 0 ? <p className="py-5 text-center text-xs text-muted">No newer executable extension release is currently available.</p> : updates.map((update) => (
            <article key={`${update.entry.extension_id}@${update.entry.version}`} className="rounded-lg border border-border bg-surface p-3 text-xs">
              <div className="flex flex-wrap items-center gap-2"><span className="font-semibold text-foreground">{update.entry.extension_id}</span><span className="text-muted">{update.installed.active_version} → {update.entry.version}</span><StatusPill tone={update.safe_auto_update ? "success" : "warning"}>{update.safe_auto_update ? "safe automatic" : "review required"}</StatusPill></div>
              {update.reasons.length > 0 && <p className="mt-2 text-muted">{update.reasons.join(" · ")}</p>}
              <Button className="mt-2" size="sm" onClick={() => void previewEntry(update.entry)}>Review update</Button>
            </article>
          ))}
        </section>
      )}

      {pending && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 p-4" role="dialog" aria-modal="true" aria-label="Review executable extension permissions">
          <div className="max-h-[85vh] w-full max-w-2xl overflow-y-auto rounded-xl border border-border bg-surface p-4 shadow-xl">
            <div className="flex items-start justify-between gap-3">
              <div>
                <h4 className="text-sm font-semibold text-foreground">{pending.mode === "update" ? "Review update" : "Review install"}: {pending.runtime_preview.manifest.display_name}</h4>
                <p className="mt-1 text-xs text-muted">{pending.runtime_preview.manifest.publisher} · {pending.runtime_preview.manifest.version} · runtime trust: {pending.runtime_preview.trust.state}</p>
                <p className="mt-1 text-[11px] text-faint">M4 snapshot {pending.entry.registry_snapshot_sha256.slice(0, 16)}… → package {pending.entry.package_sha256.slice(0, 16)}… → runtime manifest {pending.runtime_preview.trust.manifest_sha256.slice(0, 16)}…</p>
              </div>
              <Button size="sm" variant="secondary" onClick={clearPreview}>Close</Button>
            </div>

            {!pending.runtime_preview.compatible && <p className="mt-3 rounded-md border border-danger/40 bg-danger-soft p-2 text-xs text-danger">{pending.runtime_preview.compatibility_reason ?? "This release is incompatible."}</p>}
            {pending.runtime_preview.blockers.length > 0 && <p className="mt-3 rounded-md border border-danger/40 bg-danger-soft p-2 text-xs text-danger">{pending.runtime_preview.blockers.join(" · ")}</p>}
            {pending.runtime_preview.requires_unsigned_approval && <p className="mt-3 rounded-md border border-danger/40 bg-danger-soft p-2 text-xs text-danger">Unsigned network-delivered executable extensions are not installable from Marketplace.</p>}

            {pending.runtime_preview.permission_diff && (
              <div className="mt-3 rounded-md border border-border bg-background p-2 text-xs text-muted">
                Permission diff: +{pending.runtime_preview.permission_diff.added.length} / -{pending.runtime_preview.permission_diff.removed.length} / unchanged {pending.runtime_preview.permission_diff.unchanged.length}{pending.runtime_preview.permission_diff.expands_authority ? " · authority expands" : ""}
              </div>
            )}

            <div className="mt-3 space-y-2">
              {pending.runtime_preview.permissions.map((permission) => (
                <PermissionRow key={permission.permission_id} permission={permission} granted={grantedIds.has(permission.permission_id)} binding={bindings[permission.permission_id] ?? ""} onGranted={(value) => setGrantedIds((current) => { const next = new Set(current); if (value) next.add(permission.permission_id); else next.delete(permission.permission_id); return next; })} onBinding={(value) => setBindings((current) => ({ ...current, [permission.permission_id]: value }))} />
              ))}
            </div>

            {pending.runtime_preview.requires_high_risk_approval && <label className="mt-3 flex items-start gap-2 text-xs text-foreground"><input type="checkbox" checked={reviewHighRisk} onChange={(event) => setReviewHighRisk(event.target.checked)} /><span>I reviewed and accept the high/critical-risk permissions above.</span></label>}
            {pending.runtime_preview.requires_untrusted_approval && <label className="mt-3 flex items-start gap-2 text-xs text-foreground"><input type="checkbox" checked={reviewUntrusted} onChange={(event) => setReviewUntrusted(event.target.checked)} /><span>I understand the executable runtime does not currently trust this publisher key.</span></label>}
            {modalError && <p role="alert" className="mt-3 text-xs text-danger">{modalError}</p>}

            <div className="mt-4 flex justify-end gap-2">
              <Button variant="secondary" onClick={clearPreview}>Cancel</Button>
              <Button variant="primary" disabled={loading || !pending.runtime_preview.compatible || pending.runtime_preview.blockers.length > 0 || pending.runtime_preview.requires_unsigned_approval} onClick={() => void submitApproval()}>{pending.mode === "update" ? "Update with these grants" : "Install with these grants"}</Button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
