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

export const talkClient = {
  status: () => invoke<TalkStatus>('m7_talk_status'),
  metrics: () => invoke<TalkMetricsSnapshot>('m7_talk_metrics'),
  recordMetric: (metric: TalkMetric) =>
    invoke<TalkMetricsSnapshot>('m7_talk_metric_record', { metric }),
  clearMetrics: () => invoke<TalkMetricsSnapshot>('m7_talk_metrics_clear'),
  /**
   * Transcribe one utterance for Talk.
   *
   * Deliberately not `m7_transcribe_audio`: that command publishes the
   * transcript — and, when the operator turned `saveRawAudio` on, the audio
   * itself — as artifacts. A spoken conversation is not a recording somebody
   * asked to keep, and a wake-phrase fragment that turns out not to contain the
   * phrase must leave nothing behind at all. This one holds the bytes for the
   * length of the call and publishes nothing.
   */
  transcribe: (grantId: string, jobId: string, audioBase64: string, mediaType: string) =>
    invoke<TalkTranscript>('m7_talk_transcribe', { grantId, jobId, audioBase64, mediaType }),
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
