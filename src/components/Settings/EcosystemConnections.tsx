import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { openUrl } from "@tauri-apps/plugin-opener";
import { Check, Clipboard, ExternalLink, KeyRound, LockKeyhole, Play, PlugZap, RefreshCw, ShieldAlert, ShieldCheck, Square, X } from "lucide-react";
import { Button, StatusPill } from "../ui";
import {
  ecosystemClient,
  type AuthorizedBridgeAction,
  type McpUiManifest,
  type OpenedMcpUiSession,
  type UiActionApprovalChallenge,
  type UiBridgeRequest,
} from "../../lib/ecosystemClient";
import { useT } from "../../lib/i18n";
import { useEcosystemStore } from "../../store/ecosystemStore";

const FIELD = "h-9 w-full rounded-lg border border-border bg-surface-2 px-3 text-sm text-foreground placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent";
const AREA = "w-full rounded-lg border border-border bg-surface-2 px-3 py-2 font-mono text-xs text-foreground placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent";

function splitList(value: string): string[] {
  return value.split(/[\s,]+/).map((part) => part.trim()).filter(Boolean);
}

function prettyTime(timestamp: number): string {
  return timestamp ? new Date(timestamp).toLocaleString() : "—";
}

export function EcosystemOAuth() {
  const { t } = useT();
  const {
    oauthServers,
    oauthMetadata,
    busy,
    registerOAuth,
    refreshOAuthServers,
    beginOAuth,
    completeOAuth,
    refreshOAuth,
    revokeOAuth,
  } = useEcosystemStore();
  const [form, setForm] = useState({
    serverId: "",
    clientId: "",
    issuer: "",
    authorizationEndpoint: "",
    tokenEndpoint: "",
    revocationEndpoint: "",
    supportedScopes: "openid profile",
    requestedScopes: "openid profile",
    redirectUri: "http://127.0.0.1:47190/oauth/callback",
  });
  const [activeServer, setActiveServer] = useState("");
  const [callbackState, setCallbackState] = useState("");
  const [callbackCode, setCallbackCode] = useState("");

  useEffect(() => {
    void refreshOAuthServers();
  }, [refreshOAuthServers]);

  async function register() {
    await registerOAuth({
      server: {
        contract_version: 1,
        issuer: form.issuer,
        authorization_endpoint: form.authorizationEndpoint,
        token_endpoint: form.tokenEndpoint,
        revocation_endpoint: form.revocationEndpoint || null,
        supported_scopes: splitList(form.supportedScopes),
        supports_pkce_s256: true,
      },
      client: {
        server_id: form.serverId,
        client_id: form.clientId,
        redirect_uri: form.redirectUri,
        requested_scopes: splitList(form.requestedScopes),
      },
    });
    setActiveServer(form.serverId);
  }

  async function begin(serverId: string) {
    const plan = await beginOAuth(serverId);
    const parsed = new URL(plan.authorization_url);
    setCallbackState(parsed.searchParams.get("state") ?? "");
    setCallbackCode("");
    setActiveServer(serverId);
    await openUrl(plan.authorization_url);
  }

  return (
    <div className="space-y-5">
      <section className="rounded-xl border border-border bg-surface p-4">
        <div className="flex items-start gap-3">
          <div className="rounded-lg bg-accent/10 p-2 text-accent"><KeyRound size={18} /></div>
          <div>
            <h3 className="text-sm font-semibold text-foreground">{t("EcosystemOAuth.registerTitle")}</h3>
            <p className="mt-1 text-xs leading-5 text-muted">{t("EcosystemOAuth.registerDescription")}</p>
          </div>
        </div>
        <div className="mt-4 grid gap-3 md:grid-cols-2">
          {([
            ["serverId", t("EcosystemOAuth.serverId"), "github"],
            ["clientId", t("EcosystemOAuth.clientId"), "client-id"],
            ["issuer", t("EcosystemOAuth.issuer"), "https://provider.example"],
            ["authorizationEndpoint", t("EcosystemOAuth.authorizationEndpoint"), "https://provider.example/oauth/authorize"],
            ["tokenEndpoint", t("EcosystemOAuth.tokenEndpoint"), "https://provider.example/oauth/token"],
            ["revocationEndpoint", t("EcosystemOAuth.revocationEndpoint"), "https://provider.example/oauth/revoke"],
            ["supportedScopes", t("EcosystemOAuth.supportedScopes"), "openid profile repositories"],
            ["requestedScopes", t("EcosystemOAuth.requestedScopes"), "openid profile"],
          ] as const).map(([key, label, placeholder]) => (
            <label key={key} className="text-xs text-muted">
              <span className="mb-1 block">{label}</span>
              <input className={FIELD} value={form[key]} placeholder={placeholder} onChange={(event) => setForm((current) => ({ ...current, [key]: event.target.value }))} />
            </label>
          ))}
          <label className="text-xs text-muted md:col-span-2">
            <span className="mb-1 block">{t("EcosystemOAuth.redirectUri")}</span>
            <input className={FIELD} value={form.redirectUri} onChange={(event) => setForm((current) => ({ ...current, redirectUri: event.target.value }))} />
          </label>
        </div>
        <div className="mt-4 flex justify-end">
          <Button variant="primary" disabled={!form.serverId || !form.clientId || !form.issuer || !form.authorizationEndpoint || !form.tokenEndpoint || busy["oauth-register"]} onClick={() => void register()}>
            <PlugZap size={15} /> {t("EcosystemOAuth.register")}
          </Button>
        </div>
      </section>

      <section className="space-y-3">
        <div className="flex items-center justify-between">
          <h3 className="text-sm font-semibold text-foreground">{t("EcosystemOAuth.connectionsTitle")}</h3>
          <Button size="sm" variant="ghost" disabled={busy["oauth-servers"]} onClick={() => void refreshOAuthServers()}><RefreshCw size={14} />{t("EcosystemOAuth.refreshList")}</Button>
        </div>
        {oauthServers.map((registration) => {
          const serverId = registration.client.server_id;
          const metadata = oauthMetadata[serverId];
          const connected = Boolean(metadata);
          return (
            <article key={serverId} className="rounded-xl border border-border bg-surface p-4">
              <div className="flex flex-wrap items-start justify-between gap-3">
                <div>
                  <div className="flex items-center gap-2"><h4 className="text-sm font-semibold text-foreground">{serverId}</h4><StatusPill tone={connected ? "success" : "neutral"}>{connected ? t("EcosystemOAuth.connected") : t("EcosystemOAuth.notConnected")}</StatusPill></div>
                  <p className="mt-1 break-all text-xs text-muted">{registration.server.issuer}</p>
                  <p className="mt-1 text-[11px] text-faint">{registration.client.requested_scopes.join(" · ")}</p>
                </div>
                <div className="flex flex-wrap gap-2">
                  <Button size="sm" variant={connected ? "secondary" : "primary"} onClick={() => void begin(serverId)} disabled={busy["oauth-begin"]}><ExternalLink size={14} />{connected ? t("EcosystemOAuth.reauthorize") : t("EcosystemOAuth.authorize")}</Button>
                  <Button size="sm" disabled={!connected || busy["oauth-refresh"]} onClick={() => void refreshOAuth(serverId)}><RefreshCw size={14} />{t("EcosystemOAuth.refreshToken")}</Button>
                  <Button size="sm" variant="danger" title={!registration.server.revocation_endpoint ? t("EcosystemOAuth.noRevocationEndpoint") : undefined} disabled={!connected || !registration.server.revocation_endpoint || busy["oauth-revoke"]} onClick={() => void revokeOAuth(serverId)}>{t("EcosystemOAuth.revoke")}</Button>
                </div>
              </div>
              {metadata && (
                <dl className="mt-4 grid gap-2 rounded-lg bg-surface-2 p-3 text-xs sm:grid-cols-3">
                  <div><dt className="text-faint">{t("EcosystemOAuth.tokenReference")}</dt><dd className="mt-1 truncate font-mono text-foreground" title={metadata.token_reference.reference_id}>{metadata.token_reference.reference_id}</dd></div>
                  <div><dt className="text-faint">{t("EcosystemOAuth.expires")}</dt><dd className="mt-1 text-foreground">{prettyTime(metadata.expires_unix_ms)}</dd></div>
                  <div><dt className="text-faint">{t("EcosystemOAuth.grantedScopes")}</dt><dd className="mt-1 text-foreground">{metadata.granted_scopes.join(", ") || "—"}</dd></div>
                </dl>
              )}
            </article>
          );
        })}
        {oauthServers.length === 0 && <div className="rounded-xl border border-dashed border-border p-8 text-center text-sm text-muted">{t("EcosystemOAuth.noConnections")}</div>}
      </section>

      {activeServer && (
        <section className="rounded-xl border border-accent/30 bg-accent/5 p-4">
          <h3 className="text-sm font-semibold text-foreground">{t("EcosystemOAuth.callbackTitle", { server: activeServer })}</h3>
          <p className="mt-1 text-xs text-muted">{t("EcosystemOAuth.callbackDescription")}</p>
          <div className="mt-3 grid gap-3 md:grid-cols-2">
            <label className="text-xs text-muted"><span className="mb-1 block">{t("EcosystemOAuth.state")}</span><input className={FIELD} value={callbackState} onChange={(event) => setCallbackState(event.target.value)} /></label>
            <label className="text-xs text-muted"><span className="mb-1 block">{t("EcosystemOAuth.code")}</span><input className={FIELD} value={callbackCode} onChange={(event) => setCallbackCode(event.target.value)} autoComplete="off" /></label>
          </div>
          <div className="mt-3 flex justify-end"><Button variant="primary" disabled={!callbackState || !callbackCode || busy["oauth-complete"]} onClick={() => void completeOAuth(activeServer, callbackState, callbackCode).then(() => setCallbackCode(""))}><Check size={14} />{t("EcosystemOAuth.complete")}</Button></div>
        </section>
      )}

      <div className="flex items-start gap-2 rounded-lg border border-border bg-surface-2 p-3 text-xs text-muted">
        <LockKeyhole size={15} className="mt-0.5 shrink-0 text-success" />
        <p>{t("EcosystemOAuth.secretNotice")}</p>
      </div>
    </div>
  );
}

