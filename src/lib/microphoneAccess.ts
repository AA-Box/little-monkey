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

export type MicrophoneBlock = "denied" | "restricted" | "webviewDenied" | "justGranted";

const MICROPHONE_BLOCK_MESSAGE: Record<MicrophoneBlock, string> = {
  denied: "Little Monkey does not have permission to use the microphone.",
  restricted: "Little Monkey does not have permission to use the microphone.",
  webviewDenied:
    "The system allows Little Monkey to use the microphone, but this window was refused. Restart Little Monkey.",
  justGranted:
    "macOS has granted the microphone. Little Monkey has to restart before this window can use it.",
};

/** A refusal with somewhere to go, as opposed to a sentence to read. */
export class MicrophoneBlockedError extends Error {
  constructor(readonly block: MicrophoneBlock, readonly detail: string | null = null) {
    super(MICROPHONE_BLOCK_MESSAGE[block]);
    this.name = "MicrophoneBlockedError";
  }
}

/**
 * What the webview was when it refused.
 *
 * WebKit's refusal is one error name for a dozen reasons, and each of them has
 * been chased by rebuilding the app to print one more fact. These are the
 * facts: the origin decides whether capture is allowed at all, the secure flag
 * decides whether the API is even real, the activation decides whether a
 * refusal recorded earlier is fatal, and WebKit occasionally says something
 * useful in the message nobody reads.
 */
function refusalDetail(reason: Error, activation: boolean | null): string {
  // Defensive to the point of dullness: this runs on a path that is already
  // failing, and a diagnostic that throws replaces the fault it was meant to
  // describe.
  const parts = [`name=${reason.name}`];
  if (reason.message) parts.push(`message=${reason.message}`);
  parts.push(`activation=${activation ?? "unknown"}`);
  try {
    parts.push(`origin=${globalThis.location?.origin ?? "unknown"}`);
    parts.push(`secure=${globalThis.isSecureContext ?? "unknown"}`);
  } catch {
    // Somewhere without a document. The error's own name still says something.
  }
  return parts.join(" ");
}

/**
 * `getUserMedia`, with the operating system asked only if it refuses.
 *
 * The webview goes first, and nothing is awaited before it. WebKit denies a
 * capture request outright — no delegate, no dialog, a bare `NotAllowedError`
 * — when the request carries no user activation and that origin has been
 * refused once before:
 *
 *     if (!request->isUserGesturePriviledged() && wasRequestDenied(...))
 *         return RequestAction::Deny;
 *
 * An `await` before the call is enough to lose the activation, so asking the
 * OS first — one IPC round trip — cost exactly the thing the request needed.
 * Every later press then failed on a refusal recorded minutes earlier, while
 * macOS said the microphone was granted and meant it.
 *
 * So: ask the webview inside the press. `getUserMedia` raises the OS dialog by
 * itself when no decision exists yet. Only once it refuses is macOS worth
 * asking, and then its answer says which refusal this is — a recorded "no",
 * which can be withdrawn and asked again, or WebKit's own, which cannot.
 */
export async function openMicrophone(constraints: MediaStreamConstraints): Promise<MediaStream> {
  // Read before the call, because the call is what spends it.
  const activation = navigator.userActivation ? navigator.userActivation.isActive : null;
  try {
    return await navigator.mediaDevices.getUserMedia(constraints);
  } catch (reason) {
    if (!(reason instanceof Error) || reason.name !== "NotAllowedError") throw reason;
    const detail = refusalDetail(reason, activation);
    const status = await askTheOperatingSystem();
    // A platform with nothing to ask cannot tell us anything the refusal did
    // not already say, and dressing it up as a macOS problem would send the
    // operator somewhere that does not exist.
    if (status === null) throw reason;
    // The OS is content, so the refusal was WebKit's own, and no permission
    // screen anywhere will change it.
    if (status !== "denied" && status !== "restricted") {
      throw new MicrophoneBlockedError("webviewDenied", detail);
    }
    // macOS asks once, and this process cannot see the answer withdrawn — see
    // `ask_for_microphone_in_a_fresh_process`. A child can ask, and the
    // operator answers it.
    const asked = await invoke<DictationPermissionStatus>("microphone_ask_again").catch(() => status);
    if (asked === "denied" || asked === "restricted") throw new MicrophoneBlockedError(asked);
    // Granted now, and this window still cannot use it. WebKit's capture
    // process reads the system's answer once, when it starts, and holds it for
    // its lifetime — the same per-process cache `AVCaptureDevice` keeps, and
    // measured the same way. The retry is worth one attempt, because a process
    // that never asked has nothing stale to hold; when it fails, only a
    // restart clears the answer it took before the grant existed.
    try {
      return await navigator.mediaDevices.getUserMedia(constraints);
    } catch (retry) {
      if (retry instanceof Error && retry.name === "NotAllowedError") {
        throw new MicrophoneBlockedError("justGranted", refusalDetail(retry, activation));
      }
      throw retry;
    }
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
