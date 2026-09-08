/**
 * The wake-word acceptance walkthrough, executed rather than described.
 *
 * The feature's acceptance script is fifteen steps an operator performs by
 * hand: enable the wake word, enable Always Listening, stay silent, say
 * something unrelated, say the wake phrase and a question, talk over the
 * answer, and turn it all off again. Every step spans both runtimes — the
 * keyword spotter, Whisper and the settings validator are Rust; the state
 * machine that decides what any of it means is `TalkSession` here. Testing
 * either half alone leaves the join unproven, and the join is the product.
 *
 * So this drives the real `TalkSession` — the same class the chat window
 * constructs — through the real `PcmRingBuffer`, `BoundedPcmQueue`,
 * `StreamingLinearResampler` and `pcm16WavBlob`, against the real native
 * keyword spotter and the real bundled Whisper running in
 * `bin/wake-word-e2e.rs`. Nothing about detection, transcription or
 * configuration is mocked.
 *
 * Two things are still substituted, and neither is a policy:
 *
 * - `getUserMedia` and the operating system's permission dialog. A grant
 *   nobody can click cannot be clicked here. What the microphone *does* once
 *   granted — open before "armed" is shown, and stay open across a wake event —
 *   is asserted. Its *closing* is not, and cannot be: `TalkPorts` has no close,
 *   because the devices belong to `useTalkSession`. That half is asserted
 *   against the real hook in `useTalkSession.test.tsx`, including the step
 *   below that this file can only configure — turning Always Listening off,
 *   which closes the microphone there with nobody calling `stop()`.
 * - The speaker. Synthesis and playback are the operator's configured backend,
 *   which has nothing to do with wake detection; playback here is a promise
 *   this file resolves, so a barge-in can be delivered at an exact moment
 *   rather than raced against real audio.
 *
 * Run it with the staged model and the harness binary:
 *
 *   pnpm stage:wake-word && pnpm stage:whisper
 *   cargo build --manifest-path src-tauri/Cargo.toml --bin wake-word-e2e
 *   LITTLE_MONKEY_WAKE_WALKTHROUGH_E2E=1 pnpm vitest run src/lib/wakeWordWalkthrough.e2e.test.ts
 *
 * The phrase is `light up` because that is what the pinned upstream fixture
 * actually says. A walkthrough that asserted against a phrase no recording
 * contains would be asserting against a mock again.
 */

import { spawn, type ChildProcessWithoutNullStreams } from 'node:child_process';
import { createInterface, type Interface } from 'node:readline';
import path from 'node:path';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';

import {
  BoundedPcmQueue,
  PcmRingBuffer,
  pcm16WavBlob,
  rmsOf,
  StreamingLinearResampler,
} from './talkAudio';
import { TalkSession, type TalkPorts, type TalkRecording, type TalkState } from './talkEngine';

/** Exactly what `useTalkSession` uses. Divergence here would be a different test. */
const KWS_SAMPLE_RATE = 16_000;
const KWS_RING_SAMPLES = KWS_SAMPLE_RATE * 3;
const KWS_PENDING_SAMPLES = KWS_SAMPLE_RATE / 5;
/** 100 ms, the cadence a 48 kHz worklet resamples down to. */
const FRAME_SAMPLES = KWS_SAMPLE_RATE / 10;
const FRAME_MS = 100;

/** What `test_wavs/0.wav` says before its command. */
const WAKE_PHRASE = 'light up';

const REPOSITORY_ROOT = path.resolve(__dirname, '..', '..');
const HARNESS_BINARY =
  process.env.LITTLE_MONKEY_WAKE_E2E_BIN
  ?? path.join(
    REPOSITORY_ROOT,
    'src-tauri',
    'target',
    'debug',
    process.platform === 'win32' ? 'wake-word-e2e.exe' : 'wake-word-e2e',
  );

interface Detection {
  detected: boolean;
  sessionId: string;
  keywordEndSample: number;
  inferenceMs: number;
}