const DEFAULT_HTML = `<!doctype html>
<html><body style="font:14px system-ui;margin:0;padding:20px;color:#e6e6e6;background:#18181b">
  <h2>MCP App sandbox</h2>
  <p>This document has an opaque origin and no Tauri IPC.</p>
  <button id="run">Request host action</button>
  <pre id="result"></pre>
  <script>
    document.getElementById('run').onclick = () => parent.postMessage({
      type: 'little-monkey:mcp-action', actionId: 'copy-result', payload: { text: 'Hello from the sandbox' }
    }, '*');
    addEventListener('message', event => {
      if (event.data?.type === 'little-monkey:mcp-action-result') document.getElementById('result').textContent = JSON.stringify(event.data, null, 2);
    });
  <\/script>
</body></html>`;

const DEFAULT_MANIFEST: McpUiManifest = {
  contract_version: 1,
  server_id: "example-server",
  resource_uri: "ui://example/index.html",
  resource_sha256: "",
  entry_media_type: "text/html",
  network_origins: [],
  host_actions: {
    "copy-result": {
      action_id: "copy-result",
      kind: "write_clipboard_text",
      target: "clipboard",
      required_permission: "clipboard.write",
      always_requires_approval: true,
    },
  },
  text_fallback: "Example MCP App requesting clipboard access.",
};

