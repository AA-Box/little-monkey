/**
 * Talk — English source of truth for every string the voice surface uses.
 *
 * Talk lives in the chat composer now; the standalone page it used to have is
 * gone. Strings the page owned moved here rather than being dropped, including
 * the realtime engine's state labels, which the page had hardcoded in English
 * and which would otherwise have arrived untranslated in a translated composer.
 *
 * Spread into `en.ts` and, through it, into every other locale (see
 * `localeSync.test.ts`), where a real translation can override it.
 */
export const talkLocale: Record<string, string> = {
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
  "ChatWindow.talkStateWakeDetected": "Wake word detected",
  "ChatWindow.talkStateRearming": "Rearming wake word\u2026",
  "ChatWindow.talkHoldToTalk": "Hold to talk",
  "ChatWindow.talkReleaseToSend": "Release to send",
  "ChatWindow.talkStopAnswerAriaLabel": "Stop the answer",
  "ChatWindow.talkTryAgain": "Try again",
  "ChatWindow.talkMicrophoneBlocked": "Little Monkey does not have permission to use the microphone.",
  "ChatWindow.talkMicrophoneWebviewBlocked": "The system allows Little Monkey to use the microphone, but this window was refused. Restart Little Monkey.",
  "ChatWindow.talkMicrophoneJustGranted": "macOS has granted the microphone. Little Monkey has to restart before this window can use it.",
  "ChatWindow.talkRestartNow": "Restart now",
  "ChatWindow.talkOpenMicrophoneSettings": "Open microphone settings",
  "ChatWindow.talkAlwaysListeningOn": "Always listening is on: the microphone is active, local keyword spotting is armed, and full transcription starts only after the wake word.",
  "ChatWindow.talkAlwaysListeningArming": "Always listening is on, but Talk is not claiming wake readiness until the microphone and local keyword spotter are armed.",
  "ChatWindow.talkStopListening": "Stop listening",
  "TalkMenu.voiceOptionsAriaLabel": "Voice options",
  "TalkMenu.continuous": "Continuous \u2014 send each time I stop speaking",
  "TalkMenu.noTranscriptionBackend": "No transcription backend is configured, so nothing you say can be turned into text.",
  "TalkMenu.openVoiceSettings": "Open voice settings",
  "TalkMenu.moreVoiceSettings": "More voice settings",
  "RealtimeTalk.stateIdle": "Not connected",
  "RealtimeTalk.stateConnecting": "Connecting\u2026",
  "RealtimeTalk.stateReady": "Ready",
  "RealtimeTalk.stateListening": "Listening",
  "RealtimeTalk.stateResponding": "Responding",
  "RealtimeTalk.stateAwaitingApproval": "Waiting for approval",
  "RealtimeTalk.stateReconnecting": "Reconnecting\u2026",
  "RealtimeTalk.stateError": "Something went wrong",
  "RealtimeTalk.stateClosed": "Closed",
  "RealtimeTalk.start": "Start realtime Talk",
  "RealtimeTalk.stopResponse": "Stop response",
  "RealtimeTalk.retrySameProvider": "Retry same provider",
  "RealtimeTalk.beforeConnecting": "Before connecting",
  "RealtimeTalk.privacyNotice": "Audio from the selected microphone and the bounded conversation context are sent to OpenAI for this live session. Tool calls still pass through Little Monkey\u2019s existing permission, sandbox, workspace, network, and MCP controls. Audio is not stored by Little Monkey.",
  "RealtimeTalk.noProviderKey": "An OpenAI API key is not available in the OS keychain. Realtime Talk will not fall back to another provider.",
  "RealtimeTalk.awaitingApproval": "Little Monkey is running a tool through its normal boundary. If it needs a decision, the usual permission prompt appears.",
};
