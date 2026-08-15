import { useCallback, useEffect, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { Camera, Mic, Octagon, Play, Save } from "lucide-react";

import {
  companionClient,
  formatSpeakerTranscript,
  type CaptureGrant,
  type CompanionConfig,
} from "../../lib/companionClient";
import { Button } from "../ui";
import { VoiceSettingsSection } from "./VoiceSettingsSection";
import { errorMessage } from "../../lib/errors";

const INPUT = "w-full rounded-md border border-border bg-background px-2.5 py-2 text-sm text-foreground outline-none focus:ring-1 focus:ring-accent";

function errorText(error: unknown): string {
  return errorMessage(error);
}

export function CompanionPanel() {
  const [config, setConfig] = useState<CompanionConfig | null>(null);
  const [grants, setGrants] = useState<CaptureGrant[]>([]);
  const [status, setStatus] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [transcript, setTranscript] = useState("");
  const [meetingMode, setMeetingMode] = useState(false);

  const load = useCallback(async () => {
    const [nextConfig, nextGrants] = await Promise.all([
      companionClient.config(),
      companionClient.grants(),
    ]);
    setConfig(nextConfig);
    setGrants(nextGrants);
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
