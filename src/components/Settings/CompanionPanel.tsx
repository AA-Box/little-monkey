import { useCallback, useEffect, useMemo, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { Camera, Image, Mic, Octagon, Play, Plus, Save, Trash2 } from "lucide-react";

import {
  companionClient,
  formatSpeakerTranscript,
  type CaptureGrant,
  type CompanionConfig,
  type ImageEndpointConfig,
  type ImageEndpointKind,
  type ImageGalleryEntry,
} from "../../lib/companionClient";
import { Button } from "../ui";
import { errorMessage } from "../../lib/errors";

const INPUT = "w-full rounded-md border border-border bg-background px-2.5 py-2 text-sm text-foreground outline-none focus:ring-1 focus:ring-accent";

function errorText(error: unknown): string {
  return errorMessage(error);
}

function safeNumber(value: string, fallback: number): number {
  const parsed = Number(value);
  return Number.isFinite(parsed) ? parsed : fallback;
}

export function CompanionPanel() {
  const [config, setConfig] = useState<CompanionConfig | null>(null);
  const [grants, setGrants] = useState<CaptureGrant[]>([]);
  const [gallery, setGallery] = useState<ImageGalleryEntry[]>([]);
  const [previews, setPreviews] = useState<Record<string, string>>({});
  const [status, setStatus] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [transcript, setTranscript] = useState("");
  const [meetingMode, setMeetingMode] = useState(false);
  const [endpointKind, setEndpointKind] = useState<ImageEndpointKind>("comfy_ui");
  const [endpointId, setEndpointId] = useState("");
  const [endpointLabel, setEndpointLabel] = useState("");
  const [endpointUrl, setEndpointUrl] = useState("http://127.0.0.1:8188");
  const [endpointProvider, setEndpointProvider] = useState("");
  const [workflowJson, setWorkflowJson] = useState('{\n  "3": { "inputs": { "text": "{{prompt}}" } }\n}');
  const [prompt, setPrompt] = useState("");
  const [negativePrompt, setNegativePrompt] = useState("");
  const [model, setModel] = useState("");
  const [width, setWidth] = useState("1024");
  const [height, setHeight] = useState("1024");
  const [steps, setSteps] = useState("25");
  const [cfgScale, setCfgScale] = useState("7");
  const [seed, setSeed] = useState("0");
  const [progress, setProgress] = useState(0);
  const [sourceArtifactId, setSourceArtifactId] = useState<string | null>(null);
  const [selectedEndpointId, setSelectedEndpointId] = useState("");

  const load = useCallback(async () => {
    const [nextConfig, nextGrants, nextGallery] = await Promise.all([
      companionClient.config(),
      companionClient.grants(),
      companionClient.gallery(),
    ]);
    setConfig(nextConfig);
    setGrants(nextGrants);
    setGallery(nextGallery);
  }, []);

  useEffect(() => {
    void load().catch((reason) => setError(errorText(reason)));
    let disposed = false;
    let unlisten: (() => void) | null = null;
    void companionClient.onImageProgress((event) => {
      if (!disposed) setProgress(Math.max(0, Math.min(1, event.progress)));
    }).then((cleanup) => {
      if (disposed) cleanup();
      else unlisten = cleanup;
    });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [load]);

  useEffect(() => {
    const missing = gallery
      .slice(-12)
      .filter((entry) => !previews[entry.artifactId]);
    for (const entry of missing) {
      void companionClient.imageDataUrl(entry.artifactId, entry.mediaType)
        .then((url) => setPreviews((current) => ({ ...current, [entry.artifactId]: url })))
        .catch(() => undefined);
    }
  }, [gallery, previews]);

  const enabledEndpoints = useMemo(
    () => config?.imageEndpoints.filter((endpoint) => endpoint.enabled) ?? [],
    [config],
  );
  const selectedEndpoint = useMemo(
    () => enabledEndpoints.find((endpoint) => endpoint.endpointId === selectedEndpointId) ?? enabledEndpoints[0] ?? null,
    [enabledEndpoints, selectedEndpointId],
  );

  const saveConfig = useCallback(async (next: CompanionConfig, message = "Companion settings saved.") => {
    setBusy("save");
    setError(null);
    try {
      const saved = await companionClient.saveConfig(next);
      setConfig(saved);
      setStatus(message);
    } catch (reason) {
      setError(errorText(reason));
    } finally {
      setBusy(null);
    }
  }, []);

  const chooseWhisperPath = useCallback(async (kind: "binary" | "model") => {
    const selected = await open({ multiple: false, directory: false });
    if (!selected || Array.isArray(selected) || !config) return;
    setConfig({
      ...config,
      voice: {
        ...config.voice,
        whisperBinary: kind === "binary" ? selected : config.voice.whisperBinary,
        whisperModel: kind === "model" ? selected : config.voice.whisperModel,
      },
    });
  }, [config]);

  const transcribeFile = useCallback(async () => {
    const selected = await open({
      multiple: false,
      directory: false,
      filters: [{ name: "Audio", extensions: ["wav", "mp3", "m4a", "webm", "ogg", "flac"] }],
    });
    if (!selected || Array.isArray(selected)) return;
    const jobId = `transcribe-${crypto.randomUUID()}`;
    setBusy(jobId);
    setError(null);
    try {
      const grant = await companionClient.grant(meetingMode ? "meeting" : "file", 15 * 60_000, "settings-transcription");
      setGrants((current) => [...current, grant]);
      const result = await companionClient.transcribeFile(grant.grantId, jobId, selected);
      setTranscript(meetingMode ? formatSpeakerTranscript(result) : result.text);
      setStatus(
        meetingMode
          ? `Meeting transcribed with ${result.backend}; ${result.segments.length > 0 ? `${result.segments.length} timed speaker segments returned` : "the selected backend returned no speaker labels"}.`
          : `Transcribed with ${result.backend}.`,
      );
    } catch (reason) {
      setError(errorText(reason));
    } finally {
      setBusy(null);
    }
  }, [meetingMode]);

  const addEndpoint = useCallback(async () => {
    if (!config) return;
    setError(null);
    let workflowTemplate: unknown | null = null;
    if (endpointKind === "comfy_ui") {
      try {
        workflowTemplate = JSON.parse(workflowJson) as unknown;
      } catch (reason) {
        setError(`Workflow JSON: ${errorText(reason)}`);
        return;
      }
    }
    const endpoint: ImageEndpointConfig = {
      endpointId: endpointId.trim(),
      label: endpointLabel.trim(),
      kind: endpointKind,
      baseUrl: endpointUrl.trim(),
      providerId: endpointKind === "open_ai_compatible" ? endpointProvider.trim() || null : null,
      workflowTemplate,
      supportsEditing: endpointKind === "open_ai_compatible",
      enabled: true,
    };
    await saveConfig(
      { ...config, imageEndpoints: [...config.imageEndpoints.filter((item) => item.endpointId !== endpoint.endpointId), endpoint] },
      "Image endpoint activated.",
    );
    setEndpointId("");
    setEndpointLabel("");
  }, [config, endpointId, endpointKind, endpointLabel, endpointProvider, endpointUrl, saveConfig, workflowJson]);

  const removeEndpoint = useCallback(async (id: string) => {
    if (!config) return;
    await saveConfig({ ...config, imageEndpoints: config.imageEndpoints.filter((endpoint) => endpoint.endpointId !== id) }, "Image endpoint removed.");
  }, [config, saveConfig]);

  const generate = useCallback(async () => {
    const endpoint = selectedEndpoint;
    if (!endpoint || !prompt.trim() || !model.trim()) return;
    const jobId = `image-${crypto.randomUUID()}`;
    setBusy(jobId);
    setProgress(0);
    setError(null);
    try {
      const entry = await companionClient.generateImage({
        jobId,
        endpointId: endpoint.endpointId,
        prompt: prompt.trim(),
        negativePrompt: negativePrompt.trim(),
        model: model.trim(),
        width: safeNumber(width, 1024),
        height: safeNumber(height, 1024),
        steps: safeNumber(steps, 25),
        cfgScale: safeNumber(cfgScale, 7),
        seed: safeNumber(seed, 0),
        sourceArtifactId,
      });
      setGallery((current) => [...current, entry]);
      setStatus("Image generated and stored in the durable gallery.");
    } catch (reason) {
      setError(errorText(reason));
    } finally {
      setBusy(null);
    }
  }, [cfgScale, height, model, negativePrompt, prompt, seed, selectedEndpoint, sourceArtifactId, steps, width]);

  if (!config) return <p className="text-sm text-muted">Loading desktop companion…</p>;

  return (
    <div className="flex flex-col gap-6">
      <section className="rounded-lg border border-border bg-surface p-4">
        <div className="flex items-start justify-between gap-3">
          <div>
            <h3 className="text-sm font-semibold">Desktop companion</h3>
            <p className="mt-1 text-xs leading-relaxed text-muted">
              Open with {config.overlayShortcut}. Screen and microphone access always require a visible, expiring grant.
            </p>
          </div>
          <div className="flex gap-2">
            <Button size="sm" onClick={() => void companionClient.showOverlay()}><Camera size={14} />Open overlay</Button>
            <Button
              size="sm"
              variant="danger"
              onClick={() => void companionClient.emergencyStop().then(load).catch((reason) => setError(errorText(reason)))}
            ><Octagon size={14} />Emergency stop</Button>
          </div>
        </div>
        <div className="mt-3 flex flex-wrap gap-2">
          {grants.filter((grant) => grant.active).length === 0 && <span className="text-xs text-faint">No active capture grants.</span>}
          {grants.filter((grant) => grant.active).map((grant) => (
            <button
              key={grant.grantId}
              type="button"
              onClick={() => void companionClient.revoke(grant.grantId).then(load)}
              className="rounded-full border border-border bg-background px-2 py-1 text-xs text-muted hover:text-danger"
            >
              {grant.kind} · revoke
            </button>
          ))}
        </div>
        <div className="mt-3 flex flex-col gap-2 border-t border-border pt-3 sm:flex-row sm:items-end">
          <label className="min-w-0 flex-1 text-xs text-muted">Global overlay shortcut
            <input
              className={`${INPUT} mt-1`}
              value={config.overlayShortcut}
              onChange={(event) => setConfig({ ...config, overlayShortcut: event.target.value })}
              placeholder="CommandOrControl+Shift+Space"
            />
          </label>
          <Button
            size="sm"
            variant="primary"
            disabled={busy !== null || !config.overlayShortcut.trim()}
            onClick={() => void saveConfig(config, "Companion shortcut updated and active.")}
          ><Save size={14} />Save shortcut</Button>
        </div>
      </section>

      <section className="rounded-lg border border-border bg-surface p-4">
        <div className="flex items-center gap-2"><Mic size={16} /><h3 className="text-sm font-semibold">Voice and transcription</h3></div>
        <div className="mt-3 grid gap-3 md:grid-cols-2">
          <label className="text-xs text-muted">Backend
            <select className={`${INPUT} mt-1`} value={config.voice.backend} onChange={(event) => setConfig({ ...config, voice: { ...config.voice, backend: event.target.value as "local_whisper" | "provider" } })}>
              <option value="local_whisper">Local whisper.cpp</option>
              <option value="provider">BYOK provider</option>
            </select>
          </label>
          <label className="text-xs text-muted">Language
            <input className={`${INPUT} mt-1`} value={config.voice.language} onChange={(event) => setConfig({ ...config, voice: { ...config.voice, language: event.target.value } })} />
          </label>
          {config.voice.backend === "local_whisper" ? <>
            <label className="text-xs text-muted">whisper.cpp binary
              <div className="mt-1 flex gap-2"><input className={INPUT} readOnly value={config.voice.whisperBinary ?? ""} /><Button size="sm" onClick={() => void chooseWhisperPath("binary")}>Choose</Button></div>
            </label>
            <label className="text-xs text-muted">Whisper model
              <div className="mt-1 flex gap-2"><input className={INPUT} readOnly value={config.voice.whisperModel ?? ""} /><Button size="sm" onClick={() => void chooseWhisperPath("model")}>Choose</Button></div>
            </label>
          </> : <>
            <label className="text-xs text-muted">Provider id
              <input className={`${INPUT} mt-1`} value={config.voice.providerId ?? ""} onChange={(event) => setConfig({ ...config, voice: { ...config.voice, providerId: event.target.value || null } })} placeholder="openai" />
            </label>
            <label className="text-xs text-muted">Transcription model
              <input className={`${INPUT} mt-1`} value={config.voice.providerModel} onChange={(event) => setConfig({ ...config, voice: { ...config.voice, providerModel: event.target.value } })} />
            </label>
          </>}
          <label className="text-xs text-muted">System TTS voice (optional)
            <input className={`${INPUT} mt-1`} value={config.voice.ttsVoice ?? ""} onChange={(event) => setConfig({ ...config, voice: { ...config.voice, ttsVoice: event.target.value || null } })} />
          </label>
          <label className="flex items-end gap-2 pb-2 text-xs text-muted">
            <input type="checkbox" checked={config.voice.saveRawAudio} onChange={(event) => setConfig({ ...config, voice: { ...config.voice, saveRawAudio: event.target.checked } })} />Persist raw audio artifacts
          </label>
        </div>
        <div className="mt-3 flex gap-2">
          <Button size="sm" variant="primary" disabled={busy !== null} onClick={() => void saveConfig(config)}><Save size={14} />Save voice settings</Button>
          <Button size="sm" disabled={busy !== null} onClick={() => void transcribeFile()}><Play size={14} />Transcribe a file</Button>
          {busy?.startsWith("transcribe-") && <Button size="sm" variant="danger" onClick={() => void companionClient.cancelJob(busy)}>Cancel</Button>}
        </div>
        <label className="mt-3 flex items-center gap-2 text-xs text-muted">
          <input type="checkbox" checked={meetingMode} onChange={(event) => setMeetingMode(event.target.checked)} />
          Request speaker-separated meeting output when the selected backend supports it
        </label>
        {transcript && <textarea className={`${INPUT} mt-3 min-h-24`} value={transcript} onChange={(event) => setTranscript(event.target.value)} aria-label="Transcript result" />}
      </section>

      <section className="rounded-lg border border-border bg-surface p-4">
        <div className="flex items-center gap-2"><Image size={16} /><h3 className="text-sm font-semibold">User-owned image endpoints</h3></div>
        <p className="mt-1 text-xs text-muted">ComfyUI stays local; OpenAI-compatible endpoints reuse provider keys from the OS keychain.</p>
        <div className="mt-3 grid gap-3 md:grid-cols-2">
          <select className={INPUT} value={endpointKind} onChange={(event) => {
            const kind = event.target.value as ImageEndpointKind;
            setEndpointKind(kind);
            setEndpointUrl(kind === "comfy_ui" ? "http://127.0.0.1:8188" : "https://api.openai.com/v1");
          }}><option value="comfy_ui">ComfyUI</option><option value="open_ai_compatible">OpenAI-compatible</option></select>
          <input className={INPUT} value={endpointId} onChange={(event) => setEndpointId(event.target.value)} placeholder="Endpoint id" />
          <input className={INPUT} value={endpointLabel} onChange={(event) => setEndpointLabel(event.target.value)} placeholder="Display label" />
          <input className={INPUT} value={endpointUrl} onChange={(event) => setEndpointUrl(event.target.value)} placeholder="Base URL" />
          {endpointKind === "open_ai_compatible" && <input className={INPUT} value={endpointProvider} onChange={(event) => setEndpointProvider(event.target.value)} placeholder="Provider id with saved key" />}
          {endpointKind === "comfy_ui" && <textarea className={`${INPUT} min-h-28 md:col-span-2 font-mono text-xs`} value={workflowJson} onChange={(event) => setWorkflowJson(event.target.value)} aria-label="ComfyUI API workflow JSON" />}
        </div>
        <Button className="mt-3" size="sm" disabled={busy !== null || !endpointId.trim() || !endpointLabel.trim()} onClick={() => void addEndpoint()}><Plus size={14} />Validate and activate endpoint</Button>
        <div className="mt-3 flex flex-col gap-2">
          {config.imageEndpoints.map((endpoint) => (
            <div key={endpoint.endpointId} className="flex items-center justify-between rounded-md border border-border bg-background px-3 py-2 text-xs">
              <span><strong>{endpoint.label}</strong> · {endpoint.kind} · {endpoint.baseUrl}</span>
              <Button size="sm" variant="ghost" onClick={() => void removeEndpoint(endpoint.endpointId)}><Trash2 size={13} />Remove</Button>
            </div>
          ))}
        </div>
      </section>

      <section className="rounded-lg border border-border bg-surface p-4">
        <h3 className="text-sm font-semibold">Image workspace</h3>
        {enabledEndpoints.length === 0 ? <p className="mt-2 text-xs text-muted">Add and activate an image endpoint first.</p> : <>
          <label className="mt-2 block text-xs text-muted">Active endpoint
            <select
              className={`${INPUT} mt-1`}
              value={selectedEndpoint?.endpointId ?? ""}
              onChange={(event) => {
                setSelectedEndpointId(event.target.value);
                setSourceArtifactId(null);
              }}
            >
              {enabledEndpoints.map((endpoint) => <option key={endpoint.endpointId} value={endpoint.endpointId}>{endpoint.label} · {endpoint.kind}</option>)}
            </select>
          </label>
          <textarea className={`${INPUT} mt-3 min-h-20`} value={prompt} onChange={(event) => setPrompt(event.target.value)} placeholder="Describe the image…" />
          {sourceArtifactId && (
            <div className="mt-2 flex items-center justify-between rounded-md border border-accent/40 bg-accent/10 px-3 py-2 text-xs text-muted">
              <span className="truncate">Editing gallery artifact {sourceArtifactId.slice(0, 12)}…</span>
              <Button size="sm" variant="ghost" onClick={() => setSourceArtifactId(null)}>Clear source</Button>
            </div>
          )}
          <input className={`${INPUT} mt-2`} value={negativePrompt} onChange={(event) => setNegativePrompt(event.target.value)} placeholder="Negative prompt (optional)" />
          <div className="mt-2 grid grid-cols-2 gap-2 md:grid-cols-4">
            <input className={INPUT} value={model} onChange={(event) => setModel(event.target.value)} placeholder="Model" />
            <input className={INPUT} value={width} onChange={(event) => setWidth(event.target.value)} aria-label="Width" />
            <input className={INPUT} value={height} onChange={(event) => setHeight(event.target.value)} aria-label="Height" />
            <input className={INPUT} value={steps} onChange={(event) => setSteps(event.target.value)} aria-label="Steps" />
            <input className={INPUT} value={cfgScale} onChange={(event) => setCfgScale(event.target.value)} aria-label="CFG scale" />
            <input className={INPUT} value={seed} onChange={(event) => setSeed(event.target.value)} aria-label="Seed" />
          </div>
          <div className="mt-3 flex items-center gap-2">
            <Button variant="primary" disabled={busy !== null || !prompt.trim() || !model.trim()} onClick={() => void generate()}><Image size={14} />Generate</Button>
            {busy?.startsWith("image-") && <Button variant="danger" onClick={() => void companionClient.cancelJob(busy)}>Cancel</Button>}
            {busy?.startsWith("image-") && <span className="text-xs text-muted">{Math.round(progress * 100)}%</span>}
          </div>
        </>}
        <div className="mt-4 grid grid-cols-2 gap-3 md:grid-cols-3">
          {gallery.slice(-12).reverse().map((entry) => (
            <figure key={entry.entryId} className="overflow-hidden rounded-md border border-border bg-background">
              {previews[entry.artifactId] ? <img src={previews[entry.artifactId]} alt={entry.prompt} className="aspect-square w-full object-contain" /> : <div className="aspect-square animate-pulse bg-surface-2" />}
              <figcaption className="p-2 text-[11px] text-muted">
                <p className="line-clamp-2">{entry.prompt}</p>
                <div className="mt-2 flex flex-wrap gap-1">
                  <Button
                    size="sm"
                    variant="ghost"
                    onClick={() => void companionClient.insertImageInChat(entry.artifactId).then(() => setStatus("Image placed in the active chat composer for review.")).catch((reason) => setError(errorText(reason)))}
                  >Use in chat</Button>
                  {selectedEndpoint?.supportsEditing && (
                    <Button size="sm" variant="ghost" onClick={() => {
                      setSourceArtifactId(entry.artifactId);
                      setPrompt(`Edit this image: ${entry.prompt}`);
                    }}>Edit</Button>
                  )}
                </div>
              </figcaption>
            </figure>
          ))}
        </div>
      </section>

      {status && <p role="status" className="text-xs text-success">{status}</p>}
      {error && <p role="alert" className="rounded-md border border-danger/40 bg-danger/10 p-3 text-xs text-danger">{error}</p>}
    </div>
  );
}
