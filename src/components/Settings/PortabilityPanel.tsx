import { useEffect, useState } from "react";
import { open, save } from "@tauri-apps/plugin-dialog";
import {
  ArchiveRestore,
  CheckCircle2,
  Cloud,
  Download,
  FileArchive,
  LoaderCircle,
  RefreshCw,
  Save,
  ShieldCheck,
  Upload,
} from "lucide-react";

import { useT } from "../../lib/i18n";
import { daemonStatus, type DaemonStatus } from "../../lib/daemonClient";
import {
  createEncryptedSnapshot,
  downloadSnapshotFromWebDav,
  exportPortableBundle,
  getWebDavBackupStatus,
  importPortableOutcome,
  listEncryptedSnapshots,
  openEncryptedSnapshot,
  readPortableBundle,
  runWebDavBackupDue,
  saveWebDavConfig,
  stageEncryptedSnapshot,
  testWebDav,
  type PortableReadOutcome,
  type SnapshotFileInfo,
  type WebDavBackupConfig,
  type WebDavBackupStatus,
} from "../../lib/portability";

const BUNDLE_FILTER = [{ name: "Little Monkey bundle", extensions: ["lmbundle"] }];

interface PendingImport {
  label: string;
  outcome: PortableReadOutcome;
}

const actionClass =
  "inline-flex cursor-pointer items-center justify-center gap-2 rounded-lg border border-border bg-surface-2 px-3 py-2 text-sm font-medium text-foreground transition-colors hover:border-accent/50 hover:bg-surface disabled:cursor-not-allowed disabled:opacity-50";
const primaryClass = `${actionClass} border-accent bg-accent text-accent-foreground hover:bg-accent-hover`;
const inputClass =
  "w-full rounded-lg border border-border bg-surface-2 px-3 py-2 text-sm text-foreground outline-none placeholder:text-faint focus-visible:border-accent";

function statusMessage(error: string | null, success: string | null) {
  if (!error && !success) return null;
  return (
    <div
      role={error ? "alert" : "status"}
      className={`rounded-lg border px-3 py-2 text-sm ${
        error ? "border-danger/30 bg-danger-soft text-danger" : "border-success/30 bg-success-soft text-success"
      }`}
    >
      {error ?? success}
    </div>
  );
}

function formatDate(timestamp: number | null, fallback: string): string {
  return timestamp ? new Date(timestamp).toLocaleString() : fallback;
}

