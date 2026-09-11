import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type CaptureKind = "text" | "file" | "window" | "screen" | "microphone" | "meeting";
export type TranscriptionBackendKind = "local_whisper" | "provider" | "executable_extension";
export type ImageEndpointKind = "comfy_ui" | "open_ai_compatible";

export interface CaptureGrant {
  grantId: string;
  kind: CaptureKind;
  applicationId: string | null;
  createdAtMs: number;
  expiresAtMs: number;
  active: boolean;
}

export interface ArtifactBlob {
  id: string;
  size: number;
}

export interface CompanionArtifact {
  blob: ArtifactBlob;
  mediaType: string;
  source: string;
  createdAtMs: number;
}

/** Which synthesizer speaks. `system` is this machine's own voice; an
 * executable extension is a sandboxed provider the operator installed. */
export type SpeechBackendKind = 'system' | 'executable_extension';
export type VoiceEngineKind = 'pipeline' | 'realtime';
export type RealtimeTurnDetectionKind = 'semantic_vad' | 'manual';

export interface VoiceConfig {
  /** Desktop Talk engine. Absent in an older saved/test config means pipeline. */
  engineKind?: VoiceEngineKind;
  realtimeProviderId?: 'openai';
  realtimeModel?: string;
  realtimeVoice?: string;
  realtimeTurnDetection?: RealtimeTurnDetectionKind;
  backend: TranscriptionBackendKind;
  /** @deprecated Kept only for compatibility with older persisted configs; built-in local Whisper ignores it. */
  whisperBinary: string | null;
  /** @deprecated Kept only for compatibility with older persisted configs; the model is app-managed. */
  whisperModel: string | null;
  providerId: string | null;
  providerModel: string;
  extensionId: string | null;
  extensionCapabilityId: string | null;
  language: string;
  /** Which built-in speech model transcribes — an id from `talkClient.models()`. */
  transcriptionModel: string;
  ttsVoice: string | null;
  ttsBackend: SpeechBackendKind;
  ttsExtensionId: string | null;
  ttsExtensionCapabilityId: string | null;
  /** Which backend serves a live phone call, which is a session rather than a
   * clip and is therefore chosen separately from `ttsBackend`. */
  realtimeBackend: SpeechBackendKind;
  realtimeExtensionId: string | null;
  realtimeExtensionCapabilityId: string | null;
  saveRawAudio: boolean;
  /** `MediaDeviceInfo.deviceId` of the chosen microphone, or null for the
   * system default. Talk and the companion overlay both honour it. */
  inputDeviceId: string | null;
  outputDeviceId: string | null;
  vadMinSpeechMs: number;
  vadSilenceMs: number;
  vadMaxUtteranceMs: number;
  /** Local wake-phrase detection. Off by default; the Rust side refuses to
   * enable it unless transcription runs on this machine. */
  wakePhraseEnabled: boolean;
  wakePhrase: string;
  /** Native local keyword spotter. Currently `sherpa_onnx`. */
  wakeWordBackend?: string;
  /** 0 is strictest, 100 is most sensitive. */
  wakeWordSensitivity?: number;
  /** Continuous local listening for the wake phrase. Requires the phrase. */
  alwaysListening: boolean;
  /** Native composer dictation locale; null means the operating-system default. */
  dictationLanguage: string | null;
  /** macOS only: refuse network-backed recognition when on-device is unavailable. */
  dictationRequireOnDevice: boolean;
}

export interface RealtimeVoiceConnectRequest {
  sessionId: string;
  providerId: 'openai';
  model: string;
  voice: string;
  turnDetection: RealtimeTurnDetectionKind;
  sdp: string;
  instructions: string;
  tools: Array<{ type: 'function'; name: string; description: string; parameters: object }>;
}

export interface RealtimeVoiceConnectResponse {
  sessionId: string;
  sdpAnswer: string;
  providerRequestId: string | null;
}

export interface RealtimeVoiceMediaBridge {
  protocolVersion: number;
  baseUrl: string;
  token: string;
}

