import { useEffect, useState } from "react";
import { Copy, Link2, Save, Shield, ShieldOff, Trash2 } from "lucide-react";
import { Button, StatusPill } from "../../ui";
import type {
  ApiBackend,
  ApiScope,
  LanServerPolicy,
  PairingRequest,
  TlsPolicy,
} from "../../../lib/runtimeHubClient";
import { useRuntimeHubStore } from "../../../store/runtimeHubStore";
import {
  BusyButton,
  CONTROL_CLASS,
  ErrorNotice,
  Field,
  formatDate,
  labelize,
  SectionHeading,
  SuccessNotice,
  Toggle,
} from "./RuntimeHubShared";

const BACKENDS: ApiBackend[] = ["managed_local", "ollama", "mlx", "cloud_provider"];
const SCOPES: ApiScope[] = [
  "chat_completions",
  "responses",
  "messages",
  "model_discover",
  "model_download",
  "model_load",
  "model_unload",
  "model_delete",
  "model_status",
];
const MUTATION_SCOPES: ApiScope[] = ["model_download", "model_load", "model_unload", "model_delete"];

function defaultPolicy(): LanServerPolicy {
  return {
    bindAddress: "127.0.0.1",
    port: 1234,
    requireAuthentication: true,
    pairingRequired: true,
    tls: { mode: "disabled" },
    corsAllowlist: [],
    allowedBackends: ["managed_local", "ollama", "mlx"],
    allowedLanMutations: [],
    allowCloudProvidersOverLan: false,
    rateLimit: { windowMs: 60_000, maxRequests: 60, maxInputBytes: 64 * 1024 * 1024 },
    pairingTtlMs: 5 * 60_000,
  };
}

function toggleItem<T extends string>(values: T[], value: T): T[] {
  return values.includes(value) ? values.filter((entry) => entry !== value) : [...values, value];
}

function CheckboxGrid<T extends string>({
  label,
  values,
  options,
  onChange,
  disabled,
}: {
  label: string;
  values: T[];
  options: T[];
  onChange: (values: T[]) => void;
  disabled?: (value: T) => boolean;
}) {
  return (
    <fieldset>
      <legend className="mb-1.5 text-sm font-medium text-foreground">{label}</legend>
      <div className="grid gap-2 sm:grid-cols-2">
        {options.map((option) => (
          <label key={option} className="flex min-h-11 cursor-pointer items-center gap-2 rounded-md border border-border bg-surface-2 px-3 py-2 text-sm text-foreground has-[:disabled]:cursor-not-allowed has-[:disabled]:opacity-50">
            <input
              type="checkbox"
              checked={values.includes(option)}
              disabled={disabled?.(option)}
              onChange={() => onChange(toggleItem(values, option))}
              className="h-4 w-4 rounded border-border accent-[var(--color-accent)] focus-visible:ring-2 focus-visible:ring-accent"
            />
            {labelize(option)}
          </label>
        ))}
      </div>
    </fieldset>
  );
}

