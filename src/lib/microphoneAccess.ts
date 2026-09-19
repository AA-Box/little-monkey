/**
 * Opening the microphone, and knowing who said no.
 *
 * `getUserMedia` will raise the operating system's permission dialog by
 * itself — but only if WebKit lets the request get that far, and WebKit's
 * refusal is a message-less `NotAllowedError` that never reaches the OS at
 * all. It is indistinguishable between "never asked", "denied", and "no device
 * exists", it depends on gesture heuristics the app does not control, and it
 * arrives in the UI as a sentence nobody can act on:
 *
 *     The request is not allowed by the user agent or the platform in the
 *     current context, possibly because the user denied permission.
 *
 * So ask the OS first. On macOS `AVCaptureDevice requestAccessForMediaType:`
 * raises the real dialog when no decision exists yet, reports the recorded one
 * without any UI when it does, needs no user gesture, and is not subject to
 * WebKit's per-origin denial cache. Asking first also means `getUserMedia` is
 * never called in a state where WebKit would deny it — so no denial is ever
 * cached against this origin in the first place.
 *
 * Every microphone in the app goes through here. Four call sites had the same
 * shape, and fixing one would have left the others telling the same dead lie.
 */
import { invoke } from "@tauri-apps/api/core";

import { dictationClient, type DictationPermissionStatus } from "./dictationClient";

export type MicrophoneBlock = "denied" | "restricted" | "webviewDenied";

/** A refusal with somewhere to go, as opposed to a sentence to read. */
export class MicrophoneBlockedError extends Error {
  constructor(readonly block: MicrophoneBlock) {
    super(
      block === "webviewDenied"
        ? "The system allows Little Monkey to use the microphone, but this window was refused. Restart Little Monkey."
        : "Little Monkey does not have permission to use the microphone.",
    );
    this.name = "MicrophoneBlockedError";
  }
}

/** `getUserMedia`, having asked the operating system first. */
export async function openMicrophone(constraints: MediaStreamConstraints): Promise<MediaStream> {
  let status = await askTheOperatingSystem();
  // macOS asks once. After a refusal the request returns the recorded answer
  // without any dialog, which used to leave System Settings as the only way
  // back — a remedy the operator has to go and find. The app cannot grant
  // itself anything, but it can make the OS forget its answer, and a forgotten
  // answer is one it is willing to ask about again. So a refusal now costs one
  // more dialog rather than a trip through Settings.
  //
  // Only ever here, inside a press that asked for the microphone, and only
  // once per press: erasing a "no" nobody is waiting on is exactly the
  // behaviour the permission exists to prevent.
  if (status === "denied" || status === "restricted") {
    status = await invoke<DictationPermissionStatus>("microphone_ask_again").catch(() => status);
  }
  if (status === "denied" || status === "restricted") throw new MicrophoneBlockedError(status);
  try {
    return await navigator.mediaDevices.getUserMedia(constraints);
  } catch (reason) {
    // The OS says yes and the webview still says no: a denial cached against
    // this origin while the grant did not yet exist. Only a reload clears it,
    // so say that rather than sending the operator to a setting already on.
    if (status === "granted" && reason instanceof Error && reason.name === "NotAllowedError") {
      throw new MicrophoneBlockedError("webviewDenied");
    }
    throw reason;
  }
}

/** The recorded answer, or null on a platform with nothing to ask. */
async function askTheOperatingSystem(): Promise<DictationPermissionStatus | null> {
  try {
    return await invoke<DictationPermissionStatus>("microphone_request_access");
  } catch {
    // A platform with nothing to ask, or a shell that predates the command.
    // Refusing here would break the platforms that never needed asking.
    return null;
  }
}

/**
 * Open the OS pane where the decision can be changed.
 *
 * Reuses the command the dictation button already uses, which on macOS opens
 * `x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone`.
 * Not `openUrl` from the opener plugin: its URL scope allows only
 * mailto/tel/http/https, so that route needs a new capability and fails with
 * `ForbiddenUrl` without one.
 */
export const openMicrophoneSettings = () => dictationClient.openPermissionSettings("microphone");
