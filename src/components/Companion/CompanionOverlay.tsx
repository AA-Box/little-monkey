import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Camera, Clipboard, Mic, Octagon, Send, Volume2, X } from "lucide-react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

import {
  blobToBase64,
  companionClient,
  formatSpeakerTranscript,
  type CaptureGrant,
  type CaptureKind,
} from "../../lib/companionClient";
import { wrapUntrustedContent } from "../../lib/untrustedContent";
import { Button, IconButton } from "../ui";
import { errorMessage } from "../../lib/errors";

const GRANT_LIFETIME_MS = 15 * 60_000;

interface DesktopControlSession {
  sessionId: string;
  allowedApplications: string[];
  active: boolean;
  paused: boolean;
}

function message(error: unknown): string {
  return errorMessage(error);
}

export function CompanionOverlay() {
  const [text, setText] = useState("");
  const [imageDataUrl, setImageDataUrl] = useState<string | null>(null);
  const [grants, setGrants] = useState<CaptureGrant[]>([]);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [recording, setRecording] = useState<"microphone" | "meeting" | null>(null);
  // Hands-free: a finalized microphone utterance is sent as its own turn
  // instead of landing in the box for the operator to read first. Off by
  // default — speaking into a machine that acts without showing you what it
  // heard is a thing to opt into, not a default.
  const [handsFree, setHandsFree] = useState(false);
  const [desktopSessions, setDesktopSessions] = useState<DesktopControlSession[]>([]);
  const recorderRef = useRef<MediaRecorder | null>(null);
  const streamRef = useRef<MediaStream | null>(null);
  const chunksRef = useRef<Blob[]>([]);

  const activeKinds = useMemo(
    () => new Set(grants.filter((grant) => grant.active && grant.expiresAtMs > Date.now()).map((grant) => grant.kind)),
    [grants],
  );

  const refreshGrants = useCallback(async () => {
    setGrants(await companionClient.grants());
  }, []);

  useEffect(() => {
    void refreshGrants().catch((reason) => setError(message(reason)));
    void invoke<DesktopControlSession[]>("desktop_control_sessions")
      .then((sessions) => setDesktopSessions(sessions))
      .catch(() => undefined);
    let disposed = false;
    let unlisten: (() => void) | null = null;
    let unlistenDesktop: (() => void) | null = null;
    void companionClient.onEmergencyStop(() => {
      if (disposed) return;
      recorderRef.current?.stop();
      streamRef.current?.getTracks().forEach((track) => track.stop());
      recorderRef.current = null;
      streamRef.current = null;
      setRecording(null);
      void refreshGrants();
    }).then((cleanup) => {
      if (disposed) cleanup();
      else unlisten = cleanup;
    });
    void listen<DesktopControlSession | DesktopControlSession[]>("desktop-control://session-state", (event) => {
      if (disposed) return;
      setDesktopSessions(Array.isArray(event.payload) ? event.payload : [event.payload]);
    }).then((cleanup) => {
      if (disposed) cleanup();
      else unlistenDesktop = cleanup;
    }).catch((reason) => setError(message(reason)));
    return () => {
      disposed = true;
      unlisten?.();
      unlistenDesktop?.();
      streamRef.current?.getTracks().forEach((track) => track.stop());
    };
  }, [refreshGrants]);

  const ensureGrant = useCallback(async (kind: CaptureKind): Promise<CaptureGrant> => {
    const current = grants.find(
      (grant) => grant.kind === kind && grant.active && grant.expiresAtMs > Date.now(),
    );
    if (current) return current;
    const grant = await companionClient.grant(kind, GRANT_LIFETIME_MS, "companion-overlay");
    setGrants((items) => [...items.filter((item) => item.grantId !== grant.grantId), grant]);
    return grant;
  }, [grants]);

  const submit = useCallback(async () => {
    const value = text.trim() || (imageDataUrl ? "Analyze this explicit screen capture." : "");
    if (!value) return;
    setBusy("submit");
    setError(null);
    try {
      const grant = await ensureGrant("text");
      await companionClient.captureText(grant.grantId, value);
      await companionClient.submitOverlay(value, imageDataUrl ? "screen" : "text", imageDataUrl);
    } catch (reason) {
      setError(message(reason));
    } finally {
      setBusy(null);
    }
  }, [ensureGrant, imageDataUrl, text]);

  const paste = useCallback(async () => {
    setError(null);
    try {
      const value = await navigator.clipboard.readText();
      if (!value.trim()) throw new Error("The clipboard has no text.");
      setText((current) => current ? `${current}\n${value}` : value);
    } catch (reason) {
      setError(message(reason));
    }
  }, []);

  const captureScreen = useCallback(async () => {
    setBusy("screen");
    setError(null);
    try {
      const grant = await ensureGrant("screen");
      const artifact = await companionClient.captureScreen(grant.grantId);
      setImageDataUrl(await companionClient.imageDataUrl(artifact.blob.id, artifact.mediaType));
      if (!text.trim()) setText("Analyze this explicit screen capture.");
    } catch (reason) {
      setError(message(reason));
    } finally {
      setBusy(null);
    }
  }, [ensureGrant, text]);

  const stopRecording = useCallback(() => {
    if (recorderRef.current?.state === "recording") recorderRef.current.stop();
  }, []);

  const startRecording = useCallback(async (kind: "microphone" | "meeting") => {
    if (recording || busy) return;
    setError(null);
    try {
      const grant = await ensureGrant(kind);
      const stream = await navigator.mediaDevices.getUserMedia({
        audio: { echoCancellation: true, noiseSuppression: true },
        video: false,
      });
      const preferred = ["audio/webm;codecs=opus", "audio/webm"].find((kind) => MediaRecorder.isTypeSupported(kind));
      const recorder = preferred ? new MediaRecorder(stream, { mimeType: preferred }) : new MediaRecorder(stream);
      streamRef.current = stream;
      recorderRef.current = recorder;
      chunksRef.current = [];
      recorder.ondataavailable = (event) => {
        if (event.data.size > 0) chunksRef.current.push(event.data);
      };
      recorder.onerror = () => setError("Microphone recording failed.");
      recorder.onstop = () => {
        const mediaType = recorder.mimeType || "audio/webm";
        const audio = new Blob(chunksRef.current, { type: mediaType });
        stream.getTracks().forEach((track) => track.stop());
        recorderRef.current = null;
        streamRef.current = null;
        setRecording(null);
        if (audio.size === 0) return;
        const jobId = `${kind}-${crypto.randomUUID()}`;
        setBusy(jobId);
        void blobToBase64(audio)
          .then((audioBase64) => companionClient.transcribeAudio(grant.grantId, jobId, audioBase64, mediaType))
          .then((result) => {
            if (kind === "meeting") {
              const transcript = wrapUntrustedContent("meeting transcript", formatSpeakerTranscript(result));
              const request = [
                "Create concise meeting notes, decisions, open questions, and clearly assigned action items from this transcript. Do not invent speakers, owners, or deadlines.",
                transcript,
              ].join("\n\n");
              setText((current) => current ? `${current}\n\n${request}` : request);
            } else if (handsFree && result.text.trim()) {
              // The recognition job id, minted before the audio was sent, is
              // this utterance's stable identity. Reusing it is what stops a
              // resubmitted turn from becoming a second run — see
              // `ConversationSource::Voice`.
              return companionClient.submitOverlay(result.text.trim(), "voice", null, jobId);
            } else {
              setText((current) => current ? `${current}\n${result.text}` : result.text);
            }
            return undefined;
          })
          .catch((reason) => setError(message(reason)))
          .finally(() => setBusy(null));
      };
      recorder.start(250);
      setRecording(kind);
    } catch (reason) {
      streamRef.current?.getTracks().forEach((track) => track.stop());
      setRecording(null);
      setError(message(reason));
    }
  }, [busy, ensureGrant, handsFree, recording]);

  const emergencyStop = useCallback(async () => {
    recorderRef.current?.stop();
    streamRef.current?.getTracks().forEach((track) => track.stop());
    setRecording(null);
    setBusy("stop");
    try {
      await companionClient.emergencyStop();
      await invoke("desktop_control_emergency_stop");
      setDesktopSessions([]);
      await refreshGrants();
    } catch (reason) {
      setError(message(reason));
    } finally {
      setBusy(null);
    }
  }, [refreshGrants]);

  const pauseDesktopSession = useCallback(async (session: DesktopControlSession) => {
    try {
      await invoke("desktop_control_pause_session", { sessionId: session.sessionId, paused: !session.paused });
    } catch (reason) {
      setError(message(reason));
    }
  }, []);

  const stopDesktopSession = useCallback(async (session: DesktopControlSession) => {
    try {
      await invoke("desktop_control_stop_session", { sessionId: session.sessionId });
    } catch (reason) {
      setError(message(reason));
    }
  }, []);

  const speak = useCallback(async () => {
    if (!text.trim()) return;
    const jobId = `speech-${crypto.randomUUID()}`;
    setBusy(jobId);
    setError(null);
    try {
      await companionClient.speak(jobId, text.trim());
    } catch (reason) {
      setError(message(reason));
    } finally {
      setBusy(null);
    }
  }, [text]);

  return (
    <main className="flex h-screen flex-col overflow-hidden bg-background text-foreground">
      <header
        data-tauri-drag-region
        className="flex h-11 shrink-0 items-center justify-between border-b border-border bg-surface px-3"
      >
        <div data-tauri-drag-region className="flex items-center gap-2 text-sm font-semibold">
          <span className={`h-2 w-2 rounded-full ${recording ? "animate-pulse bg-danger" : "bg-success"}`} />
          Little Monkey Companion
        </div>
        <IconButton size="sm" aria-label="Close companion" onClick={() => void companionClient.hideOverlay()}>
          <X size={15} />
        </IconButton>
      </header>

      {desktopSessions.some((session) => session.active) && (
        <section className="flex shrink-0 flex-col gap-2 border-b border-warning/50 bg-warning/10 px-3 py-2">
          <p className="text-xs font-semibold text-foreground">
            Little Monkey is controlling {desktopSessions.filter((session) => session.active).map((session) => session.allowedApplications.join(", ")).join("; ")}
          </p>
          <div className="flex flex-wrap gap-2">
            {desktopSessions.filter((session) => session.active).map((session) => (
              <span key={session.sessionId} className="flex items-center gap-1">
                <Button size="sm" variant="secondary" onClick={() => void pauseDesktopSession(session)}>
                  {session.paused ? "Resume" : "Pause"}
                </Button>
                <Button size="sm" variant="danger" onClick={() => void stopDesktopSession(session)}>Stop</Button>
              </span>
            ))}
            <Button size="sm" variant="danger" onClick={() => void emergencyStop()}><Octagon size={13} />Emergency stop</Button>
          </div>
        </section>
      )}

      <section className="flex min-h-0 flex-1 flex-col gap-3 overflow-y-auto p-4">
        <div className="rounded-lg border border-border bg-surface p-3">
          <p className="text-xs font-medium text-muted">Explicit context</p>
          <textarea
            autoFocus
            value={text}
            onChange={(event) => setText(event.target.value)}
            placeholder="Type or paste only the context you want to share…"
            className="mt-2 min-h-32 w-full resize-y rounded-md border border-border bg-background p-2 text-sm outline-none focus:ring-1 focus:ring-accent"
          />
          {imageDataUrl && (
            <div className="relative mt-2 overflow-hidden rounded-md border border-border">
              <img src={imageDataUrl} alt="Explicit screen context" className="max-h-44 w-full object-contain" />
              <button
                type="button"
                onClick={() => setImageDataUrl(null)}
                className="absolute right-1 top-1 rounded bg-black/70 px-2 py-1 text-xs text-white"
              >
                Remove
              </button>
            </div>
          )}
        </div>

        {recording && (
          <div role="status" className="flex items-center justify-between rounded-lg border border-danger/50 bg-danger/10 px-3 py-2 text-sm">
            <span className="font-medium text-danger">{recording === "meeting" ? "Meeting recording" : "Microphone recording"} is active</span>
            <Button size="sm" variant="danger" onClick={stopRecording}>Stop</Button>
          </div>
        )}

        <div className="grid grid-cols-2 gap-2">
          <Button size="sm" onClick={() => void paste()}><Clipboard size={14} />Paste</Button>
          <Button size="sm" disabled={busy !== null} onClick={() => void captureScreen()}><Camera size={14} />Screen area</Button>
          <Button
            size="sm"
            variant={recording === "microphone" ? "danger" : "secondary"}
            disabled={(busy !== null && recording !== "microphone") || recording === "meeting"}
            onPointerDown={() => void startRecording("microphone")}
            onPointerUp={stopRecording}
            onPointerCancel={stopRecording}
            onKeyDown={(event) => {
              if ((event.key === " " || event.key === "Enter") && !event.repeat) void startRecording("microphone");
            }}
            onKeyUp={(event) => {
              if (event.key === " " || event.key === "Enter") stopRecording();
            }}
          >
            <Mic size={14} />{recording === "microphone" ? "Release to stop" : "Hold to talk"}
          </Button>
          <Button
            size="sm"
            variant={recording === "meeting" ? "danger" : "secondary"}
            disabled={(busy !== null && recording !== "meeting") || recording === "microphone"}
            onClick={() => recording === "meeting" ? stopRecording() : void startRecording("meeting")}
          >
            <Mic size={14} />{recording === "meeting" ? "Stop meeting" : "Record meeting"}
          </Button>
          <Button size="sm" disabled={!text.trim() || busy !== null} onClick={() => void speak()}><Volume2 size={14} />Read aloud</Button>
        </div>

        <label className="flex items-center gap-2 text-xs text-muted">
          <input
            type="checkbox"
            checked={handsFree}
            disabled={recording !== null}
            onChange={(event) => setHandsFree(event.target.checked)}
          />
          Send what I say straight to the chat, without showing it to me first
        </label>

        <p className="text-[11px] leading-relaxed text-faint">
          Active grants: {activeKinds.size === 0 ? "none" : [...activeKinds].join(", ")}. Grants expire after 15 minutes and are revoked on emergency stop or app exit.
        </p>
        {error && <p role="alert" className="rounded-md border border-danger/40 bg-danger/10 p-2 text-xs text-danger">{error}</p>}
      </section>

      <footer className="flex shrink-0 items-center gap-2 border-t border-border bg-surface p-3">
        <Button variant="danger" size="sm" disabled={busy === "stop"} onClick={() => void emergencyStop()}>
          <Octagon size={14} />Emergency stop
        </Button>
        <Button className="ml-auto" variant="primary" disabled={busy !== null || (!text.trim() && !imageDataUrl)} onClick={() => void submit()}>
          <Send size={15} />Use in chat
        </Button>
      </footer>
    </main>
  );
}
