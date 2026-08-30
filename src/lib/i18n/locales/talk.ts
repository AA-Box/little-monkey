/**
 * Realtime Talk — English source of truth for the `AppMenu.talk` key
 * namespace. Spread into `en.ts` and, through it, into every other locale (see
 * `localeSync.test.ts`), where a real translation can override it.
 */
export const talkLocale: Record<string, string> = {
  "AppMenu.talk": "Talk",
  "AppMenu.groupTalk": "Talk",
  "ChatWindow.talkAriaLabel": "Talk",
  "ChatWindow.talkStopAriaLabel": "End Talk",
  "ChatWindow.talkAwaitingWakePhrase": "waiting for the wake phrase",
  "ChatWindow.talkLevelAriaLabel": "Microphone level",
  "ChatWindow.talkStateIdle": "Not listening",
  "ChatWindow.talkStateStarting": "Starting\u2026",
  "ChatWindow.talkStateListening": "Listening",
  "ChatWindow.talkStateTranscribing": "Transcribing",
  "ChatWindow.talkStateThinking": "Thinking",
  "ChatWindow.talkStateSpeaking": "Speaking",
  "ChatWindow.talkStateInterrupted": "Interrupted",
  "ChatWindow.talkStateError": "Something went wrong",
};
