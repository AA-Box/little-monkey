/**
 * Who gets asked, in what order, and what the caller is told.
 *
 * The order is the whole subject, and it has been wrong in both directions.
 * Asking WebKit alone told the operator "not allowed by the user agent" while
 * macOS had never been asked at all. Asking macOS first fixed that and broke
 * something worse: WebKit denies a capture request carrying no user activation
 * once that origin has been refused once, and the IPC round trip to macOS was
 * enough to lose the activation the press supplied. The microphone was granted,
 * the app said it was granted, and every request was refused anyway.
 *
 * So the webview goes first, inside the press, and macOS is asked only about a
 * refusal that already happened.
 */
import { afterEach, describe, expect, it, vi } from "vitest";

const invoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invoke(...args) }));
vi.mock("@tauri-apps/api/event", () => ({ listen: () => Promise.resolve(() => undefined) }));

import { MicrophoneBlockedError, openMicrophone } from "./microphoneAccess";

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
  it("asks the webview first, and does not spend the press on an IPC round trip", async () => {
    getUserMedia.mockResolvedValue(STREAM);

    expect(await openMicrophone({ audio: true })).toBe(STREAM);
    // The await this replaced cost the user activation the request needed:
    // WebKit denies an unprivileged request against an origin it has refused
    // before, without prompting and without consulting the app.
    expect(invoke).not.toHaveBeenCalled();
  });

  it("asks the operating system only once the webview has refused", async () => {
    getUserMedia.mockRejectedValue(notAllowed());
    invoke.mockResolvedValue("granted");

    const reason = await openMicrophone({ audio: true }).catch((error) => error);
    expect(invoke).toHaveBeenCalledWith("microphone_request_access");
    // macOS is content, so this refusal is WebKit's own and no permission
    // screen will change it.
    expect(reason).toBeInstanceOf(MicrophoneBlockedError);
    expect(reason.block).toBe("webviewDenied");
    expect(reason.message).toMatch(/restart/i);
  });

  it("passes other failures through untouched", async () => {
    // "No microphone is plugged in" is not a permission problem, and dressing
    // it as one would send the operator to a setting that is already correct.
    const missing = new Error("Requested device not found");
    missing.name = "NotFoundError";
    getUserMedia.mockRejectedValue(missing);

    const reason = await openMicrophone({ audio: true }).catch((error) => error);
    expect(reason).toBe(missing);
    expect(reason).not.toBeInstanceOf(MicrophoneBlockedError);
    expect(invoke).not.toHaveBeenCalled();
  });

  it("says nothing about macOS on a platform with nothing to ask", async () => {
    // Linux, or a shell predating the command. The webview's refusal is the
    // whole truth there, and inventing a system permission to blame would send
    // the operator to a screen that does not exist.
    const refusal = notAllowed();
    getUserMedia.mockRejectedValue(refusal);
    invoke.mockRejectedValue(new Error("command microphone_request_access not found"));

    const reason = await openMicrophone({ audio: true }).catch((error) => error);
    expect(reason).toBe(refusal);
  });
});

describe("a refusal macOS recorded", () => {
  /** The webview refuses `first` times, then succeeds. */
  function webviewRefuses(first: number) {
    let attempts = 0;
    getUserMedia.mockImplementation(async () => {
      attempts += 1;
      if (attempts <= first) throw notAllowed();
      return STREAM;
    });
  }

  it("has the system forget it and ask again, then opens the microphone", async () => {
    webviewRefuses(1);
    invoke.mockImplementation(async (command: string) =>
      command === "microphone_request_access" ? "denied" : "granted");

    expect(await openMicrophone({ audio: true })).toBe(STREAM);
    expect(invoke).toHaveBeenCalledWith("microphone_ask_again");
  });

  it("asks once, not until the answer changes", async () => {
    webviewRefuses(99);
    invoke.mockResolvedValue("denied");

    const reason = await openMicrophone({ audio: true }).catch((error) => error);
    expect(reason).toBeInstanceOf(MicrophoneBlockedError);
    expect(reason.block).toBe("denied");
    // A second no is a decision. Resetting again would be nagging with extra
    // steps, and it is the behaviour the permission exists to prevent.
    expect(invoke.mock.calls.filter(([command]) => command === "microphone_ask_again")).toHaveLength(1);
  });

  it("asks for a press rather than blaming the webview, right after a grant", async () => {
    // Answering the operating system's dialog is not a press, so the retry
    // carries no activation of its own and WebKit may still refuse it. What is
    // needed is one more press, and saying "restart" would be a lie.
    webviewRefuses(99);
    invoke.mockImplementation(async (command: string) =>
      command === "microphone_request_access" ? "denied" : "granted");

    const reason = await openMicrophone({ audio: true }).catch((error) => error);
    expect(reason.block).toBe("justGranted");
    expect(reason.message).toMatch(/press talk again/i);
  });

  it("keeps the recorded answer where there is no decision to reset", async () => {
    webviewRefuses(99);
    invoke.mockImplementation(async (command: string) => {
      if (command === "microphone_request_access") return "denied";
      throw new Error("Resetting the microphone permission is a macOS feature");
    });

    const reason = await openMicrophone({ audio: true }).catch((error) => error);
    expect(reason).toBeInstanceOf(MicrophoneBlockedError);
    expect(reason.block).toBe("denied");
  });
});
