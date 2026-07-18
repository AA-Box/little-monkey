import { useEffect, useState, type FormEvent } from "react";
import { Copy, Download, Search, Sparkles } from "lucide-react";
import { Button, StatusPill, type PillTone } from "../../ui";
import type { AgentConfigWarning, AgentTool, AgentWarningKind } from "../../../lib/runtimeHubClient";
import { useRuntimeHubStore } from "../../../store/runtimeHubStore";
import {
  BusyButton,
  CONTROL_CLASS,
  ErrorNotice,
  Field,
  labelize,
  SectionHeading,
  SuccessNotice,
} from "./RuntimeHubShared";

export const AGENT_TOOLS: Array<{ value: AgentTool; label: string; filename: string }> = [
  { value: "continue_dev", label: "Continue.dev", filename: ".continue/config.yaml" },
  { value: "aider", label: "aider", filename: ".aider.conf.yml" },
  { value: "openai_env", label: "Generic OpenAI-compatible CLI (.env)", filename: ".env" },
];

/**
 * Findings that could silently break the connection (missing/placeholder
 * auth, a model or endpoint that no longer exists) are surfaced as
 * "danger"; findings that are informational or preference-level (context
 * length, telemetry defaults) are surfaced as "warning".
 */
export function warningTone(kind: AgentWarningKind): PillTone {
  switch (kind) {
    case "auth":
    case "auth_drift":
    case "model_missing":
    case "endpoint_drift":
      return "danger";
    case "context_length":
    case "telemetry":
      return "warning";
    default:
      return "warning";
  }
}

function WarningList({ warnings }: { warnings: AgentConfigWarning[] }) {
  if (!warnings.length) return null;
  return (
    <ul className="mt-3 flex flex-col gap-2">
      {warnings.map((warning, index) => (
        <li
          key={`${warning.kind}-${index}`}
          className="flex flex-wrap items-start gap-2 rounded-md border border-border bg-background p-2.5 text-xs leading-5"
        >
          <StatusPill tone={warningTone(warning.kind)}>{labelize(warning.kind)}</StatusPill>
          <span className="min-w-0 flex-1 text-muted">{warning.message}</span>
        </li>
      ))}
    </ul>
  );
}

function GeneratePanel() {
  const installedModels = useRuntimeHubStore((state) => state.installedModels);
  const pairedToken = useRuntimeHubStore((state) => state.pairedToken);
  const generated = useRuntimeHubStore((state) => state.agentGeneratedConfig);
  const generate = useRuntimeHubStore((state) => state.generateAgentConfig);
  const clearConfig = useRuntimeHubStore((state) => state.clearAgentConfig);
  const busy = useRuntimeHubStore((state) => state.busy["agent-launcher-generate"]);
  const error = useRuntimeHubStore((state) => state.errors["agent-launcher-generate"]);

  const [tool, setTool] = useState<AgentTool>("continue_dev");
  const [modelId, setModelId] = useState(installedModels[0]?.modelId ?? "");
  const [authToken, setAuthToken] = useState("");
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    if (!modelId && installedModels[0]) setModelId(installedModels[0].modelId);
  }, [installedModels, modelId]);

  function changeTool(next: AgentTool) {
    setTool(next);
    clearConfig();
  }

  function submit(event: FormEvent) {
    event.preventDefault();
    if (!modelId) return;
    const trimmed = authToken.trim();
    void generate(tool, modelId, trimmed ? trimmed : null).catch(() => {});
  }

  async function copyContent() {
    if (!generated) return;
    try {
      await navigator.clipboard.writeText(generated.content);
      setCopied(true);
      globalThis.setTimeout(() => setCopied(false), 1500);
    } catch {
      setCopied(false);
    }
  }

  function downloadContent() {
    if (!generated) return;
    const blob = new Blob([generated.content], { type: "text/plain" });
    const url = URL.createObjectURL(blob);
    const link = document.createElement("a");
    link.href = url;
    link.download = generated.filename.split("/").pop() ?? generated.filename;
    link.click();
    URL.revokeObjectURL(url);
  }

  return (
    <section className="rounded-lg border border-border bg-background p-4" aria-labelledby="agent-launcher-generate-heading">
      <SectionHeading
        title="Generate a config"
        description="Points a real external tool at this app's own local endpoint: a currently installed model, the server's real bind address/port, and — if paired — a real bearer token. Never a placeholder host or fake key."
      />
      <form onSubmit={submit} className="mt-4 flex flex-col gap-4">
        <div className="grid gap-4 sm:grid-cols-2">
          <Field label="Target tool">
            <select value={tool} onChange={(event) => changeTool(event.target.value as AgentTool)} className={CONTROL_CLASS}>
              {AGENT_TOOLS.map((entry) => (
                <option key={entry.value} value={entry.value}>{entry.label}</option>
              ))}
            </select>
          </Field>
          <Field label="Model" hint="Must be a currently installed model.">
            <select
              value={modelId}
              onChange={(event) => setModelId(event.target.value)}
              className={CONTROL_CLASS}
              disabled={!installedModels.length}
            >
              {!installedModels.length && <option value="">No installed models</option>}
              {installedModels.map((model) => (
                <option key={model.assetId} value={model.modelId}>{model.displayName}</option>
              ))}
            </select>
          </Field>
          <Field label="Bearer token" hint="Paste a token paired in the LAN tab. Leave blank if the server does not require authentication.">
            <input
              type="password"
              value={authToken}
              onChange={(event) => setAuthToken(event.target.value)}
              className={`${CONTROL_CLASS} font-mono`}
              autoComplete="off"
            />
          </Field>
          {pairedToken && (
            <div className="flex items-end">
              <Button type="button" className="min-h-11" onClick={() => setAuthToken(pairedToken.token)}>
                Use just-paired token
              </Button>
            </div>
          )}
        </div>
        <ErrorNotice message={error} />
        <div className="flex justify-end">
          <BusyButton type="submit" variant="primary" busy={busy} disabled={!modelId}>
            <Sparkles size={15} aria-hidden="true" /> Generate config
          </BusyButton>
        </div>
      </form>

      {generated && (
        <div className="mt-4 rounded-md border border-border bg-surface-2 p-3">
          <div className="flex flex-wrap items-center justify-between gap-2">
            <code className="font-mono text-sm text-foreground">{generated.filename}</code>
            <div className="flex gap-2">
              <Button type="button" className="min-h-11" onClick={() => void copyContent()}>
                <Copy size={14} aria-hidden="true" /> {copied ? "Copied" : "Copy"}
              </Button>
              <Button type="button" className="min-h-11" onClick={downloadContent}>
                <Download size={14} aria-hidden="true" /> Download
              </Button>
            </div>
          </div>
          <pre className="mt-3 max-h-72 overflow-auto whitespace-pre-wrap break-all rounded-md border border-border bg-background p-3 font-mono text-xs leading-5 text-foreground [overscroll-behavior:contain]">
            {generated.content}
          </pre>
          <WarningList warnings={generated.warnings} />
        </div>
      )}
    </section>
  );
}