export function PortabilityPanel() {
  const { t } = useT();
  const [snapshots, setSnapshots] = useState<SnapshotFileInfo[]>([]);
  const [config, setConfig] = useState<WebDavBackupConfig | null>(null);
  const [backupStatus, setBackupStatus] = useState<WebDavBackupStatus | null>(null);
  const [daemon, setDaemon] = useState<DaemonStatus | null>(null);
  const [password, setPassword] = useState("");
  const [pending, setPending] = useState<PendingImport | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [success, setSuccess] = useState<string | null>(null);

  const resetStatus = () => {
    setError(null);
    setSuccess(null);
  };

  const refresh = async () => {
    const [nextSnapshots, nextStatus, nextDaemon] = await Promise.all([
      listEncryptedSnapshots(),
      getWebDavBackupStatus(),
      daemonStatus().catch(() => null),
    ]);
    setSnapshots(nextSnapshots);
    setBackupStatus(nextStatus);
    setConfig(nextStatus.config);
    setDaemon(nextDaemon);
  };

  useEffect(() => {
    void refresh().catch((caught) => setError(caught instanceof Error ? caught.message : String(caught)));
  }, []);

  const run = async (name: string, operation: () => Promise<void>) => {
    setBusy(name);
    resetStatus();
    try {
      await operation();
    } catch (caught) {
      setError(caught instanceof Error ? caught.message : String(caught));
    } finally {
      setBusy(null);
    }
  };

  const exportAll = () => run("export", async () => {
    const path = await save({ defaultPath: "little-monkey-profile.lmbundle", filters: BUNDLE_FILTER });
    if (!path) return;
    await exportPortableBundle(path);
    setSuccess(t("Portability.exportComplete"));
  });

  const inspectBundle = () => run("import", async () => {
    const path = await open({ multiple: false, directory: false, filters: BUNDLE_FILTER });
    if (!path) return;
    setPending({ label: path, outcome: await readPortableBundle(path) });
  });

  const createSnapshot = () => run("snapshot", async () => {
    await createEncryptedSnapshot();
    setSnapshots(await listEncryptedSnapshots());
    setSuccess(t("Portability.snapshotCreated"));
  });

  const inspectSnapshot = (snapshot: SnapshotFileInfo) => run(`restore:${snapshot.path}`, async () => {
    setPending({ label: snapshot.path, outcome: await openEncryptedSnapshot(snapshot.path) });
  });

  const commitImport = (mode: "merge" | "replace") => run(`commit-${mode}`, async () => {
    if (!pending) return;
    const count = await importPortableOutcome(pending.outcome, mode);
    setPending(null);
    setSuccess(t("Portability.imported", { count }));
  });

  const saveConfig = () => run("save-config", async () => {
    if (!config) return;
    const saved = await saveWebDavConfig({
      enabled: config.enabled,
      baseUrl: config.baseUrl,
      username: config.username,
      password: password || null,
      remotePath: config.remotePath,
      intervalMinutes: config.intervalMinutes,
    });
    setConfig(saved);
    setPassword("");
    if (saved.enabled) await stageEncryptedSnapshot();
    const nextStatus = await getWebDavBackupStatus();
    setBackupStatus(nextStatus);
    setConfig(nextStatus.config);
    setSuccess(t("Portability.webdavSaved"));
  });

  const testConnection = () => run("test-webdav", async () => {
    await testWebDav();
    setSuccess(t("Portability.webdavConnected"));
  });

  const backupNow = () => run("backup-now", async () => {
    await stageEncryptedSnapshot();
    const outcome = await runWebDavBackupDue(true);
    await refresh();
    if (outcome.status === "conflict_copy") {
      setSuccess(t("Portability.webdavConflict"));
    } else if (outcome.status === "busy") {
      setSuccess(`The daemon or another app window is already uploading (${outcome.owner}).`);
    } else if (outcome.status === "missing_staged_source" || outcome.status === "disabled") {
      throw new Error("Scheduled WebDAV backup is not ready.");
    } else {
      setSuccess(t("Portability.webdavUploaded"));
    }
  });

  const downloadRemote = () => run("download", async () => {
    const outcome = await downloadSnapshotFromWebDav();
    if (outcome.status === "downloaded") {
      setPending({ label: outcome.remotePath, outcome: outcome.payload });
    } else {
      setSuccess(t(outcome.status === "not_modified" ? "Portability.notModified" : "Portability.remoteMissing"));
    }
  });

  return (
    <div className="flex flex-col gap-6">
      <div>
        <h3 className="text-base font-semibold text-foreground">{t("Portability.title")}</h3>
        <p className="mt-1 text-sm leading-relaxed text-muted">{t("Portability.subtitle")}</p>
      </div>

      {statusMessage(error, success)}

      {pending && (
        <section className="rounded-xl border border-accent/40 bg-accent-soft p-4">
          <div className="flex items-start gap-3">
            <ShieldCheck size={20} className="mt-0.5 shrink-0 text-accent" />
            <div className="min-w-0 flex-1">
              <p className="truncate text-sm font-medium text-foreground">{pending.label}</p>
              <p className="mt-1 text-sm text-muted">
                {t("Portability.preflight", {
                  sessions: pending.outcome.preflight.sessionCount,
                  messages: pending.outcome.preflight.messageCount,
                  artifacts: pending.outcome.preflight.artifactCount,
                })}
              </p>
              <div className="mt-3 flex flex-wrap gap-2">
                <button type="button" className={primaryClass} disabled={busy !== null} onClick={() => void commitImport("merge")}>
                  {t("Portability.restoreMerge")}
                </button>
                <button type="button" className={actionClass} disabled={busy !== null} onClick={() => void commitImport("replace")}>
                  {t("Portability.restoreReplace")}
                </button>
                <button type="button" className={actionClass} onClick={() => setPending(null)}>
                  {t("Portability.cancel")}
                </button>
              </div>
            </div>
          </div>
        </section>
      )}

      <section className="rounded-xl border border-border bg-surface p-4">
        <div className="flex items-start justify-between gap-4">
          <div>
            <h4 className="flex items-center gap-2 text-sm font-semibold text-foreground">
              <FileArchive size={16} />
              {t("Portability.title")}
            </h4>
            <p className="mt-1 text-xs leading-relaxed text-muted">{t("Portability.originalSafety")}</p>
          </div>
        </div>
        <div className="mt-4 flex flex-wrap gap-2">
          <button type="button" className={primaryClass} disabled={busy !== null} onClick={() => void exportAll()}>
            <Upload size={15} /> {t("Portability.exportAll")}
          </button>
          <button type="button" className={actionClass} disabled={busy !== null} onClick={() => void inspectBundle()}>
            <Download size={15} /> {t("Portability.importBundle")}
          </button>
        </div>
      </section>

      <section className="rounded-xl border border-border bg-surface p-4">
        <div className="flex items-center justify-between gap-3">
          <h4 className="flex items-center gap-2 text-sm font-semibold text-foreground">
            <Save size={16} /> {t("Portability.localSnapshots")}
          </h4>
          <div className="flex gap-1">
            <button type="button" className={actionClass} disabled={busy !== null} onClick={() => void refresh()} aria-label="Refresh snapshots">
              <RefreshCw size={14} />
            </button>
            <button type="button" className={primaryClass} disabled={busy !== null} onClick={() => void createSnapshot()}>
              <Save size={14} /> {t("Portability.createSnapshot")}
            </button>
          </div>
        </div>
        <div className="mt-3 flex flex-col gap-2">
          {snapshots.length === 0 && <p className="text-sm text-faint">{t("Portability.noSnapshots")}</p>}
          {snapshots.map((snapshot) => (
            <div key={snapshot.path} className="flex items-center justify-between gap-3 rounded-lg border border-border bg-surface-2 px-3 py-2">
              <div className="min-w-0">
                <p className="truncate text-sm text-foreground">{new Date(snapshot.createdAtMs).toLocaleString()}</p>
                <p className="truncate text-xs text-faint">{(snapshot.byteSize / 1024 / 1024).toFixed(2)} MB · {snapshot.sha256.slice(0, 12)}</p>
              </div>
              <button type="button" className={actionClass} disabled={busy !== null} onClick={() => void inspectSnapshot(snapshot)}>
                <ArchiveRestore size={14} /> {t("Portability.restore")}
              </button>
            </div>
          ))}
        </div>
      </section>

      <section className="rounded-xl border border-border bg-surface p-4">
        <h4 className="flex items-center gap-2 text-sm font-semibold text-foreground">
          <Cloud size={16} /> {t("Portability.webdav")}
        </h4>
        <p className="mt-1 text-xs leading-relaxed text-muted">{t("Portability.webdavHint")}</p>
        {backupStatus && (
          <div className="mt-4 grid grid-cols-1 gap-2 sm:grid-cols-3">
            <div className="rounded-lg border border-border bg-surface-2 p-3">
              <p className="text-xs font-semibold text-foreground">{t("Portability.daemonState")}</p>
              <p className={`mt-1 text-xs ${daemon?.serviceRunning && daemon.heartbeatFresh ? "text-success" : "text-muted"}`}>
                {daemon?.serviceRunning && daemon.heartbeatFresh
                  ? t("Portability.daemonReady")
                  : daemon?.installed
                    ? t("Portability.daemonStopped")
                    : t("Portability.daemonMissing")}
              </p>
            </div>
            <div className="rounded-lg border border-border bg-surface-2 p-3">
              <p className="text-xs font-semibold text-foreground">{t("Portability.stagedSource")}</p>
              <p className={`mt-1 text-xs ${backupStatus.stagedSnapshot ? "text-success" : "text-muted"}`}>
                {backupStatus.stagedSnapshot
                  ? t("Portability.stagedReady", {
                      date: new Date(backupStatus.stagedSnapshot.createdAtMs).toLocaleString(),
                      size: (backupStatus.stagedSnapshot.byteSize / 1024 / 1024).toFixed(2),
                      digest: backupStatus.stagedSnapshot.sha256.slice(0, 12),
                    })
                  : t("Portability.stagedMissing")}
              </p>
            </div>
            <div className="rounded-lg border border-border bg-surface-2 p-3">
              <p className="text-xs font-semibold text-foreground">{t("Portability.keychainState")}</p>
              <p className={`mt-1 text-xs ${backupStatus.config.enabled && backupStatus.credentialsAvailable ? "text-success" : "text-muted"}`}>
                {!backupStatus.config.enabled
                  ? t("Portability.keychainDisabled")
                  : backupStatus.credentialsAvailable
                    ? t("Portability.keychainReady")
                    : t("Portability.keychainMissing")}
              </p>
            </div>
            {backupStatus.uploadClaimed && backupStatus.claimOwner && backupStatus.claimExpiresMs && (
              <p className="text-xs text-accent sm:col-span-3">
                {t("Portability.uploadClaimed", {
                  owner: backupStatus.claimOwner,
                  date: new Date(backupStatus.claimExpiresMs).toLocaleTimeString(),
                })}
              </p>
            )}
            {backupStatus.config.lastError && (
              <p role="alert" className="rounded-lg border border-danger/30 bg-danger-soft px-3 py-2 text-xs text-danger sm:col-span-3">
                {t("Portability.lastError", { error: backupStatus.config.lastError })}
              </p>
            )}
          </div>
        )}
        {config ? (
          <div className="mt-4 grid grid-cols-1 gap-3 sm:grid-cols-2">
            <label className="text-xs font-medium text-muted sm:col-span-2">
              {t("Portability.serverUrl")}
              <input className={`${inputClass} mt-1`} value={config.baseUrl} onChange={(event) => setConfig({ ...config, baseUrl: event.target.value })} placeholder="https://dav.example.com/backups/" />
            </label>
            <label className="text-xs font-medium text-muted">
              {t("Portability.username")}
              <input className={`${inputClass} mt-1`} value={config.username} onChange={(event) => setConfig({ ...config, username: event.target.value })} autoComplete="username" />
            </label>
            <label className="text-xs font-medium text-muted">
              {t("Portability.password")}
              <input className={`${inputClass} mt-1`} type="password" value={password} onChange={(event) => setPassword(event.target.value)} autoComplete="current-password" />
            </label>
            <label className="text-xs font-medium text-muted">
              {t("Portability.remotePath")}
              <input className={`${inputClass} mt-1`} value={config.remotePath} onChange={(event) => setConfig({ ...config, remotePath: event.target.value })} />
            </label>
            <label className="text-xs font-medium text-muted">
              {t("Portability.interval")}
              <input className={`${inputClass} mt-1`} type="number" min={5} max={10080} value={config.intervalMinutes} onChange={(event) => setConfig({ ...config, intervalMinutes: Number(event.target.value) })} />
            </label>
            <label className="flex items-center gap-2 text-sm text-foreground sm:col-span-2">
              <input type="checkbox" checked={config.enabled} onChange={(event) => setConfig({ ...config, enabled: event.target.checked })} />
              {t("Portability.enabled")}
            </label>
            <p className="text-xs text-faint sm:col-span-2">
              {t("Portability.lastSuccess", { date: formatDate(config.lastSuccessMs, t("Portability.never")) })}
            </p>
            <div className="flex flex-wrap gap-2 sm:col-span-2">
              <button type="button" className={primaryClass} disabled={busy !== null} onClick={() => void saveConfig()}>
                <Save size={14} /> {t("Portability.save")}
              </button>
              <button type="button" className={actionClass} disabled={busy !== null} onClick={() => void testConnection()}>
                <CheckCircle2 size={14} /> {t("Portability.test")}
              </button>
              <button type="button" className={actionClass} disabled={busy !== null || !config.enabled} onClick={() => void backupNow()}>
                <Upload size={14} /> {t("Portability.backupNow")}
              </button>
              <button type="button" className={actionClass} disabled={busy !== null} onClick={() => void downloadRemote()}>
                <Download size={14} /> {t("Portability.download")}
              </button>
            </div>
          </div>
        ) : (
          <div className="mt-4 flex items-center gap-2 text-sm text-muted">
            <LoaderCircle size={15} className="animate-spin" /> {t("Portability.busy")}
          </div>
        )}
      </section>
    </div>
  );
}

export default PortabilityPanel;
