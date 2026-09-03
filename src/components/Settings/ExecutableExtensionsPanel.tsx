import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import {
  Activity,
  Boxes,
  Check,
  FileCode2,
  FolderOpen,
  Loader2,
  Play,
  RefreshCw,
  RotateCcw,
  ShieldAlert,
  Square,
  Trash2,
  Upload,
  Webhook,
} from "lucide-react";

import {
  executableExtensionsClient,
  type ConfigField,
  type ExtensionApproval,
  type ExtensionDetail,
  type ExtensionLogRow,
  type ExtensionPreview,
  type ExtensionWebhookStatus,
  type HealthState,
  type InstallSource,
  type PermissionGrant,
  type PermissionRisk,
  type PermissionView,
  type TrustState,
} from "../../lib/executableExtensionsClient";
import { errorMessage } from "../../lib/errors";
import { useT } from "../../lib/i18n";
import { Button, StatusPill, type PillTone } from "../ui";

const HEALTH_TONE: Record<HealthState, PillTone> = {
  not_validated: "neutral",
  stopped: "neutral",
  healthy: "success",
  degraded: "warning",
  unhealthy: "danger",
  protective_disabled: "danger",
};

const TRUST_TONE: Record<TrustState, PillTone> = {
  verified: "success",
  unsigned: "warning",
  untrusted: "danger",
  invalid: "danger",
};

const RISK_TONE: Record<PermissionRisk, PillTone> = {
  low: "neutral",
  medium: "warning",
  high: "danger",
  critical: "danger",
};

function sourceLabel(source: InstallSource): string {
  if ("local_folder" in source) return source.local_folder.canonical_path;
  if ("git" in source) return `${source.git.remote} @ ${source.git.commit_sha.slice(0, 12)}`;
  return `Registry: ${source.curated_registry.registry_id}`;
}

function configInputValue(value: unknown): string {
  if (value === null || value === undefined) return "";
  return typeof value === "string" ? value : JSON.stringify(value);
}

function parseConfigValue(field: ConfigField, value: string): unknown {
  if (field.kind === "boolean") return value === "true";
  if (field.kind === "integer") return Number.parseInt(value, 10);
  return value;
}

function permissionNeedsWorkspace(permission: PermissionView): boolean {
  return permission.kind === "workspace_read" || permission.kind === "workspace_write";
}

function DiffRows({ preview }: { preview: ExtensionPreview }) {
  const { t } = useT();
  const diff = preview.permission_diff;
  if (!diff) return null;
  const groups: { label: string; rows: PermissionView[]; classes: string }[] = [
    {
      label: t("ExecutableExtensions.diffAdded"),
      rows: diff.added,
      classes: "border-danger/40 bg-danger-soft text-danger",
    },
    {
      label: t("ExecutableExtensions.diffRemoved"),
      rows: diff.removed,
      classes: "border-success/40 bg-success-soft text-success",
    },
    {
      label: t("ExecutableExtensions.diffUnchanged"),
      rows: diff.unchanged,
      classes: "border-border bg-surface-2 text-muted",
    },
  ];
  return (
    <section aria-label={t("ExecutableExtensions.permissionDiff")} className="space-y-2">
      <h4 className="text-xs font-semibold text-foreground">
        {t("ExecutableExtensions.permissionDiff")}
      </h4>
      {groups.map((group) =>
        group.rows.length > 0 ? (
          <div key={group.label} className={`rounded-md border p-2 ${group.classes}`}>
            <p className="text-[11px] font-semibold uppercase tracking-wide">{group.label}</p>
            <ul className="mt-1 space-y-1 text-xs">
              {group.rows.map((row) => (
                <li key={`${group.label}-${row.permission_id}`}>
                  <span className="font-mono">{row.kind}</span> · {row.scope}
                </li>
              ))}
            </ul>
          </div>
        ) : null,
      )}
    </section>
  );
}

