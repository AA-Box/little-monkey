import { useEffect, useMemo, useState } from "react";
import { open, save } from "@tauri-apps/plugin-dialog";
import { openUrl } from "@tauri-apps/plugin-opener";
import { Activity, Ban, Cloud, KeyRound, Loader2, Play, Power, RefreshCw, RotateCw, ShieldAlert, Square, Trash2 } from "lucide-react";
import {
  daemonInstall,
  daemonKillSwitch,
  daemonQueue,
  daemonStart,
  daemonStatus,
  daemonStop,
  daemonTriggers,
  daemonUninstall,
  MAX_REMOTE_ARTIFACT_BYTES,
  remoteAudit,
  remoteHostConfigure,
  remoteHostDisable,
  remoteHostStatus,
  remotePairCreate,
  remotePairList,
  remotePairRevoke,
  remotePairRotate,
  type DaemonQueueRequest,
  type DaemonStatus,
  validateDaemonQueuePolicy,
  validateRemotePairRequest,
} from "../../lib/daemonClient";
import { useRecipeStore } from "../../store/recipeStore";
import { useRunStore } from "../../store/runStore";
import { Button, Tabs } from "../ui";

function errorText(error: unknown) { return error instanceof Error ? error.message : String(error); }

export function BackgroundAgentsPanel() {
  const [tab, setTab] = useState<"daemon" | "queue" | "remote">("daemon");
  const [status, setStatus] = useState<DaemonStatus | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const recipes = useRecipeStore((state) => state.recipes);
  const refreshRecipes = useRecipeStore((state) => state.refresh);
  // Reuse the shared durable-run store (same source the Runs view uses) rather
  // than a bespoke listing mechanism; filter it down to control sessions.
  const runs = useRunStore((state) => state.runs);
  const eventsByRun = useRunStore((state) => state.eventsByRun);
  const selectedRunId = useRunStore((state) => state.selectedRunId);
  const refreshRuns = useRunStore((state) => state.refresh);
  const selectRun = useRunStore((state) => state.selectRun);
  const controlSessions = useMemo(
    () => runs.filter((run) => run.spec.kind === "remote_desktop_control").slice(0, 25),
    [runs],
  );
  const [install, setInstall] = useState({ concurrency: 2, maxQueue: 100, retentionDays: 30, webhookPort: "", notifications: true });
  const [queue, setQueue] = useState<DaemonQueueRequest>({ recipe: "", runKey: null, priority: 0, maxAttempts: 1, maxRuntimeSeconds: 3600, maxMemoryMb: null, ownedWorktree: false, repository: null, branchPrefix: "codex/background/", allowedRemotes: ["origin"], allowCommit: true, allowPush: false, allowCreatePullRequest: false, allowReviewComment: false });
  const [triggers, setTriggers] = useState<unknown>(null);
  const [remote, setRemote] = useState({ listen: "127.0.0.1:48321", advertiseUrl: "https://127.0.0.1:48321", tlsCertificate: "", tlsPrivateKey: "" });
  const [remoteStatus, setRemoteStatus] = useState<Record<string, unknown> | null>(null);
  const [devices, setDevices] = useState("");
  const [pair, setPair] = useState({ expiresMinutes: 15, actions: ["view-runs", "view-events", "read-artifacts"], runIds: "", workspaceIds: "", maxArtifactBytes: 8 * 1024 * 1024 });
  const [audit, setAudit] = useState<unknown>(null);
  const [deviceId, setDeviceId] = useState("");

  const queueWarnings = useMemo(() => validateDaemonQueuePolicy(queue), [queue]);
  const pairRequest = useMemo(() => ({
    output: "__selected_after_validation__",
    expiresMinutes: pair.expiresMinutes,
    actions: pair.actions,
    runIds: [...new Set(pair.runIds.split(",").map((value) => value.trim()).filter(Boolean))],
    workspaceIds: [...new Set(pair.workspaceIds.split(",").map((value) => value.trim()).filter(Boolean))],
    maxArtifactBytes: pair.maxArtifactBytes,
  }), [pair]);
  const pairWarnings = useMemo(() => validateRemotePairRequest(pairRequest), [pairRequest]);

  async function refresh() {
    try { setStatus(await daemonStatus()); } catch (cause) { setError(errorText(cause)); }
  }

  useEffect(() => { void refresh(); void refreshRecipes(); }, [refreshRecipes]);
  useEffect(() => { if (tab === "remote") void refreshRuns(); }, [tab, refreshRuns]);
  useEffect(() => {
    if (!status?.serviceRunning) return;
    const timer = window.setInterval(() => void refresh(), 2_000);
    return () => window.clearInterval(timer);
  }, [status?.serviceRunning]);

  async function act(name: string, action: () => Promise<unknown>, after?: (value: unknown) => void) {
    setBusy(name); setError(null); setNotice(null);
    try { const value = await action(); setNotice(typeof value === "string" ? value : `${name} completed.`); after?.(value); await refresh(); }
    catch (cause) { setError(errorText(cause)); }
    finally { setBusy(null); }
  }

  async function chooseFile(kind: "certificate" | "key") {
    const path = await open({ multiple: false, directory: false, filters: [{ name: "PEM", extensions: ["pem", "crt", "cer", "key"] }] });
    if (typeof path === "string") setRemote((value) => ({ ...value, [kind === "certificate" ? "tlsCertificate" : "tlsPrivateKey"]: path }));
  }

  const enabledActions = ["view-runs", "view-events", "read-artifacts", "approve", "cancel", "kill", "control-desktop"];
  const controller = typeof remoteStatus?.advertise_url === "string" ? `${remoteStatus.advertise_url.replace(/\/$/, "")}/remote` : null;

  return (
    <section className="flex flex-col gap-4">
      <div><h3 className="text-sm font-semibold text-foreground">Background agents and user-owned handoff</h3><p className="mt-1 text-xs leading-5 text-muted">The installed daemon is the authoritative engine for background, CLI, ACP, scheduler, and workflow runs. Remote control is opt-in and keeps inference, provider keys, tools, and repository access on your runner.</p></div>
      <Tabs tabs={[{ id: "daemon", label: "Service" }, { id: "queue", label: "Queue a run" }, { id: "remote", label: "Remote handoff" }]} active={tab} onChange={(id) => setTab(id as typeof tab)} />

      {tab === "daemon" && <>
        <div className="grid grid-cols-2 gap-2 md:grid-cols-4">
          {[["Service", status?.serviceRunning ? "Running" : status?.installed ? "Stopped" : "Not installed"], ["Heartbeat", status?.heartbeatFresh ? "Healthy" : "Offline"], ["Active", status?.active ?? 0], ["Waiting approval", status?.waitingApproval ?? 0], ["Queued", status?.queued ?? 0], ["Paused", status?.paused ?? 0], ["PID", status?.pid ?? "—"], ["Kill switch", status?.killSwitch ? "Engaged" : "Released"]].map(([label, value]) => <div key={String(label)} className="rounded-lg border border-border bg-surface p-3"><p className="text-[11px] text-faint">{label}</p><p className="mt-1 text-sm font-medium text-foreground">{value}</p></div>)}
        </div>
        {!status?.installed ? <div className="rounded-lg border border-border bg-surface p-3">
          <h4 className="text-xs font-semibold text-foreground">Install current-user service</h4>
          <div className="mt-3 grid gap-2 sm:grid-cols-2"><label className="text-xs text-muted">Concurrency<input type="number" min={1} max={32} value={install.concurrency} onChange={(event) => setInstall({ ...install, concurrency: Number(event.target.value) })} className="mt-1 w-full rounded-md border border-border bg-background p-2" /></label><label className="text-xs text-muted">Max queue<input type="number" min={1} value={install.maxQueue} onChange={(event) => setInstall({ ...install, maxQueue: Number(event.target.value) })} className="mt-1 w-full rounded-md border border-border bg-background p-2" /></label><label className="text-xs text-muted">Retention days<input type="number" min={1} value={install.retentionDays} onChange={(event) => setInstall({ ...install, retentionDays: Number(event.target.value) })} className="mt-1 w-full rounded-md border border-border bg-background p-2" /></label><label className="text-xs text-muted">Loopback webhook port (optional)<input value={install.webhookPort} onChange={(event) => setInstall({ ...install, webhookPort: event.target.value })} className="mt-1 w-full rounded-md border border-border bg-background p-2" /></label></div>
          <label className="mt-3 flex gap-2 text-xs text-muted"><input type="checkbox" checked={install.notifications} onChange={(event) => setInstall({ ...install, notifications: event.target.checked })} /> Notifications</label>
          <Button className="mt-3" variant="primary" disabled={busy !== null} onClick={() => void act("install", () => daemonInstall({ concurrency: install.concurrency, maxQueue: install.maxQueue, retentionDays: install.retentionDays, webhookPort: install.webhookPort ? Number(install.webhookPort) : null, notifications: install.notifications }))}><Power size={14} /> Install service</Button>
        </div> : <div className="flex flex-wrap gap-2"><Button disabled={busy !== null || Boolean(status.serviceRunning)} onClick={() => void act("start", daemonStart)}><Play size={14} /> Start</Button><Button disabled={busy !== null || !status.serviceRunning} onClick={() => void act("stop", daemonStop)}><Square size={14} /> Stop safely</Button><Button variant={status.killSwitch ? "secondary" : "danger"} disabled={busy !== null} onClick={() => void act("kill switch", () => daemonKillSwitch(!status.killSwitch))}><ShieldAlert size={14} /> {status.killSwitch ? "Release kill switch" : "Engage kill switch"}</Button><Button variant="danger" disabled={busy !== null || status.serviceRunning} onClick={() => { if (window.confirm("Uninstall the current-user daemon service? Durable run history will be retained.")) void act("uninstall", () => daemonUninstall(false)); }}><Trash2 size={14} /> Uninstall service</Button><Button disabled={busy !== null} onClick={() => void act("triggers", daemonTriggers, setTriggers)}><RefreshCw size={14} /> Inspect triggers</Button></div>}
        {triggers !== null && <pre className="max-h-64 overflow-auto rounded-lg border border-border bg-surface p-3 text-[10px] text-muted">{JSON.stringify(triggers, null, 2)}</pre>}
      </>}

      {tab === "queue" && <div className="rounded-lg border border-border bg-surface p-3">
        <label className="text-xs text-muted">Immutable recipe<select value={queue.recipe} onChange={(event) => setQueue({ ...queue, recipe: event.target.value })} className="mt-1 w-full rounded-md border border-border bg-background p-2 text-foreground"><option value="">Select a valid recipe</option>{recipes.filter((entry) => entry.recipe && !entry.error).map((entry) => <option key={entry.path} value={entry.path}>{entry.recipe?.name} · {entry.source}</option>)}</select></label>
        <div className="mt-3 grid gap-2 sm:grid-cols-3"><label className="text-xs text-muted">Max attempts<input type="number" min={1} max={100} value={queue.maxAttempts} onChange={(event) => setQueue({ ...queue, maxAttempts: Number(event.target.value) })} className="mt-1 w-full rounded-md border border-border bg-background p-2" /></label><label className="text-xs text-muted">Max runtime seconds<input type="number" min={1} value={queue.maxRuntimeSeconds} onChange={(event) => setQueue({ ...queue, maxRuntimeSeconds: Number(event.target.value) })} className="mt-1 w-full rounded-md border border-border bg-background p-2" /></label><label className="text-xs text-muted">Max memory MiB<input value={queue.maxMemoryMb ?? ""} onChange={(event) => setQueue({ ...queue, maxMemoryMb: event.target.value ? Number(event.target.value) : null })} className="mt-1 w-full rounded-md border border-border bg-background p-2" /></label></div>
        <label className="mt-3 flex gap-2 text-xs text-muted"><input type="checkbox" checked={queue.ownedWorktree} onChange={(event) => setQueue({ ...queue, ownedWorktree: event.target.checked })} /> Require a daemon-owned isolated worktree</label>
        {queue.ownedWorktree && <div className="mt-2 grid gap-2 sm:grid-cols-2"><label className="text-xs text-muted">Repository<input value={queue.repository ?? ""} onChange={(event) => setQueue({ ...queue, repository: event.target.value || null })} className="mt-1 w-full rounded-md border border-border bg-background p-2" /></label><label className="text-xs text-muted">Protected branch prefix<input value={queue.branchPrefix} onChange={(event) => setQueue({ ...queue, branchPrefix: event.target.value })} className="mt-1 w-full rounded-md border border-border bg-background p-2" /></label></div>}
        <div className="mt-3 flex flex-wrap gap-4 text-xs text-muted">{[["allowPush", "Push owned branch"], ["allowCreatePullRequest", "Create draft PR"], ["allowReviewComment", "Publish review comments"]].map(([key, label]) => <label key={key} className="flex gap-2"><input type="checkbox" checked={Boolean(queue[key as keyof DaemonQueueRequest])} onChange={(event) => setQueue({ ...queue, [key]: event.target.checked })} /> {label}</label>)}</div>
        {queueWarnings.map((warning) => <p key={warning} role="alert" className="mt-2 text-xs text-warning">{warning}</p>)}
        <Button className="mt-3" variant="primary" disabled={!status?.serviceRunning || !queue.recipe || queueWarnings.length > 0 || busy !== null} onClick={() => { const writes = queue.allowPush || queue.allowCreatePullRequest || queue.allowReviewComment; if (!writes || window.confirm("Queue this background job with the displayed Git/GitHub write scopes?")) void act("queue", () => daemonQueue(queue)); }}><Activity size={14} /> Queue durable run</Button>
      </div>}

      {tab === "remote" && <>
        <div className="rounded-lg border border-border bg-surface p-3"><h4 className="text-xs font-semibold text-foreground">Runner-owned TLS host</h4><p className="mt-1 text-xs text-muted">Use a certificate valid for the advertised Tailscale, SSH-forwarded, or direct HTTPS hostname. Little Monkey never relays this traffic.</p>
          <div className="mt-3 grid gap-2 sm:grid-cols-2"><label className="text-xs text-muted">Listen address<input value={remote.listen} onChange={(event) => setRemote({ ...remote, listen: event.target.value })} className="mt-1 w-full rounded-md border border-border bg-background p-2" /></label><label className="text-xs text-muted">Advertised HTTPS URL<input value={remote.advertiseUrl} onChange={(event) => setRemote({ ...remote, advertiseUrl: event.target.value })} className="mt-1 w-full rounded-md border border-border bg-background p-2" /></label></div>
          <div className="mt-2 flex flex-wrap gap-2"><Button size="sm" onClick={() => void chooseFile("certificate")}>Certificate…</Button><span className="max-w-xs truncate text-[10px] text-faint">{remote.tlsCertificate || "None"}</span><Button size="sm" onClick={() => void chooseFile("key")}>Private key…</Button><span className="max-w-xs truncate text-[10px] text-faint">{remote.tlsPrivateKey || "None"}</span></div>
          <div className="mt-3 flex flex-wrap gap-2"><Button variant="primary" disabled={!remote.tlsCertificate || !remote.tlsPrivateKey || busy !== null} onClick={() => { if (window.confirm("Enable the user-owned remote TLS listener with this exact address and certificate?")) void act("configure remote", () => remoteHostConfigure(remote), () => void remoteHostStatus().then(setRemoteStatus)); }}>Configure host</Button><Button disabled={busy !== null} onClick={() => void act("remote status", remoteHostStatus, (value) => setRemoteStatus(value as Record<string, unknown> | null))}>Refresh status</Button><Button variant="danger" disabled={busy !== null} onClick={() => void act("disable remote", remoteHostDisable)}>Disable</Button>{controller && <Button onClick={() => void openUrl(controller)}><Cloud size={14} /> Open responsive controller</Button>}</div>
          {remoteStatus && <pre className="mt-3 max-h-52 overflow-auto rounded-md bg-background p-2 text-[10px] text-muted">{JSON.stringify(remoteStatus, null, 2)}</pre>}
        </div>
        <div className="rounded-lg border border-border bg-surface p-3">
          <h4 className="text-xs font-semibold text-foreground">One-time scoped invitation</h4>
          <p className="mt-1 text-[11px] leading-4 text-muted">Every invitation must name exact runs or declared workspaces. It cannot expand runner policy.</p>
          <div className="mt-2 flex flex-wrap gap-3">{enabledActions.map((action) => <label key={action} className="flex gap-1 text-xs text-muted"><input type="checkbox" checked={pair.actions.includes(action)} onChange={(event) => setPair({ ...pair, actions: event.target.checked ? [...pair.actions, action] : pair.actions.filter((value) => value !== action) })} /> {action}</label>)}</div>
          <div className="mt-2 grid gap-2 sm:grid-cols-2">
            <label className="text-xs text-muted">Allowed run IDs (comma separated)<input value={pair.runIds} onChange={(event) => setPair({ ...pair, runIds: event.target.value })} className="mt-1 w-full rounded-md border border-border bg-background p-2" /></label>
            <label className="text-xs text-muted">Allowed workspace IDs<input value={pair.workspaceIds} onChange={(event) => setPair({ ...pair, workspaceIds: event.target.value })} className="mt-1 w-full rounded-md border border-border bg-background p-2" /></label>
            <label className="text-xs text-muted">Expires in minutes<input type="number" min={1} max={1440} value={pair.expiresMinutes} onChange={(event) => setPair({ ...pair, expiresMinutes: Number(event.target.value) })} className="mt-1 w-full rounded-md border border-border bg-background p-2" /></label>
            <label className="text-xs text-muted">Artifact limit (MiB)<input type="number" min={1} max={MAX_REMOTE_ARTIFACT_BYTES / (1024 * 1024)} value={pair.maxArtifactBytes / (1024 * 1024)} onChange={(event) => setPair({ ...pair, maxArtifactBytes: Number(event.target.value) * 1024 * 1024 })} className="mt-1 w-full rounded-md border border-border bg-background p-2" /></label>
          </div>
          {pairWarnings.map((warning) => <p key={warning} role="alert" className="mt-2 text-xs text-warning">{warning}</p>)}
          <Button className="mt-3" disabled={pairWarnings.length > 0 || busy !== null} onClick={async () => { const output = await save({ defaultPath: "little-monkey-pairing.json", filters: [{ name: "JSON", extensions: ["json"] }] }); if (output) void act("pair invitation", () => remotePairCreate({ ...pairRequest, output })); }}><KeyRound size={14} /> Create invitation…</Button>
        </div>
        <div className="rounded-lg border border-border bg-surface p-3"><div className="flex flex-wrap gap-2"><Button size="sm" onClick={() => void act("devices", remotePairList, (value) => setDevices(String(value)))}>List devices</Button><input placeholder="Device ID" value={deviceId} onChange={(event) => setDeviceId(event.target.value)} className="rounded-md border border-border bg-background px-2 text-xs" /><Button size="sm" variant="danger" disabled={!deviceId} onClick={() => { if (window.confirm(`Revoke ${deviceId} immediately?`)) void act("revoke", () => remotePairRevoke(deviceId, "revoked from desktop")); }}><Ban size={12} /> Revoke</Button><Button size="sm" disabled={!deviceId} onClick={async () => { const output = await save({ defaultPath: `${deviceId}-rotation.json`, filters: [{ name: "JSON", extensions: ["json"] }] }); if (output) void act("rotate", () => remotePairRotate(deviceId, output)); }}><RotateCw size={12} /> Rotate key</Button><Button size="sm" onClick={() => void act("audit", () => remoteAudit(100), setAudit)}>Audit</Button></div>{devices && <pre className="mt-2 whitespace-pre-wrap rounded-md bg-background p-2 text-[10px] text-muted">{devices}</pre>}{audit !== null && <pre className="mt-2 max-h-52 overflow-auto rounded-md bg-background p-2 text-[10px] text-muted">{JSON.stringify(audit, null, 2)}</pre>}</div>
        <div className="rounded-lg border border-border bg-surface p-3">
          <div className="flex items-center justify-between"><h4 className="text-xs font-semibold text-foreground">Remote desktop-control sessions</h4><Button size="sm" onClick={() => void refreshRuns()}><RefreshCw size={12} /> Refresh</Button></div>
          <p className="mt-1 text-[11px] leading-4 text-muted">Every remote desktop-control session is recorded here with start, periodic, and stop screenshots as tamper-evident evidence. Control requires local on-screen consent on this runner, and can be stopped instantly with <code>monkey daemon desktop-control emergency-stop</code>, the kill switch, or revoking the device.</p>
          {controlSessions.length === 0 ? <p className="mt-2 text-[11px] text-faint">No remote desktop-control sessions recorded yet.</p> : <ul className="mt-2 space-y-1">
            {controlSessions.map((run) => {
              const screenshots = (eventsByRun[run.spec.run_id] ?? []).filter((envelope) => envelope.event.type === "artifact_added");
              const expanded = selectedRunId === run.spec.run_id;
              return <li key={run.spec.run_id} className="rounded-md border border-border bg-background">
                <button type="button" className="flex w-full flex-wrap items-center justify-between gap-2 p-2 text-left text-[11px]" onClick={() => void selectRun(run.spec.run_id)}>
                  <span className="truncate font-mono text-foreground" title={run.spec.run_id}>{run.spec.run_id}</span>
                  <span className="text-muted">{run.status}</span>
                  <span className="text-faint">{new Date(run.spec.created_at_ms).toLocaleString()}</span>
                </button>
                {expanded && <div className="border-t border-border p-2">
                  {screenshots.length === 0 ? <p className="text-[11px] text-faint">No screenshots recorded for this session yet.</p> : <ol className="space-y-1">
                    {screenshots.map((envelope) => envelope.event.type === "artifact_added" ? <li key={envelope.event_id} className="flex items-center justify-between gap-2 text-[11px] text-muted"><span className="truncate" title={envelope.event.payload.content_sha256}>{envelope.event.payload.name}</span><span className="text-faint">{new Date(envelope.occurred_at_ms).toLocaleTimeString()} · {(envelope.event.payload.size_bytes / 1024).toFixed(0)} KiB</span></li> : null)}
                  </ol>}
                </div>}
              </li>;
            })}
          </ul>}
        </div>
      </>}

      {busy && <p role="status" className="flex items-center gap-2 text-xs text-muted"><Loader2 size={13} className="animate-spin" /> {busy}…</p>}
      {notice && <p className="rounded-md border border-success/40 bg-success/10 p-2 text-xs text-success">{notice}</p>}
      {error && <p role="alert" className="rounded-md border border-danger/40 bg-danger/10 p-2 text-xs text-danger">{error}</p>}
    </section>
  );
}