function PolicyEditor() {
  const savedPolicy = useRuntimeHubStore((state) => state.lanPolicy);
  const configure = useRuntimeHubStore((state) => state.configureLan);
  const validate = useRuntimeHubStore((state) => state.validateLanPolicy);
  const disable = useRuntimeHubStore((state) => state.disableLan);
  const serverStatus = useRuntimeHubStore((state) => state.httpServerStatus);
  const startServer = useRuntimeHubStore((state) => state.startHttpServer);
  const stopServer = useRuntimeHubStore((state) => state.stopHttpServer);
  const storeTlsIdentity = useRuntimeHubStore((state) => state.storeTlsIdentity);
  const busy = useRuntimeHubStore((state) => state.busy);
  const error = useRuntimeHubStore((state) => state.errors["lan-policy"] ?? state.errors["lan-disable"] ?? state.errors["http-server"]);
  const tlsError = useRuntimeHubStore((state) => state.errors["tls-identity"]);
  const [policy, setPolicy] = useState<LanServerPolicy>(() => savedPolicy ?? defaultPolicy());
  const [validated, setValidated] = useState(false);
  const [disableConfirmation, setDisableConfirmation] = useState("");
  const [showDisable, setShowDisable] = useState(false);
  const [certificatePem, setCertificatePem] = useState("");
  const [privateKeyPem, setPrivateKeyPem] = useState("");
  const [identityStored, setIdentityStored] = useState(false);

  useEffect(() => {
    if (savedPolicy) setPolicy(savedPolicy);
  }, [savedPolicy]);

  const tls = policy.tls;
  const effectivePolicy = savedPolicy ?? policy;
  const effectiveTls = effectivePolicy.tls;
  const visibleEndpoint = serverStatus?.bindAddress && serverStatus.port
    ? `${serverStatus.tls ? "https" : "http"}://${serverStatus.bindAddress}:${serverStatus.port}`
    : `${effectiveTls.mode === "certificate" ? "https" : "http"}://${effectivePolicy.bindAddress}:${effectivePolicy.port}`;

  function patch(next: Partial<LanServerPolicy>) {
    setPolicy((current) => ({ ...current, ...next }));
    setValidated(false);
  }

  function setTlsMode(mode: "disabled" | "certificate") {
    const next: TlsPolicy = mode === "disabled"
      ? { mode: "disabled" }
      : { mode: "certificate", certificate_sha256: "", private_key_reference: "", minimum_version: "1.3" };
    patch({ tls: next });
  }

  function installIdentity() {
    if (tls.mode !== "certificate") return;
    setIdentityStored(false);
    void storeTlsIdentity(tls.private_key_reference, certificatePem, privateKeyPem)
      .then((fingerprint) => {
        patch({ tls: { ...tls, certificate_sha256: fingerprint } });
        setPrivateKeyPem("");
        setIdentityStored(true);
      })
      .catch(() => {});
  }

  return (
    <section className="rounded-lg border border-border bg-background p-4" aria-labelledby="lan-policy-heading">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <div className="flex flex-wrap items-center gap-2">
            <h3 id="lan-policy-heading" className="text-sm font-semibold text-foreground">Server policy</h3>
            <StatusPill tone={serverStatus?.status === "running" ? "success" : serverStatus?.status === "error" ? "danger" : savedPolicy ? "warning" : "neutral"}>
              {serverStatus?.status === "running" ? "Listening" : serverStatus?.status === "error" ? "Listener error" : savedPolicy ? "Configured, stopped" : "Disabled"}
            </StatusPill>
          </div>
          <p className="mt-1 text-xs leading-5 text-muted">Non-loopback interfaces require TLS, authentication, pairing, a narrow CORS allowlist, and local-only backends.</p>
        </div>
      </div>

      <div className="mt-4 grid gap-4 sm:grid-cols-2">
        <Field label="Bind address" hint="Exact interface IP only; wildcard and multicast addresses are rejected.">
          <input value={policy.bindAddress} onChange={(event) => patch({ bindAddress: event.target.value })} className={`${CONTROL_CLASS} font-mono`} />
        </Field>
        <Field label="Port">
          <input type="number" min={1} max={65535} value={policy.port} onChange={(event) => patch({ port: Number(event.target.value) })} className={CONTROL_CLASS} />
        </Field>
        <Field label="Pairing lifetime (seconds)" hint="30 seconds to 1 hour.">
          <input type="number" min={30} max={3600} value={policy.pairingTtlMs / 1000} onChange={(event) => patch({ pairingTtlMs: Number(event.target.value) * 1000 })} className={CONTROL_CLASS} />
        </Field>
        <Field label="CORS origins" hint="One exact http(s) origin per line; required outside loopback.">
          <textarea
            value={policy.corsAllowlist.join("\n")}
            onChange={(event) => patch({ corsAllowlist: event.target.value.split("\n").map((line) => line.trim()).filter(Boolean) })}
            rows={3}
            className={`${CONTROL_CLASS} resize-y font-mono`}
            placeholder="https://trusted-device.example"
          />
        </Field>
      </div>

      <div className="mt-4 grid gap-3 sm:grid-cols-2">
        <Toggle checked={policy.requireAuthentication} onChange={(requireAuthentication) => patch({ requireAuthentication })} label="Require bearer authentication" />
        <Toggle checked={policy.pairingRequired} onChange={(pairingRequired) => patch({ pairingRequired })} label="Require pairing" />
        <Toggle
          checked={policy.allowCloudProvidersOverLan}
          onChange={(allowCloudProvidersOverLan) => patch({ allowCloudProvidersOverLan })}
          label="Allow cloud providers over LAN"
          description="Only valid for loopback policy. Non-loopback validation rejects this route."
        />
      </div>

      <div className="mt-4">
        <Field label="TLS policy">
          <select value={tls.mode} onChange={(event) => setTlsMode(event.target.value as "disabled" | "certificate")} className={CONTROL_CLASS}>
            <option value="disabled">Disabled (loopback only)</option>
            <option value="certificate">Certificate reference</option>
          </select>
        </Field>
        {tls.mode === "certificate" && (
          <div className="mt-3 grid gap-4 sm:grid-cols-2">
            <Field label="Certificate SHA-256" hint="Calculated from the leaf certificate when you store the identity below.">
              <input value={tls.certificate_sha256} onChange={(event) => patch({ tls: { ...tls, certificate_sha256: event.target.value } })} className={`${CONTROL_CLASS} font-mono`} />
            </Field>
            <Field label="Private key reference" hint="A secure keychain/reference id, never key material.">
              <input value={tls.private_key_reference} onChange={(event) => patch({ tls: { ...tls, private_key_reference: event.target.value } })} className={`${CONTROL_CLASS} font-mono`} />
            </Field>
            <Field label="Minimum TLS version">
              <select value={tls.minimum_version} onChange={(event) => patch({ tls: { ...tls, minimum_version: event.target.value as "1.2" | "1.3" } })} className={CONTROL_CLASS}>
                <option value="1.3">TLS 1.3</option>
                <option value="1.2">TLS 1.2</option>
              </select>
            </Field>
            <div className="sm:col-span-2 rounded-md border border-border bg-surface-2 p-3">
              <p className="text-sm font-medium text-foreground">Install TLS identity in the OS keychain</p>
              <p className="mt-1 text-xs leading-5 text-muted">The private key crosses local Tauri IPC once, is stored only in the operating-system keychain under the reference above, and is cleared from this form after success.</p>
              <div className="mt-3 grid gap-3 lg:grid-cols-2">
                <Field label="Certificate PEM">
                  <textarea value={certificatePem} onChange={(event) => setCertificatePem(event.target.value)} rows={7} spellCheck={false} className={`${CONTROL_CLASS} resize-y font-mono text-xs`} placeholder="-----BEGIN CERTIFICATE-----" />
                </Field>
                <Field label="Private key PEM">
                  <textarea value={privateKeyPem} onChange={(event) => setPrivateKeyPem(event.target.value)} rows={7} spellCheck={false} className={`${CONTROL_CLASS} resize-y font-mono text-xs`} placeholder="-----BEGIN PRIVATE KEY-----" autoComplete="off" />
                </Field>
              </div>
              <ErrorNotice message={tlsError} />
              {identityStored && <div className="mt-3"><SuccessNotice>Identity validated and stored; the certificate fingerprint was applied to this policy.</SuccessNotice></div>}
              <div className="mt-3 flex justify-end">
                <BusyButton type="button" busy={busy["tls-identity"]} disabled={!tls.private_key_reference || !certificatePem || !privateKeyPem} onClick={installIdentity}>
                  <Shield size={15} aria-hidden="true" /> Store identity securely
                </BusyButton>
              </div>
            </div>
          </div>
        )}
      </div>

      <div className="mt-4 grid gap-4 lg:grid-cols-2">
        <CheckboxGrid
          label="Allowed backends"
          values={policy.allowedBackends}
          options={BACKENDS}
          onChange={(allowedBackends) => patch({ allowedBackends })}
        />
        <CheckboxGrid
          label="Allowed LAN mutations"
          values={policy.allowedLanMutations}
          options={MUTATION_SCOPES}
          onChange={(allowedLanMutations) => patch({ allowedLanMutations })}
        />
      </div>

      <div className="mt-4 grid gap-4 sm:grid-cols-3">
        <Field label="Rate window (seconds)">
          <input type="number" min={1} value={policy.rateLimit.windowMs / 1000} onChange={(event) => patch({ rateLimit: { ...policy.rateLimit, windowMs: Number(event.target.value) * 1000 } })} className={CONTROL_CLASS} />
        </Field>
        <Field label="Requests per window">
          <input type="number" min={1} value={policy.rateLimit.maxRequests} onChange={(event) => patch({ rateLimit: { ...policy.rateLimit, maxRequests: Number(event.target.value) } })} className={CONTROL_CLASS} />
        </Field>
        <Field label="Input bytes per window">
          <input type="number" min={1} value={policy.rateLimit.maxInputBytes} onChange={(event) => patch({ rateLimit: { ...policy.rateLimit, maxInputBytes: Number(event.target.value) } })} className={CONTROL_CLASS} />
        </Field>
      </div>

      <ErrorNotice message={error} />
      {validated && <div className="mt-3"><SuccessNotice>Policy passed strict server-side validation.</SuccessNotice></div>}
      {serverStatus && (
        <div className="mt-3 rounded-md border border-border bg-surface-2 p-3 text-xs text-muted">
          <p className="font-medium text-foreground">
            {serverStatus.status === "running"
              ? visibleEndpoint
              : `Listener ${serverStatus.status}`}
          </p>
          <p className="mt-1">{serverStatus.requestCount} completed · {serverStatus.activeRequests} active · last request {formatDate(serverStatus.lastRequestAtMs)}</p>
          {serverStatus.lastError && <p className="mt-1 text-danger">{serverStatus.lastError}</p>}
        </div>
      )}

      <div className="mt-3 rounded-md border border-border bg-surface-2 p-3" aria-label="Effective LAN API security">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <p className="text-sm font-medium text-foreground">{savedPolicy ? "Effective listener security" : "Draft listener security"}</p>
          <code className="break-all rounded bg-background px-2 py-1 font-mono text-xs text-foreground">{visibleEndpoint}</code>
        </div>
        <dl className="mt-3 grid gap-3 text-xs sm:grid-cols-2 lg:grid-cols-4">
          <div>
            <dt className="font-medium text-foreground">Authentication</dt>
            <dd className="mt-1 text-muted">{effectivePolicy.requireAuthentication ? "Scoped bearer token required" : "Internal loopback access"}{effectivePolicy.pairingRequired ? " · pairing required" : ""}</dd>
          </div>
          <div>
            <dt className="font-medium text-foreground">TLS</dt>
            <dd className="mt-1 break-words text-muted">{effectiveTls.mode === "certificate" ? `TLS ${effectiveTls.minimum_version}+ · fingerprint pinned` : "Plain HTTP · loopback only"}</dd>
          </div>
          <div>
            <dt className="font-medium text-foreground">Browser origins</dt>
            <dd className="mt-1 break-words text-muted">{effectivePolicy.corsAllowlist.length ? effectivePolicy.corsAllowlist.join(", ") : "No cross-origin browser access"}</dd>
          </div>
          <div>
            <dt className="font-medium text-foreground">API boundary</dt>
            <dd className="mt-1 text-muted">Inference and scoped model lifecycle only · no files, shell, Git, MCP, or agent tools</dd>
          </div>
        </dl>
        <p className="mt-3 break-words border-t border-border pt-3 text-xs text-muted">
          Backends: {effectivePolicy.allowedBackends.map(labelize).join(", ") || "none"} · LAN mutations: {effectivePolicy.allowedLanMutations.map(labelize).join(", ") || "none"} · CORS uses exact origin matching.
        </p>
      </div>

      <div className="mt-4 flex flex-wrap justify-end gap-2">
        {savedPolicy && serverStatus?.status === "running" && (
          <BusyButton type="button" busy={busy["http-server"]} onClick={() => void stopServer().catch(() => {})}>
            <ShieldOff size={15} aria-hidden="true" /> Stop listener
          </BusyButton>
        )}
        {savedPolicy && serverStatus?.status !== "running" && (
          <BusyButton type="button" busy={busy["http-server"]} onClick={() => void startServer().catch(() => {})}>
            <Shield size={15} aria-hidden="true" /> Start listener
          </BusyButton>
        )}
        {savedPolicy && (
          <Button type="button" variant="danger" className="min-h-11" onClick={() => setShowDisable(true)}>
            <ShieldOff size={15} aria-hidden="true" /> Disable LAN API
          </Button>
        )}
        <BusyButton
          type="button"
          busy={busy["lan-policy"]}
          onClick={() => void validate(policy).then(() => setValidated(true)).catch(() => setValidated(false))}
        >
          <Shield size={15} aria-hidden="true" /> Validate
        </BusyButton>
        <BusyButton type="button" variant="primary" busy={busy["lan-policy"]} onClick={() => void configure(policy).then(() => setValidated(true)).catch(() => setValidated(false))}>
          <Save size={15} aria-hidden="true" /> Save and start
        </BusyButton>
      </div>

      {showDisable && (
        <div className="mt-4 rounded-md border border-danger/30 bg-danger-soft p-3">
          <p className="text-xs leading-5 text-danger">Disabling the LAN API revokes all persisted tokens. Type <code className="font-mono font-semibold">DISABLE LAN API</code> to continue.</p>
          <input value={disableConfirmation} onChange={(event) => setDisableConfirmation(event.target.value)} className={`${CONTROL_CLASS} mt-3 font-mono`} aria-label="Disable LAN API confirmation" autoComplete="off" />
          <div className="mt-3 flex flex-wrap justify-end gap-2">
            <Button type="button" className="min-h-11" onClick={() => { setShowDisable(false); setDisableConfirmation(""); }}>Keep enabled</Button>
            <BusyButton
              type="button"
              variant="danger"
              busy={busy["lan-disable"]}
              disabled={disableConfirmation !== "DISABLE LAN API"}
              onClick={() => void disable().then(() => { setShowDisable(false); setDisableConfirmation(""); }).catch(() => {})}
            >
              <ShieldOff size={15} aria-hidden="true" /> Disable and revoke tokens
            </BusyButton>
          </div>
        </div>
      )}
    </section>
  );
}

