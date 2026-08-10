import { useEffect, useMemo, useState } from "react";
import {
  Bug,
  Download,
  Loader2,
  PackageCheck,
  PlusCircle,
  Search,
  ShieldCheck,
  ShieldOff,
  Trash2,
  Users,
} from "lucide-react";
import { Button, StatusPill } from "../ui";
import type { PillTone } from "../ui/StatusPill";
import { useT } from "../../lib/i18n";
import { useEcosystemStore } from "../../store/ecosystemStore";
import type { AdditionalRegistryRecord, PackageCatalogEntry, RegistrySnapshot, VulnerabilitySeverity } from "../../lib/ecosystemClient";
import {
  DEFAULT_DISCOVER_FILTERS,
  canApproveInstall,
  distinctKinds,
  distinctPublishers,
  filterCatalogEntries,
  localInstallCountFor,
  splitTeamCollectionsFirst,
  worstVulnerabilitySeverity,
  type DiscoverFilters,
  type TrustFilter,
} from "../../lib/ecosystemDiscover";
import { errorMessage } from "../../lib/errors";
import { ResolutionSection } from "./PackageResolution";

type PreviewIntent = "install" | "update";
type DiscoverView = "browse" | "registries";

const SEVERITY_TONE: Record<VulnerabilitySeverity, PillTone> = {
  low: "neutral",
  medium: "warning",
  high: "danger",
  critical: "danger",
};

function describeInstallSource(source: Record<string, unknown>): { kind: string; detail: string } {
  if ("curated_registry" in source) {
    const value = source.curated_registry as { registry_id?: string } | undefined;
    return { kind: "curated_registry", detail: value?.registry_id ?? "" };
  }
  if ("git" in source) {
    const value = source.git as { remote?: string; commit_sha?: string } | undefined;
    return { kind: "git", detail: `${value?.remote ?? ""} @ ${(value?.commit_sha ?? "").slice(0, 12)}` };
  }
  if ("local_folder" in source) {
    const value = source.local_folder as { canonical_path?: string } | undefined;
    return { kind: "local_folder", detail: value?.canonical_path ?? "" };
  }
  return { kind: "unknown", detail: JSON.stringify(source) };
}

