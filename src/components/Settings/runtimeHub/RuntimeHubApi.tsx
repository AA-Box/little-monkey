import { useEffect, useMemo, useState, type FormEvent } from "react";
import { Braces, Copy, Play, Square } from "lucide-react";
import { Button, StatusPill } from "../../ui";
import type {
  CompatibilityProtocol,
  M3DiagnosticDispatchRequest,
} from "../../../lib/runtimeHubClient";
import { createM3OperationId } from "../../../lib/runtimeHubClient";
import { useRuntimeHubStore } from "../../../store/runtimeHubStore";
import {
  BusyButton,
  CONTROL_CLASS,
  ErrorNotice,
  Field,
  JsonView,
  SectionHeading,
  SuccessNotice,
} from "./RuntimeHubShared";
import { errorMessage } from "../../../lib/errors";

const PROTOCOLS: Array<{ value: CompatibilityProtocol; label: string; endpoint: string }> = [
  { value: "open_ai_chat_completions", label: "OpenAI Chat Completions", endpoint: "POST /v1/chat/completions" },
  { value: "open_ai_responses", label: "OpenAI Responses", endpoint: "POST /v1/responses" },
  { value: "anthropic_messages", label: "Anthropic Messages", endpoint: "POST /v1/messages" },
];

function defaultBody(protocol: CompatibilityProtocol, model: string): string {
  const body = protocol === "open_ai_chat_completions"
    ? {
        model,
        messages: [{ role: "user", content: "Reply with a short runtime health check." }],
        max_tokens: 128,
        temperature: 0.2,
        stream: false,
      }
    : protocol === "open_ai_responses"
      ? {
          model,
          input: "Reply with a short runtime health check.",
          max_output_tokens: 128,
          temperature: 0.2,
          stream: false,
        }
      : {
          model,
          max_tokens: 128,
          messages: [{ role: "user", content: "Reply with a short runtime health check." }],
          temperature: 0.2,
          stream: false,
        };
  return JSON.stringify(body, null, 2);
}