function DriftPanel() {
  const checkDrift = useRuntimeHubStore((state) => state.checkAgentConfigDrift);
  const report = useRuntimeHubStore((state) => state.agentDriftReport);
  const clearReport = useRuntimeHubStore((state) => state.clearAgentDriftReport);
  const busy = useRuntimeHubStore((state) => state.busy["agent-launcher-drift"]);
  const error = useRuntimeHubStore((state) => state.errors["agent-launcher-drift"]);

  const [tool, setTool] = useState<AgentTool>("continue_dev");
  const [pasted, setPasted] = useState("");

  function changeTool(next: AgentTool) {
    setTool(next);
    clearReport();
  }

  function submit(event: FormEvent) {
    event.preventDefault();
    if (!pasted.trim()) return;
    void checkDrift(tool, pasted).catch(() => {});
  }

  return (
    <section className="rounded-lg border border-border bg-background p-4" aria-labelledby="agent-launcher-drift-heading">
      <SectionHeading
        title="Check an existing config for drift"
        description="Paste a previously-generated (or hand-edited) config to see if it references a model that's no longer installed, an endpoint that has moved, a missing auth header, an oversized context length, or a telemetry default worth revisiting."
      />
      <form onSubmit={submit} className="mt-4 flex flex-col gap-4">
        <Field label="Config format">
          <select value={tool} onChange={(event) => changeTool(event.target.value as AgentTool)} className={CONTROL_CLASS}>
            {AGENT_TOOLS.map((entry) => (
              <option key={entry.value} value={entry.value}>{entry.label}</option>
            ))}
          </select>
        </Field>
        <Field label="Pasted config">
          <textarea
            value={pasted}
            onChange={(event) => setPasted(event.target.value)}
            rows={10}
            spellCheck={false}
            className={`${CONTROL_CLASS} resize-y font-mono text-xs`}
            placeholder="Paste the file contents here"
          />
        </Field>
        <ErrorNotice message={error} />
        <div className="flex justify-end">
          <BusyButton type="submit" variant="primary" busy={busy} disabled={!pasted.trim()}>
            <Search size={15} aria-hidden="true" /> Check for drift
          </BusyButton>
        </div>
      </form>

      {report && (
        <div className="mt-4 rounded-md border border-border bg-surface-2 p-3">
          <dl className="grid gap-2 text-xs sm:grid-cols-3">
            <div>
              <dt className="font-medium text-foreground">Model</dt>
              <dd className="mt-1 break-all text-muted">{report.parsedModelId ?? "—"}</dd>
            </div>
            <div>
              <dt className="font-medium text-foreground">Endpoint</dt>
              <dd className="mt-1 break-all text-muted">{report.parsedBaseUrl ?? "—"}</dd>
            </div>
            <div>
              <dt className="font-medium text-foreground">Declared context length</dt>
              <dd className="mt-1 text-muted">{report.parsedContextTokens ?? "—"}</dd>
            </div>
          </dl>
          {report.findings.length ? (
            <WarningList warnings={report.findings} />
          ) : (
            <div className="mt-3">
              <SuccessNotice>No drift detected — this config matches the current server state.</SuccessNotice>
            </div>
          )}
        </div>
      )}
    </section>
  );
}

export function RuntimeHubAgents() {
  return (
    <div role="tabpanel" id="runtime-hub-panel-agents" aria-labelledby="runtime-hub-tab-agents" className="flex flex-col gap-5">
      <SectionHeading
        title="Local Agent Integration Launcher"
        description="Connect external agent tools and editors — Continue.dev, aider, or any generic OpenAI-SDK-compatible CLI — to this app's own local API endpoint, without hand-editing their config files."
      />
      <GeneratePanel />
      <DriftPanel />
    </div>
  );
}

export default RuntimeHubAgents;
