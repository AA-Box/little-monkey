/**
 * Voice settings: the devices Talk uses, how it decides you stopped speaking,
 * whether anything listens on its own, and what the last hundred turns cost in
 * milliseconds.
 *
 * The wake phrase and always-listening are the two settings here that can make
 * a machine listen without anyone pressing anything, so they are the two that
 * are gated: a plain checkbox arms nothing, and the arming step spells out what
 * it turns on. The Rust side refuses the pair unless transcription is local, so
 * "always listening" can never quietly mean "always uploading".
 */

import { useCallback, useEffect, useState } from 'react';
import { AlertTriangle, Gauge, Mic, Radio, Save, Trash2, Volume2 } from 'lucide-react';

import {
  blobToBase64,
  companionClient,
  type CompanionConfig,
  type VoiceConfig,
} from '../../lib/companionClient';
import { errorMessage } from '../../lib/errors';
import { latencySummary, talkClient, type TalkMetricsSnapshot } from '../../lib/talkClient';
import { Button } from '../ui';

const INPUT =
  'w-full rounded-md border border-border bg-background px-2.5 py-2 text-sm text-foreground outline-none focus:ring-1 focus:ring-accent';

/** How long the microphone test records before playing itself back. */
const MIC_TEST_MS = 3_000;

export interface VoiceSettingsSectionProps {
  config: CompanionConfig;
  onChange: (voice: VoiceConfig) => void;
  onSave: (config: CompanionConfig, message?: string) => Promise<void>;
}

interface DeviceOption {
  deviceId: string;
  label: string;
}