export interface RealtimeVoiceStatus {
  providerId: 'openai';
  configured: boolean;
  activeSessions: number;
  endpoint: string;
}

export interface RealtimeVoiceMetric {
  createdAtMs: number;
  connectionMs: number | null;
  firstRecognizedSpeechMs: number | null;
  firstModelEventMs: number | null;
  firstAudioMs: number | null;
  endToEndMs: number | null;
  interrupted: boolean;
  reconnectCount: number;
  errorCode: string | null;
  toolRoundTripMs: number[];
  outputUnderruns: number;
  inputTokens: number;
  outputTokens: number;
}

export interface RealtimeVoiceMetricsSnapshot {
  metrics: RealtimeVoiceMetric[];
  interruptCount: number;
  reconnectCount: number;
}

export const realtimeVoiceClient = {
  connect: (request: RealtimeVoiceConnectRequest) =>
    invoke<RealtimeVoiceConnectResponse>('realtime_voice_connect', { request }),
  disconnect: (sessionId: string) =>
    invoke<void>('realtime_voice_disconnect', { sessionId }),
  status: () => invoke<RealtimeVoiceStatus>('realtime_voice_status'),
  mediaBridge: () => invoke<RealtimeVoiceMediaBridge>('realtime_voice_media_bridge'),
  recordMetric: (metric: RealtimeVoiceMetric) =>
    invoke<void>('realtime_voice_metric_record', { metric }),
  metrics: () => invoke<RealtimeVoiceMetricsSnapshot>('realtime_voice_metrics'),
  clearMetrics: () => invoke<void>('realtime_voice_metrics_clear'),
};

export interface ImageEndpointConfig {
  endpointId: string;
  label: string;
  kind: ImageEndpointKind;
  baseUrl: string;
  providerId: string | null;
  workflowTemplate: unknown | null;
  supportsEditing: boolean;
  enabled: boolean;
}

export interface CompanionConfig {
  schemaVersion: number;
  overlayShortcut: string;
  voice: VoiceConfig;
  imageEndpoints: ImageEndpointConfig[];
}

export interface TranscriptResult {
  jobId: string;
  text: string;
  segments: SpeakerSegment[];
  transcript: CompanionArtifact;
  rawAudio: CompanionArtifact | null;
  backend: string;
}

export interface SpeakerSegment {
  speaker: string;
  startMs: number | null;
  endMs: number | null;
  text: string;
  confidence: number | null;
}

export function formatSpeakerTranscript(result: TranscriptResult): string {
  if (result.segments.length === 0) return result.text;
  return result.segments.map((segment) => {
    const time = segment.startMs === null
      ? ""
      : ` [${Math.floor(segment.startMs / 60_000).toString().padStart(2, "0")}:${Math.floor((segment.startMs % 60_000) / 1_000).toString().padStart(2, "0")}]`;
    return `${segment.speaker}${time}: ${segment.text}`;
  }).join("\n");
}

export interface ImageGenerationRequest {
  jobId: string;
  endpointId: string;
  prompt: string;
  negativePrompt: string;
  model: string;
  width: number;
  height: number;
  steps: number;
  cfgScale: number;
  seed: number;
  sourceArtifactId: string | null;
}

export interface ImageGalleryEntry {
  entryId: string;
  artifactId: string;
  sizeBytes: number;
  mediaType: string;
  endpointId: string;
  endpointKind: ImageEndpointKind;
  model: string;
  prompt: string;
  negativePrompt: string;
  width: number;
  height: number;
  steps: number;
  cfgScale: number;
  seed: number;
  sourceArtifactId: string | null;
  createdAtMs: number;
}

export interface CompanionComposePayload {
  text: string;
  imageDataUrl: string | null;
  source: string;
  /** Set only for a finalized hands-free utterance. The chat sends that turn
   * immediately, as a voice turn, using this as its stable id — so a retried
   * submission collapses onto the run the first attempt made. Null means the
   * text lands in the composer for the operator to read and send. */
  utteranceId: string | null;
}

export interface ImageProgressPayload {
  jobId: string;
  phase: string;
  progress: number;
}