function escapeAttribute(value: string): string {
  return value.split("&").join("&amp;").split('"').join("&quot;").split("<").join("&lt;");
}

async function sha256Hex(bytes: Uint8Array): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", bytes);
  return [...new Uint8Array(digest)].map((value) => value.toString(16).padStart(2, "0")).join("");
}

interface PendingMcpApproval {
  challenge: UiActionApprovalChallenge;
  request: UiBridgeRequest;
}

async function dispatchAuthorizedAction(action: AuthorizedBridgeAction, manifest: McpUiManifest): Promise<unknown> {
  if (action.action.kind === "write_clipboard_text") {
    const payload = action.payload as { text?: unknown } | string;
    const text = typeof payload === "string" ? payload : typeof payload?.text === "string" ? payload.text : JSON.stringify(action.payload);
    await navigator.clipboard.writeText(text);
    return { copied: true };
  }
  if (action.action.kind === "open_external_url") {
    const parsed = new URL(action.action.target);
    if (parsed.protocol !== "https:") throw new Error("Only HTTPS external URLs are allowed.");
    await openUrl(parsed.toString());
    return { opened: parsed.toString() };
  }
  if (action.action.kind === "invoke_tool") {
    const prefix = `mcp__${manifest.server_id}__`;
    if (!action.action.target.startsWith(prefix) || action.action.target.length === prefix.length) {
      throw new Error("The approved MCP tool target is not bound to this server.");
    }
    const payload = typeof action.payload === "object" && action.payload !== null ? action.payload : { value: action.payload };
    return invoke("mcp_call_tool", {
      server_id: manifest.server_id,
      tool_name: action.action.target.slice(prefix.length),
      arguments: payload,
      turn_id: `mcp-app-${action.session_id}`,
      tool_call_id: action.approval_id,
    });
  }
  const payload = action.payload as { content?: unknown; kind?: unknown } | string;
  const content = typeof payload === "string" ? payload : typeof payload?.content === "string" ? payload.content : JSON.stringify(action.payload, null, 2);
  const kind = typeof payload === "object" && payload !== null && payload.kind === "html" ? "html" : "html";
  const artifactId = await invoke<string>("artifact_publish", { content, kind });
  return { artifact_id: artifactId };
}

