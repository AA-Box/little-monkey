import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Camera, Clipboard, Mic, Octagon, Send, Volume2, X } from "lucide-react";

import {
  blobToBase64,
  companionClient,
  formatSpeakerTranscript,
  type CaptureGrant,
  type CaptureKind,
} from "../../lib/companionClient";
import { wrapUntrustedContent } from "../../lib/untrustedContent";
import { Button, IconButton } from "../ui";

const GRANT_LIFETIME_MS = 15 * 60_000;

function message(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

export function CompanionOverlay() {
  const [text, setText] = useState("");
  const [imageDataUrl, setImageDataUrl] = useState<string | null>(null);
  const [grants, setGrants] = useState<CaptureGrant[]>([]);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [recording, setRecording] = useState<"microphone" | "meeting" | null>(null);
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
    let disposed = false;
    let unlisten: (() => void) | null = null;
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
    return () => {
      disposed = true;
      unlisten?.();
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
            } else {
              setText((current) => current ? `${current}\n${result.text}` : result.text);
            }
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
  }, [busy, ensureGrant, recording]);

  const emergencyStop = useCallback(async () => {
    recorderRef.current?.stop();
    streamRef.current?.getTracks().forEach((track) => track.stop());
    setRecording(null);
    setBusy("stop");
    try {
      await companionClient.emergencyStop();
      await refreshGrants();
    } catch (reason) {
      setError(message(reason));
    } finally {
      setBusy(null);
    }
  }, [refreshGrants]);

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
