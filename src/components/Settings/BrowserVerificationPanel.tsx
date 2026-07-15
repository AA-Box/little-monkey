import { useEffect, useMemo, useState } from "react";
import { Camera, CheckCircle2, ExternalLink, Loader2, MousePointerClick, Square, TextCursorInput } from "lucide-react";
import { artifactDataUrl, readDurableArtifact } from "../../lib/durableArtifacts";
import {
  captureBrowserEvidence,
  clickBrowser,
  exactBrowserOrigin,
  inspectBrowser,
  isLoopbackBrowserUrl,
  listBrowserSessions,
  navigateBrowser,
  scrollBrowser,
  startBrowserSession,
  stopBrowserSession,
  type BrowserEvidence,
  type BrowserInspection,
  type BrowserSessionView,
  typeBrowserText,
} from "../../lib/browserVerification";
import { Button } from "../ui";

function message(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function ArtifactLink({ label, id }: { label: string; id?: string | null }) {
  if (!id) return null;
  return <span className="rounded-md border border-border bg-surface-2 px-2 py-1 font-mono text-[10px] text-muted">{label}: {id.slice(0, 14)}…</span>;
}

export function BrowserVerificationPanel() {
  const [url, setUrl] = useState("http://127.0.0.1:1420");
  const [allowLoopback, setAllowLoopback] = useState(false);
  const [session, setSession] = useState<BrowserSessionView | null>(null);
  const [selector, setSelector] = useState("button");
  const [text, setText] = useState("");
  const [inspection, setInspection] = useState<BrowserInspection | null>(null);
  const [evidence, setEvidence] = useState<BrowserEvidence | null>(null);
  const [screenshotUrl, setScreenshotUrl] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const origin = useMemo(() => {
    try { return exactBrowserOrigin(url); } catch { return null; }
  }, [url]);
  const local = useMemo(() => {
    try { return isLoopbackBrowserUrl(url); } catch { return false; }
  }, [url]);

  useEffect(() => {
    void listBrowserSessions().then((sessions) => setSession(sessions.find((candidate) => candidate.runId.startsWith("settings-browser-")) ?? null)).catch(() => undefined);
  }, []);

  useEffect(() => {
    let active = true;
    const screenshot = evidence?.screenshot;
    if (!screenshot) { setScreenshotUrl(null); return; }
    void readDurableArtifact(screenshot.id).then((content) => {
      if (active) setScreenshotUrl(artifactDataUrl("image/png", content.contentBase64));
    }).catch((cause) => active && setError(message(cause)));
    return () => { active = false; };
  }, [evidence?.screenshot?.id]);

  async function perform<T>(name: string, action: () => Promise<T>, onDone?: (value: T) => void) {
    setBusy(name);
    setError(null);
    try {
      const value = await action();
      onDone?.(value);
    } catch (cause) {
      setError(message(cause));
    } finally {
      setBusy(null);
    }
  }

  async function refreshEvidence() {
    if (!session) return;
    await perform("evidence", () => captureBrowserEvidence(session.sessionId), setEvidence);
  }

  return (
    <section className="flex flex-col gap-4" aria-labelledby="browser-verification-heading">
      <div>
        <h3 id="browser-verification-heading" className="text-sm font-semibold text-foreground">Isolated browser verification</h3>
        <p className="mt-1 text-xs leading-5 text-muted">Each run owns a disposable Chrome profile. Navigation is intercepted and DNS is re-checked before requests continue. File URLs, downloads, uploads, clipboard, extensions, and desktop control are unavailable.</p>
      </div>

      <div className="rounded-lg border border-border bg-surface p-3">
        <label className="text-xs font-medium text-foreground" htmlFor="browser-url">Granted URL</label>
        <div className="mt-2 flex flex-col gap-2 sm:flex-row">
          <input id="browser-url" value={url} onChange={(event) => setUrl(event.target.value)} className="min-w-0 flex-1 rounded-md border border-border bg-background px-3 py-2 text-sm text-foreground" />
          {!session ? (
            <Button variant="primary" disabled={!origin || busy !== null || (local && !allowLoopback)} onClick={() => void perform("start", () => startBrowserSession({ runId: `settings-browser-${crypto.randomUUID()}`, url, allowLoopback }), (next) => setSession(next))}>
              {busy === "start" ? <Loader2 size={14} className="animate-spin" /> : <ExternalLink size={14} />} Start
            </Button>
          ) : (
            <Button disabled={busy !== null} onClick={() => void perform("navigate", () => navigateBrowser(session.sessionId, url), (result) => setEvidence(result.evidence))}>Go</Button>
          )}
        </div>
        <p className="mt-2 break-all text-[11px] text-faint">Exact origin: {origin ?? "invalid URL"}</p>
        {local && (
          <label className="mt-3 flex items-start gap-2 rounded-md border border-warning/40 bg-warning/10 p-2 text-xs text-foreground">
            <input type="checkbox" checked={allowLoopback} disabled={Boolean(session)} onChange={(event) => setAllowLoopback(event.target.checked)} className="mt-0.5" />
            Grant this run access to loopback testing. Other private and link-local destinations remain blocked.
          </label>
        )}
      </div>

      {session && (
        <>
          <div className="flex flex-wrap items-center gap-2 rounded-lg border border-border bg-surface p-3 text-xs text-muted">
            <CheckCircle2 size={15} className="text-success" />
            <span className="font-medium text-foreground">Owned session</span>
            <span className="font-mono">{session.sessionId.slice(0, 22)}…</span>
            <span>{session.actionCount} actions</span>
            <Button size="sm" variant="danger" disabled={busy !== null} onClick={() => void perform("stop", () => stopBrowserSession(session.sessionId), () => { setSession(null); setEvidence(null); setInspection(null); })}><Square size={12} /> Stop and erase profile</Button>
          </div>

          <div className="grid gap-3 md:grid-cols-2">
            <div className="rounded-lg border border-border bg-surface p-3">
              <label htmlFor="browser-selector" className="text-xs font-medium text-foreground">CSS selector</label>
              <input id="browser-selector" value={selector} onChange={(event) => setSelector(event.target.value)} className="mt-2 w-full rounded-md border border-border bg-background px-3 py-2 font-mono text-xs text-foreground" />
              <div className="mt-2 flex flex-wrap gap-2">
                <Button size="sm" disabled={!selector || busy !== null} onClick={() => void perform("click", () => clickBrowser(session.sessionId, selector), (result) => setEvidence(result.evidence))}><MousePointerClick size={12} /> Click</Button>
                <Button size="sm" disabled={busy !== null} onClick={() => void perform("scroll", () => scrollBrowser(session.sessionId, 0, 640), (result) => setEvidence(result.evidence))}>Scroll</Button>
              </div>
            </div>
            <div className="rounded-lg border border-border bg-surface p-3">
              <label htmlFor="browser-type" className="text-xs font-medium text-foreground">Text to type</label>
              <textarea id="browser-type" value={text} onChange={(event) => setText(event.target.value)} className="mt-2 h-20 w-full resize-y rounded-md border border-border bg-background px-3 py-2 text-xs text-foreground" />
              <Button size="sm" disabled={!selector || !text || busy !== null} onClick={() => void perform("type", () => typeBrowserText(session.sessionId, selector, text), (result) => setEvidence(result.evidence))}><TextCursorInput size={12} /> Type</Button>
            </div>
          </div>

          <div className="flex flex-wrap gap-2">
            <Button disabled={busy !== null} onClick={() => void perform("inspect", () => inspectBrowser(session.sessionId), setInspection)}>Inspect DOM + accessibility</Button>
            <Button disabled={busy !== null} onClick={() => void refreshEvidence()}><Camera size={14} /> Capture evidence</Button>
          </div>

          {inspection && <div className="rounded-lg border border-border bg-surface p-3 text-xs"><p className="font-medium text-foreground">{inspection.title || "Untitled page"}</p><p className="mt-1 break-all text-muted">{inspection.url}</p><div className="mt-2 flex flex-wrap gap-2"><ArtifactLink label="DOM" id={inspection.dom.id} /><ArtifactLink label="AX" id={inspection.accessibility.id} /></div></div>}

          {evidence && (
            <div className="rounded-lg border border-border bg-surface p-3">
              <div className="flex flex-wrap gap-2"><ArtifactLink label="Screenshot" id={evidence.screenshot?.id} /><ArtifactLink label="DOM" id={evidence.dom?.id} /><ArtifactLink label="Console" id={evidence.console?.id} /><ArtifactLink label="Network" id={evidence.network?.id} /></div>
              {screenshotUrl && <img src={screenshotUrl} alt="Latest isolated browser screenshot" className="mt-3 max-h-80 w-full rounded-md border border-border object-contain" />}
            </div>
          )}
        </>
      )}

      {busy && <p role="status" className="text-xs text-muted">Running {busy}…</p>}
      {error && <p role="alert" className="rounded-md border border-danger/40 bg-danger/10 p-3 text-xs text-danger">{error}</p>}
    </section>
  );
}
