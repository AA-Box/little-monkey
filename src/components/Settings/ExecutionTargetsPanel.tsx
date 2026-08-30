import { invoke } from "@tauri-apps/api/core";
import { useEffect, useState } from "react";
import { Button } from "../ui";

interface TargetIdentity {
  stableId: string;
  displayName: string;
  kind: string;
  platform: string;
  runnerVersion: string;
  capabilities: Record<string, unknown>;
  lastSuccessfulProbeMs: number | null;
  trustState: string;
}

interface TargetConfig { kind: string; identity: TargetIdentity; image?: string; config?: Record<string, unknown> }

function capabilityLabels(capabilities: Record<string, unknown>): string[] {
  return Object.entries(capabilities)
    .filter(([key, value]) => value === true || (key.startsWith("max") && value != null))
    .map(([key, value]) => key.startsWith("max") ? `${key}: ${String(value)}` : key)
    .slice(0, 10);
}

export function ExecutionTargetsPanel() {
  const [targets, setTargets] = useState<Record<string, TargetConfig>>({});
  const [diagnostics, setDiagnostics] = useState<Record<string, unknown> | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [form, setForm] = useState({ id: "", kind: "docker", name: "", image: "", host: "", user: "", knownHosts: "" });
  const [globalDefault, setGlobalDefault] = useState(() => localStorage.getItem("little-monkey.execution-target.global") ?? "");
  const [workspaceDefault, setWorkspaceDefault] = useState(() => localStorage.getItem("little-monkey.execution-target.workspace") ?? "");

  const refresh = async () => {
    try {
      setError(null);
      setTargets(await invoke<Record<string, TargetConfig>>("execution_targets_list"));
    } catch (value) {
      setError(value instanceof Error ? value.message : String(value));
    }
  };

  useEffect(() => { void refresh(); }, []);

  const probe = async (id: string) => {
    setBusy(id);
    try {
      const snapshot = await invoke<unknown>("execution_target_probe", { id });
      setDiagnostics({ [id]: snapshot });
      await refresh();
    } catch (value) {
      setError(value instanceof Error ? value.message : String(value));
    } finally {
      setBusy(null);
    }
  };

  const remove = async (id: string) => {
    setBusy(id);
    try {
      await invoke("execution_target_remove", { id });
      await refresh();
    } catch (value) {
      setError(value instanceof Error ? value.message : String(value));
    } finally {
      setBusy(null);
    }
  };

  const save = async () => {
    setBusy("form");
    try {
      await invoke("execution_target_add", { request: { ...form, name: form.name || null, image: form.image || null, host: form.host || null, user: form.user || null, knownHosts: form.knownHosts || null } });
      setForm({ id: "", kind: "docker", name: "", image: "", host: "", user: "", knownHosts: "" });
      setEditingId(null);
      await refresh();
    } catch (value) {
      setError(value instanceof Error ? value.message : String(value));
    } finally {
      setBusy(null);
    }
  };

  return (
    <div className="space-y-4">
      <div>
        <h3 className="text-sm font-semibold text-foreground">Execution targets</h3>
        <p className="mt-1 text-xs leading-5 text-muted">Each run freezes the selected target identity, capabilities, workspace policy, and protocol version before submission.</p>
      </div>
      <form className="grid gap-2 rounded-lg border border-border bg-background p-3 md:grid-cols-2" onSubmit={(event) => { event.preventDefault(); void save(); }}>
        <div className="md:col-span-2 flex items-center justify-between"><h4 className="text-xs font-semibold text-foreground">{editingId ? `Edit ${editingId}` : "Add target"}</h4>{editingId && <Button size="sm" variant="ghost" type="button" onClick={() => { setEditingId(null); setForm({ id: "", kind: "docker", name: "", image: "", host: "", user: "", knownHosts: "" }); }}>Cancel</Button>}</div>
        <input required className="rounded-md border border-border bg-surface px-2 py-1.5 text-xs text-foreground" placeholder="Stable target ID" value={form.id} disabled={Boolean(editingId)} onChange={(event) => setForm({ ...form, id: event.target.value })} />
        <select className="rounded-md border border-border bg-surface px-2 py-1.5 text-xs text-foreground" value={form.kind} onChange={(event) => setForm({ ...form, kind: event.target.value })}><option value="docker">Docker</option><option value="ssh_runner">SSH runner</option></select>
        <input className="rounded-md border border-border bg-surface px-2 py-1.5 text-xs text-foreground" placeholder="Display name" value={form.name} onChange={(event) => setForm({ ...form, name: event.target.value })} />
        {form.kind === "docker" ? <input required className="rounded-md border border-border bg-surface px-2 py-1.5 text-xs text-foreground" placeholder="Docker image" value={form.image} onChange={(event) => setForm({ ...form, image: event.target.value })} /> : <><input required className="rounded-md border border-border bg-surface px-2 py-1.5 text-xs text-foreground" placeholder="SSH host" value={form.host} onChange={(event) => setForm({ ...form, host: event.target.value })} /><input className="rounded-md border border-border bg-surface px-2 py-1.5 text-xs text-foreground" placeholder="SSH user" value={form.user} onChange={(event) => setForm({ ...form, user: event.target.value })} /><input required className="rounded-md border border-border bg-surface px-2 py-1.5 text-xs text-foreground md:col-span-2" placeholder="Absolute known_hosts path" value={form.knownHosts} onChange={(event) => setForm({ ...form, knownHosts: event.target.value })} /></>}
        <Button type="submit" variant="primary" disabled={busy === "form"}>{editingId ? "Save changes" : "Add target"}</Button>
      </form>
      <div className="grid gap-2 rounded-lg border border-border bg-background p-3 md:grid-cols-2">
        <label className="grid gap-1 text-[11px] text-muted"><span>Global default</span><select className="rounded-md border border-border bg-surface px-2 py-1.5 text-xs text-foreground" value={globalDefault} onChange={(event) => { setGlobalDefault(event.target.value); localStorage.setItem("little-monkey.execution-target.global", event.target.value); }}><option value="">Automatic</option>{Object.entries(targets).map(([id, target]) => <option key={id} value={id}>{target.identity.displayName}</option>)}</select></label>
        <label className="grid gap-1 text-[11px] text-muted"><span>Workspace default</span><select className="rounded-md border border-border bg-surface px-2 py-1.5 text-xs text-foreground" value={workspaceDefault} onChange={(event) => { setWorkspaceDefault(event.target.value); localStorage.setItem("little-monkey.execution-target.workspace", event.target.value); }}><option value="">Use global</option>{Object.entries(targets).map(([id, target]) => <option key={id} value={id}>{target.identity.displayName}</option>)}</select></label>
      </div>
      {error && <p className="rounded-md border border-danger/40 bg-danger/10 p-2 text-xs text-danger">{error}</p>}
      {Object.keys(targets).length === 0 && <p className="rounded-md border border-border p-3 text-xs text-muted">No configured targets. Use <code>monkey targets add docker</code> or <code>monkey targets add ssh</code>.</p>}
      <div className="space-y-2">
        {Object.entries(targets).map(([id, target]) => {
          const identity = target.identity;
          return (
            <article key={id} className="rounded-lg border border-border bg-background p-3">
              <div className="flex flex-wrap items-start justify-between gap-2">
                <div><h4 className="text-sm font-medium text-foreground">{identity.displayName}</h4><p className="font-mono text-[11px] text-faint">{id} · {identity.kind} · {identity.platform}</p></div>
                <span className="rounded-full border border-border px-2 py-0.5 text-[10px] text-muted">{identity.trustState}</span>
              </div>
              <div className="mt-2 flex flex-wrap gap-1">{capabilityLabels(identity.capabilities).map((capability) => <span key={capability} className="rounded bg-surface-2 px-1.5 py-0.5 text-[10px] text-muted">{capability}</span>)}</div>
              <p className="mt-2 text-[11px] text-faint">Runner {identity.runnerVersion} · last checked {identity.lastSuccessfulProbeMs ? new Date(identity.lastSuccessfulProbeMs).toLocaleString() : "never"}</p>
              <div className="mt-3 flex gap-2"><Button size="sm" onClick={() => void probe(id)} disabled={busy === id}>{busy === id ? "Checking…" : "Test"}</Button><Button size="sm" variant="ghost" onClick={() => { setEditingId(id); setForm({ id, kind: target.kind === "sshRunner" ? "ssh_runner" : "docker", name: identity.displayName, image: target.image ?? "", host: String(target.config?.host ?? ""), user: String(target.config?.user ?? ""), knownHosts: String(target.config?.knownHosts ?? "") }); }}>Edit</Button><Button size="sm" variant="ghost" onClick={() => setDiagnostics({ [id]: target })}>View diagnostics</Button><Button size="sm" variant="ghost" onClick={() => void remove(id)} disabled={busy === id}>Remove</Button></div>
            </article>
          );
        })}
      </div>
      {diagnostics && <pre className="max-h-64 overflow-auto rounded-md border border-border bg-surface-2 p-3 text-[10px] text-muted">{JSON.stringify(diagnostics, null, 2)}</pre>}
    </div>
  );
}