function PairingPanel() {
  const policy = useRuntimeHubStore((state) => state.lanPolicy);
  const challenge = useRuntimeHubStore((state) => state.pairingChallenge);
  const pairedToken = useRuntimeHubStore((state) => state.pairedToken);
  const beginPairing = useRuntimeHubStore((state) => state.beginPairing);
  const completePairing = useRuntimeHubStore((state) => state.completePairing);
  const dismissToken = useRuntimeHubStore((state) => state.dismissPairedToken);
  const busy = useRuntimeHubStore((state) => state.busy["lan-pairing"]);
  const error = useRuntimeHubStore((state) => state.errors["lan-pairing"]);
  const installedModels = useRuntimeHubStore((state) => state.installedModels);
  const [clientLabel, setClientLabel] = useState("");
  const [scopes, setScopes] = useState<ApiScope[]>(["chat_completions", "responses", "messages", "model_discover", "model_status"]);
  const [backends, setBackends] = useState<ApiBackend[]>(["managed_local", "ollama", "mlx"]);
  const [allowedModels, setAllowedModels] = useState<string[]>([]);
  const [expiresHours, setExpiresHours] = useState(24);
  const [pairingCode, setPairingCode] = useState("");
  const [remoteAddress, setRemoteAddress] = useState("127.0.0.1");
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    if (challenge) setPairingCode(challenge.pairingCode);
  }, [challenge]);

  function begin() {
    const request: PairingRequest = {
      clientLabel: clientLabel.trim(),
      scopes,
      backends,
      allowedModels,
      tokenExpiresAtMs: expiresHours > 0 ? Date.now() + expiresHours * 60 * 60_000 : null,
    };
    void beginPairing(request, remoteAddress).catch(() => {});
  }

  async function copyToken() {
    if (!pairedToken) return;
    try {
      await navigator.clipboard.writeText(pairedToken.token);
      setCopied(true);
      globalThis.setTimeout(() => setCopied(false), 1500);
    } catch {
      setCopied(false);
    }
  }

  return (
    <section className="rounded-lg border border-border bg-background p-4" aria-labelledby="lan-pairing-heading">
      <SectionHeading
        title="Pair a new API client (recommended)"
        description="Use this flow for every new IDE, script, agent, or widget. It issues a narrowly scoped token whose plaintext is returned once and is never available from the token list again."
      />
      {!policy && <div className="mt-3"><ErrorNotice message="Enable and save a LAN policy before pairing clients." /></div>}
      <div className="mt-4 grid gap-4 sm:grid-cols-2">
        <Field label="Client label">
          <input value={clientLabel} onChange={(event) => setClientLabel(event.target.value)} className={CONTROL_CLASS} placeholder="VS Code on laptop" />
        </Field>
        <Field label="Observed remote address">
          <input value={remoteAddress} onChange={(event) => setRemoteAddress(event.target.value)} className={`${CONTROL_CLASS} font-mono`} />
        </Field>
        <Field label="Token lifetime (hours)" hint="Use 0 for no expiry.">
          <input type="number" min={0} value={expiresHours} onChange={(event) => setExpiresHours(Number(event.target.value))} className={CONTROL_CLASS} />
        </Field>
      </div>
      <div className="mt-4 grid gap-4 lg:grid-cols-2">
        <CheckboxGrid label="Token scopes" values={scopes} options={SCOPES} onChange={setScopes} />
        <CheckboxGrid label="Token backends" values={backends} options={BACKENDS} onChange={setBackends} disabled={(backend) => !policy?.allowedBackends.includes(backend)} />
      </div>
      <fieldset className="mt-4">
        <legend className="mb-1.5 text-sm font-medium text-foreground">Allowed models</legend>
        <p className="mb-2 text-xs text-muted">Leave empty only when this client may access every model allowed by its backend scopes.</p>
        <div className="grid gap-2 sm:grid-cols-2">
          {installedModels.map((model) => (
            <label key={model.assetId} className="flex min-h-11 cursor-pointer items-center gap-2 rounded-md border border-border bg-surface-2 px-3 py-2 text-sm text-foreground">
              <input type="checkbox" checked={allowedModels.includes(model.modelId)} onChange={() => setAllowedModels(toggleItem(allowedModels, model.modelId))} className="h-4 w-4 accent-[var(--color-accent)]" />
              <span className="min-w-0 truncate">{model.displayName}</span>
            </label>
          ))}
        </div>
      </fieldset>
      <ErrorNotice message={error} />
      <div className="mt-4 flex justify-end">
        <BusyButton type="button" variant="primary" busy={busy} disabled={!policy || !clientLabel.trim() || !scopes.length || !backends.length} onClick={begin}>
          <Link2 size={15} aria-hidden="true" /> Begin pairing
        </BusyButton>
      </div>

      {challenge && (
        <div className="mt-4 rounded-md border border-warning/30 bg-warning-soft p-3">
          <p className="text-sm font-medium text-warning">Pairing challenge</p>
          <p className="mt-1 text-xs text-warning">Expires {formatDate(challenge.expiresAtMs)}. Confirm the code on the intended client.</p>
          <div className="mt-3 grid gap-3 sm:grid-cols-2">
            <Field label="Challenge id"><input readOnly value={challenge.challengeId} className={`${CONTROL_CLASS} font-mono`} /></Field>
            <Field label="Pairing code"><input value={pairingCode} onChange={(event) => setPairingCode(event.target.value)} className={`${CONTROL_CLASS} font-mono text-lg tracking-widest`} /></Field>
          </div>
          <div className="mt-3 flex justify-end">
            <BusyButton type="button" variant="primary" busy={busy} onClick={() => void completePairing(challenge.challengeId, pairingCode, remoteAddress).catch(() => {})}>
              <Shield size={15} aria-hidden="true" /> Complete pairing
            </BusyButton>
          </div>
        </div>
      )}

      {pairedToken && (
        <div className="mt-4">
          <SuccessNotice>
            <span className="block font-medium">Token created. Copy it now; it cannot be retrieved again.</span>
            <code className="mt-2 block break-all rounded bg-background/60 p-2 font-mono text-xs text-foreground">{pairedToken.token}</code>
          </SuccessNotice>
          <div className="mt-2 flex flex-wrap justify-end gap-2">
            <Button type="button" className="min-h-11" onClick={() => void copyToken()}><Copy size={14} aria-hidden="true" /> {copied ? "Copied" : "Copy token"}</Button>
            <Button type="button" className="min-h-11" onClick={dismissToken}>I stored it securely</Button>
          </div>
        </div>
      )}
    </section>
  );
}