/** The Rust half, one request at a time over newline-delimited JSON. */
class Harness {
  private readonly pending: Array<{
    resolve: (value: Record<string, unknown>) => void;
    reject: (reason: Error) => void;
  }> = [];
  private ready: (() => void) | null = null;

  private constructor(
    private readonly child: ChildProcessWithoutNullStreams,
    private readonly lines: Interface,
  ) {}

  static async start(): Promise<Harness> {
    const child = spawn(HARNESS_BINARY, {
      stdio: ['pipe', 'pipe', 'pipe'],
      env: { ...process.env },
    });
    // The harness prints diagnostics and never audio, so surfacing them keeps a
    // failed acceptance run readable in CI.
    child.stderr.pipe(process.stderr);
    const harness = new Harness(child, createInterface({ input: child.stdout }));
    const started = new Promise<void>((resolve) => {
      harness.ready = resolve;
    });
    harness.lines.on('line', (line) => harness.receive(line));
    child.on('error', (error) => {
      while (harness.pending.length) harness.pending.shift()!.reject(error);
    });
    await started;
    return harness;
  }

  private receive(line: string): void {
    const message = JSON.parse(line) as Record<string, unknown>;
    if (message.ready === true) {
      this.ready?.();
      this.ready = null;
      return;
    }
    const waiter = this.pending.shift();
    if (!waiter) return;
    if (message.ok === true) waiter.resolve((message.result ?? {}) as Record<string, unknown>);
    else waiter.reject(new Error(String(message.error ?? 'wake-word harness failed')));
  }

  call<T = Record<string, unknown>>(request: Record<string, unknown>): Promise<T> {
    return new Promise<T>((resolve, reject) => {
      this.pending.push({ resolve: resolve as (value: Record<string, unknown>) => void, reject });
      this.child.stdin.write(`${JSON.stringify(request)}\n`);
    });
  }

  async stop(): Promise<void> {
    this.child.stdin.end();
    this.lines.close();
    this.child.kill();
  }
}

function encodePcm(samples: Float32Array): string {
  return Buffer.from(samples.buffer, samples.byteOffset, samples.byteLength).toString('base64');
}

function decodePcm(encoded: string): Float32Array {
  const bytes = Buffer.from(encoded, 'base64');
  return new Float32Array(bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.length));
}

function silence(ms: number): Float32Array {
  return new Float32Array((KWS_SAMPLE_RATE * ms) / 1_000);
}

/**
 * The engine ends an utterance from inside `observeLevel` without awaiting it,
 * exactly as a real microphone callback does — so pushing the last frame of
 * silence is not the same as the turn having been transcribed. Real Whisper
 * takes real seconds; wait for the observable result rather than for a guess.
 */
