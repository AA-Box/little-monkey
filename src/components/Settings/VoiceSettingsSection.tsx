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

import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { AlertTriangle, Gauge, Mic, Radio, Save, Trash2, Volume2 } from 'lucide-react';

import {
  blobToBase64,
  companionClient,
  type CompanionConfig,
  type TranscriptionBackendKind,
  type VoiceConfig,
} from '../../lib/companionClient';
import { dictationClient, type DictationCapabilities } from '../../lib/dictationClient';
import { errorMessage } from '../../lib/errors';
import { useT } from '../../lib/i18n';
import { base64AudioBlob } from '../../lib/talkAudio';
import {
  latencySummary,
  talkClient,
  type TalkMetricsSnapshot,
  type TranscriptionLanguage,
  type TranscriptionModel,
} from '../../lib/talkClient';
import { createTalkPlayer } from '../../lib/talkPlayback';
import { Button } from '../ui';

/** Download size, in the units the choice is actually weighed in. */
function formatModelSize(bytes: number): string {
  const mb = bytes / 1_000_000;
  return mb >= 1_000 ? `${(mb / 1_000).toFixed(1)} GB` : `${Math.round(mb)} MB`;
}

const INPUT =
  'w-full rounded-md border border-border bg-background px-2.5 py-2 text-sm text-foreground outline-none focus:ring-1 focus:ring-accent';

/** How long the microphone test records before playing itself back. */
const MIC_TEST_MS = 3_000;

/**
 * The transcription backend is chosen in “Voice and transcription” above, not
 * here — but it decides whether the wake phrase can be armed at all, so this
 * section has to be able to say which one is selected instead of asking the
 * operator to go and find out.
 */