function RegistriesView({ onBack }: { onBack: () => void }) {
  const { t } = useT();
  const { registrySources, busy, addRegistrySource, removeRegistrySource, verifyRegistrySource } = useEcosystemStore();
  const [sourceId, setSourceId] = useState("");
  const [displayName, setDisplayName] = useState("");
  const [location, setLocation] = useState("");
  const [snapshotDrafts, setSnapshotDrafts] = useState<Record<string, string>>({});
  const [confirmRemove, setConfirmRemove] = useState<string | null>(null);

  async function registerSource() {
    if (!sourceId.trim() || !displayName.trim() || !location.trim()) return;
    try {
      await addRegistrySource(sourceId.trim(), displayName.trim(), location.trim());
      setSourceId("");
      setDisplayName("");
      setLocation("");
    } catch (error) {
      useEcosystemStore.setState({ error: errorMessage(error) });
    }
  }

  async function verifySource(record: AdditionalRegistryRecord) {
    const draft = snapshotDrafts[record.source.source_id] ?? "";
    try {
      const snapshot = JSON.parse(draft) as RegistrySnapshot;
      await verifyRegistrySource(record.source.source_id, snapshot);
    } catch (error) {
      useEcosystemStore.setState({
        error: error instanceof Error ? error.message : t("EcosystemDiscover.invalidSnapshotJson"),
      });
    }
  }

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between gap-3">
        <div>
          <h3 className="text-sm font-semibold text-foreground">{t("EcosystemDiscover.registriesTitle")}</h3>
          <p className="mt-1 max-w-2xl text-xs leading-5 text-muted">{t("EcosystemDiscover.registriesDescription")}</p>
        </div>
        <Button size="sm" variant="ghost" onClick={onBack}>{t("EcosystemDiscover.backToDiscover")}</Button>
      </div>

      <div className="rounded-xl border border-border bg-surface p-4">
        <h4 className="text-xs font-semibold text-foreground">{t("EcosystemDiscover.addRegistryTitle")}</h4>
        <div className="mt-3 grid gap-2 sm:grid-cols-3">
          <label className="text-xs text-muted">
            <span className="mb-1 block">{t("EcosystemDiscover.registrySourceId")}</span>
            <input value={sourceId} onChange={(event) => setSourceId(event.target.value)} className="h-9 w-full rounded-md border border-border bg-surface-2 px-2 text-xs text-foreground focus:outline-none focus:ring-2 focus:ring-accent" />
          </label>
          <label className="text-xs text-muted">
            <span className="mb-1 block">{t("EcosystemDiscover.registryDisplayName")}</span>
            <input value={displayName} onChange={(event) => setDisplayName(event.target.value)} className="h-9 w-full rounded-md border border-border bg-surface-2 px-2 text-xs text-foreground focus:outline-none focus:ring-2 focus:ring-accent" />
          </label>
          <label className="text-xs text-muted">
            <span className="mb-1 block">{t("EcosystemDiscover.registryLocation")}</span>
            <input value={location} onChange={(event) => setLocation(event.target.value)} placeholder={t("EcosystemDiscover.registryLocationPlaceholder")} className="h-9 w-full rounded-md border border-border bg-surface-2 px-2 text-xs text-foreground placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent" />
          </label>
        </div>
        <div className="mt-3">
          <Button size="sm" disabled={busy["registry-source-add"]} onClick={() => void registerSource()}>
            <PlusCircle size={14} /> {t("EcosystemDiscover.registerSource")}
          </Button>
        </div>
      </div>

      {registrySources.length === 0 ? (
        <div className="rounded-xl border border-dashed border-border p-8 text-center text-sm text-muted">
          {t("EcosystemDiscover.noRegistries")}
        </div>
      ) : (
        <div className="space-y-3">
          {registrySources.map((record) => {
            const packageCount = record.verified ? Object.keys(record.verified.snapshot.packages).length : 0;
            return (
              <article key={record.source.source_id} className="rounded-xl border border-border bg-surface p-4">
                <div className="flex flex-wrap items-start justify-between gap-3">
                  <div className="min-w-0">
                    <div className="flex flex-wrap items-center gap-2">
                      <h4 className="text-sm font-semibold text-foreground">{record.source.display_name}</h4>
                      <StatusPill tone={record.verified ? "success" : record.last_verification_error ? "danger" : "neutral"}>
                        {record.verified ? t("EcosystemDiscover.registryVerified") : record.last_verification_error ? t("EcosystemDiscover.registryError") : t("EcosystemDiscover.registryNeverVerified")}
                      </StatusPill>
                    </div>
                    <p className="mt-0.5 truncate font-mono text-[11px] text-faint">{record.source.source_id} · {record.source.location}</p>
                    {record.verified && (
                      <p className="mt-1 text-xs text-muted">{t("EcosystemDiscover.registryPackages", { count: packageCount })}</p>
                    )}
                    {record.last_verification_error && (
                      <p className="mt-1 whitespace-pre-wrap break-words text-xs text-danger">{record.last_verification_error}</p>
                    )}
                  </div>
                  {confirmRemove === record.source.source_id ? (
                    <span className="inline-flex shrink-0 items-center gap-2 rounded-lg border border-danger/30 bg-danger-soft px-2 py-1">
                      <span className="text-xs text-danger">{t("EcosystemDiscover.confirmRemoveRegistry")}</span>
                      <Button size="sm" variant="danger" onClick={() => { setConfirmRemove(null); void removeRegistrySource(record.source.source_id); }}>
                        {t("EcosystemDiscover.removeRegistry")}
                      </Button>
                      <Button size="sm" variant="ghost" onClick={() => setConfirmRemove(null)}>{t("EcosystemDiscover.cancel")}</Button>
                    </span>
                  ) : (
                    <Button size="sm" variant="ghost" onClick={() => setConfirmRemove(record.source.source_id)}>
                      <Trash2 size={14} /> {t("EcosystemDiscover.removeRegistry")}
                    </Button>
                  )}
                </div>
                <div className="mt-3">
                  <label className="text-xs text-muted">
                    <span className="mb-1 block">{t("EcosystemDiscover.registrySnapshotJson")}</span>
                    <textarea
                      value={snapshotDrafts[record.source.source_id] ?? ""}
                      onChange={(event) => setSnapshotDrafts((state) => ({ ...state, [record.source.source_id]: event.target.value }))}
                      placeholder={t("EcosystemDiscover.registrySnapshotPlaceholder")}
                      rows={3}
                      className="w-full rounded-md border border-border bg-surface-2 p-2 font-mono text-[11px] text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
                    />
                  </label>
                  <div className="mt-2">
                    <Button size="sm" disabled={busy[`registry-source-verify-${record.source.source_id}`]} onClick={() => void verifySource(record)}>
                      <ShieldCheck size={14} /> {record.verified ? t("EcosystemDiscover.reverify") : t("EcosystemDiscover.addAndVerify")}
                    </Button>
                  </div>
                </div>
              </article>
            );
          })}
        </div>
      )}
    </div>
  );
}