export function RuntimeHubApi() {
  const runtimes = useRuntimeHubStore((state) => state.runtimes);
  const installedModels = useRuntimeHubStore((state) => state.installedModels);
  const result = useRuntimeHubStore((state) => state.apiResult);
  const dispatchApi = useRuntimeHubStore((state) => state.dispatchApi);
  const cancelInference = useRuntimeHubStore((state) => state.cancelInference);
  const busy = useRuntimeHubStore((state) => state.busy);
  const backendError = useRuntimeHubStore((state) => state.errors.api);
  const cancelError = useRuntimeHubStore((state) => state.errors["api-cancel"]);

  const inferRuntimes = runtimes.filter((runtime) => runtime.canInfer);
  const initialRuntime = inferRuntimes[0]?.descriptor.runtimeId ?? "";
  const initialModel = installedModels[0]?.modelId ?? "";
  const [protocol, setProtocol] = useState<CompatibilityProtocol>("open_ai_chat_completions");
  const [runtimeId, setRuntimeId] = useState(initialRuntime);
  const [modelId, setModelId] = useState(initialModel);
  const [requestId, setRequestId] = useState(() => createM3OperationId("diagnostic"));
  const [body, setBody] = useState(() => defaultBody("open_ai_chat_completions", initialModel));
  const [localError, setLocalError] = useState<string | null>(null);
  const [cancelled, setCancelled] = useState<boolean | null>(null);
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    if (!runtimeId && initialRuntime) setRuntimeId(initialRuntime);
  }, [initialRuntime, runtimeId]);
  useEffect(() => {
    if (!modelId && initialModel) {
      setModelId(initialModel);
      setBody(defaultBody(protocol, initialModel));
    }
  }, [initialModel, modelId, protocol]);

  const endpoint = useMemo(() => PROTOCOLS.find((entry) => entry.value === protocol)?.endpoint ?? "", [protocol]);

  function changeProtocol(next: CompatibilityProtocol) {
    setProtocol(next);
    setBody(defaultBody(next, modelId));
    setLocalError(null);
  }

  function changeModel(next: string) {
    setModelId(next);
    try {
      const parsed = JSON.parse(body) as Record<string, unknown>;
      parsed.model = next;
      setBody(JSON.stringify(parsed, null, 2));
    } catch {
      setBody(defaultBody(protocol, next));
    }
  }

  function request(): M3DiagnosticDispatchRequest | null {
    setLocalError(null);
    if (!runtimeId || !modelId || !requestId.trim()) {
      setLocalError("Runtime, model, and request id are required.");
      return null;
    }
    try {
      const parsed = JSON.parse(body) as Record<string, unknown>;
      if (parsed.stream === true) {
        setLocalError("The desktop diagnostics command is non-streaming. Set stream to false; SSE is served by the HTTP compatibility endpoint.");
        return null;
      }
    } catch (error) {
      setLocalError(`Request body is not valid JSON: ${errorMessage(error)}`);
      return null;
    }
    return {
      protocol,
      runtimeId,
      requestId: requestId.trim(),
      body: Array.from(new TextEncoder().encode(body)),
    };
  }

  function submit(event: FormEvent) {
    event.preventDefault();
    const next = request();
    if (!next) return;
    setCancelled(null);
    void dispatchApi(next).catch(() => {});
  }

  function handleCancel() {
    const next = request();
    if (!next) return;
    void cancelInference({
      protocol,
      runtimeId,
      requestId: next.requestId,
      modelId,
    }).then(setCancelled).catch(() => {});
  }

  async function copyResult() {
    if (!result) return;
    try {
      await navigator.clipboard.writeText(JSON.stringify(result, null, 2));
      setCopied(true);
      globalThis.setTimeout(() => setCopied(false), 1500);
    } catch (error) {
      setLocalError(`Could not copy the response: ${errorMessage(error)}`);
    }
  }

  return (
    <div role="tabpanel" id="runtime-hub-panel-api" aria-labelledby="runtime-hub-tab-api" className="flex flex-col gap-5">
      <SectionHeading
        title="Compatibility diagnostics"
        description="Send a desktop-only, non-streaming request through the same strict OpenAI or Anthropic translation layer used by the HTTP server. The native backend supplies caller identity and time; test paired LAN credentials through the HTTP endpoint."
      />

      <form onSubmit={submit} className="flex flex-col gap-4 rounded-lg border border-border bg-background p-4">
        <div className="grid gap-4 sm:grid-cols-2">
          <Field label="Protocol" hint={endpoint}>
            <select value={protocol} onChange={(event) => changeProtocol(event.target.value as CompatibilityProtocol)} className={CONTROL_CLASS}>
              {PROTOCOLS.map((entry) => <option key={entry.value} value={entry.value}>{entry.label}</option>)}
            </select>
          </Field>
          <Field label="Runtime">
            <select value={runtimeId} onChange={(event) => setRuntimeId(event.target.value)} className={CONTROL_CLASS} disabled={!inferRuntimes.length}>
              {!inferRuntimes.length && <option value="">No inference runtime available</option>}
              {inferRuntimes.map((runtime) => <option key={runtime.descriptor.runtimeId} value={runtime.descriptor.runtimeId}>{runtime.descriptor.label}</option>)}
            </select>
          </Field>
          <Field label="Model id" hint="The exact model field sent in the compatibility body.">
            <input list="runtime-hub-api-models" value={modelId} onChange={(event) => changeModel(event.target.value)} className={CONTROL_CLASS} />
            <datalist id="runtime-hub-api-models">
              {installedModels.map((model) => <option key={model.assetId} value={model.modelId}>{model.displayName}</option>)}
            </datalist>
          </Field>
          <Field label="Request id" hint="Use this same id to cancel an in-flight generation.">
            <input value={requestId} onChange={(event) => setRequestId(event.target.value)} className={`${CONTROL_CLASS} font-mono`} />
          </Field>
        </div>

        <Field label="JSON request body" hint="Streaming is intentionally exercised through the HTTP SSE route, not this IPC diagnostic.">
          <textarea value={body} onChange={(event) => setBody(event.target.value)} rows={14} spellCheck={false} className={`${CONTROL_CLASS} resize-y font-mono leading-5`} />
        </Field>

        <ErrorNotice message={localError ?? backendError ?? cancelError} />
        {cancelled !== null && <SuccessNotice>{cancelled ? "Cancellation was accepted by the runtime." : "No matching in-flight request was found."}</SuccessNotice>}

        <div className="flex flex-wrap justify-end gap-2">
          <BusyButton type="button" variant="danger" busy={busy["api-cancel"]} disabled={!requestId || !runtimeId || !modelId} onClick={handleCancel}>
            <Square size={14} aria-hidden="true" /> Cancel request
          </BusyButton>
          <BusyButton type="submit" variant="primary" busy={busy.api} disabled={!runtimeId || !modelId}>
            <Play size={15} aria-hidden="true" /> Dispatch request
          </BusyButton>
        </div>
      </form>

      {result && (
        <section className="rounded-lg border border-border bg-background p-4" aria-labelledby="runtime-api-response-heading">
          <div className="mb-3 flex flex-wrap items-center justify-between gap-2">
            <div className="flex items-center gap-2">
              <Braces size={16} className="text-muted" aria-hidden="true" />
              <h3 id="runtime-api-response-heading" className="text-sm font-semibold text-foreground">Protocol response</h3>
              <StatusPill tone={result.status >= 200 && result.status < 300 ? "success" : "danger"}>HTTP {result.status}</StatusPill>
            </div>
            <Button type="button" className="min-h-11" onClick={() => void copyResult()}>
              <Copy size={14} aria-hidden="true" /> {copied ? "Copied" : "Copy JSON"}
            </Button>
          </div>
          <JsonView value={result.body} label="Translated response body" />
        </section>
      )}
    </div>
  );
}