export function ExecutableExtensionsPanel() {
  const { t } = useT();
  const [items, setItems] = useState<ExtensionDetail[] | null>(null);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const selectedIdRef = useRef<string | null>(null);
  const draftsResetFor = useRef<string | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [preview, setPreview] = useState<ExtensionPreview | null>(null);
  const [previewMode, setPreviewMode] = useState<"install" | "update">("install");
  const [grantedIds, setGrantedIds] = useState<Set<string>>(new Set());
  const [workspaceBindings, setWorkspaceBindings] = useState<Record<string, string>>({});
  const [allowUnsigned, setAllowUnsigned] = useState(false);
  const [allowUntrusted, setAllowUntrusted] = useState(false);
  const [highRiskReviewed, setHighRiskReviewed] = useState(false);
  const [logs, setLogs] = useState<ExtensionLogRow[]>([]);
  const [configDraft, setConfigDraft] = useState<Record<string, string>>({});
  const [secretDraft, setSecretDraft] = useState<Record<string, string>>({});
  const [invokeCapability, setInvokeCapability] = useState("");
  const [invokeInput, setInvokeInput] = useState("{}");
  const [invokeOutput, setInvokeOutput] = useState<string | null>(null);
  const [webhooks, setWebhooks] = useState<ExtensionWebhookStatus[]>([]);
  const [webhookTriggerId, setWebhookTriggerId] = useState("");
  const [webhookHandlerId, setWebhookHandlerId] = useState("");
  const [webhookSecret, setWebhookSecret] = useState("");

  const selected = useMemo(
    () => items?.find((item) => item.manifest.extension_id === selectedId) ?? null,
    [items, selectedId],
  );

  const refresh = useCallback(async (silent = false) => {
    if (!silent) setLoadError(null);
    try {
      const next = await executableExtensionsClient.list();
      setItems(next);
      setLoadError(null);
      const current = selectedIdRef.current;
      const nextSelectedId = current && next.some((item) => item.manifest.extension_id === current)
        ? current
        : next[0]?.manifest.extension_id ?? null;
      selectedIdRef.current = nextSelectedId;
      setSelectedId(nextSelectedId);
    } catch (reason) {
      setLoadError(errorMessage(reason));
    }
  }, []);

  const refreshLogs = useCallback(async (extensionId: string) => {
    try {
      const next = await executableExtensionsClient.logs(extensionId, 100);
      if (selectedIdRef.current === extensionId) setLogs(next);
    } catch (reason) {
      if (selectedIdRef.current === extensionId) setActionError(errorMessage(reason));
    }
  }, []);

  const refreshWebhooks = useCallback(async (extensionId: string) => {
    try {
      const next = await executableExtensionsClient.webhooks(extensionId);
      if (selectedIdRef.current === extensionId) setWebhooks(next);
    } catch (reason) {
      if (selectedIdRef.current === extensionId) setActionError(errorMessage(reason));
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  useEffect(() => {
    if (!items?.some((item) => item.health.running)) return;
    const timer = window.setInterval(() => void refresh(true), 2_000);
    return () => window.clearInterval(timer);
  }, [items, refresh]);

  // Cleared while rendering the new selection, not from an effect. A passive
  // effect runs after its commit, so a reset belonging to the previous
  // selection lands after — and silently wipes — an approval or a secret
  // entered in between.
  if (draftsResetFor.current !== selectedId) {
    draftsResetFor.current = selectedId;
    setGrantedIds(new Set());
    setWorkspaceBindings({});
    setAllowUnsigned(false);
    setAllowUntrusted(false);
    setHighRiskReviewed(false);
    setConfigDraft({});
    setSecretDraft({});
    setInvokeCapability("");
    setInvokeInput("{}");
    setInvokeOutput(null);
    setLogs([]);
    setWebhooks([]);
    setWebhookTriggerId("");
    setWebhookHandlerId("");
    setWebhookSecret("");
  }

  useEffect(() => {
    if (preview && preview.manifest.extension_id !== selectedId) setPreview(null);
  }, [preview, selectedId]);

  useEffect(() => {
    if (!selected) {
      return;
    }
    setConfigDraft(
      Object.fromEntries(
        selected.manifest.config_schema.map((field) => [
          field.key,
          configInputValue(selected.config[field.key] ?? field.default),
        ]),
      ),
    );
    setInvokeCapability(selected.manifest.capabilities[0]?.capability_id ?? "");
    const firstWebhook = selected.permissions.find(
      (permission) => permission.kind === "webhook_receive" && permission.granted,
    );
    setWebhookHandlerId(firstWebhook?.scope ?? "");
    void refreshLogs(selected.manifest.extension_id);
    void refreshWebhooks(selected.manifest.extension_id);
  }, [refreshLogs, refreshWebhooks, selected]);

  const runAction = async (key: string, operation: () => Promise<unknown>, success: string) => {
    setBusy(key);
    setActionError(null);
    setNotice(null);
    try {
      await operation();
      setNotice(success);
      setPreview(null);
      await refresh(true);
    } catch (reason) {
      setActionError(errorMessage(reason));
    } finally {
      setBusy(null);
    }
  };

  const webhookHandlers = selected?.permissions.filter(
    (permission) => permission.kind === "webhook_receive" && permission.granted,
  ) ?? [];
  const missingWorkspaceBinding = preview?.permissions.some(
    (permission) => grantedIds.has(permission.permission_id)
      && permissionNeedsWorkspace(permission)
      && !workspaceBindings[permission.permission_id]?.trim(),
  ) ?? false;

  const selectExtension = (extensionId: string) => {
    selectedIdRef.current = extensionId;
    setActionError(null);
    setNotice(null);
    setSelectedId(extensionId);
  };

  const choosePreview = async (mode: "install" | "update") => {
    const updateTargetId = mode === "update" ? selectedIdRef.current : null;
    const path = await open({ directory: true, multiple: false });
    if (typeof path !== "string") return;
    setBusy(`preview-${mode}`);
    setActionError(null);
    setNotice(null);
    try {
      const next = mode === "install"
        ? await executableExtensionsClient.discover(path)
        : await executableExtensionsClient.previewUpdate(path);
      if (mode === "update" && updateTargetId !== selectedIdRef.current) {
        throw new Error(t("ExecutableExtensions.updateSelectionChanged"));
      }
      if (mode === "update" && updateTargetId && next.manifest.extension_id !== updateTargetId) {
        throw new Error(t("ExecutableExtensions.wrongUpdateSource"));
      }
      setPreview(next);
      setPreviewMode(mode);
      setGrantedIds(new Set(next.permissions.filter((permission) => permission.granted).map((permission) => permission.permission_id)));
      setWorkspaceBindings({});
      setAllowUnsigned(false);
      setAllowUntrusted(false);
      setHighRiskReviewed(false);
    } catch (reason) {
      setActionError(errorMessage(reason));
    } finally {
      setBusy(null);
    }
  };

  const approvalForPreview = (): ExtensionApproval | null => {
    if (!preview) return null;
    const grants: PermissionGrant[] = [];
    for (const permission of preview.permissions) {
      if (!grantedIds.has(permission.permission_id)) continue;
      const binding = permissionNeedsWorkspace(permission)
        ? workspaceBindings[permission.permission_id]?.trim() || null
        : null;
      if (permissionNeedsWorkspace(permission) && !binding) {
        setActionError(t("ExecutableExtensions.workspaceBindingRequired"));
        return null;
      }
      grants.push({ permission_id: permission.permission_id, binding });
    }
    return {
      approval_digest: preview.approval_digest,
      grants,
      allow_unsigned: allowUnsigned,
      allow_untrusted: allowUntrusted,
      allow_high_risk: highRiskReviewed,
    };
  };

  const applyPreview = async () => {
    if (!preview) return;
    if (previewMode === "update" && preview.manifest.extension_id !== selectedIdRef.current) {
      setPreview(null);
      setActionError(t("ExecutableExtensions.updateSelectionChanged"));
      return;
    }
    const approval = approvalForPreview();
    if (!approval) return;
    await runAction(
      previewMode,
      () => previewMode === "install"
        ? executableExtensionsClient.install(preview.source_path, approval)
        : executableExtensionsClient.update(preview.source_path, approval),
      previewMode === "install"
        ? t("ExecutableExtensions.installSuccess")
        : t("ExecutableExtensions.updateSuccess"),
    );
  };

  const saveConfig = async () => {
    if (!selected) return;
    const values: Record<string, unknown> = {};
    for (const field of selected.manifest.config_schema) {
      const raw = configDraft[field.key] ?? "";
      if (!raw && !field.required) continue;
      values[field.key] = parseConfigValue(field, raw);
    }
    await runAction(
      "config",
      () => executableExtensionsClient.setConfig(selected.manifest.extension_id, values),
      t("ExecutableExtensions.configSaved"),
    );
  };

  const runCapability = async () => {
    if (!selected || !invokeCapability) return;
    setBusy("invoke");
    setActionError(null);
    setInvokeOutput(null);
    try {
      JSON.parse(invokeInput);
      const result = await executableExtensionsClient.invoke({
        extension_id: selected.manifest.extension_id,
        capability_id: invokeCapability,
        input_json: invokeInput,
        invocation_id: null,
        input_artifact_ids: [],
        expected_kind: null,
        expected_version: null,
      });
      setInvokeOutput(result.output_json);
      await refresh(true);
      await refreshLogs(selected.manifest.extension_id);
    } catch (reason) {
      setActionError(errorMessage(reason));
    } finally {
      setBusy(null);
    }
  };

  if (items === null && !loadError) {
    return (
      <div role="status" className="flex min-h-48 items-center justify-center gap-2 text-sm text-muted">
        <Loader2 className="animate-spin motion-reduce:animate-none" size={16} />
        {t("ExecutableExtensions.loading")}
      </div>
    );
  }

  if (items === null && loadError) {
    return (
      <div role="alert" className="rounded-lg border border-danger/40 bg-danger-soft p-4 text-sm text-danger">
        <p>{loadError}</p>
        <Button className="mt-3" size="sm" onClick={() => void refresh()}>
          <RefreshCw size={13} /> {t("ExecutableExtensions.retry")}
        </Button>
      </div>
    );
  }

  return (
    <div className="space-y-4">
      <section className="rounded-lg border border-border bg-surface p-4">
        <div className="flex flex-wrap items-start justify-between gap-3">
          <div className="max-w-2xl">
            <h3 className="flex items-center gap-2 text-sm font-semibold text-foreground">
              <Boxes size={16} /> {t("ExecutableExtensions.title")}
            </h3>
            <p className="mt-1 text-xs leading-5 text-muted">
              {t("ExecutableExtensions.intro")}
            </p>
          </div>
          <div className="flex gap-2">
            <Button size="sm" disabled={busy !== null} onClick={() => void refresh()}>
              <RefreshCw size={13} /> {t("ExecutableExtensions.refresh")}
            </Button>
            <Button
              variant="primary"
              size="sm"
              disabled={busy !== null}
              onClick={() => void choosePreview("install")}
            >
              <FolderOpen size={13} /> {t("ExecutableExtensions.installFromFolder")}
            </Button>
          </div>
        </div>
      </section>

      {loadError && (
        <p role="alert" className="rounded-md border border-warning/40 bg-warning-soft px-3 py-2 text-xs text-warning">
          {loadError}
        </p>
      )}
      {actionError && (
        <p role="alert" className="rounded-md border border-danger/40 bg-danger-soft px-3 py-2 text-xs text-danger">
          {actionError}
        </p>
      )}
      {notice && (
        <p role="status" className="rounded-md border border-success/40 bg-success-soft px-3 py-2 text-xs text-success">
          {notice}
        </p>
      )}

      {preview && (
        <section className="rounded-lg border border-accent/40 bg-surface p-4" aria-label={t("ExecutableExtensions.reviewTitle")}>
          <div className="flex flex-wrap items-start justify-between gap-3">
            <div>
              <h3 className="text-sm font-semibold text-foreground">
                {t("ExecutableExtensions.reviewTitle")}: {preview.manifest.display_name} {preview.manifest.version}
              </h3>
            </div>
            <div className="flex gap-2">
              <StatusPill tone={TRUST_TONE[preview.trust.state]}>{preview.trust.state}</StatusPill>
              <StatusPill tone={preview.compatible ? "success" : "danger"}>
                {preview.compatible ? t("ExecutableExtensions.compatible") : t("ExecutableExtensions.incompatible")}
              </StatusPill>
            </div>
          </div>
          <p className="mt-2 text-xs text-muted">{preview.trust.reason}</p>
          <div className="mt-3 grid gap-3 rounded-md border border-border bg-background p-3 lg:grid-cols-2">
            <dl className="grid min-w-0 gap-x-3 gap-y-1.5 text-xs sm:grid-cols-[8rem_1fr]">
              <dt className="text-faint">{t("ExecutableExtensions.extensionId")}</dt>
              <dd className="break-all font-mono text-foreground">{preview.manifest.extension_id}</dd>
              <dt className="text-faint">{t("ExecutableExtensions.publisher")}</dt>
              <dd className="break-words text-foreground">{preview.manifest.publisher}</dd>
              <dt className="text-faint">{t("ExecutableExtensions.selectedSource")}</dt>
              <dd className="break-all text-muted">{preview.source_path}</dd>
              <dt className="text-faint">{t("ExecutableExtensions.declaredProvenance")}</dt>
              <dd className="break-all text-muted">{sourceLabel(preview.manifest.provenance.source)}</dd>
              <dt className="text-faint">{t("ExecutableExtensions.sourceRevision")}</dt>
              <dd className="break-all font-mono text-muted">{preview.manifest.provenance.source_revision}</dd>
              <dt className="text-faint">{t("ExecutableExtensions.signature")}</dt>
              <dd className="break-words text-muted">
                {preview.manifest.signature
                  ? `${preview.manifest.signature.algorithm} · ${preview.manifest.signature.trust_root_id}/${preview.manifest.signature.key_id}`
                  : t("ExecutableExtensions.none")}
              </dd>
              <dt className="text-faint">{t("ExecutableExtensions.sourceDigest")}</dt>
              <dd className="break-all font-mono text-[11px] text-muted">{preview.source_digest}</dd>
              <dt className="text-faint">{t("ExecutableExtensions.approvalDigest")}</dt>
              <dd className="break-all font-mono text-[11px] text-muted">{preview.approval_digest}</dd>
            </dl>
            <section aria-label={t("ExecutableExtensions.capabilities")}>
              <h4 className="text-xs font-semibold text-foreground">{t("ExecutableExtensions.capabilities")}</h4>
              <ul className="mt-2 space-y-1.5">
                {preview.manifest.capabilities.map((capability) => (
                  <li key={`${capability.kind}:${capability.capability_id}`} className="rounded border border-border bg-surface px-2 py-1.5 text-xs">
                    <span className="font-medium text-foreground">{capability.display_name}</span>
                    <span className="ml-2 font-mono text-faint">{capability.kind}:{capability.capability_id}</span>
                  </li>
                ))}
              </ul>
            </section>
          </div>
          {preview.blockers.length > 0 && (
            <ul className="mt-3 rounded-md border border-danger/40 bg-danger-soft p-2 text-xs text-danger">
              {preview.blockers.map((blocker) => <li key={blocker}>• {blocker}</li>)}
            </ul>
          )}

          <div className="mt-4 grid gap-4 lg:grid-cols-2">
            <section>
              <h4 className="text-xs font-semibold text-foreground">{t("ExecutableExtensions.requestedPermissions")}</h4>
              {preview.permissions.length === 0 ? (
                <p className="mt-2 text-xs text-muted">{t("ExecutableExtensions.noPermissions")}</p>
              ) : (
                <div className="mt-2 space-y-2">
                  {preview.permissions.map((permission) => (
                    <label key={permission.permission_id} className="block rounded-md border border-border bg-background p-2.5 text-xs">
                      <span className="flex items-start gap-2">
                        <input
                          type="checkbox"
                          checked={grantedIds.has(permission.permission_id)}
                          onChange={(event) => {
                            setGrantedIds((current) => {
                              const next = new Set(current);
                              if (event.target.checked) next.add(permission.permission_id);
                              else next.delete(permission.permission_id);
                              return next;
                            });
                          }}
                          className="mt-0.5 accent-accent"
                        />
                        <span className="min-w-0 flex-1">
                          <span className="flex flex-wrap items-center gap-1.5 font-medium text-foreground">
                            {permission.kind}
                            <StatusPill tone={RISK_TONE[permission.risk]}>{permission.risk}</StatusPill>
                          </span>
                          <span className="mt-1 block break-all font-mono text-faint">{permission.scope}</span>
                          <span className="mt-1 block text-muted">{permission.reason}</span>
                        </span>
                      </span>
                      {grantedIds.has(permission.permission_id) && permissionNeedsWorkspace(permission) && (
                        <span className="mt-2 flex gap-2">
                          <input
                            readOnly
                            value={workspaceBindings[permission.permission_id] ?? ""}
                            placeholder={t("ExecutableExtensions.chooseWorkspace")}
                            className="h-8 min-w-0 flex-1 rounded-md border border-border bg-surface px-2 text-xs text-foreground"
                          />
                          <Button
                            size="sm"
                            onClick={async (event) => {
                              event.preventDefault();
                              const path = await open({ directory: true, multiple: false });
                              if (typeof path === "string") {
                                setWorkspaceBindings((current) => ({ ...current, [permission.permission_id]: path }));
                              }
                            }}
                          >
                            <FolderOpen size={12} /> {t("ExecutableExtensions.choose")}
                          </Button>
                        </span>
                      )}
                      {grantedIds.has(permission.permission_id)
                        && permissionNeedsWorkspace(permission)
                        && permission.binding_label
                        && !workspaceBindings[permission.permission_id]
                        && (
                          <span className="mt-1 block text-[11px] text-warning">
                            {t("ExecutableExtensions.workspaceReselect")} {permission.binding_label}
                          </span>
                        )}
                    </label>
                  ))}
                </div>
              )}
            </section>
            <DiffRows preview={preview} />
          </div>

          <div className="mt-4 space-y-2 text-xs">
            {preview.requires_unsigned_approval && (
              <label className="flex items-start gap-2 rounded-md border border-warning/40 bg-warning-soft p-2.5 text-warning">
                <input type="checkbox" checked={allowUnsigned} onChange={(event) => setAllowUnsigned(event.target.checked)} className="mt-0.5" />
                {t("ExecutableExtensions.unsignedApproval")}
              </label>
            )}
            {preview.requires_untrusted_approval && (
              <label className="flex items-start gap-2 rounded-md border border-danger/40 bg-danger-soft p-2.5 text-danger">
                <input type="checkbox" checked={allowUntrusted} onChange={(event) => setAllowUntrusted(event.target.checked)} className="mt-0.5" />
                {t("ExecutableExtensions.untrustedApproval")}
              </label>
            )}
            {preview.requires_high_risk_approval && (
              <label className="flex items-start gap-2 rounded-md border border-danger/40 bg-danger-soft p-2.5 text-danger">
                <input type="checkbox" checked={highRiskReviewed} onChange={(event) => setHighRiskReviewed(event.target.checked)} className="mt-0.5" />
                <ShieldAlert className="shrink-0" size={14} /> {t("ExecutableExtensions.highRiskApproval")}
              </label>
            )}
          </div>

          <div className="mt-4 flex justify-end gap-2">
            <Button size="sm" onClick={() => setPreview(null)} disabled={busy !== null}>
              {t("ExecutableExtensions.cancel")}
            </Button>
            <Button
              variant="primary"
              size="sm"
              disabled={
                busy !== null
                || preview.blockers.length > 0
                || missingWorkspaceBinding
                || (preview.requires_unsigned_approval && !allowUnsigned)
                || (preview.requires_untrusted_approval && !allowUntrusted)
                || (preview.requires_high_risk_approval && !highRiskReviewed)
              }
              onClick={() => void applyPreview()}
            >
              {busy === previewMode ? <Loader2 className="animate-spin motion-reduce:animate-none" size={13} /> : <Check size={13} />}
              {previewMode === "install" ? t("ExecutableExtensions.install") : t("ExecutableExtensions.update")}
            </Button>
          </div>
        </section>
      )}

      {items?.length === 0 ? (
        <section className="rounded-lg border border-dashed border-border p-8 text-center">
          <FileCode2 className="mx-auto text-faint" size={28} />
          <h3 className="mt-3 text-sm font-medium text-foreground">{t("ExecutableExtensions.emptyTitle")}</h3>
          <p className="mt-1 text-xs text-muted">{t("ExecutableExtensions.emptyBody")}</p>
        </section>
      ) : (
        <div className="grid min-h-[30rem] gap-4 lg:grid-cols-[15rem_minmax(0,1fr)]">
          <nav aria-label={t("ExecutableExtensions.installedList")} className="space-y-1 rounded-lg border border-border bg-surface p-2">
            {items?.map((item) => (
              <button
                key={item.manifest.extension_id}
                type="button"
                disabled={busy !== null}
                onClick={() => selectExtension(item.manifest.extension_id)}
                className={`w-full rounded-md border px-3 py-2.5 text-left transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent ${selectedId === item.manifest.extension_id ? "border-accent/50 bg-accent-soft" : "border-transparent hover:bg-surface-2"}`}
              >
                <span className="block truncate text-xs font-medium text-foreground">{item.manifest.display_name}</span>
                <span className="mt-1 flex flex-wrap gap-1">
                  <StatusPill tone={TRUST_TONE[item.trust.state]}>{item.trust.state}</StatusPill>
                  <StatusPill tone={HEALTH_TONE[item.health.state]}>{item.health.state}</StatusPill>
                </span>
              </button>
            ))}
          </nav>

          {selected && (
            <article className="min-w-0 space-y-4 rounded-lg border border-border bg-surface p-4">
              <header className="flex flex-wrap items-start justify-between gap-3">
                <div>
                  <h3 className="text-base font-semibold text-foreground">{selected.manifest.display_name}</h3>
                  <p className="font-mono text-xs text-faint">{selected.manifest.extension_id} · {selected.active_version}</p>
                  <p className="mt-1 text-xs text-muted">{selected.manifest.publisher}</p>
                </div>
                <div className="flex flex-wrap gap-1.5">
                  <StatusPill tone={TRUST_TONE[selected.trust.state]}>{selected.trust.state}</StatusPill>
                  <StatusPill tone={selected.compatible ? "success" : "danger"}>
                    {selected.compatible ? t("ExecutableExtensions.compatible") : t("ExecutableExtensions.incompatible")}
                  </StatusPill>
                  <StatusPill tone={HEALTH_TONE[selected.health.state]}>{selected.health.state}</StatusPill>
                </div>
              </header>

              <p className="text-xs leading-5 text-muted">{selected.manifest.description}</p>
              {selected.blockers.length > 0 && (
                <ul className="rounded-md border border-danger/40 bg-danger-soft p-2.5 text-xs text-danger">
                  {selected.blockers.map((blocker) => <li key={blocker}>• {blocker}</li>)}
                </ul>
              )}

              <section className="rounded-md border border-border bg-background p-3" aria-label={t("ExecutableExtensions.runtimeHealth")}>
                <h4 className="text-xs font-semibold text-foreground">{t("ExecutableExtensions.runtimeHealth")}</h4>
                <dl className="mt-2 grid gap-x-3 gap-y-2 text-xs sm:grid-cols-[10rem_1fr]">
                  <dt className="text-faint">{t("ExecutableExtensions.enabledState")}</dt>
                  <dd className="text-foreground">{selected.health.enabled ? t("ExecutableExtensions.yes") : t("ExecutableExtensions.no")}</dd>
                  <dt className="text-faint">{t("ExecutableExtensions.validated")}</dt>
                  <dd className="text-foreground">{selected.health.validated ? t("ExecutableExtensions.yes") : t("ExecutableExtensions.no")}</dd>
                  <dt className="text-faint">{t("ExecutableExtensions.running")}</dt>
                  <dd className="text-foreground">{selected.health.running ? t("ExecutableExtensions.yes") : t("ExecutableExtensions.no")}</dd>
                  <dt className="text-faint">{t("ExecutableExtensions.consecutiveFailures")}</dt>
                  <dd className="font-mono text-foreground">{selected.health.consecutive_failures}</dd>
                  <dt className="text-faint">{t("ExecutableExtensions.traps")}</dt>
                  <dd className="font-mono text-foreground">{selected.health.trap_count}</dd>
                  <dt className="text-faint">{t("ExecutableExtensions.deniedAttempts")}</dt>
                  <dd className="font-mono text-foreground">{selected.health.undeclared_attempts}</dd>
                  {selected.health.last_error && (
                    <>
                      <dt className="text-faint">{t("ExecutableExtensions.lastError")}</dt>
                      <dd className="break-words text-danger">{selected.health.last_error}</dd>
                    </>
                  )}
                </dl>
              </section>

              <div className="flex flex-wrap gap-2 border-y border-border py-3">
                <Button size="sm" disabled={busy !== null} onClick={() => void runAction("validate", () => executableExtensionsClient.validate(selected.manifest.extension_id), t("ExecutableExtensions.validationSuccess"))}>
                  <Activity size={13} /> {t("ExecutableExtensions.validate")}
                </Button>
                {selected.allowed_actions.includes("enable") && (
                  <Button size="sm" disabled={busy !== null} onClick={() => void runAction("enable", () => executableExtensionsClient.setEnabled(selected.manifest.extension_id, true), t("ExecutableExtensions.enabled"))}>
                    {t("ExecutableExtensions.enable")}
                  </Button>
                )}
                {selected.allowed_actions.includes("disable") && (
                  <Button size="sm" disabled={busy !== null} onClick={() => void runAction("disable", () => executableExtensionsClient.setEnabled(selected.manifest.extension_id, false), t("ExecutableExtensions.disabled"))}>
                    {t("ExecutableExtensions.disable")}
                  </Button>
                )}
                {selected.allowed_actions.includes("start") && (
                  <Button variant="primary" size="sm" disabled={busy !== null} onClick={() => void runAction("start", () => executableExtensionsClient.setRunning(selected.manifest.extension_id, true), t("ExecutableExtensions.started"))}>
                    <Play size={13} /> {t("ExecutableExtensions.start")}
                  </Button>
                )}
                {selected.allowed_actions.includes("stop") && (
                  <Button size="sm" disabled={busy !== null} onClick={() => void runAction("stop", () => executableExtensionsClient.setRunning(selected.manifest.extension_id, false), t("ExecutableExtensions.stopped"))}>
                    <Square size={12} /> {t("ExecutableExtensions.stop")}
                  </Button>
                )}
                <Button size="sm" disabled={busy !== null} onClick={() => void choosePreview("update")}>
                  <Upload size={13} /> {t("ExecutableExtensions.update")}
                </Button>
                {selected.allowed_actions.includes("rollback") && (
                  <Button size="sm" disabled={busy !== null} onClick={() => {
                    if (window.confirm(t("ExecutableExtensions.rollbackConfirm"))) {
                      void runAction("rollback", async () => {
                        if (selected.health.running) {
                          await executableExtensionsClient.setRunning(selected.manifest.extension_id, false);
                        }
                        await executableExtensionsClient.rollback(selected.manifest.extension_id);
                      }, t("ExecutableExtensions.rolledBack"));
                    }
                  }}>
                    <RotateCcw size={13} /> {t("ExecutableExtensions.rollback")}
                  </Button>
                )}
                <Button variant="danger" size="sm" disabled={busy !== null} onClick={() => {
                  if (window.confirm(t("ExecutableExtensions.uninstallConfirm"))) {
                    void runAction("uninstall", async () => {
                      if (selected.health.running) {
                        await executableExtensionsClient.setRunning(selected.manifest.extension_id, false);
                      }
                      await executableExtensionsClient.uninstall(selected.manifest.extension_id);
                    }, t("ExecutableExtensions.uninstalled"));
                  }
                }}>
                  <Trash2 size={13} /> {t("ExecutableExtensions.uninstall")}
                </Button>
              </div>

              <div className="grid gap-4 xl:grid-cols-2">
                <section>
                  <h4 className="text-xs font-semibold text-foreground">{t("ExecutableExtensions.capabilities")}</h4>
                  <div className="mt-2 space-y-2">
                    {selected.manifest.capabilities.map((capability) => (
                      <div key={`${capability.kind}:${capability.capability_id}`} className="rounded-md border border-border bg-background p-2.5 text-xs">
                        <div className="flex items-center justify-between gap-2">
                          <span className="font-medium text-foreground">{capability.display_name}</span>
                          <StatusPill>{capability.kind}</StatusPill>
                        </div>
                        <p className="mt-1 font-mono text-faint">{capability.capability_id}</p>
                        <p className="mt-1 text-muted">{capability.description}</p>
                      </div>
                    ))}
                  </div>
                </section>

                <section>
                  <h4 className="text-xs font-semibold text-foreground">{t("ExecutableExtensions.permissions")}</h4>
                  {selected.permissions.length === 0 ? (
                    <p className="mt-2 text-xs text-muted">{t("ExecutableExtensions.noPermissions")}</p>
                  ) : (
                    <div className="mt-2 space-y-2">
                      {selected.permissions.map((permission) => (
                        <div key={permission.permission_id} className="rounded-md border border-border bg-background p-2.5 text-xs">
                          <div className="flex flex-wrap items-center gap-1.5">
                            <span className="font-mono text-foreground">{permission.kind}</span>
                            <StatusPill tone={RISK_TONE[permission.risk]}>{permission.risk}</StatusPill>
                            <StatusPill tone={permission.granted ? "success" : "neutral"}>
                              {permission.granted ? t("ExecutableExtensions.granted") : t("ExecutableExtensions.notGranted")}
                            </StatusPill>
                          </div>
                          <p className="mt-1 break-all font-mono text-faint">{permission.scope}</p>
                          <p className="mt-1 text-muted">{permission.reason}</p>
                        </div>
                      ))}
                    </div>
                  )}
                </section>
              </div>

              {selected.manifest.config_schema.length > 0 && (
                <section className="rounded-md border border-border bg-background p-3">
                  <h4 className="text-xs font-semibold text-foreground">{t("ExecutableExtensions.configuration")}</h4>
                  <div className="mt-3 grid gap-3 sm:grid-cols-2">
                    {selected.manifest.config_schema.map((field) => (
                      <label key={field.key} className="text-xs text-muted">
                        <span className="mb-1 block text-foreground">{field.label}{field.required ? " *" : ""}</span>
                        {field.kind === "boolean" ? (
                          <select value={configDraft[field.key] ?? "false"} onChange={(event) => setConfigDraft((current) => ({ ...current, [field.key]: event.target.value }))} className="h-9 w-full rounded-md border border-border bg-surface px-2 text-foreground">
                            <option value="false">False</option><option value="true">True</option>
                          </select>
                        ) : field.kind === "select" ? (
                          <select value={configDraft[field.key] ?? ""} onChange={(event) => setConfigDraft((current) => ({ ...current, [field.key]: event.target.value }))} className="h-9 w-full rounded-md border border-border bg-surface px-2 text-foreground">
                            {field.options.map((option) => <option key={option} value={option}>{option}</option>)}
                          </select>
                        ) : (
                          <input type={field.kind === "integer" ? "number" : "text"} value={configDraft[field.key] ?? ""} min={field.minimum ?? undefined} max={field.maximum ?? undefined} onChange={(event) => setConfigDraft((current) => ({ ...current, [field.key]: event.target.value }))} className="h-9 w-full rounded-md border border-border bg-surface px-2 text-foreground" />
                        )}
                        <span className="mt-1 block text-faint">{field.description}</span>
                      </label>
                    ))}
                  </div>
                  <Button className="mt-3" size="sm" disabled={busy !== null} onClick={() => void saveConfig()}>{t("ExecutableExtensions.saveConfig")}</Button>
                </section>
              )}

              {selected.secret_slots.length > 0 && (
                <section className="rounded-md border border-border bg-background p-3">
                  <h4 className="text-xs font-semibold text-foreground">{t("ExecutableExtensions.secrets")}</h4>
                  <p className="mt-1 text-xs text-muted">{t("ExecutableExtensions.secretsIntro")}</p>
                  <div className="mt-3 space-y-3">
                    {selected.secret_slots.map((slot) => (
                      <div key={slot.slot_id} className="rounded-md border border-border bg-surface p-2.5">
                        <div className="flex flex-wrap items-center gap-2 text-xs">
                          <span className="font-medium text-foreground">{slot.label}</span>
                          <StatusPill tone={slot.configured ? "success" : "neutral"}>{slot.configured ? t("ExecutableExtensions.configured") : t("ExecutableExtensions.missing")}</StatusPill>
                        </div>
                        <p className="mt-1 text-xs text-muted">{slot.description}</p>
                        <div className="mt-2 flex flex-wrap gap-2">
                          <input type="password" autoComplete="new-password" value={secretDraft[slot.slot_id] ?? ""} onChange={(event) => setSecretDraft((current) => ({ ...current, [slot.slot_id]: event.target.value }))} placeholder={t("ExecutableExtensions.secretPlaceholder")} className="h-9 min-w-52 flex-1 rounded-md border border-border bg-background px-2 text-xs text-foreground" />
                          <Button size="sm" disabled={busy !== null || !(secretDraft[slot.slot_id] ?? "")} onClick={() => void runAction(`secret-${slot.slot_id}`, async () => {
                            await executableExtensionsClient.setSecret(selected.manifest.extension_id, slot.slot_id, secretDraft[slot.slot_id] ?? "");
                            setSecretDraft((current) => ({ ...current, [slot.slot_id]: "" }));
                          }, t("ExecutableExtensions.secretSaved"))}>{t("ExecutableExtensions.saveSecret")}</Button>
                          {slot.configured && <Button variant="danger" size="sm" disabled={busy !== null} onClick={() => void runAction(`secret-remove-${slot.slot_id}`, () => executableExtensionsClient.removeSecret(selected.manifest.extension_id, slot.slot_id), t("ExecutableExtensions.secretCleared"))}>{t("ExecutableExtensions.clear")}</Button>}
                        </div>
                      </div>
                    ))}
                  </div>
                </section>
              )}

              {webhookHandlers.length > 0 && (
                <section className="rounded-md border border-border bg-background p-3">
                  <h4 className="flex items-center gap-2 text-xs font-semibold text-foreground">
                    <Webhook size={14} /> {t("ExecutableExtensions.webhooks")}
                  </h4>
                  <p className="mt-1 text-xs text-muted">{t("ExecutableExtensions.webhooksIntro")}</p>
                  <div className="mt-3 grid gap-2 sm:grid-cols-[1fr_1fr_1fr_auto]">
                    <input
                      value={webhookTriggerId}
                      onChange={(event) => setWebhookTriggerId(event.target.value)}
                      placeholder={t("ExecutableExtensions.webhookTriggerId")}
                      className="h-9 rounded-md border border-border bg-surface px-2 text-xs text-foreground"
                    />
                    <select
                      value={webhookHandlerId}
                      onChange={(event) => setWebhookHandlerId(event.target.value)}
                      className="h-9 rounded-md border border-border bg-surface px-2 text-xs text-foreground"
                    >
                      {webhookHandlers.map((permission) => (
                        <option key={permission.permission_id} value={permission.scope}>{permission.scope}</option>
                      ))}
                    </select>
                    <input
                      type="password"
                      autoComplete="new-password"
                      value={webhookSecret}
                      onChange={(event) => setWebhookSecret(event.target.value)}
                      placeholder={t("ExecutableExtensions.webhookSecret")}
                      className="h-9 rounded-md border border-border bg-surface px-2 text-xs text-foreground"
                    />
                    <Button
                      size="sm"
                      disabled={busy !== null || !webhookTriggerId.trim() || !webhookHandlerId || !webhookSecret}
                      onClick={() => void runAction(
                        "webhook-register",
                        async () => {
                          await executableExtensionsClient.registerWebhook(
                            webhookTriggerId.trim(),
                            selected.manifest.extension_id,
                            webhookHandlerId,
                            webhookSecret,
                          );
                          setWebhookSecret("");
                        },
                        t("ExecutableExtensions.webhookRegistered"),
                      )}
                    >
                      {t("ExecutableExtensions.register")}
                    </Button>
                  </div>
                  <p className="mt-2 text-[11px] text-faint">{t("ExecutableExtensions.webhookHeaders")}</p>
                  <div className="mt-3 space-y-2">
                    {webhooks.length === 0 ? (
                      <p className="text-xs text-muted">{t("ExecutableExtensions.noWebhooks")}</p>
                    ) : webhooks.map((webhook) => (
                      <div key={webhook.trigger_id} className="flex flex-wrap items-center justify-between gap-2 rounded-md border border-border bg-surface p-2.5 text-xs">
                        <div className="min-w-0">
                          <p className="font-medium text-foreground">{webhook.trigger_id} · {webhook.handler_id}</p>
                          <p className="mt-1 break-all font-mono text-faint">POST /v1/triggers/{webhook.trigger_id} · v{webhook.version}</p>
                        </div>
                        <div className="flex items-center gap-2">
                          <StatusPill tone={webhook.enabled ? "success" : "neutral"}>
                            {webhook.enabled ? t("ExecutableExtensions.webhookEnabled") : t("ExecutableExtensions.webhookDisabled")}
                          </StatusPill>
                          <Button
                            variant="danger"
                            size="sm"
                            disabled={busy !== null}
                            onClick={() => {
                              if (window.confirm(t("ExecutableExtensions.removeWebhookConfirm"))) {
                                void runAction(
                                  `webhook-remove-${webhook.trigger_id}`,
                                  () => executableExtensionsClient.removeWebhook(webhook.trigger_id, selected.manifest.extension_id),
                                  t("ExecutableExtensions.webhookRemoved"),
                                );
                              }
                            }}
                          >
                            {t("ExecutableExtensions.remove")}
                          </Button>
                        </div>
                      </div>
                    ))}
                  </div>
                </section>
              )}

              <section className="rounded-md border border-border bg-background p-3">
                <h4 className="text-xs font-semibold text-foreground">{t("ExecutableExtensions.runCapability")}</h4>
                <div className="mt-2 grid gap-2 sm:grid-cols-[12rem_1fr_auto]">
                  <select value={invokeCapability} onChange={(event) => setInvokeCapability(event.target.value)} className="h-9 rounded-md border border-border bg-surface px-2 text-xs text-foreground">
                    {selected.manifest.capabilities.map((capability) => <option key={capability.capability_id} value={capability.capability_id}>{capability.display_name}</option>)}
                  </select>
                  <input value={invokeInput} onChange={(event) => setInvokeInput(event.target.value)} className="h-9 rounded-md border border-border bg-surface px-2 font-mono text-xs text-foreground" aria-label={t("ExecutableExtensions.inputJson")} />
                  <Button variant="primary" size="sm" disabled={busy !== null || selected.health.state !== "healthy"} onClick={() => void runCapability()}>
                    {busy === "invoke" ? <Loader2 className="animate-spin motion-reduce:animate-none" size={13} /> : <Play size={13} />} {t("ExecutableExtensions.run")}
                  </Button>
                </div>
                {invokeOutput !== null && <pre className="mt-2 max-h-40 overflow-auto rounded bg-surface p-2 text-xs text-foreground">{invokeOutput}</pre>}
              </section>

              <details className="rounded-md border border-border bg-background p-3 text-xs">
                <summary className="cursor-pointer font-semibold text-foreground">{t("ExecutableExtensions.provenance")}</summary>
                <dl className="mt-3 grid gap-x-3 gap-y-2 sm:grid-cols-[8rem_1fr]">
                  <dt className="text-faint">{t("ExecutableExtensions.source")}</dt><dd className="break-all text-muted">{sourceLabel(selected.installed_source)}</dd>
                  <dt className="text-faint">{t("ExecutableExtensions.declaredProvenance")}</dt><dd className="break-all text-muted">{sourceLabel(selected.manifest.provenance.source)}</dd>
                  <dt className="text-faint">Manifest SHA-256</dt><dd className="break-all font-mono text-muted">{selected.trust.manifest_sha256}</dd>
                  <dt className="text-faint">Component SHA-256</dt><dd className="break-all font-mono text-muted">{selected.trust.component_sha256}</dd>
                  <dt className="text-faint">{t("ExecutableExtensions.signature")}</dt><dd className="text-muted">{selected.manifest.signature ? `${selected.manifest.signature.algorithm} · ${selected.manifest.signature.trust_root_id}/${selected.manifest.signature.key_id}` : t("ExecutableExtensions.none")}</dd>
                  <dt className="text-faint">{t("ExecutableExtensions.rollbackVersion")}</dt><dd className="text-muted">{selected.previous_version ?? t("ExecutableExtensions.none")}</dd>
                </dl>
              </details>

              <section>
                <div className="flex items-center justify-between gap-2">
                  <h4 className="text-xs font-semibold text-foreground">{t("ExecutableExtensions.logs")}</h4>
                  <Button size="sm" disabled={busy !== null} onClick={() => void refreshLogs(selected.manifest.extension_id)}><RefreshCw size={12} /> {t("ExecutableExtensions.refresh")}</Button>
                </div>
                <div className="mt-2 max-h-64 overflow-auto rounded-md border border-border bg-background">
                  {logs.length === 0 ? <p className="p-3 text-xs text-muted">{t("ExecutableExtensions.noLogs")}</p> : logs.map((row, index) => (
                    <div key={`${row.at_ms}-${index}`} className="grid grid-cols-[7rem_4rem_1fr] gap-2 border-b border-border px-2.5 py-2 text-[11px] last:border-b-0">
                      <span className="text-faint">{new Date(row.at_ms).toLocaleTimeString()}</span>
                      <span className={row.level === "error" ? "text-danger" : row.level === "warn" ? "text-warning" : "text-muted"}>{row.level}</span>
                      <span className="break-words text-foreground">{row.message}</span>
                    </div>
                  ))}
                </div>
                <p className="mt-1 text-[11px] text-faint">{t("ExecutableExtensions.logsBounded")}</p>
              </section>
            </article>
          )}
        </div>
      )}
    </div>
  );
}
