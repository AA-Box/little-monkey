import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type DictationPlatform = "macos" | "windows" | "unsupported";
export type DictationState = "idle" | "starting" | "listening" | "stopping" | "error";

export interface DictationLanguage {
  id: string;
  label: string;
}

export interface DictationCapabilities {
  supported: boolean;
  platform: DictationPlatform;
  engine: string;
  supportsPartialResults: boolean;
  supportsOnDevice: boolean;
  languages: DictationLanguage[];
}

export interface DictationPartialEvent {
  sessionId: string;
  text: string;
}

export interface DictationFinalEvent {
  sessionId: string;
  text: string;
}

export interface DictationStateEvent {
  sessionId: string;
  state: DictationState;
}

export interface DictationErrorEvent {
  sessionId: string;
  code: string;
  message: string;
}

export interface DictationStartOptions {
  language: string | null;
  requireOnDevice: boolean;
}

export interface DictationStartResult {
  sessionId: string;
}

const STATE_EVENT = "dictation://state";
const PARTIAL_EVENT = "dictation://partial";
const FINAL_EVENT = "dictation://final";
const ERROR_EVENT = "dictation://error";

export type DictationUnlisten = UnlistenFn;

export const dictationClient = {
  capabilities: () => invoke<DictationCapabilities>("dictation_capabilities"),
  start: (options: DictationStartOptions) =>
    invoke<DictationStartResult>("dictation_start", {
      language: options.language,
      requireOnDevice: options.requireOnDevice,
    }),
  stop: (sessionId: string) => invoke<void>("dictation_stop", { sessionId }),
  cancel: (sessionId: string) => invoke<void>("dictation_cancel", { sessionId }),
  onState: (handler: (event: DictationStateEvent) => void) =>
    listen<DictationStateEvent>(STATE_EVENT, (event) => handler(event.payload)),
  onPartial: (handler: (event: DictationPartialEvent) => void) =>
    listen<DictationPartialEvent>(PARTIAL_EVENT, (event) => handler(event.payload)),
  onFinal: (handler: (event: DictationFinalEvent) => void) =>
    listen<DictationFinalEvent>(FINAL_EVENT, (event) => handler(event.payload)),
  onError: (handler: (event: DictationErrorEvent) => void) =>
    listen<DictationErrorEvent>(ERROR_EVENT, (event) => handler(event.payload)),
};
