import { useCallback, useEffect, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { Camera, Mic, Octagon, Play, Save } from "lucide-react";

import {
  companionClient,
  formatSpeakerTranscript,
  type CaptureGrant,
  type CompanionConfig,
  type SpeechBackendKind,
  type TranscriptionBackendKind,
} from "../../lib/companionClient";
import {
  executableExtensionsClient,
  type ActiveCapability,
} from "../../lib/executableExtensionsClient";
import { Button } from "../ui";
import { VoiceSettingsSection } from "./VoiceSettingsSection";
import { errorMessage } from "../../lib/errors";

const INPUT = "w-full rounded-md border border-border bg-background px-2.5 py-2 text-sm text-foreground outline-none focus:ring-1 focus:ring-accent";

function errorText(error: unknown): string {
  return errorMessage(error);
}

function capabilityValue(extensionId: string | null, capabilityId: string): string {
  return JSON.stringify([extensionId, capabilityId]);
}

/**
 * One provider picker over whatever capabilities the backend says are active.
 *
 * The list is backend truth: `activeCapabilities` only returns extensions that
 * are installed, validated, enabled, running and healthy, so nothing here can
 * offer a provider that would then fail. A selection whose owner has since
 * gone is shown as an explicitly unavailable option rather than silently
 * dropped, because a picker that quietly reverts to blank hides the fact that
 * a working feature stopped working.
 */
function CapabilityPicker({
  label,
  hint,
  capabilities,
  extensionId,
  capabilityId,
  onSelect,
}: {
  label: string;
  hint: string;
  capabilities: ActiveCapability[];
  extensionId: string | null;
  capabilityId: string | null;
  onSelect: (selected: ActiveCapability | null) => void;
}) {
  const value = capabilityId ? capabilityValue(extensionId, capabilityId) : "";
  const available = capabilities.some(
    (capability) => capability.extension_id === extensionId
      && capability.capability_id === capabilityId,
  );
  return (
    <label className="text-xs text-muted md:col-span-2">{label}
      <select
        className={`${INPUT} mt-1`}
        value={value}
        onChange={(event) => onSelect(
          capabilities.find(
            (capability) => capabilityValue(capability.extension_id, capability.capability_id)
              === event.target.value,
          ) ?? null,
        )}
      >
        <option value="">Select a healthy, running capability</option>
        {capabilityId && !available
          && <option value={value} disabled>Configured owner/capability is unavailable — reselect it</option>}
        {capabilities.map((capability) => (
          <option
            key={`${capability.extension_id}:${capability.capability_id}`}
            value={capabilityValue(capability.extension_id, capability.capability_id)}
          >
            {capability.display_name} · {capability.extension_id} · {capability.version}
          </option>
        ))}
      </select>
      <span className="mt-1 block text-faint">{hint}</span>
    </label>
  );
}

export function CompanionPanel() {
  const [config, setConfig] = useState<CompanionConfig | null>(null);
  const [sttCapabilities, setSttCapabilities] = useState<ActiveCapability[]>([]);
  const [ttsCapabilities, setTtsCapabilities] = useState<ActiveCapability[]>([]);
  const [realtimeCapabilities, setRealtimeCapabilities] = useState<ActiveCapability[]>([]);
  const [grants, setGrants] = useState<CaptureGrant[]>([]);
  const [status, setStatus] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [transcript, setTranscript] = useState("");
  const [meetingMode, setMeetingMode] = useState(false);

  const load = useCallback(async () => {
    const discover = (kind: "stt" | "tts" | "realtime_voice", label: string) =>
      executableExtensionsClient.activeCapabilities(kind).catch((reason) => {
        setError(`Could not discover executable ${label} capabilities: ${errorText(reason)}`);
        return [];
      });
    const [nextConfig, nextGrants, nextStt, nextTts, nextRealtime] = await Promise.all([
      companionClient.config(),
      companionClient.grants(),
      discover("stt", "STT"),
      discover("tts", "speech"),
      discover("realtime_voice", "realtime voice"),
    ]);
    setConfig(nextConfig);
    setGrants(nextGrants);
    setSttCapabilities(nextStt);
    setTtsCapabilities(nextTts);
    setRealtimeCapabilities(nextRealtime);
  }, []);

  useEffect(() => {
    void load().catch((reason) => setError(errorText(reason)));
  }, [load]);

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
            <select className={`${INPUT} mt-1`} value={config.voice.backend} onChange={(event) => setConfig({ ...config, voice: { ...config.voice, backend: event.target.value as TranscriptionBackendKind } })}>
              <option value="local_whisper">Built-in local Whisper</option>
              <option value="provider">BYOK provider</option>
              <option value="executable_extension">Executable extension</option>
            </select>
          </label>
          <label className="text-xs text-muted">Language
            <input className={`${INPUT} mt-1`} value={config.voice.language} onChange={(event) => setConfig({ ...config, voice: { ...config.voice, language: event.target.value } })} />
          </label>
          {config.voice.backend === "local_whisper" ? (
            <div className="rounded-md border border-border bg-background p-3 text-xs text-muted md:col-span-2">
              Local transcription is built in. Little Monkey ships its multilingual Whisper model with the app, so it works offline; there is no binary or model path to configure.
            </div>
          ) : config.voice.backend === "provider" ? <>
            <label className="text-xs text-muted">Provider id
              <input className={`${INPUT} mt-1`} value={config.voice.providerId ?? ""} onChange={(event) => setConfig({ ...config, voice: { ...config.voice, providerId: event.target.value || null } })} placeholder="openai" />
            </label>
            <label className="text-xs text-muted">Transcription model
              <input className={`${INPUT} mt-1`} value={config.voice.providerModel} onChange={(event) => setConfig({ ...config, voice: { ...config.voice, providerModel: event.target.value } })} />
            </label>
          </> : <>
            <CapabilityPicker
              label="Executable STT capability"
              hint="Only validated, enabled, running, healthy STT extensions are listed. Unless raw-audio persistence is enabled, delegated audio uses a private per-job artifact store that is removed after the invocation."
              capabilities={sttCapabilities}
              extensionId={config.voice.extensionId}
              capabilityId={config.voice.extensionCapabilityId}
              onSelect={(selected) => setConfig({
                ...config,
                voice: {
                  ...config.voice,
                  extensionId: selected?.extension_id ?? null,
                  extensionCapabilityId: selected?.capability_id ?? null,
                },
              })}
            />
          </>}
          <label className="text-xs text-muted">Speech synthesis
            <select
              className={`${INPUT} mt-1`}
              value={config.voice.ttsBackend}
              onChange={(event) => setConfig({ ...config, voice: { ...config.voice, ttsBackend: event.target.value as SpeechBackendKind } })}
            >
              <option value="system">This machine&apos;s voice</option>
              <option value="executable_extension">Executable extension</option>
            </select>
          </label>
          <label className="text-xs text-muted">Voice name (optional)
            <input className={`${INPUT} mt-1`} value={config.voice.ttsVoice ?? ""} onChange={(event) => setConfig({ ...config, voice: { ...config.voice, ttsVoice: event.target.value || null } })} />
          </label>
          {config.voice.ttsBackend === "executable_extension" && (
            <CapabilityPicker
              label="Executable speech capability"
              hint="The extension returns audio as an artifact it wrote during the same invocation; audio it did not write is refused before anything plays it."
              capabilities={ttsCapabilities}
              extensionId={config.voice.ttsExtensionId}
              capabilityId={config.voice.ttsExtensionCapabilityId}
              onSelect={(selected) => setConfig({
                ...config,
                voice: {
                  ...config.voice,
                  ttsExtensionId: selected?.extension_id ?? null,
                  ttsExtensionCapabilityId: selected?.capability_id ?? null,
                },
              })}
            />
          )}
          <label className="text-xs text-muted">Live call voice
            <select
              className={`${INPUT} mt-1`}
              value={config.voice.realtimeBackend}
              onChange={(event) => setConfig({ ...config, voice: { ...config.voice, realtimeBackend: event.target.value as SpeechBackendKind } })}
            >
              <option value="system">Transcribe and synthesize per turn</option>
              <option value="executable_extension">Executable realtime extension</option>
            </select>
            <span className="mt-1 block text-faint">
              A live phone call is a session, not a clip, so it is chosen separately from speech synthesis.
            </span>
          </label>
          {config.voice.realtimeBackend === "executable_extension" && (
            <CapabilityPicker
              label="Executable realtime voice capability"
              hint="One sandboxed session is held open for the whole call and closed when it ends. Updating or disabling the extension mid-call fails the call rather than handing the rest of it to different code."
              capabilities={realtimeCapabilities}
              extensionId={config.voice.realtimeExtensionId}
              capabilityId={config.voice.realtimeExtensionCapabilityId}
              onSelect={(selected) => setConfig({
                ...config,
                voice: {
                  ...config.voice,
                  realtimeExtensionId: selected?.extension_id ?? null,
                  realtimeExtensionCapabilityId: selected?.capability_id ?? null,
                },
              })}
            />
          )}
          <label className="flex items-end gap-2 pb-2 text-xs text-muted">
            <input type="checkbox" checked={config.voice.saveRawAudio} onChange={(event) => setConfig({ ...config, voice: { ...config.voice, saveRawAudio: event.target.checked } })} />Persist raw audio artifacts
          </label>
        </div>
        <div className="mt-3 flex gap-2">
          <Button
            size="sm"
            variant="primary"
            disabled={busy !== null
              || (config.voice.backend === "executable_extension" && (!config.voice.extensionId || !config.voice.extensionCapabilityId))
              || (config.voice.ttsBackend === "executable_extension" && (!config.voice.ttsExtensionId || !config.voice.ttsExtensionCapabilityId))
              || (config.voice.realtimeBackend === "executable_extension" && (!config.voice.realtimeExtensionId || !config.voice.realtimeExtensionCapabilityId))}
            onClick={() => void saveConfig(config)}
          ><Save size={14} />Save voice settings</Button>
          <Button size="sm" disabled={busy !== null} onClick={() => void transcribeFile()}><Play size={14} />Transcribe a file</Button>
          {busy?.startsWith("transcribe-") && <Button size="sm" variant="danger" onClick={() => void companionClient.cancelJob(busy)}>Cancel</Button>}
        </div>
        <label className="mt-3 flex items-center gap-2 text-xs text-muted">
          <input type="checkbox" checked={meetingMode} onChange={(event) => setMeetingMode(event.target.checked)} />
          Request speaker-separated meeting output when the selected backend supports it
        </label>
        {transcript && <textarea className={`${INPUT} mt-3 min-h-24`} value={transcript} onChange={(event) => setTranscript(event.target.value)} aria-label="Transcript result" />}
      </section>

      <VoiceSettingsSection
        config={config}
        onChange={(voice) => setConfig({ ...config, voice })}
        onSave={saveConfig}
      />

      {status && <p role="status" className="text-xs text-success">{status}</p>}
      {error && <p role="alert" className="rounded-md border border-danger/40 bg-danger/10 p-3 text-xs text-danger">{error}</p>}
    </div>
  );
}
