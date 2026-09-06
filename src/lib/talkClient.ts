/**
 * The typed bridge for Talk's Rust-side state.
 *
 * Same shape as every other client in this directory: named commands with
 * project-native request/response types, never a generic invoke the UI composes
 * arguments for. Voice configuration itself rides `companionClient`'s
 * `CompanionConfig` — Talk did not get a second settings file.
 */

import { invoke } from '@tauri-apps/api/core';
import type { TranscriptionBackendKind } from './companionClient';

export interface TalkStatus {
  /** Whether the configured transcription backend can actually run. */
  configured: boolean;
  wakePhraseEnabled: boolean;
  alwaysListening: boolean;
  backend: TranscriptionBackendKind;
  activeJobs: number;
  /** Live microphone/meeting capture grants. Non-zero means something can hear. */
  activeMicrophoneGrants: number;
  wakeWord?: WakeWordRuntimeStatus;
}

export interface WakeWordRuntimeStatus {
  backend: string;
  local: boolean;
  runtimeVersion: string;
  modelId: string;
  modelLicense: string;
  available: boolean;
  loaded: boolean;
  acceptingAudio: boolean;
  sampleRate: number;
  modelBytes: number;
  modelMemoryBytes: number | null;
  idleCpuPercent: number | null;
  averageInferenceMs: number | null;
  averageDetectionLatencyMs: number | null;
  detections: number;
  droppedFrames: number;
  lastError: string | null;
}

export interface WakeWordSessionStarted {
  sessionId: string;
  status: WakeWordRuntimeStatus;
}

export interface WakeWordDetection {
  detected: true;
  sessionId: string;
  keywordEndSample: number;
  inferenceMs: number;
}

/** One turn's bounded latency sample. Never carries a transcript or audio. */
export interface TalkMetric {
  createdAtMs: number;
  speechDetectionMs: number | null;
  sttMs: number | null;
  modelFirstTokenMs: number | null;
  ttsFirstAudioMs: number | null;
  endToEndMs: number | null;
  interrupted: boolean;
  fallback: boolean;
}

export interface TalkMetricsSnapshot {
  metrics: TalkMetric[];
  interruptCount: number;
  fallbackCount: number;
}

export interface SpeechAudioResult {
  jobId: string;
  mediaType: string;
  audioBase64: string;
}

/** What Talk's own transcription returns: the text, and nothing kept. */
export interface TalkTranscript {
  jobId: string;
  text: string;
}

/** One speech model that can be chosen, and what it would cost to install. */
export interface TranscriptionModel {
  id: string;
  label: string;
  bytes: number;
  installed: boolean;
}

/** One language transcription can be pinned to. Whisper's own table. */
export interface TranscriptionLanguage {
  id: string;
  label: string;
}

export const talkClient = {
  status: () => invoke<TalkStatus>('m7_talk_status'),
  languages: () => invoke<TranscriptionLanguage[]>('m7_transcription_languages'),
  models: () => invoke<TranscriptionModel[]>('m7_transcription_models'),
  /** Fetch and verify one model. Resolves when it is ready to transcribe. */
  installModel: (modelId: string) =>
    invoke<void>('m7_transcription_model_install', { modelId }),
  metrics: () => invoke<TalkMetricsSnapshot>('m7_talk_metrics'),
  recordMetric: (metric: TalkMetric) =>
    invoke<TalkMetricsSnapshot>('m7_talk_metric_record', { metric }),
  clearMetrics: () => invoke<TalkMetricsSnapshot>('m7_talk_metrics_clear'),
  wakeWordStatus: () => invoke<WakeWordRuntimeStatus>('m7_wake_word_status'),
  wakeWordStart: (grantId: string) =>
    invoke<WakeWordSessionStarted>('m7_wake_word_start', { grantId }),
  wakeWordPush: (grantId: string, sessionId: string, samples: ArrayLike<number>) =>
    invoke<WakeWordDetection | null>('m7_wake_word_push', {
      grantId,
      sessionId,
      samples: Array.from(samples),
    }),
  wakeWordStop: (sessionId: string, droppedFrames = 0) =>
    invoke<boolean>('m7_wake_word_stop', { sessionId, droppedFrames }),
  /**
   * Transcribe one utterance for Talk.
   *
   * Deliberately not `m7_transcribe_audio`: that command publishes the
   * transcript — and, when the operator turned `saveRawAudio` on, the audio
   * itself — as artifacts. A spoken conversation is not a recording somebody
   * asked to keep. Passive wake audio never calls this command; this one holds
   * only the post-wake command bytes for the length of the call and publishes
   * nothing.
   */
  transcribe: (
    grantId: string,
    jobId: string,
    audioBase64: string,
    mediaType: string,
    /** What has already been said here, as vocabulary for the decoder. */
    context: string | null = null,
  ) =>
    invoke<TalkTranscript>('m7_talk_transcribe', {
      grantId,
      jobId,
      audioBase64,
      mediaType,
      context,
    }),
  /** Synthesize one chunk and hand back the bytes, rather than playing them on
   * this machine's default output — Talk chooses its own device. */
  synthesize: (jobId: string, text: string) =>
    invoke<SpeechAudioResult>('m7_tts_synthesize', { jobId, text }),
  cancelJob: (jobId: string) => invoke<boolean>('m7_job_cancel', { jobId }),
};

/** Median and worst case of a metric across the kept samples. */
export function latencySummary(
  metrics: readonly TalkMetric[],
  field: Exclude<keyof TalkMetric, 'createdAtMs' | 'interrupted' | 'fallback'>,
): { median: number; worst: number; samples: number } | null {
  const values = metrics
    .map((metric) => metric[field])
    .filter((value): value is number => typeof value === 'number')
    .sort((left, right) => left - right);
  if (values.length === 0) return null;
  return {
    median: values[Math.floor((values.length - 1) / 2)],
    worst: values[values.length - 1],
    samples: values.length,
  };
}