export function VoiceSettingsSection({ config, onChange, onSave }: VoiceSettingsSectionProps) {
  const [inputs, setInputs] = useState<DeviceOption[]>([]);
  const [outputs, setOutputs] = useState<DeviceOption[]>([]);
  const [metrics, setMetrics] = useState<TalkMetricsSnapshot | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  /** Second step before always-listening arms. Reset whenever it is turned off. */
  const [confirmingAlwaysListening, setConfirmingAlwaysListening] = useState(false);

  const voice = config.voice;

  const loadDevices = useCallback(async () => {
    // Labels only exist after permission has been given once — an unlabelled
    // list is the honest state, not an error.
    const devices = await navigator.mediaDevices.enumerateDevices();
    const named = (kind: MediaDeviceKind) =>
      devices
        .filter((device) => device.kind === kind)
        .map((device, index) => ({
          deviceId: device.deviceId,
          label: device.label || `${kind === 'audioinput' ? 'Microphone' : 'Speaker'} ${index + 1}`,
        }));
    setInputs(named('audioinput'));
    setOutputs(named('audiooutput'));
  }, []);

  useEffect(() => {
    void loadDevices().catch((reason) => setError(errorMessage(reason)));
    void talkClient.metrics().then(setMetrics).catch(() => undefined);
  }, [loadDevices]);

  const patch = (change: Partial<VoiceConfig>) => onChange({ ...voice, ...change });

  const testMicrophone = useCallback(async () => {
    setBusy('mic');
    setError(null);
    setNote(null);
    let stream: MediaStream | null = null;
    try {
      const grant = await companionClient.grant('microphone', 60_000, 'voice-settings-test');
      stream = await navigator.mediaDevices.getUserMedia({
        audio: voice.inputDeviceId
          ? { deviceId: { exact: voice.inputDeviceId } }
          : true,
        video: false,
      });
      // Labels become readable once permission exists, so refresh the lists.
      await loadDevices();
      const recorder = new MediaRecorder(stream);
      const chunks: Blob[] = [];
      recorder.ondataavailable = (event) => {
        if (event.data.size > 0) chunks.push(event.data);
      };
      const recorded = new Promise<Blob>((resolve) => {
        recorder.onstop = () => resolve(new Blob(chunks, { type: recorder.mimeType || 'audio/webm' }));
      });
      recorder.start();
      await new Promise((resolve) => window.setTimeout(resolve, MIC_TEST_MS));
      recorder.stop();
      const blob = await recorded;
      const jobId = `mic-test-${crypto.randomUUID()}`;
      const result = await companionClient.transcribeAudio(
        grant.grantId,
        jobId,
        await blobToBase64(blob),
        blob.type,
      );
      setNote(
        result.text.trim()
          ? `Heard: “${result.text.trim()}” (via ${result.backend}).`
          : `The microphone recorded ${blob.size} bytes, but ${result.backend} found no words in it.`,
      );
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      stream?.getTracks().forEach((track) => track.stop());
      setBusy(null);
    }
  }, [loadDevices, voice.inputDeviceId]);

  const testSpeaker = useCallback(async () => {
    setBusy('speaker');
    setError(null);
    setNote(null);
    try {
      const jobId = `speaker-test-${crypto.randomUUID()}`;
      const speech = await talkClient.synthesize(
        jobId,
        'This is Little Monkey. If you can hear this, speech output is working.',
      );
      const blob = new Blob(
        [Uint8Array.from(atob(speech.audioBase64), (character) => character.charCodeAt(0))],
        { type: speech.mediaType },
      );
      const url = URL.createObjectURL(blob);
      const player = new Audio(url);
      // `setSinkId` is how a chosen output is honoured; browsers that do not
      // expose it play on the system default rather than failing the test.
      const withSink = player as HTMLAudioElement & {
        setSinkId?: (id: string) => Promise<void>;
      };
      if (voice.outputDeviceId && typeof withSink.setSinkId === 'function') {
        await withSink.setSinkId(voice.outputDeviceId).catch(() => undefined);
      }
      await player.play();
      player.onended = () => URL.revokeObjectURL(url);
      setNote('Played a test phrase through the selected output.');
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setBusy(null);
    }
  }, [voice.outputDeviceId]);

  const localOnly = voice.backend === 'local_whisper';
  const stt = metrics ? latencySummary(metrics.metrics, 'sttMs') : null;
  const firstToken = metrics ? latencySummary(metrics.metrics, 'modelFirstTokenMs') : null;
  const firstAudio = metrics ? latencySummary(metrics.metrics, 'ttsFirstAudioMs') : null;
  const endToEnd = metrics ? latencySummary(metrics.metrics, 'endToEndMs') : null;

  return (
    <section className="rounded-lg border border-border bg-surface p-4">
      <div className="flex items-center gap-2">
        <Mic size={16} />
        <h3 className="text-sm font-semibold">Talk</h3>
      </div>
      <p className="mt-1 text-xs text-muted">
        Devices, how Talk decides you have stopped speaking, and whether anything listens on its own.
      </p>

      <div className="mt-3 grid gap-3 sm:grid-cols-2">
        <label className="text-xs text-muted">
          Microphone
          <select
            className={`${INPUT} mt-1`}
            value={voice.inputDeviceId ?? ''}
            onChange={(event) => patch({ inputDeviceId: event.target.value || null })}
          >
            <option value="">System default</option>
            {inputs.map((device) => (
              <option key={device.deviceId} value={device.deviceId}>
                {device.label}
              </option>
            ))}
          </select>
        </label>
        <label className="text-xs text-muted">
          Speaker
          <select
            className={`${INPUT} mt-1`}
            value={voice.outputDeviceId ?? ''}
            onChange={(event) => patch({ outputDeviceId: event.target.value || null })}
          >
            <option value="">System default</option>
            {outputs.map((device) => (
              <option key={device.deviceId} value={device.deviceId}>
                {device.label}
              </option>
            ))}
          </select>
        </label>
        <label className="text-xs text-muted">
          Speech backend
          <select
            className={`${INPUT} mt-1`}
            value={voice.ttsBackend}
            onChange={(event) => patch({ ttsBackend: event.target.value as VoiceConfig['ttsBackend'] })}
          >
            <option value="system">This machine&apos;s system voice</option>
          </select>
        </label>
        <div className="flex items-end gap-2">
          <Button size="sm" disabled={busy !== null} onClick={() => void testMicrophone()}>
            <Mic size={14} />
            {busy === 'mic' ? 'Listening…' : 'Test microphone'}
          </Button>
          <Button size="sm" disabled={busy !== null} onClick={() => void testSpeaker()}>
            <Volume2 size={14} />
            Test speaker
          </Button>
        </div>
      </div>

      <div className="mt-4 grid gap-3 sm:grid-cols-3">
        <label className="text-xs text-muted">
          Minimum speech (ms)
          <input
            className={`${INPUT} mt-1`}
            type="number"
            min={50}
            max={2000}
            step={10}
            value={voice.vadMinSpeechMs}
            onChange={(event) => patch({ vadMinSpeechMs: Number(event.target.value) })}
          />
          <span className="mt-1 block text-[11px] text-faint">
            How long a sound must last to count as speech rather than a door.
          </span>
        </label>
        <label className="text-xs text-muted">
          End-of-speech silence (ms)
          <input
            className={`${INPUT} mt-1`}
            type="number"
            min={400}
            max={2000}
            step={50}
            value={voice.vadSilenceMs}
            onChange={(event) => patch({ vadSilenceMs: Number(event.target.value) })}
          />
          <span className="mt-1 block text-[11px] text-faint">
            Raise it if Talk cuts you off mid-thought. 400–2000.
          </span>
        </label>
        <label className="text-xs text-muted">
          Longest utterance (ms)
          <input
            className={`${INPUT} mt-1`}
            type="number"
            min={1000}
            max={90000}
            step={1000}
            value={voice.vadMaxUtteranceMs}
            onChange={(event) => patch({ vadMaxUtteranceMs: Number(event.target.value) })}
          />
          <span className="mt-1 block text-[11px] text-faint">
            A monologue is answered at this point rather than left running.
          </span>
        </label>
      </div>

      <div className="mt-4 rounded-md border border-border p-3">
        <div className="flex items-center gap-2">
          <Radio size={14} className={voice.alwaysListening ? 'animate-pulse text-danger' : ''} />
          <h4 className="text-xs font-semibold">Wake phrase</h4>
          {voice.alwaysListening && (
            <span className="rounded-full bg-danger/15 px-2 py-0.5 text-[11px] font-medium text-danger">
              Listening now
            </span>
          )}
        </div>
        {!localOnly && (
          <p className="mt-2 flex items-start gap-2 rounded border border-warning/40 bg-warning/10 p-2 text-[11px]">
            <AlertTriangle size={12} className="mt-0.5 shrink-0" />
            Wake detection is local-only. Switch transcription to local Whisper above to enable it.
          </p>
        )}
        <label className="mt-2 flex items-center gap-2 text-xs">
          <input
            type="checkbox"
            disabled={!localOnly}
            checked={voice.wakePhraseEnabled}
            onChange={(event) => {
              const enabled = event.target.checked;
              setConfirmingAlwaysListening(false);
              patch({
                wakePhraseEnabled: enabled,
                alwaysListening: enabled ? voice.alwaysListening : false,
              });
            }}
          />
          Listen for a wake phrase when Talk is open
        </label>
        <label className="mt-2 block text-xs text-muted">
          Phrase
          <input
            className={`${INPUT} mt-1`}
            disabled={!voice.wakePhraseEnabled}
            value={voice.wakePhrase}
            maxLength={128}
            onChange={(event) => patch({ wakePhrase: event.target.value })}
          />
        </label>

        {voice.alwaysListening ? (
          <div className="mt-3 rounded border border-danger/40 bg-danger/10 p-2">
            <p className="text-[11px] text-danger">
              This machine keeps the microphone open and transcribes locally to hear the phrase.
              Nothing is uploaded or sent to a model until it is heard.
            </p>
            <Button
              className="mt-2"
              size="sm"
              variant="danger"
              onClick={() => {
                setConfirmingAlwaysListening(false);
                patch({ alwaysListening: false });
              }}
            >
              Stop always-listening
            </Button>
          </div>
        ) : confirmingAlwaysListening ? (
          <div className="mt-3 rounded border border-danger/40 bg-danger/10 p-2">
            <p className="text-[11px] text-danger">
              Turning this on keeps your microphone open whenever Talk is open. Detection runs on
              this machine and no audio is uploaded until the phrase is heard — but the microphone
              is open the whole time.
            </p>
            <div className="mt-2 flex gap-2">
              <Button
                size="sm"
                variant="danger"
                onClick={() => {
                  setConfirmingAlwaysListening(false);
                  patch({ alwaysListening: true });
                }}
              >
                Yes, listen continuously
              </Button>
              <Button size="sm" onClick={() => setConfirmingAlwaysListening(false)}>
                Cancel
              </Button>
            </div>
          </div>
        ) : (
          <Button
            className="mt-3"
            size="sm"
            disabled={!voice.wakePhraseEnabled}
            onClick={() => setConfirmingAlwaysListening(true)}
          >
            <Radio size={14} />
            Listen continuously…
          </Button>
        )}
      </div>

      <div className="mt-4 rounded-md border border-border p-3">
        <div className="flex items-center gap-2">
          <Gauge size={14} />
          <h4 className="text-xs font-semibold">Latency</h4>
          <span className="text-[11px] text-faint">
            {metrics ? `${metrics.metrics.length} turns kept` : 'loading…'}
          </span>
          <Button
            className="ml-auto"
            size="sm"
            disabled={!metrics || metrics.metrics.length === 0}
            onClick={() => {
              void talkClient
                .clearMetrics()
                .then(setMetrics)
                .catch((reason) => setError(errorMessage(reason)));
            }}
          >
            <Trash2 size={13} />
            Clear
          </Button>
        </div>
        <p className="mt-1 text-[11px] text-faint">
          Durations only. No transcript, no answer and no audio is kept here or in a support bundle.
        </p>
        <dl className="mt-2 grid gap-2 text-[11px] sm:grid-cols-2">
          {[
            ['Transcription', stt],
            ['Model first token', firstToken],
            ['First audio out', firstAudio],
            ['End to end', endToEnd],
          ].map(([label, summary]) => (
            <div key={String(label)} className="flex justify-between gap-2">
              <dt className="text-muted">{String(label)}</dt>
              <dd className="tabular-nums">
                {summary && typeof summary === 'object'
                  ? `${summary.median} ms median · ${summary.worst} ms worst`
                  : '—'}
              </dd>
            </div>
          ))}
          <div className="flex justify-between gap-2">
            <dt className="text-muted">Interrupted</dt>
            <dd className="tabular-nums">{metrics?.interruptCount ?? 0}</dd>
          </div>
          <div className="flex justify-between gap-2">
            <dt className="text-muted">Fell back to text</dt>
            <dd className="tabular-nums">{metrics?.fallbackCount ?? 0}</dd>
          </div>
        </dl>
      </div>

      {note && <p className="mt-3 text-xs text-muted">{note}</p>}
      {error && (
        <p role="alert" className="mt-3 rounded-md border border-danger/40 bg-danger/10 p-2 text-xs text-danger">
          {error}
        </p>
      )}

      <Button
        className="mt-4"
        size="sm"
        variant="primary"
        disabled={busy !== null}
        onClick={() => {
          void onSave(config, 'Talk settings saved.').then(() =>
            talkClient.metrics().then(setMetrics).catch(() => undefined),
          );
        }}
      >
        <Save size={14} />
        Save Talk settings
      </Button>
    </section>
  );
}
