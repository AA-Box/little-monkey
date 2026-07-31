import { useMemo, useState } from "react";
import { Download, FileDown, FileUp, PackageCheck, Pin, RefreshCw, RotateCcw, Search, ShieldCheck, Trash2 } from "lucide-react";
import { open, save } from "@tauri-apps/plugin-dialog";
import { readTextFile, stat, writeTextFile } from "@tauri-apps/plugin-fs";
import { Button, StatusPill } from "../ui";
import { ecosystemClient, type InstalledPackageState, type PackageCatalogEntry, type PackagePermission, type PortablePackageExport } from "../../lib/ecosystemClient";
import { useT } from "../../lib/i18n";
import { useEcosystemStore } from "../../store/ecosystemStore";
import { errorMessage } from "../../lib/errors";

type PreviewIntent = "install" | "update";

function versionParts(version: string): number[] {
  return version.split(/[.+-]/).map((part) => Number.parseInt(part, 10) || 0);
}

function compareVersions(left: string, right: string): number {
  const a = versionParts(left);
  const b = versionParts(right);
  for (let index = 0; index < Math.max(a.length, b.length); index += 1) {
    const difference = (a[index] ?? 0) - (b[index] ?? 0);
    if (difference !== 0) return difference;
  }
  return 0;
}

function PermissionList({ permissions, empty }: { permissions: PackagePermission[]; empty: string }) {
  if (permissions.length === 0) return <p className="text-xs text-muted">{empty}</p>;
  return (
    <ul className="space-y-2">
      {permissions.map((permission) => (
        <li key={`${permission.permission_id}:${permission.scope}`} className="rounded-lg border border-border bg-surface-2 p-2.5">
          <div className="flex flex-wrap items-center gap-2">
            <code className="text-xs font-medium text-foreground">{permission.permission_id}</code>
            <span className="rounded bg-surface px-1.5 py-0.5 text-[11px] text-muted">{permission.kind}</span>
            <span className="break-all text-[11px] text-faint">{permission.scope}</span>
          </div>
          <p className="mt-1 text-xs text-muted">{permission.reason}</p>
        </li>
      ))}
    </ul>
  );
}

function newestCatalogEntry(entries: PackageCatalogEntry[]): PackageCatalogEntry | undefined {
  return [...entries].sort((a, b) => compareVersions(b.manifest.version, a.manifest.version))[0];
}