const BACKEND_LABEL: Record<TranscriptionBackendKind, string> = {
  local_whisper: 'local whisper.cpp on this machine',
  provider: 'a BYOK provider',
  executable_extension: 'a sandboxed executable extension',
};

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
  const [dictationCapabilities, setDictationCapabilities] = useState<DictationCapabilities | null>(null);
  const clearedDictationLanguageRef = useRef<string | null>(null);
  /** Second step before always-listening arms. Reset whenever it is turned off. */
  const [confirmingAlwaysListening, setConfirmingAlwaysListening] = useState(false);

  const voice = config.voice;
  const { t } = useT();
  const player = useMemo(() => createTalkPlayer(), []);
  const selectedDictationLanguage = dictationCapabilities?.languages.find(
    (language) => language.id === voice.dictationLanguage,
  );
  const supportsSelectedOnDevice = voice.dictationLanguage
    ? selectedDictationLanguage?.supportsOnDevice === true
    : dictationCapabilities?.supportsOnDevice === true;

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
    void dictationClient.capabilities().then(setDictationCapabilities).catch(() => undefined);
  }, [loadDevices]);

  useEffect(() => {
    const selected = voice.dictationLanguage;
    if (!selected || !dictationCapabilities) return;
    const available = dictationCapabilities.languages;
    if (available.length === 0 || !available.some((language) => language.id === selected)) {
      if (clearedDictationLanguageRef.current === selected) return;
      clearedDictationLanguageRef.current = selected;
      onChange({ ...voice, dictationLanguage: null });
    } else {
      clearedDictationLanguageRef.current = null;
    }
  }, [dictationCapabilities, onChange, voice.dictationLanguage]);

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
      // The same player a conversation uses, so what this button proves is what
      // Talk will do — including the chosen output, and including falling back
      // to the system default where the browser cannot route at all.
      const played = await player.play(
        base64AudioBlob(speech.audioBase64, speech.mediaType),
        voice.outputDeviceId,
      );
      setNote(
        played
          ? 'Played a test phrase through the selected output.'
          : 'The output refused to play the test phrase. Try another speaker.',
      );
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setBusy(null);
    }
  }, [player, voice.outputDeviceId]);

  const localOnly = voice.backend === 'local_whisper';
  /** Wake listening left armed under a backend that cannot have it: the
   * checkbox that turns it off is disabled by the same condition, so without a
   * way out of this the whole configuration becomes unsaveable. */
  const armedElsewhere = !localOnly && (voice.wakePhraseEnabled || voice.alwaysListening);
  const [transcriptionLanguages, setTranscriptionLanguages] = useState<TranscriptionLanguage[]>([
    { id: 'auto', label: 'Detect automatically' },
  ]);
  const [models, setModels] = useState<TranscriptionModel[]>([]);
  const [installing, setInstalling] = useState<string | null>(null);
  const [modelError, setModelError] = useState<string | null>(null);
  const refreshModels = useCallback(() => {
    void talkClient
      .models()
      .then((list) => {
        if (Array.isArray(list)) setModels(list);
      })
      .catch(() => undefined);
  }, []);
  useEffect(refreshModels, [refreshModels]);

  /** Choosing a model that is not here yet downloads it now, rather than
   * stalling the first thing the operator says afterwards. */
  const chooseModel = useCallback(
    async (modelId: string) => {
      setModelError(null);
      patch({ transcriptionModel: modelId });
      if (models.find((model) => model.id === modelId)?.installed) return;
      setInstalling(modelId);
      try {
        await talkClient.installModel(modelId);
        refreshModels();
      } catch (reason) {
        setModelError(errorMessage(reason));
      } finally {
        setInstalling(null);
      }
    },
    [models, patch, refreshModels],
  );
  useEffect(() => {
    // Whisper's own table, so this list cannot drift from what the model honours.
    // Keep the built-in default unless a real list comes back: a backend
    // without this command answers null, and "Detect automatically" alone is a
    // working control while a missing list is a blank one.
    void talkClient
      .languages()
      .then((list) => {
        if (Array.isArray(list) && list.length > 0) setTranscriptionLanguages(list);
      })
      .catch(() => undefined);
  }, []);
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
      <p className="mt-1 text-[11px] text-faint">
        Transcription runs through {BACKEND_LABEL[voice.backend]}, chosen under “Voice and
        transcription” above. A spoken turn goes through Talk&apos;s own transcription, which keeps
        nothing: “Persist raw audio artifacts” there does not apply to a conversation, and no
        recording of one is written anywhere.
      </p>

      <div className="mt-4 rounded-md border border-border p-3">
        <div className="flex items-center gap-2">
          <Mic size={14} />
          <h4 className="text-xs font-semibold">{t('VoiceSettings.composerDictation')}</h4>
        </div>
        <p className="mt-1 text-[11px] text-faint">
          {t('VoiceSettings.composerDictationDescription')}
        </p>
        <div className="mt-3 grid gap-3 sm:grid-cols-2">
          <label className="text-xs text-muted">
            {t('VoiceSettings.recognition')}
            <select className={`${INPUT} mt-1`} value="native" disabled>
              <option value="native">{t('VoiceSettings.systemSpeechRecognition')}</option>
            </select>
          </label>
          <label className="text-xs text-muted">
            {t('VoiceSettings.language')}
            <select
              className={`${INPUT} mt-1`}
              value={voice.dictationLanguage ?? ''}
              onChange={(event) => patch({ dictationLanguage: event.target.value || null })}
            >
              <option value="">{t('VoiceSettings.systemDefault')}</option>
              {dictationCapabilities?.languages.map((language) => (
                <option key={language.id} value={language.id}>{language.label}</option>
              ))}
            </select>
          </label>
        </div>
        {dictationCapabilities?.platform === 'macos' && (
          <label className="mt-3 flex items-start gap-2 text-xs text-muted">
            <input
              className="mt-0.5"
              type="checkbox"
              checked={voice.dictationRequireOnDevice}
              disabled={!supportsSelectedOnDevice}
              onChange={(event) => patch({ dictationRequireOnDevice: event.target.checked })}
            />
            <span>
              {t('VoiceSettings.requireOnDevice')}
              {!supportsSelectedOnDevice && (
                <span className="mt-1 block text-[11px] text-faint">{t('VoiceSettings.onDeviceUnavailable')}</span>
              )}
            </span>
          </label>
        )}
        {dictationCapabilities && !dictationCapabilities.supported && (
          <p className="mt-2 text-[11px] text-warning">{t('VoiceSettings.speechRecognitionUnavailable')}</p>
        )}
      </div>

      <label className="mt-3 block text-xs text-muted">
        Speech model
        <select
          className={`${INPUT} mt-1`}
          value={voice.transcriptionModel || 'base'}
          disabled={installing !== null}
          onChange={(event) => void chooseModel(event.target.value)}
        >
          {models.map((model) => (
            <option key={model.id} value={model.id}>
              {model.label} · {formatModelSize(model.bytes)}
              {model.installed ? '' : ' · downloads'}
            </option>
          ))}
        </select>
        <span className="mt-1 block text-[11px] text-faint">
          {installing
            ? `Downloading ${installing}… it is verified against its published checksum before anything uses it.`
            : 'A bigger model hears names and accents the small one guesses at, and takes longer per utterance. Each one is downloaded once.'}
        </span>
        {modelError && (
          <span role="alert" className="mt-1 block text-[11px] text-danger">
            {modelError}
          </span>
        )}
      </label>

      <label className="mt-3 block text-xs text-muted">
        Spoken language
        <select
          className={`${INPUT} mt-1`}
          value={voice.language || 'auto'}
          onChange={(event) => patch({ language: event.target.value })}
        >
          {transcriptionLanguages.map((language) => (
            <option key={language.id} value={language.id}>
              {language.label}
            </option>
          ))}
        </select>
        <span className="mt-1 block text-[11px] text-faint">
          Automatic detection decides from the audio, and one sentence is not much to go on: a
          mostly-English question with a Swedish name in it detects as English, and the name comes
          back spelled the way an English speaker would have said it. Naming the language you
          actually speak fixes that.
        </span>
      </label>

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
            How long a sound must last to count as speech rather than a door. 50–2000.
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
            A monologue is answered at this point rather than left running. 1000–90000.
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
          <div className="mt-2 flex items-start gap-2 rounded border border-warning/40 bg-warning/10 p-2 text-[11px]">
            <AlertTriangle size={12} className="mt-0.5 shrink-0" />
            <div>
              Wake detection is local-only, and transcription is set to{' '}
              {BACKEND_LABEL[voice.backend]}. Set Backend to “Local whisper.cpp” under “Voice and
              transcription” above to enable it.
              {armedElsewhere && (
                <>
                  {' '}
                  Until then this pair cannot be saved at all — the checkbox below is left on and
                  disabled, and every save is refused with it.
                  <Button
                    className="mt-2"
                    size="sm"
                    onClick={() => {
                      setConfirmingAlwaysListening(false);
                      patch({ wakePhraseEnabled: false, alwaysListening: false });
                    }}
                  >
                    Turn wake listening off
                  </Button>
                </>
              )}
            </div>
          </div>
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
          Require the wake phrase before anything spoken is sent
        </label>
        <span className="mt-1 block text-[11px] text-faint">
          Applies while Talk is capturing continuously. Everything else said is transcribed on this
          machine, matched against the phrase, and dropped.
        </span>
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
              Opening Talk starts capturing straight away, with nobody pressing Start. This machine
              transcribes locally to hear the phrase, and nothing is uploaded or sent to a model
              until it is heard. Closing Talk closes the microphone.
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
              Turning this on makes Talk start capturing the moment it is opened, without pressing
              Start, and keep the microphone open until it is closed. Detection runs on this machine
              and no audio is uploaded until the phrase is heard — but the microphone is open the
              whole time.
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