export function EcosystemMcpApps() {
  const { t } = useT();
  const [manifestText, setManifestText] = useState(JSON.stringify(DEFAULT_MANIFEST, null, 2));
  const [resourceText, setResourceText] = useState(DEFAULT_HTML);
  const [permissionsText, setPermissionsText] = useState("clipboard.write");
  const [session, setSession] = useState<OpenedMcpUiSession | null>(null);
  const [activeManifest, setActiveManifest] = useState<McpUiManifest | null>(null);
  const [srcDoc, setSrcDoc] = useState("");
  const [pending, setPending] = useState<PendingMcpApproval | null>(null);
  const [status, setStatus] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const iframeRef = useRef<HTMLIFrameElement>(null);
  const sessionRef = useRef<OpenedMcpUiSession | null>(null);
  const manifestRef = useRef<McpUiManifest | null>(null);
  const pendingRef = useRef<PendingMcpApproval | null>(null);
  sessionRef.current = session;
  manifestRef.current = activeManifest;

  async function closeSession() {
    const current = sessionRef.current;
    sessionRef.current = null;
    setSession(null);
    setPending(null);
    pendingRef.current = null;
    setSrcDoc("");
    if (current) await ecosystemClient.closeMcpUi(current.session_id);
  }

  useEffect(() => () => { const current = sessionRef.current; if (current) void ecosystemClient.closeMcpUi(current.session_id); }, []);

  useEffect(() => {
    async function onMessage(event: MessageEvent) {
      if (!sessionRef.current || !manifestRef.current || event.source !== iframeRef.current?.contentWindow) return;
      const data = event.data as { type?: unknown; actionId?: unknown; payload?: unknown };
      if (data?.type !== "little-monkey:mcp-action" || typeof data.actionId !== "string") return;
      if (pendingRef.current) {
        iframeRef.current?.contentWindow?.postMessage({ type: "little-monkey:mcp-action-result", actionId: data.actionId, ok: false, error: "Another host action is awaiting user approval" }, "*");
        return;
      }
      const current = sessionRef.current;
      const manifest = manifestRef.current;
      const request: UiBridgeRequest = {
        session_id: current.session_id,
        server_id: manifest.server_id,
        resource_sha256: manifest.resource_sha256,
        action_id: data.actionId,
        payload: data.payload ?? null,
      };
      try {
        const challenge = await ecosystemClient.prepareMcpUiAction(current.session_id, current.bridge_capability, request);
        const approval = { challenge, request };
        pendingRef.current = approval;
        setPending(approval);
        setStatus(t("EcosystemMcpApps.awaitingApproval"));
      } catch (caught) {
        const message = caught instanceof Error ? caught.message : String(caught);
        setError(message);
        iframeRef.current?.contentWindow?.postMessage({ type: "little-monkey:mcp-action-result", actionId: data.actionId, ok: false, error: message }, "*");
      }
    }
    window.addEventListener("message", onMessage);
    return () => window.removeEventListener("message", onMessage);
  }, [t]);

  async function launch() {
    setBusy(true);
    setError(null);
    try {
      await closeSession();
      const parsed = JSON.parse(manifestText) as McpUiManifest;
      const bytes = new TextEncoder().encode(resourceText);
      parsed.resource_sha256 = await sha256Hex(bytes);
      const opened = await ecosystemClient.openMcpUi(parsed, Array.from(bytes), splitList(permissionsText));
      if (!opened.host_plan.opaque_origin_required
        || opened.host_plan.iframe_sandbox_tokens.some((token) => token !== "allow-scripts")
        || opened.host_plan.tauri_ipc_exposed
        || opened.host_plan.filesystem_exposed
        || opened.host_plan.keychain_exposed) {
        await ecosystemClient.closeMcpUi(opened.session_id);
        throw new Error(t("EcosystemMcpApps.unsafeHostPlan"));
      }
      const csp = `<meta http-equiv="Content-Security-Policy" content="${escapeAttribute(opened.host_plan.content_security_policy)}">`;
      setActiveManifest(parsed);
      manifestRef.current = parsed;
      setSession(opened);
      sessionRef.current = opened;
      setSrcDoc(`${csp}${resourceText}`);
      setManifestText(JSON.stringify(parsed, null, 2));
      setStatus(t("EcosystemMcpApps.sessionReady"));
    } catch (caught) {
      setError(caught instanceof Error ? caught.message : String(caught));
    } finally {
      setBusy(false);
    }
  }

  async function decide(approved: boolean) {
    if (!pending || !session || !activeManifest) return;
    setBusy(true);
    setError(null);
    const actionId = pending.request.action_id;
    try {
      await ecosystemClient.decideMcpUiAction(pending.challenge.challenge_id, approved);
      if (!approved) {
        iframeRef.current?.contentWindow?.postMessage({ type: "little-monkey:mcp-action-result", actionId, ok: false, error: "Denied by user" }, "*");
        setStatus(t("EcosystemMcpApps.actionDenied"));
        return;
      }
      const action = await ecosystemClient.authorizeMcpUiAction(session.session_id, session.bridge_capability, pending.request);
      const result = await dispatchAuthorizedAction(action, activeManifest);
      iframeRef.current?.contentWindow?.postMessage({ type: "little-monkey:mcp-action-result", actionId, ok: true, result }, "*");
      setStatus(t("EcosystemMcpApps.actionCompleted"));
    } catch (caught) {
      const message = caught instanceof Error ? caught.message : String(caught);
      setError(message);
      iframeRef.current?.contentWindow?.postMessage({ type: "little-monkey:mcp-action-result", actionId, ok: false, error: message }, "*");
    } finally {
      setPending(null);
      pendingRef.current = null;
      setBusy(false);
    }
  }

  const hostSummary = useMemo(() => session ? [
    { label: t("EcosystemMcpApps.opaqueOrigin"), safe: session.host_plan.opaque_origin_required },
    { label: t("EcosystemMcpApps.noTauriIpc"), safe: !session.host_plan.tauri_ipc_exposed },
    { label: t("EcosystemMcpApps.noFilesystem"), safe: !session.host_plan.filesystem_exposed },
    { label: t("EcosystemMcpApps.noKeychain"), safe: !session.host_plan.keychain_exposed },
  ] : [], [session, t]);

  return (
    <div className="space-y-4">
      <div className="grid gap-4 xl:grid-cols-2">
        <section className="space-y-3 rounded-xl border border-border bg-surface p-4">
          <div><h3 className="text-sm font-semibold text-foreground">{t("EcosystemMcpApps.manifestTitle")}</h3><p className="mt-1 text-xs leading-5 text-muted">{t("EcosystemMcpApps.manifestDescription")}</p></div>
          <label className="block text-xs text-muted"><span className="mb-1 block">{t("EcosystemMcpApps.manifestJson")}</span><textarea rows={16} className={AREA} value={manifestText} onChange={(event) => setManifestText(event.target.value)} spellCheck={false} /></label>
          <label className="block text-xs text-muted"><span className="mb-1 block">{t("EcosystemMcpApps.resourceHtml")}</span><textarea rows={12} className={AREA} value={resourceText} onChange={(event) => setResourceText(event.target.value)} spellCheck={false} /></label>
          <label className="block text-xs text-muted"><span className="mb-1 block">{t("EcosystemMcpApps.grantedPermissions")}</span><input className={FIELD} value={permissionsText} onChange={(event) => setPermissionsText(event.target.value)} /></label>
          <div className="flex flex-wrap justify-end gap-2">
            {session && <Button variant="ghost" onClick={() => void closeSession()}><Square size={14} />{t("EcosystemMcpApps.closeSession")}</Button>}
            <Button variant="primary" disabled={busy} onClick={() => void launch()}><Play size={14} />{t("EcosystemMcpApps.openSandbox")}</Button>
          </div>
        </section>

        <section className="min-h-[30rem] overflow-hidden rounded-xl border border-border bg-surface">
          <div className="flex flex-wrap items-center justify-between gap-2 border-b border-border p-3">
            <div className="flex items-center gap-2"><ShieldCheck size={16} className="text-success" /><h3 className="text-sm font-semibold text-foreground">{t("EcosystemMcpApps.sandboxTitle")}</h3></div>
            <StatusPill tone={session ? "success" : "neutral"}>{session ? t("EcosystemMcpApps.live") : t("EcosystemMcpApps.closed")}</StatusPill>
          </div>
          {session ? (
            <>
              <div className="grid grid-cols-2 gap-2 border-b border-border p-3">
                {hostSummary.map((item) => <div key={item.label} className="flex items-center gap-1.5 text-[11px] text-muted">{item.safe ? <Check size={12} className="text-success" /> : <X size={12} className="text-danger" />}{item.label}</div>)}
              </div>
              <iframe
                ref={iframeRef}
                title={t("EcosystemMcpApps.iframeTitle")}
                sandbox="allow-scripts"
                srcDoc={srcDoc}
                className="h-[34rem] w-full border-0 bg-white"
              />
            </>
          ) : (
            <div className="flex h-[34rem] flex-col items-center justify-center gap-3 p-8 text-center text-sm text-muted"><ShieldAlert size={32} className="text-faint" /><p>{t("EcosystemMcpApps.emptySandbox")}</p></div>
          )}
        </section>
      </div>

      {pending && (
        <section role="alertdialog" aria-labelledby="mcp-action-approval-title" className="rounded-xl border border-warning/40 bg-warning-soft p-4">
          <div className="flex items-start gap-3"><ShieldAlert size={18} className="mt-0.5 shrink-0 text-warning" /><div className="min-w-0"><h3 id="mcp-action-approval-title" className="text-sm font-semibold text-foreground">{t("EcosystemMcpApps.approvalTitle")}</h3><p className="mt-1 text-xs text-muted">{t("EcosystemMcpApps.approvalDescription")}</p></div></div>
          <dl className="mt-3 grid gap-2 rounded-lg bg-background/50 p-3 text-xs sm:grid-cols-2">
            <div><dt className="text-faint">{t("EcosystemMcpApps.action")}</dt><dd className="mt-1 font-mono text-foreground">{pending.challenge.action_id}</dd></div>
            <div><dt className="text-faint">{t("EcosystemMcpApps.target")}</dt><dd className="mt-1 break-all font-mono text-foreground">{pending.challenge.action_target}</dd></div>
            <div><dt className="text-faint">{t("EcosystemMcpApps.permission")}</dt><dd className="mt-1 font-mono text-foreground">{pending.challenge.required_permission}</dd></div>
            <div><dt className="text-faint">{t("EcosystemMcpApps.payloadDigest")}</dt><dd className="mt-1 truncate font-mono text-foreground" title={pending.challenge.payload_summary_sha256}>{pending.challenge.payload_summary_sha256}</dd></div>
          </dl>
          <pre className="mt-3 max-h-40 overflow-auto rounded-lg bg-background p-3 text-xs text-foreground">{JSON.stringify(pending.request.payload, null, 2)}</pre>
          <div className="mt-3 flex justify-end gap-2"><Button variant="ghost" disabled={busy} onClick={() => void decide(false)}>{t("EcosystemMcpApps.deny")}</Button><Button variant="primary" disabled={busy} onClick={() => void decide(true)}>{t("EcosystemMcpApps.approveOnce")}</Button></div>
        </section>
      )}

      {(status || error) && <div role="status" className={`flex items-center gap-2 rounded-lg border p-3 text-xs ${error ? "border-danger/30 bg-danger-soft text-danger" : "border-border bg-surface-2 text-muted"}`}>{error ? <X size={14} /> : <Clipboard size={14} />}{error ?? status}</div>}
    </div>
  );
}
