/**
 * Who gets asked, in what order, and what the caller is told.
 *
 * The bug this file pins: Talk asked WebKit for the microphone and let WebKit
 * decide whether macOS was ever consulted. It was not — the TCC database had no
 * row at all — and the operator was shown a DOMException with no way to act on
 * it. Asking the OS first is what makes "the dialog appears" and "send them to
 * Settings" two distinguishable outcomes instead of one dead sentence.
 */
import { afterEach, describe, expect, it, vi } from "vitest";

const invoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invoke(...args) }));
vi.mock("@tauri-apps/api/event", () => ({ listen: () => Promise.resolve(() => undefined) }));

import { askForMicrophoneAgain, MicrophoneBlockedError, openMicrophone } from "./microphoneAccess";

const getUserMedia = vi.fn();
Object.defineProperty(navigator, "mediaDevices", {
  configurable: true,
  value: { getUserMedia: (...args: unknown[]) => getUserMedia(...args) },
});

function notAllowed(): Error {
  const refusal = new Error("The request is not allowed by the user agent or the platform");
  refusal.name = "NotAllowedError";
  return refusal;
}

afterEach(() => {
  invoke.mockReset();
  getUserMedia.mockReset();
});

const STREAM = { id: "stream-1" } as unknown as MediaStream;

describe("opening the microphone", () => {
  it("asks the operating system before it asks the webview", async () => {
    const order: string[] = [];
    invoke.mockImplementation(async () => { order.push("os"); return "granted"; });
    getUserMedia.mockImplementation(async () => { order.push("webview"); return STREAM; });

    expect(await openMicrophone({ audio: true })).toBe(STREAM);
    // The other order is the bug: WebKit refuses without a gesture, the refusal
    // never reaches the OS, and nothing is ever prompted.
    expect(order).toEqual(["os", "webview"]);
    expect(invoke).toHaveBeenCalledWith("microphone_request_access");
  });

  it("does not touch the microphone at all once the answer is no", async () => {
    invoke.mockResolvedValue("denied");
    await expect(openMicrophone({ audio: true })).rejects.toBeInstanceOf(MicrophoneBlockedError);
    // Calling anyway would cache a denial against this origin that only a
    // reload clears — a second failure mode earned for nothing.
    expect(getUserMedia).not.toHaveBeenCalled();
  });

  it("reports a refusal the operator can act on, not the one they cannot", async () => {
    invoke.mockResolvedValue("denied");
    const denied = await openMicrophone({ audio: true }).catch((reason) => reason);
    expect(denied).toBeInstanceOf(MicrophoneBlockedError);
    expect(denied.block).toBe("denied");

    invoke.mockResolvedValue("restricted");
    const restricted = await openMicrophone({ audio: true }).catch((reason) => reason);
    expect(restricted.block).toBe("restricted");
  });

  it("names the one case where Settings is the wrong advice", async () => {
    // The OS already says yes, so the pane shows a switch that is on. The
    // denial is cached in the webview from before the grant existed.
    invoke.mockResolvedValue("granted");
    getUserMedia.mockRejectedValue(notAllowed());

    const reason = await openMicrophone({ audio: true }).catch((error) => error);
    expect(reason).toBeInstanceOf(MicrophoneBlockedError);
    expect(reason.block).toBe("webviewDenied");
    expect(reason.message).toMatch(/restart/i);
  });

  it("still opens the microphone where there is nothing to ask", async () => {
    // Linux, or a shell predating the command. Refusing here would break every
    // platform that never needed a prompt.
    invoke.mockRejectedValue(new Error("command microphone_request_access not found"));
    getUserMedia.mockResolvedValue(STREAM);
    expect(await openMicrophone({ audio: true })).toBe(STREAM);
  });

  it("passes other failures through untouched", async () => {
    // "No microphone is plugged in" is not a permission problem, and dressing
    // it as one would send the operator to a setting that is already correct.
    invoke.mockResolvedValue("granted");
    const missing = new Error("Requested device not found");
    missing.name = "NotFoundError";
    getUserMedia.mockRejectedValue(missing);

    const reason = await openMicrophone({ audio: true }).catch((error) => error);
    expect(reason).toBe(missing);
    expect(reason).not.toBeInstanceOf(MicrophoneBlockedError);
  });
});

describe("asking again after a refusal", () => {
  it("has the operating system forget its answer, then asks", async () => {
    invoke.mockResolvedValue("granted");
    expect(await askForMicrophoneAgain()).toBe("granted");
    expect(invoke).toHaveBeenCalledWith("microphone_ask_again");
  });

  it("reports a second refusal rather than pretending it asked", async () => {
    // The dialog appeared and the answer was no again. Saying anything else
    // would send the operator back to a button that changes nothing.
    invoke.mockResolvedValue("denied");
    expect(await askForMicrophoneAgain()).toBe("denied");
  });

  it("surfaces platforms where there is no decision to reset", async () => {
    invoke.mockRejectedValue(new Error("Resetting the microphone permission is a macOS feature"));
    await expect(askForMicrophoneAgain()).rejects.toThrow(/macOS feature/);
  });
});