async function waitUntil(
  predicate: () => boolean,
  what: string,
  timeoutMs = 180_000,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (!predicate()) {
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}`);
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
}

async function blobPcmLength(blob: Blob): Promise<number> {
  // 44-byte RIFF header, then 16-bit mono samples.
  return (blob.size - 44) / 2;
}

describe.skipIf(!process.env.LITTLE_MONKEY_WAKE_WALKTHROUGH_E2E)(
  'wake-word acceptance walkthrough',
  () => {
    let harness: Harness;
    let wakeAndCommand: Float32Array;
    let unrelatedSpeech: Float32Array;

    beforeAll(async () => {
      harness = await Harness.start();
      const positive = await harness.call<{ pcm: string; sampleRate: number }>({
        op: 'fixture',
        name: '0.wav',
      });
      const negative = await harness.call<{ pcm: string; sampleRate: number }>({
        op: 'fixture',
        name: '1.wav',
      });
      expect(positive.sampleRate).toBe(KWS_SAMPLE_RATE);
      expect(negative.sampleRate).toBe(KWS_SAMPLE_RATE);
      wakeAndCommand = decodePcm(positive.pcm);
      unrelatedSpeech = decodePcm(negative.pcm);
    }, 120_000);

    afterAll(async () => {
      await harness?.stop();
    });

    it('walks the fifteen acceptance steps against the real runtime', async () => {
      const observed: TalkState[] = [];
      const transcribed: number[] = [];
      const submitted: Array<{ text: string; utteranceId: string }> = [];
      const played: string[] = [];
      let stopPlaybackCalls = 0;
      let cancelTurnCalls = 0;
      let clock = 0;

      // --- the microphone, minus the grant dialog -------------------------
      //
      // Deliberately the same shape as `useTalkSession`'s worklet handler: one
      // resampler, one ring, one bounded queue, and a wake session that exists
      // only between arm and disarm.
      const resampler = new StreamingLinearResampler(KWS_SAMPLE_RATE);
      let ring = new PcmRingBuffer(KWS_RING_SAMPLES);
      let queue = new BoundedPcmQueue(KWS_PENDING_SAMPLES);
      let microphoneOpen = false;
      let microphoneOpens = 0;
      let recording: Float32Array[] | null = null;
      let lastRecordedPcm: Float32Array | null = null;
      /** `startSample` is why a wake event lands in the right place in the ring:
       * sherpa counts from the moment the session armed, the ring counts from
       * the moment the microphone opened. Mirrors `useTalkSession`. */
      let wakeSession: { sessionId: string; startSample: number } | null = null;
      /** Playback is held open only while the walkthrough needs the session to
       * still be speaking, so a barge-in lands at an exact moment instead of
       * racing a real speaker. */
      let holdPlayback = true;
      let heldPlayback: Array<() => void> = [];
      const releasePlayback = () => {
        const held = heldPlayback;
        heldPlayback = [];
        for (const release of held) release();
      };

      const openMicrophone = () => {
        if (microphoneOpen) return;
        microphoneOpen = true;
        microphoneOpens += 1;
      };

      const ports: TalkPorts = {
        now: () => clock,
        startRecording: async (options) => {
          openMicrophone();
          const buffered = options?.afterSample === undefined
            ? new Float32Array()
            : ring.sliceFrom(options.afterSample);
          recording = buffered.length > 0 ? [buffered] : [];
        },
        stopRecording: async (): Promise<TalkRecording | null> => {
          const chunks = recording;
          recording = null;
          if (!chunks) return null;
          const total = chunks.reduce((sum, chunk) => sum + chunk.length, 0);
          if (total === 0) return null;
          const pcm = new Float32Array(total);
          let offset = 0;
          for (const chunk of chunks) {
            pcm.set(chunk, offset);
            offset += chunk.length;
          }
          lastRecordedPcm = pcm;
          const blob = pcm16WavBlob(pcm, KWS_SAMPLE_RATE);
          return { blob, mediaType: blob.type };
        },
        transcribe: async (capture) => {
          const pcm = lastRecordedPcm;
          expect(pcm).not.toBeNull();
          // The WAV the product would have uploaded and the PCM sent to the
          // harness are the same utterance; `pcm16WavBlob` has its own unit
          // coverage, so the walkthrough only checks they agree in length.
          expect(await blobPcmLength(capture.blob)).toBe(pcm!.length);
          transcribed.push(pcm!.length);
          const result = await harness.call<{ text: string }>({
            op: 'transcribe',
            pcm: encodePcm(pcm!),
          });
          return result.text;
        },
        submitTurn: async (text, utteranceId) => {
          submitted.push({ text, utteranceId });
        },
        cancelTurn: () => {
          cancelTurnCalls += 1;
        },
        synthesize: async (text) => ({ audioBase64: text, mediaType: 'audio/wav' }),
        play: async (audioBase64) => {
          played.push(audioBase64);
          if (!holdPlayback) return;
          await new Promise<void>((resolve) => {
            heldPlayback.push(resolve);
          });
        },
        stopPlayback: () => {
          stopPlaybackCalls += 1;
          releasePlayback();
        },
        recordMetric: () => undefined,
        armWakeWord: async () => {
          openMicrophone();
          const started = await harness.call<{ sessionId: string }>({
            op: 'arm',
            phrase: WAKE_PHRASE,
            sensitivity: 0.5,
          });
          queue = new BoundedPcmQueue(KWS_PENDING_SAMPLES);
          wakeSession = { sessionId: started.sessionId, startSample: ring.totalWritten };
        },
        disarmWakeWord: async () => {
          const armed = wakeSession;
          wakeSession = null;
          if (!armed) return;
          await harness.call({
            op: 'disarm',
            sessionId: armed.sessionId,
            droppedFrames: queue.droppedFrames,
          });
        },
      };

      const session = new TalkSession(ports, { mode: 'continuous', wakeWordEnabled: true });
      session.subscribe((snapshot) => {
        if (observed[observed.length - 1] !== snapshot.state) observed.push(snapshot.state);
      });

      /** One worklet frame, exactly as the production handler routes it. */
      const pushFrame = async (frame: Float32Array): Promise<void> => {
        const pcm = resampler.process(frame, KWS_SAMPLE_RATE);
        if (pcm.length === 0) return;
        ring.write(pcm);
        recording?.push(pcm.slice());
        clock += FRAME_MS;
        session.observeLevel(rmsOf(pcm));
        const armed = wakeSession;
        if (!armed) return;
        queue.enqueue(pcm);
        const pending = queue.take();
        if (pending.length === 0) return;
        const { detection } = await harness.call<{ detection: Detection | null }>({
          op: 'push',
          sessionId: armed.sessionId,
          pcm: encodePcm(pending),
        });
        if (!detection?.detected || wakeSession?.sessionId !== detection.sessionId) return;
        // Same order as production: stop forwarding, close the native
        // generation, and only then hand the absolute command start to the
        // state machine.
        wakeSession = null;
        await harness.call({
          op: 'disarm',
          sessionId: detection.sessionId,
          droppedFrames: queue.droppedFrames,
        });
        await session.onWakeDetected(armed.startSample + detection.keywordEndSample);
      };

      const speak = async (samples: Float32Array): Promise<void> => {
        for (let offset = 0; offset < samples.length; offset += FRAME_SAMPLES) {
          await pushFrame(samples.subarray(offset, offset + FRAME_SAMPLES));
        }
      };

      // 1. Fresh configuration: both switches off, and the default is accepted.
      const fresh = await harness.call<{ accepted: boolean; alwaysListening: boolean }>({
        op: 'configure',
      });
      expect(fresh.accepted).toBe(true);
      expect(fresh.alwaysListening).toBe(false);

      // 2. Enable the wake word.
      const wakeEnabled = await harness.call<{ accepted: boolean }>({
        op: 'configure',
        wakePhraseEnabled: true,
        phrase: WAKE_PHRASE,
        sensitivity: 50,
      });
      expect(wakeEnabled.accepted).toBe(true);

      // 3. Enable Always Listening — and confirm the two configurations the
      //    product must refuse rather than quietly accept.
      const always = await harness.call<{ accepted: boolean }>({
        op: 'configure',
        wakePhraseEnabled: true,
        alwaysListening: true,
        phrase: WAKE_PHRASE,
        sensitivity: 50,
      });
      expect(always.accepted).toBe(true);
      const withoutWake = await harness.call<{ accepted: boolean; refusal: string }>({
        op: 'configure',
        alwaysListening: true,
      });
      expect(withoutWake.accepted).toBe(false);
      expect(withoutWake.refusal).toContain('wake phrase');
      const hostedTranscription = await harness.call<{ accepted: boolean; refusal: string }>({
        op: 'configure',
        wakePhraseEnabled: true,
        alwaysListening: true,
        phrase: WAKE_PHRASE,
        transcriptionBackend: 'provider',
      });
      expect(hostedTranscription.accepted).toBe(false);
      expect(hostedTranscription.refusal).toContain('local Whisper');

      // 4. The microphone opens, and only then does the UI say "armed".
      await session.start();
      expect(microphoneOpen).toBe(true);
      expect(microphoneOpens).toBe(1);
      expect(session.snapshot().state).toBe<TalkState>('armed');
      expect(session.snapshot().awaitingWakeWord).toBe(true);
      expect(wakeSession).not.toBeNull();

      // 5. Silence: Whisper is not invoked and nothing is submitted.
      await speak(silence(2_000));
      expect(transcribed).toEqual([]);
      expect(submitted).toEqual([]);
      expect(session.snapshot().state).toBe<TalkState>('armed');
      const afterSilence = await harness.call<{ detections: number }>({ op: 'status' });
      expect(afterSilence.detections).toBe(0);

      // 6. An unrelated sentence: heard by the spotter, rejected by it, and
      //    still no transcription and no turn.
      await speak(unrelatedSpeech);
      await speak(silence(1_000));
      expect(transcribed).toEqual([]);
      expect(submitted).toEqual([]);
      expect(session.snapshot().state).toBe<TalkState>('armed');
      const afterUnrelated = await harness.call<{ detections: number }>({ op: 'status' });
      expect(afterUnrelated.detections).toBe(0);

      // Everything the microphone heard before the phrase, and what it cost:
      // nothing was transcribed and nothing was submitted. Captured here rather
      // than restated in the log, so the evidence is the counters themselves.
      const passiveSamplesBeforeWake = ring.totalWritten;
      const transcriptionsBeforeWake = transcribed.length;
      const turnsBeforeWake = submitted.length;
      expect(transcriptionsBeforeWake).toBe(0);
      expect(turnsBeforeWake).toBe(0);

      // 7. The wake phrase, then the question, on the same open microphone.
      await speak(wakeAndCommand);
      expect(observed).toContain<TalkState>('wake_detected');
      expect(observed).toContain<TalkState>('capturing_command');
      expect(microphoneOpens).toBe(1);
      const afterWake = await harness.call<{
        detections: number;
        averageDetectionLatencyMs: number | null;
      }>({ op: 'status' });
      expect(afterWake.detections).toBe(1);
      // Nineteen seconds into an armed session sherpa's timestamps are offsets
      // into a segment it restarted without reporting, so this keyword's exact
      // position is not provable and the command is anchored to the frame that
      // revealed it. A latency is recorded only where the position *was*
      // provable, so there is deliberately none here — the alternative is
      // publishing the anchor's zero as a measured trigger latency.
      expect(afterWake.averageDetectionLatencyMs).toBeNull();

      // The utterance ends the way a real one does: the operator stops talking.
      await speak(silence(1_500));
      await waitUntil(() => submitted.length === 1, 'the first turn to be submitted');

      // 8. Only the command was transcribed, and the wake phrase is not in it.
      expect(transcribed).toHaveLength(1);
      expect(transcribed[0]).toBeGreaterThan(0);
      expect(transcribed[0]).toBeLessThan(wakeAndCommand.length);
      expect(submitted).toHaveLength(1);
      const command = submitted[0].text.toLowerCase();
      expect(command).not.toContain(WAKE_PHRASE);
      // What the fixture actually says after "light up".
      expect(command).toMatch(/here|quarter|brothel/);

      // 9. Exactly one ordinary durable turn.
      expect(submitted).toHaveLength(1);
      expect(new Set(submitted.map((turn) => turn.utteranceId)).size).toBe(1);

      // 10. The answer is spoken.
      session.onAssistantDelta('The weather is clear. ', submitted[0].utteranceId);
      await waitUntil(() => played.length > 0, 'the answer to reach the speaker');
      expect(session.snapshot().state).toBe<TalkState>('speaking');

      // 11 & 12. Talking over the answer stops playback, asks the run to stop,
      //          and the interrupting sentence becomes the next turn without a
      //          second wake word.
      const playbackStopsBefore = stopPlaybackCalls;
      await speak(wakeAndCommand.subarray(0, KWS_SAMPLE_RATE));
      expect(stopPlaybackCalls).toBeGreaterThan(playbackStopsBefore);
      expect(cancelTurnCalls).toBe(1);
      expect(observed).toContain<TalkState>('interrupted');
      expect(session.snapshot().state).toBe<TalkState>('capturing_command');
      expect(wakeSession).toBeNull();

      await speak(silence(1_500));
      await waitUntil(() => submitted.length === 2, 'the interrupting sentence to be submitted');
      expect(transcribed).toHaveLength(2);

      // 13. The second turn settles and the session re-arms itself.
      session.onAssistantDelta('Understood.', submitted[1].utteranceId);
      session.onTurnFinished(submitted[1].utteranceId);
      holdPlayback = false;
      releasePlayback();
      await waitUntil(() => session.snapshot().state === 'armed', 'the session to re-arm');
      expect(observed).toContain<TalkState>('rearming');
      expect(wakeSession).not.toBeNull();
      expect(microphoneOpens).toBe(1);

      // 14. Always Listening off — accepted by the operator's own validator,
      //     and the wake word with it. What a *running* surface does when this
      //     save lands is the hook's, and `useTalkSession.test.tsx` asserts it
      //     on the real hook: the microphone closes on the event alone.
      const disabled = await harness.call<{ accepted: boolean; alwaysListening: boolean }>({
        op: 'configure',
        wakePhraseEnabled: false,
        alwaysListening: false,
      });
      expect(disabled.accepted).toBe(true);
      expect(disabled.alwaysListening).toBe(false);

      // 15. Stopping ends the native session, and the runtime confirms it is no
      //      longer accepting audio.
      //
      //      Not the microphone. The engine has never held one — `TalkPorts` has
      //      no close, because in the product the devices belong to
      //      `useTalkSession`, which releases them in `releaseDevices`. An
      //      earlier version of this step called the stub's own
      //      `closeMicrophone()` here and then asserted the microphone had
      //      closed, which asserted this file's arithmetic and nothing about the
      //      product. The two claims that step is really making are the hook's,
      //      and both are made against the real hook in
      //      `useTalkSession.test.tsx`: turning Always Listening off closes the
      //      microphone with nobody calling `stop()`, and disabling the surface
      //      does the same.
      await session.stop();
      expect(wakeSession).toBeNull();
      expect(session.snapshot().state).toBe<TalkState>('off');
      const stopped = await harness.call<{ acceptingAudio: boolean; detections: number }>({
        op: 'status',
      });
      expect(stopped.acceptingAudio).toBe(false);
      expect(stopped.detections).toBe(1);

      // Evidence for the acceptance record. Durations, counts and state names
      // only: printing a transcript here would put the thing the feature
      // promises never to keep into a CI log.
      console.info(
        [
          'wake-word acceptance walkthrough',
          `  states: ${observed.join(' -> ')}`,
          `  passive audio pushed before wake: ${passiveSamplesBeforeWake / KWS_SAMPLE_RATE}s`,
          `  before wake — transcriptions: ${transcriptionsBeforeWake}, turns: ${turnsBeforeWake}`,
          `  transcriptions: ${transcribed.length} (${transcribed.join(', ')} samples)`,
          `  turns submitted: ${submitted.length}`,
          `  wake events: ${stopped.detections}`,
          `  playback chunks: ${played.length}, stopPlayback: ${stopPlaybackCalls}, cancelTurn: ${cancelTurnCalls}`,
          `  microphone opens: ${microphoneOpens} (closing is useTalkSession's; see its own tests)`,
          `  ring window: ${ring.totalWritten} samples written`,
        ].join('\n'),
      );
    }, 600_000);
  },
);