export function EcosystemDiscover() {
  const { t } = useT();
  const {
    catalog,
    installed,
    installPreview,
    busy,
    previewPackage,
    installPackage,
    updatePackage,
    setPackageTeamApproved,
    refreshRegistrySources,
  } = useEcosystemStore();
  const [view, setView] = useState<DiscoverView>("browse");
  const [filters, setFilters] = useState<DiscoverFilters>(DEFAULT_DISCOVER_FILTERS);
  const [previewIntent, setPreviewIntent] = useState<PreviewIntent>("install");
  const [reviewed, setReviewed] = useState(false);

  useEffect(() => { void refreshRegistrySources(); }, [refreshRegistrySources]);

  const installedById = useMemo(() => new Map(installed.map((item) => [item.package_id, item])), [installed]);
  const filtered = useMemo(() => filterCatalogEntries(catalog, filters), [catalog, filters]);
  const { teamCollections, rest } = useMemo(
    () => splitTeamCollectionsFirst(filtered, installedById),
    [filtered, installedById],
  );
  const kinds = useMemo(() => distinctKinds(catalog), [catalog]);
  const publishers = useMemo(() => distinctPublishers(catalog), [catalog]);

  async function openPreview(entry: PackageCatalogEntry, intent: PreviewIntent) {
    setReviewed(false);
    setPreviewIntent(intent);
    await previewPackage(entry.manifest.package_id, entry.manifest.version);
  }

  function closePreview() {
    setReviewed(false);
    useEcosystemStore.setState({ installPreview: null });
  }

  function renderEntry(entry: PackageCatalogEntry) {
    const current = installedById.get(entry.manifest.package_id);
    const isCurrent = current?.active_version === entry.manifest.version && !current.tombstoned;
    const installCount = localInstallCountFor(entry, installedById);
    const severity = worstVulnerabilitySeverity(entry.manifest.vulnerability_notices);
    const isCollection = entry.manifest.kind === "collection";
    const canToggleTeamApproved = isCollection && current && !current.tombstoned;
    return (
      <article key={`${entry.manifest.package_id}@${entry.manifest.version}`} className="flex min-h-52 flex-col rounded-xl border border-border bg-surface p-4">
        <div className="flex items-start justify-between gap-3">
          <div className="min-w-0">
            <h3 className="truncate text-sm font-semibold text-foreground">{entry.manifest.display_name}</h3>
            <p className="mt-0.5 truncate font-mono text-[11px] text-faint">{entry.manifest.package_id} · v{entry.manifest.version}</p>
          </div>
          <StatusPill tone={entry.trust?.signed ? "success" : "warning"}>
            {entry.trust?.signed ? t("EcosystemDiscover.trustSigned") : t("EcosystemDiscover.trustUnsigned")}
          </StatusPill>
        </div>
        <p className="mt-3 line-clamp-3 text-xs leading-5 text-muted">{entry.manifest.description}</p>
        <div className="mt-3 flex flex-wrap gap-1.5 text-[11px] text-muted">
          <span className="rounded bg-surface-2 px-2 py-1">{entry.manifest.kind}</span>
          <span className="rounded bg-surface-2 px-2 py-1">{entry.manifest.provenance.publisher}</span>
          <span className="rounded bg-surface-2 px-2 py-1">
            {installCount === null ? t("EcosystemDiscover.installCountNone") : t("EcosystemDiscover.installCount", { count: installCount })}
          </span>
          {severity && (
            <span className="inline-flex">
              <StatusPill tone={SEVERITY_TONE[severity]}>
                <Bug size={11} /> {t("EcosystemDiscover.vulnerabilityBadge", { count: entry.manifest.vulnerability_notices?.length ?? 0 })}
              </StatusPill>
            </span>
          )}
          {isCollection && current?.team_approved && (
            <span className="inline-flex"><StatusPill tone="success"><Users size={11} /> {t("EcosystemDiscover.teamApproved")}</StatusPill></span>
          )}
        </div>
        <div className="mt-auto flex flex-wrap items-center gap-2 pt-4">
          <Button
            variant={isCurrent ? "secondary" : "primary"}
            size="sm"
            disabled={!entry.available || isCurrent || busy["package-preview"]}
            onClick={() => void openPreview(entry, current?.active_version ? "update" : "install")}
          >
            {isCurrent ? <PackageCheck size={14} /> : <Download size={14} />}
            {isCurrent ? t("EcosystemPackages.installed") : current?.active_version ? t("EcosystemPackages.reviewUpdate") : t("EcosystemPackages.reviewInstall")}
          </Button>
          {canToggleTeamApproved && (
            <Button
              size="sm"
              variant="ghost"
              disabled={busy[`package-team-approved-${entry.manifest.package_id}`]}
              onClick={() => void setPackageTeamApproved(entry.manifest.package_id, !current?.team_approved)}
              title={t("EcosystemDiscover.teamApprovalNonGoal")}
            >
              <Users size={14} /> {current?.team_approved ? t("EcosystemDiscover.unmarkTeamApproved") : t("EcosystemDiscover.markTeamApproved")}
            </Button>
          )}
        </div>
        {!entry.available && <p className="mt-2 text-xs text-danger">{entry.validation_error ?? t("EcosystemPackages.unavailable")}</p>}
      </article>
    );
  }

  if (view === "registries") {
    return <RegistriesView onBack={() => setView("browse")} />;
  }

  const previewDiff = installPreview?.preview.permission_diff;
  const previewSource = installPreview ? describeInstallSource(installPreview.preview.source) : null;
  const currentInstalledForPreview = installPreview ? installedById.get(installPreview.preview.package_id) : undefined;

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center gap-2">
        <div className="relative min-w-64 max-w-md flex-1">
          <Search size={15} className="pointer-events-none absolute left-3 top-1/2 -translate-y-1/2 text-faint" />
          <input
            value={filters.query}
            onChange={(event) => setFilters((state) => ({ ...state, query: event.target.value }))}
            aria-label={t("EcosystemDiscover.searchLabel")}
            placeholder={t("EcosystemDiscover.searchPlaceholder")}
            className="h-9 w-full rounded-lg border border-border bg-surface pl-9 pr-3 text-sm text-foreground placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
          />
        </div>
        <select
          value={filters.kind}
          onChange={(event) => setFilters((state) => ({ ...state, kind: event.target.value }))}
          aria-label={t("EcosystemDiscover.filterKind")}
          className="h-9 rounded-lg border border-border bg-surface px-2 text-xs text-foreground focus:outline-none focus:ring-2 focus:ring-accent"
        >
          <option value="any">{t("EcosystemDiscover.filterAllKinds")}</option>
          {kinds.map((kind) => <option key={kind} value={kind}>{kind}</option>)}
        </select>
        <select
          value={filters.publisher}
          onChange={(event) => setFilters((state) => ({ ...state, publisher: event.target.value }))}
          aria-label={t("EcosystemDiscover.filterPublisher")}
          className="h-9 rounded-lg border border-border bg-surface px-2 text-xs text-foreground focus:outline-none focus:ring-2 focus:ring-accent"
        >
          <option value="any">{t("EcosystemDiscover.filterAllPublishers")}</option>
          {publishers.map((publisher) => <option key={publisher} value={publisher}>{publisher}</option>)}
        </select>
        <select
          value={filters.trust}
          onChange={(event) => setFilters((state) => ({ ...state, trust: event.target.value as TrustFilter }))}
          aria-label={t("EcosystemDiscover.filterTrust")}
          className="h-9 rounded-lg border border-border bg-surface px-2 text-xs text-foreground focus:outline-none focus:ring-2 focus:ring-accent"
        >
          <option value="any">{t("EcosystemDiscover.filterAllTrust")}</option>
          <option value="signed">{t("EcosystemDiscover.trustSigned")}</option>
          <option value="unsigned">{t("EcosystemDiscover.trustUnsigned")}</option>
        </select>
        <Button size="sm" variant="ghost" onClick={() => setView("registries")}>
          <ShieldOff size={14} /> {t("EcosystemDiscover.manageRegistries")}
        </Button>
      </div>

      {teamCollections.length > 0 && (
        <section>
          <h3 className="mb-2 flex items-center gap-1.5 text-xs font-semibold text-foreground"><Users size={13} /> {t("EcosystemDiscover.teamCollectionsTitle")}</h3>
          <div className="grid gap-3 md:grid-cols-2">{teamCollections.map(renderEntry)}</div>
        </section>
      )}

      <section>
        {teamCollections.length > 0 && <h3 className="mb-2 text-xs font-semibold text-foreground">{t("EcosystemDiscover.allPackagesTitle")}</h3>}
        <div className="grid gap-3 md:grid-cols-2">{rest.map(renderEntry)}</div>
      </section>

      {filtered.length === 0 && (
        <div className="rounded-xl border border-dashed border-border p-8 text-center text-sm text-muted">
          {t("EcosystemDiscover.noResults")}
        </div>
      )}

      {installPreview && previewSource && (
        <div role="dialog" aria-modal="true" aria-labelledby="discover-preview-title" className="fixed inset-0 z-[70] flex items-center justify-center bg-black/50 p-4" onClick={closePreview}>
          <div
            tabIndex={-1}
            autoFocus
            className="max-h-[85vh] w-full max-w-2xl overflow-y-auto rounded-xl border border-border bg-background p-5 shadow-2xl focus:outline-none"
            onClick={(event) => event.stopPropagation()}
            onKeyDown={(event) => {
              if (event.key === "Escape") {
                event.preventDefault();
                event.stopPropagation();
                closePreview();
              }
            }}
          >
            <div className="flex items-start justify-between gap-4">
              <div>
                <h3 id="discover-preview-title" className="text-base font-semibold text-foreground">
                  {previewIntent === "update" ? t("EcosystemDiscover.confirmTitleUpdate") : t("EcosystemDiscover.confirmTitleInstall")}
                </h3>
                <p className="mt-1 font-mono text-xs text-muted">{installPreview.preview.package_id} · v{installPreview.preview.version}</p>
              </div>
              <StatusPill tone={installPreview.preview.trust.signed ? "success" : "danger"}>
                <ShieldCheck size={12} /> {installPreview.preview.trust.signed ? t("EcosystemPackages.signatureVerified") : t("EcosystemPackages.signatureMissing")}
              </StatusPill>
            </div>

            <dl className="mt-4 grid gap-2 rounded-lg bg-surface p-3 text-xs sm:grid-cols-2">
              <div>
                <dt className="text-faint">{t("EcosystemDiscover.confirmSource")}</dt>
                <dd className="mt-1 break-all text-foreground">{previewSource.kind}{previewSource.detail ? ` · ${previewSource.detail}` : ""}</dd>
              </div>
              <div>
                <dt className="text-faint">{t("EcosystemDiscover.confirmSignature")}</dt>
                <dd className="mt-1 break-all font-mono text-foreground">
                  {installPreview.preview.trust.trust_root_id ?? t("EcosystemPackages.signatureMissing")}
                  {installPreview.preview.trust.key_id ? ` / ${installPreview.preview.trust.key_id}` : ""}
                </dd>
              </div>
              <div className="sm:col-span-2">
                <dt className="text-faint">{t("EcosystemDiscover.confirmUpdatePolicy")}</dt>
                <dd className="mt-1 text-foreground">
                  {currentInstalledForPreview?.pinned_version
                    ? t("EcosystemDiscover.confirmUpdatePolicyPinned", { version: currentInstalledForPreview.pinned_version })
                    : t("EcosystemDiscover.confirmUpdatePolicyLatest")}
                </dd>
              </div>
            </dl>

            <section className="mt-4">
              <h4 className="mb-2 text-xs font-semibold text-foreground">{t("EcosystemDiscover.confirmPermissions")}</h4>
              {(previewDiff ? [...previewDiff.added, ...previewDiff.unchanged] : installPreview.preview.permissions).length === 0 ? (
                <p className="text-xs text-muted">{t("EcosystemPackages.noPermissions")}</p>
              ) : (
                <ul className="space-y-2">
                  {(previewDiff ? [...previewDiff.added, ...previewDiff.unchanged] : installPreview.preview.permissions).map((permission) => (
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
              )}
            </section>

            {installPreview.preview.mcp_actions_separate.length > 0 && (
              <div className="mt-4 rounded-lg border border-warning/30 bg-warning-soft p-3 text-xs text-warning">
                {t("EcosystemPackages.mcpSeparate", { count: installPreview.preview.mcp_actions_separate.length })}
              </div>
            )}
            {installPreview.preview.warnings.map((warning) => <p key={warning} className="mt-2 text-xs text-warning">{warning}</p>)}
            <ResolutionSection plan={installPreview.preview.plan} />

            <label className="mt-5 flex items-start gap-2 rounded-lg border border-border bg-surface p-3 text-xs text-foreground">
              <input
                type="checkbox"
                checked={reviewed}
                onChange={(event) => setReviewed(event.target.checked)}
                className="mt-0.5 h-4 w-4 accent-accent"
              />
              {t("EcosystemDiscover.confirmReviewCheckbox")}
            </label>

            <div className="mt-5 flex justify-end gap-2">
              <Button variant="ghost" onClick={closePreview}>{t("EcosystemPackages.deny")}</Button>
              <Button
                variant="primary"
                disabled={!canApproveInstall(true, reviewed, installPreview.preview.plan.satisfiable) || busy["package-install"] || busy["package-update"]}
                onClick={() => void (previewIntent === "install"
                  ? installPackage(true)
                  : updatePackage(installPreview.preview.package_id, installPreview.preview.version, true))}
              >
                {(busy["package-install"] || busy["package-update"]) && <Loader2 size={14} className="animate-spin" />}
                {previewIntent === "install" ? t("EcosystemPackages.approveInstall") : t("EcosystemPackages.approveUpdate")}
              </Button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