function TokenAndAuditPanel() {
  const tokens = useRuntimeHubStore((state) => state.lanTokens);
  const audit = useRuntimeHubStore((state) => state.lanAudit);
  const revokeToken = useRuntimeHubStore((state) => state.revokeToken);
  const busy = useRuntimeHubStore((state) => state.busy);
  const errors = useRuntimeHubStore((state) => state.errors);
  const [confirming, setConfirming] = useState<string | null>(null);

  return (
    <>
      <section className="rounded-lg border border-border bg-background p-4" aria-labelledby="lan-tokens-heading">
        <SectionHeading title="Paired tokens" description="Revoked tokens remain visible for audit, but cannot authorize requests." />
        <div className="mt-3 flex flex-col gap-2">
          {tokens.length ? tokens.map((token) => {
            const key = `lan-revoke:${token.tokenId}`;
            return (
              <div key={token.tokenId} className="rounded-md border border-border bg-surface-2 p-3">
                <div className="flex flex-wrap items-start justify-between gap-3">
                  <div className="min-w-0">
                    <div className="flex flex-wrap items-center gap-2">
                      <p className="text-sm font-medium text-foreground">{token.clientLabel}</p>
                      <StatusPill tone={token.revokedAtMs ? "danger" : "success"}>{token.revokedAtMs ? "Revoked" : "Active"}</StatusPill>
                    </div>
                    <p className="mt-1 break-all font-mono text-xs text-muted">{token.tokenId}</p>
                    <p className="mt-1 text-xs text-muted">{token.scopes.map(labelize).join(", ")} · {token.backends.map(labelize).join(", ")}</p>
                    <p className="mt-1 text-xs text-muted">Created {formatDate(token.createdAtMs)} · last used {formatDate(token.lastUsedAtMs)} · expires {formatDate(token.expiresAtMs)}</p>
                  </div>
                  {!token.revokedAtMs && (
                    confirming === token.tokenId ? (
                      <div className="flex flex-wrap gap-2">
                        <Button type="button" className="min-h-11" onClick={() => setConfirming(null)}>Keep</Button>
                        <BusyButton type="button" variant="danger" busy={busy[key]} onClick={() => void revokeToken(token.tokenId).then(() => setConfirming(null)).catch(() => {})}>
                          <Trash2 size={14} aria-hidden="true" /> Confirm revoke
                        </BusyButton>
                      </div>
                    ) : (
                      <Button type="button" variant="danger" className="min-h-11" onClick={() => setConfirming(token.tokenId)}>Revoke</Button>
                    )
                  )}
                </div>
                <ErrorNotice message={errors[key]} />
              </div>
            );
          }) : <p className="rounded-md border border-dashed border-border p-4 text-center text-sm text-muted">No paired tokens.</p>}
        </div>
      </section>

      <section className="rounded-lg border border-border bg-background p-4" aria-labelledby="lan-audit-heading">
        <SectionHeading title="Security audit" description="Pairing, authorization, rate-limit, denial, and revocation decisions are recorded without plaintext tokens." />
        <div className="mt-3 max-h-80 overflow-auto rounded-md border border-border [overscroll-behavior:contain]">
          <table className="w-full min-w-[640px] text-left text-xs">
            <thead className="sticky top-0 bg-surface-2 text-muted">
              <tr><th className="px-3 py-2 font-medium">Time</th><th className="px-3 py-2 font-medium">Event</th><th className="px-3 py-2 font-medium">Outcome</th><th className="px-3 py-2 font-medium">Remote</th><th className="px-3 py-2 font-medium">Detail</th></tr>
            </thead>
            <tbody className="divide-y divide-border">
              {audit.map((event) => (
                <tr key={event.eventId} className="text-foreground">
                  <td className="whitespace-nowrap px-3 py-2 text-muted">{formatDate(event.occurredAtMs)}</td>
                  <td className="px-3 py-2">{labelize(event.kind)}</td>
                  <td className="px-3 py-2">{event.outcome}</td>
                  <td className="px-3 py-2 font-mono text-muted">{event.remoteAddress ?? "—"}</td>
                  <td className="max-w-xs break-words px-3 py-2 text-muted">{event.detail}</td>
                </tr>
              ))}
              {!audit.length && <tr><td colSpan={5} className="px-3 py-6 text-center text-muted">No security events yet.</td></tr>}
            </tbody>
          </table>
        </div>
      </section>
    </>
  );
}

export function RuntimeHubLan() {
  const refreshError = useRuntimeHubStore((state) => state.errors["lan-refresh"]);
  return (
    <div role="tabpanel" id="runtime-hub-panel-lan" aria-labelledby="runtime-hub-tab-lan" className="flex flex-col gap-5">
      <SectionHeading
        title="Secure local network access"
        description="Expose local inference only through an exact interface policy, scoped pairing tokens, rate limits, and an auditable authorization boundary."
      />
      <ErrorNotice message={refreshError} />
      <PolicyEditor />
      <PairingPanel />
      <TokenAndAuditPanel />
    </div>
  );
}