export function EcosystemPackages({ view }: { view: "marketplace" | "installed" }) {
  const { t } = useT();
  const {
    catalog,
    installed,
    installPreview,
    busy,
    previewPackage,
    importPortablePackage,
    installPackage,
    updatePackage,
    setPackageEnabled,
    pinPackage,
    rollbackPackage,
    uninstallPackage,
  } = useEcosystemStore();
  const [query, setQuery] = useState("");
  const [expectedImportDigest, setExpectedImportDigest] = useState("");
  const [previewIntent, setPreviewIntent] = useState<PreviewIntent>("install");
  const [confirmUninstall, setConfirmUninstall] = useState<string | null>(null);

  const entries = useMemo(() => {
    const normalized = query.trim().toLowerCase();
    return catalog.filter(({ manifest }) => !normalized || [manifest.display_name, manifest.package_id, manifest.description, manifest.kind]
      .some((value) => value.toLowerCase().includes(normalized)));
  }, [catalog, query]);

  const installedById = useMemo(
    () => new Map(installed.map((item) => [item.package_id, item])),
    [installed],
  );
  const catalogById = useMemo(() => {
    const map = new Map<string, PackageCatalogEntry[]>();
    for (const entry of catalog) map.set(entry.manifest.package_id, [...(map.get(entry.manifest.package_id) ?? []), entry]);
    return map;
  }, [catalog]);

  async function openPreview(entry: PackageCatalogEntry, intent: PreviewIntent) {
    setPreviewIntent(intent);
    await previewPackage(entry.manifest.package_id, entry.manifest.version);
  }

  async function exportInstalled(item: InstalledPackageState) {
    const destination = await save({
      title: t("EcosystemPackages.exportTitle"),
      defaultPath: `${item.package_id.replace(/[^a-zA-Z0-9._-]/g, "-")}.little-monkey-package.json`,
      filters: [{ name: "Little Monkey package", extensions: ["json"] }],
    });
    if (!destination) return;
    const portable = await ecosystemClient.exportPackage(item.package_id);
    await writeTextFile(destination, `${JSON.stringify(portable, null, 2)}\n`);
  }

  async function importPortable() {
    try {
      const selected = await open({
        title: t("EcosystemPackages.importTitle"),
        multiple: false,
        directory: false,
        filters: [{ name: "Little Monkey package", extensions: ["json"] }],
      });
      if (typeof selected !== "string") return;
      const fileInfo = await stat(selected);
      if (fileInfo.size > 270 * 1024 * 1024) {
        throw new Error(t("EcosystemPackages.portableTooLarge"));
      }
      const value = JSON.parse(await readTextFile(selected)) as Partial<PortablePackageExport>;
      if (
        value.schema_version !== 1
        || typeof value.bundle_sha256 !== "string"
        || !/^[a-fA-F0-9]{64}$/.test(value.bundle_sha256)
        || !value.manifest
        || typeof value.manifest.package_id !== "string"
        || typeof value.manifest.version !== "string"
        || !value.files_hex
        || typeof value.files_hex !== "object"
      ) {
        throw new Error(t("EcosystemPackages.invalidPortable"));
      }
      const current = installedById.get(value.manifest.package_id);
      setPreviewIntent(current?.active_version ? "update" : "install");
      const expected = expectedImportDigest.trim().toLowerCase();
      if (expected && !/^[a-f0-9]{64}$/.test(expected)) {
        throw new Error(t("EcosystemPackages.invalidDigestPin"));
      }
      await importPortablePackage(
        value as PortablePackageExport,
        expected || value.bundle_sha256.toLowerCase(),
      );
    } catch (error) {
      useEcosystemStore.setState({ error: errorMessage(error) });
    }
  }

  const previewDiff = installPreview?.preview.permission_diff;

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center gap-2">
        <div className="relative min-w-64 max-w-md flex-1">
          <Search size={15} className="pointer-events-none absolute left-3 top-1/2 -translate-y-1/2 text-faint" />
          <input
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            aria-label={t("EcosystemPackages.searchLabel")}
            placeholder={t("EcosystemPackages.searchPlaceholder")}
            className="h-9 w-full rounded-lg border border-border bg-surface pl-9 pr-3 text-sm text-foreground placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
          />
        </div>
        {view === "marketplace" && (
          <>
            <label className="min-w-64 flex-1 text-xs text-muted sm:max-w-sm">
              <span className="sr-only">{t("EcosystemPackages.digestPinLabel")}</span>
              <input
                value={expectedImportDigest}
                onChange={(event) => setExpectedImportDigest(event.target.value)}
                maxLength={64}
                spellCheck={false}
                placeholder={t("EcosystemPackages.digestPinPlaceholder")}
                className="h-9 w-full rounded-lg border border-border bg-surface px-3 font-mono text-xs text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
              />
            </label>
            <Button size="sm" disabled={busy["package-import"]} onClick={() => void importPortable()}>
              <FileUp size={14} /> {t("EcosystemPackages.importPortable")}
            </Button>
          </>
        )}
      </div>

      {view === "marketplace" ? (
        <div className="grid gap-3 md:grid-cols-2">
          {entries.map((entry) => {
            const current = installedById.get(entry.manifest.package_id);
            const isCurrent = current?.active_version === entry.manifest.version && !current.tombstoned;
            return (
              <article key={`${entry.manifest.package_id}@${entry.manifest.version}`} className="flex min-h-48 flex-col rounded-xl border border-border bg-surface p-4">
                <div className="flex items-start justify-between gap-3">
                  <div className="min-w-0">
                    <h3 className="truncate text-sm font-semibold text-foreground">{entry.manifest.display_name}</h3>
                    <p className="mt-0.5 truncate font-mono text-[11px] text-faint">{entry.manifest.package_id} · v{entry.manifest.version}</p>
                  </div>
                  <StatusPill tone={entry.trust?.signed ? "success" : "warning"}>
                    {entry.trust?.signed ? t("EcosystemPackages.signed") : t("EcosystemPackages.unverified")}
                  </StatusPill>
                </div>
                <p className="mt-3 line-clamp-3 text-xs leading-5 text-muted">{entry.manifest.description}</p>
                <div className="mt-3 flex flex-wrap gap-1.5 text-[11px] text-muted">
                  <span className="rounded bg-surface-2 px-2 py-1">{entry.manifest.kind}</span>
                  <span className="rounded bg-surface-2 px-2 py-1">{t("EcosystemPackages.permissionCount", { count: entry.manifest.permissions.length })}</span>
                  <span className="rounded bg-surface-2 px-2 py-1">{entry.manifest.provenance.publisher}</span>
                </div>
                <div className="mt-auto pt-4">
                  <Button
                    variant={isCurrent ? "secondary" : "primary"}
                    size="sm"
                    disabled={!entry.available || isCurrent || busy["package-preview"]}
                    onClick={() => void openPreview(entry, current?.active_version ? "update" : "install")}
                  >
                    {isCurrent ? <PackageCheck size={14} /> : <Download size={14} />}
                    {isCurrent ? t("EcosystemPackages.installed") : current?.active_version ? t("EcosystemPackages.reviewUpdate") : t("EcosystemPackages.reviewInstall")}
                  </Button>
                  {!entry.available && <p className="mt-2 text-xs text-danger">{entry.validation_error ?? t("EcosystemPackages.unavailable")}</p>}
                </div>
              </article>
            );
          })}
        </div>
      ) : (
        <div className="space-y-3">
          {installed
            .filter((item) => !item.tombstoned)
            .filter((item) => !query.trim() || item.package_id.toLowerCase().includes(query.trim().toLowerCase()))
            .map((item) => {
              const available = newestCatalogEntry(catalogById.get(item.package_id) ?? []);
              const canUpdate = Boolean(available && item.active_version && compareVersions(available.manifest.version, item.active_version) > 0);
              const rollbackVersions = item.activation_history.filter((version, index, all) => all.indexOf(version) === index && version !== item.active_version);
              return (
                <article key={item.package_id} className="rounded-xl border border-border bg-surface p-4">
                  <div className="flex flex-wrap items-start justify-between gap-3">
                    <div>
                      <div className="flex flex-wrap items-center gap-2">
                        <h3 className="text-sm font-semibold text-foreground">{item.package_id}</h3>
                        <StatusPill tone={item.revoked || item.tombstoned ? "danger" : item.enabled ? "success" : "neutral"}>
                          {item.tombstoned ? t("EcosystemPackages.uninstalled") : item.revoked ? t("EcosystemPackages.revoked") : item.enabled ? t("EcosystemPackages.enabled") : t("EcosystemPackages.disabled")}
                        </StatusPill>
                      </div>
                      <p className="mt-1 text-xs text-muted">
                        {t("EcosystemPackages.activeVersion", { version: item.active_version ?? "—" })} · {t("EcosystemPackages.cachedVersions", { count: Object.keys(item.versions).length })}
                      </p>
                    </div>
                    <label className="flex min-h-8 items-center gap-2 text-xs text-muted">
                      <input
                        type="checkbox"
                        checked={item.enabled && !item.tombstoned}
                        disabled={item.tombstoned || busy[`package-enable-${item.package_id}`]}
                        onChange={(event) => void setPackageEnabled(item.package_id, event.target.checked)}
                        className="h-4 w-4 accent-accent"
                      />
                      {t("EcosystemPackages.enabled")}
                    </label>
                  </div>

                  <div className="mt-4 flex flex-wrap items-end gap-2">
                    <label className="text-xs text-muted">
                      <span className="mb-1 block">{t("EcosystemPackages.pinVersion")}</span>
                      <select
                        value={item.pinned_version ?? ""}
                        disabled={item.tombstoned || busy[`package-pin-${item.package_id}`]}
                        onChange={(event) => void pinPackage(item.package_id, event.target.value || null)}
                        className="h-8 rounded-md border border-border bg-surface-2 px-2 text-xs text-foreground focus:outline-none focus:ring-2 focus:ring-accent"
                      >
                        <option value="">{t("EcosystemPackages.followLatest")}</option>
                        {Object.keys(item.versions).sort(compareVersions).reverse().map((version) => <option key={version} value={version}>v{version}</option>)}
                      </select>
                    </label>
                    {canUpdate && available && (
                      <Button size="sm" onClick={() => void openPreview(available, "update")}>
                        <RefreshCw size={14} /> {t("EcosystemPackages.updateTo", { version: available.manifest.version })}
                      </Button>
                    )}
                    <Button size="sm" disabled={rollbackVersions.length === 0 || item.tombstoned} onClick={() => void rollbackPackage(item.package_id)}>
                      <RotateCcw size={14} /> {t("EcosystemPackages.rollback")}
                    </Button>
                    <Button size="sm" onClick={() => void exportInstalled(item)} disabled={item.tombstoned}>
                      <FileDown size={14} /> {t("EcosystemPackages.export")}
                    </Button>
                    {confirmUninstall === item.package_id ? (
                      <span className="inline-flex items-center gap-2 rounded-lg border border-danger/30 bg-danger-soft px-2 py-1">
                        <span className="text-xs text-danger">{t("EcosystemPackages.confirmUninstall")}</span>
                        <Button size="sm" variant="danger" onClick={() => { setConfirmUninstall(null); void uninstallPackage(item.package_id); }}>{t("EcosystemPackages.uninstall")}</Button>
                        <Button size="sm" variant="ghost" onClick={() => setConfirmUninstall(null)}>{t("EcosystemPackages.cancel")}</Button>
                      </span>
                    ) : (
                      <Button size="sm" variant="ghost" disabled={item.tombstoned} onClick={() => setConfirmUninstall(item.package_id)}>
                        <Trash2 size={14} /> {t("EcosystemPackages.uninstall")}
                      </Button>
                    )}
                  </div>
                  {item.pinned_version && <p className="mt-3 flex items-center gap-1 text-xs text-warning"><Pin size={12} />{t("EcosystemPackages.pinnedNotice", { version: item.pinned_version })}</p>}
                </article>
              );
            })}
        </div>
      )}

      {((view === "marketplace" && entries.length === 0) || (view === "installed" && installed.every((item) => item.tombstoned))) && (
        <div className="rounded-xl border border-dashed border-border p-8 text-center text-sm text-muted">
          {view === "marketplace" ? t("EcosystemPackages.noMarketplaceResults") : t("EcosystemPackages.noInstalledPackages")}
        </div>
      )}

      {installPreview && (
        <div role="dialog" aria-modal="true" aria-labelledby="package-preview-title" className="fixed inset-0 z-[70] flex items-center justify-center bg-black/50 p-4" onClick={() => useEcosystemStore.setState({ installPreview: null })}>
          <div
            tabIndex={-1}
            autoFocus
            className="max-h-[82vh] w-full max-w-2xl overflow-y-auto rounded-xl border border-border bg-background p-5 shadow-2xl focus:outline-none"
            onClick={(event) => event.stopPropagation()}
            onKeyDown={(event) => {
              if (event.key === "Escape") {
                event.preventDefault();
                event.stopPropagation();
                useEcosystemStore.setState({ installPreview: null });
              }
            }}
          >
            <div className="flex items-start justify-between gap-4">
              <div>
                <h3 id="package-preview-title" className="text-base font-semibold text-foreground">
                  {previewIntent === "update" ? t("EcosystemPackages.updatePreviewTitle") : t("EcosystemPackages.installPreviewTitle")}
                </h3>
                <p className="mt-1 font-mono text-xs text-muted">{installPreview.preview.package_id} · v{installPreview.preview.version}</p>
              </div>
              <StatusPill tone={installPreview.preview.trust.signed ? "success" : "danger"}>
                <ShieldCheck size={12} /> {installPreview.preview.trust.signed ? t("EcosystemPackages.signatureVerified") : t("EcosystemPackages.signatureMissing")}
              </StatusPill>
            </div>
            <dl className="mt-4 grid gap-2 rounded-lg bg-surface p-3 text-xs sm:grid-cols-3">
              <div><dt className="text-faint">{t("EcosystemPackages.bundleDigest")}</dt><dd className="mt-1 truncate font-mono text-foreground" title={installPreview.preview.bundle_sha256}>{installPreview.preview.bundle_sha256}</dd></div>
              <div><dt className="text-faint">{t("EcosystemPackages.files")}</dt><dd className="mt-1 text-foreground">{installPreview.preview.file_count}</dd></div>
              <div><dt className="text-faint">{t("EcosystemPackages.size")}</dt><dd className="mt-1 text-foreground">{installPreview.preview.total_bytes.toLocaleString()} B</dd></div>
            </dl>
            {previewDiff ? (
              <div className="mt-5 grid gap-4 md:grid-cols-3">
                <section><h4 className="mb-2 text-xs font-semibold text-danger">{t("EcosystemPackages.permissionsAdded", { count: previewDiff.added.length })}</h4><PermissionList permissions={previewDiff.added} empty={t("EcosystemPackages.none")} /></section>
                <section><h4 className="mb-2 text-xs font-semibold text-success">{t("EcosystemPackages.permissionsRemoved", { count: previewDiff.removed.length })}</h4><PermissionList permissions={previewDiff.removed} empty={t("EcosystemPackages.none")} /></section>
                <section><h4 className="mb-2 text-xs font-semibold text-muted">{t("EcosystemPackages.permissionsUnchanged", { count: previewDiff.unchanged.length })}</h4><PermissionList permissions={previewDiff.unchanged} empty={t("EcosystemPackages.none")} /></section>
              </div>
            ) : (
              <section className="mt-5"><h4 className="mb-2 text-xs font-semibold text-foreground">{t("EcosystemPackages.requestedPermissions")}</h4><PermissionList permissions={installPreview.preview.permissions} empty={t("EcosystemPackages.noPermissions")} /></section>
            )}
            {installPreview.preview.mcp_actions_separate.length > 0 && (
              <div className="mt-4 rounded-lg border border-warning/30 bg-warning-soft p-3 text-xs text-warning">
                {t("EcosystemPackages.mcpSeparate", { count: installPreview.preview.mcp_actions_separate.length })}
              </div>
            )}
            {installPreview.preview.warnings.map((warning) => <p key={warning} className="mt-2 text-xs text-warning">{warning}</p>)}
            <div className="mt-5 flex justify-end gap-2">
              <Button variant="ghost" onClick={() => useEcosystemStore.setState({ installPreview: null })}>{t("EcosystemPackages.deny")}</Button>
              <Button
                variant="primary"
                disabled={busy["package-install"] || busy["package-update"]}
                onClick={() => void (previewIntent === "install"
                  ? installPackage(true)
                  : updatePackage(installPreview.preview.package_id, installPreview.preview.version, true))}
              >
                {previewIntent === "install" ? t("EcosystemPackages.approveInstall") : t("EcosystemPackages.approveUpdate")}
              </Button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