export const companionClient = {
  showOverlay: () => invoke<void>("m7_overlay_show"),
  hideOverlay: () => invoke<void>("m7_overlay_hide"),
  submitOverlay: (
    text: string,
    source: string,
    imageDataUrl: string | null = null,
    utteranceId: string | null = null,
  ) => invoke<void>("m7_overlay_submit", { text, source, imageDataUrl, utteranceId }),
  config: () => invoke<CompanionConfig>("m7_config_get"),
  saveConfig: (config: CompanionConfig) => invoke<CompanionConfig>("m7_config_save", { config }),
  grant: (kind: CaptureKind, lifetimeMs = 15 * 60_000, applicationId: string | null = null) =>
    invoke<CaptureGrant>("m7_capture_grant", { kind, lifetimeMs, applicationId }),
  revoke: (grantId: string) => invoke<boolean>("m7_capture_revoke", { grantId }),
  grants: () => invoke<CaptureGrant[]>("m7_capture_grants"),
  captureText: (grantId: string, text: string) =>
    invoke<CompanionArtifact>("m7_capture_text", { grantId, text }),
  captureFile: (grantId: string, path: string) =>
    invoke<CompanionArtifact>("m7_capture_file", { grantId, path }),
  captureScreen: (grantId: string) =>
    invoke<CompanionArtifact>("m7_capture_screen", { grantId }),
  transcribeFile: (grantId: string, jobId: string, path: string) =>
    invoke<TranscriptResult>("m7_transcribe_file", { grantId, jobId, path }),
  transcribeAudio: (
    grantId: string,
    jobId: string,
    audioBase64: string,
    mediaType: string,
  ) => invoke<TranscriptResult>("m7_transcribe_audio", { grantId, jobId, audioBase64, mediaType }),
  speak: (jobId: string, text: string) => invoke<void>("m7_tts_speak", { jobId, text }),
  cancelJob: (jobId: string) => invoke<boolean>("m7_job_cancel", { jobId }),
  generateImage: (request: ImageGenerationRequest) =>
    invoke<ImageGalleryEntry>("m7_image_generate", { request }),
  gallery: () => invoke<ImageGalleryEntry[]>("m7_image_gallery"),
  imageDataUrl: (artifactId: string, mediaType: string) =>
    invoke<string>("m7_image_data_url", { artifactId, mediaType }),
  insertImageInChat: (artifactId: string) =>
    invoke<void>("m7_image_insert_chat", { artifactId }),
  emergencyStop: () => invoke<{ revokedGrants: number; cancelledJobs: number }>("m7_emergency_stop"),
  onCompose: (listener: (payload: CompanionComposePayload) => void): Promise<UnlistenFn> =>
    listen<CompanionComposePayload>("m7://compose", (event) => listener(event.payload)),
  onImageProgress: (listener: (payload: ImageProgressPayload) => void): Promise<UnlistenFn> =>
    listen<ImageProgressPayload>("m7://image-progress", (event) => listener(event.payload)),
  onEmergencyStop: (listener: () => void): Promise<UnlistenFn> =>
    listen("m7://emergency-stop", () => listener()),
  /**
   * A saved configuration replaced the one in memory — somewhere, by somebody.
   *
   * Emitted by `m7_config_save` and carrying only the saving window's label:
   * re-read `config()` rather than trusting a payload. What this exists for is
   * Always Listening, which is switched off in Settings while the surface it
   * opened a microphone for is running on the configuration it read once.
   */
  onConfigChanged: (listener: () => void): Promise<UnlistenFn> =>
    listen("m7://config-changed", () => listener()),
};

export function blobToBase64(blob: Blob): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onerror = () => reject(reader.error ?? new Error("Could not read recorded audio"));
    reader.onload = () => {
      const result = String(reader.result ?? "");
      const comma = result.indexOf(",");
      if (comma < 0) reject(new Error("Recorded audio did not produce a data URL"));
      else resolve(result.slice(comma + 1));
    };
    reader.readAsDataURL(blob);
  });
}
