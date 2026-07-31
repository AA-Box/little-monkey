import { useEffect, useState } from "react";
import { Camera, Loader2, RotateCcw, X } from "lucide-react";

import { useT } from "../../lib/i18n";
import {
  annotateBrowser,
  captureBrowserEvidence,
  listBrowserSessions,
  type BrowserAnnotation,
  type BrowserSessionView,
} from "../../lib/browserVerification";
import { artifactDataUrl, readDurableArtifact } from "../../lib/durableArtifacts";
import { useVisualEditModeStore, type VisualEdit, type VisualEditStatus } from "../../store/visualEditModeStore";
import { Button, IconButton, StatusPill, type PillTone } from "../ui";
import { DiffViewer } from "../Workspace";
import { errorMessage } from "../../lib/errors";

interface VisualEditModePanelProps {
  onClose: () => void;
}

const STATUS_TONE: Record<VisualEditStatus, PillTone> = {
  generating: "neutral",
  pending: "warning",
  accepted: "success",
  rejected: "danger",
  error: "danger",
};

const STATUS_LABEL_KEY: Record<VisualEditStatus, string> = {
  generating: "VisualEditModePanel.status.generating",
  pending: "VisualEditModePanel.status.pending",
  accepted: "VisualEditModePanel.status.accepted",
  rejected: "VisualEditModePanel.status.rejected",
  error: "VisualEditModePanel.status.error",
};

async function screenshotFromArtifact(
  artifact: { id: string } | null | undefined,
): Promise<{ path: string; dataUrl: string } | null> {
  if (!artifact) return null;
  try {
    const content = await readDurableArtifact(artifact.id);
    return { path: `browser-evidence://${artifact.id}.png`, dataUrl: artifactDataUrl("image/png", content.contentBase64) };
  } catch {
    return null;
  }
}

function EditCard({ edit }: { edit: VisualEdit }) {
  const { t } = useT();
  const accept = useVisualEditModeStore((state) => state.accept);
  const reject = useVisualEditModeStore((state) => state.reject);
  const replay = useVisualEditModeStore((state) => state.replay);
  const setAfterScreenshot = useVisualEditModeStore((state) => state.setAfterScreenshot);
  const remove = useVisualEditModeStore((state) => state.remove);
  const [busy, setBusy] = useState(false);

  async function handleAccept() {
    setBusy(true);
    try {
      await accept(edit.id);
      // Best-effort "after" screenshot: only meaningful if the same Browser
      // Workbench session is still open on a page that reflects the write
      // (e.g. an HMR-enabled dev server) — a failure here never blocks the
      // accepted status itself, it just means no after-screenshot is shown.
      try {
        const evidence = await captureBrowserEvidence(edit.sessionId);
        const screenshot = await screenshotFromArtifact(evidence.screenshot);
        if (screenshot) setAfterScreenshot(edit.id, screenshot);
      } catch {
        // no live session to re-capture from — fine, before/diff still stand.
      }
    } catch {
      // error message already recorded on the edit by the store
    } finally {
      setBusy(false);
    }
  }

  async function handleReplay() {
    setBusy(true);
    try {
      await replay(edit.id);
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="rounded-lg border border-border bg-background p-4">
      <div className="flex flex-wrap items-start justify-between gap-2">
        <div className="min-w-0 flex-1">
          <p className="truncate text-sm font-medium text-foreground">{edit.description}</p>
          <p className="mt-0.5 truncate font-mono text-xs text-faint">{edit.element.selector}</p>
        </div>
        <div className="flex shrink-0 items-center gap-2">
          <StatusPill tone={STATUS_TONE[edit.status]}>{t(STATUS_LABEL_KEY[edit.status])}</StatusPill>
          <IconButton size="sm" onClick={() => remove(edit.id)} aria-label={t("VisualEditModePanel.dismiss")}>
            <X size={14} />
          </IconButton>
        </div>
      </div>

      {edit.status === "generating" && (
        <p className="mt-3 flex items-center gap-2 text-xs text-muted">
          <Loader2 size={13} className="animate-spin" />
          {t("VisualEditModePanel.generating")}
        </p>
      )}

      {edit.error && (
        <p role="alert" className="mt-3 rounded-md border border-danger/30 bg-danger-soft px-3 py-2 text-xs text-danger">
          {edit.error}
        </p>
      )}

      {(edit.beforeScreenshot || edit.afterScreenshot) && (
        <div className="mt-3 grid gap-2 sm:grid-cols-2">
          <div>
            <p className="mb-1 text-[11px] font-medium uppercase tracking-wide text-faint">
              {t("VisualEditModePanel.before")}
            </p>
            {edit.beforeScreenshot ? (
              <img
                src={edit.beforeScreenshot.dataUrl}
                alt={t("VisualEditModePanel.before")}
                className="w-full rounded-md border border-border object-contain"
              />
            ) : (
              <p className="text-xs text-faint">{t("VisualEditModePanel.noScreenshot")}</p>
            )}
          </div>
          <div>
            <p className="mb-1 text-[11px] font-medium uppercase tracking-wide text-faint">
              {t("VisualEditModePanel.after")}
            </p>
            {edit.afterScreenshot ? (
              <img
                src={edit.afterScreenshot.dataUrl}
                alt={t("VisualEditModePanel.after")}
                className="w-full rounded-md border border-border object-contain"
              />
            ) : (
              <p className="text-xs text-faint">{t("VisualEditModePanel.noScreenshot")}</p>
            )}
          </div>
        </div>
      )}

      {edit.targetFile && edit.oldContent !== null && edit.newContent !== null && (
        <div className="mt-3">
          {edit.summary && <p className="mb-2 text-xs text-muted">{edit.summary}</p>}
          <DiffViewer oldValue={edit.oldContent} newValue={edit.newContent} fileName={edit.targetFile} className="max-h-72" />
        </div>
      )}

      {(edit.status === "pending" || edit.status === "error") && (
        <div className="mt-3 flex flex-wrap items-center gap-2">
          {edit.status === "pending" && edit.targetFile && (
            <>
              <Button size="sm" variant="primary" onClick={() => void handleAccept()} disabled={busy}>
                {t("VisualEditModePanel.accept")}
              </Button>
              <Button size="sm" variant="secondary" onClick={() => reject(edit.id)} disabled={busy}>
                {t("VisualEditModePanel.reject")}
              </Button>
            </>
          )}
          <Button size="sm" variant="ghost" onClick={() => void handleReplay()} disabled={busy}>
            <RotateCcw size={13} />
            {t("VisualEditModePanel.replay")}
          </Button>
        </div>
      )}

      {edit.status === "accepted" && (
        <p className="mt-3 text-xs text-success">{t("VisualEditModePanel.acceptedNote", { file: edit.targetFile ?? "" })}</p>
      )}
    </div>
  );
}

export function VisualEditModePanel({ onClose }: VisualEditModePanelProps) {
  const { t } = useT();
  const edits = useVisualEditModeStore((state) => state.edits);
  const order = useVisualEditModeStore((state) => state.order);
  const start = useVisualEditModeStore((state) => state.start);

  const [sessions, setSessions] = useState<BrowserSessionView[]>([]);
  const [selectedSessionId, setSelectedSessionId] = useState("");
  const [selector, setSelector] = useState("button");
  const [annotation, setAnnotation] = useState<BrowserAnnotation | null>(null);
  const [beforeScreenshot, setBeforeScreenshot] = useState<{ path: string; dataUrl: string } | null>(null);
  const [description, setDescription] = useState("");
  const [busy, setBusy] = useState<"sessions" | "capture" | null>(null);
  const [error, setError] = useState<string | null>(null);

  async function refreshSessions() {
    setBusy("sessions");
    setError(null);
    try {
      const list = await listBrowserSessions();
      setSessions(list);
      if (list.length > 0 && !list.some((session) => session.sessionId === selectedSessionId)) {
        setSelectedSessionId(list[0].sessionId);
      }
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(null);
    }
  }

  useEffect(() => {
    void refreshSessions();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  async function handleCapture() {
    if (!selectedSessionId || !selector.trim()) return;
    setBusy("capture");
    setError(null);
    try {
      const result = await annotateBrowser(selectedSessionId, selector.trim());
      setAnnotation(result);
      const screenshot = await screenshotFromArtifact(result.evidence.screenshot);
      setBeforeScreenshot(screenshot);
    } catch (err) {
      setError(errorMessage(err));
      setAnnotation(null);
      setBeforeScreenshot(null);
    } finally {
      setBusy(null);
    }
  }

  function handleGenerate() {
    if (!annotation || !selectedSessionId || description.trim().length === 0) return;
    const session = sessions.find((candidate) => candidate.sessionId === selectedSessionId);
    start({
      sessionId: selectedSessionId,
      pageUrl: session?.currentUrl ?? "",
      description: description.trim(),
      element: {
        selector: annotation.selector,
        tag: annotation.tag,
        role: annotation.role,
        ariaLabel: annotation.ariaLabel,
        text: annotation.text,
        rect: annotation.rect,
      },
      beforeScreenshot,
    });
    setDescription("");
  }

  const canCapture = selectedSessionId.length > 0 && selector.trim().length > 0 && busy !== "capture";
  const canGenerate = annotation !== null && description.trim().length > 0;

  return (
    <section className="flex min-h-0 flex-1 flex-col" aria-labelledby="visual-edit-mode-title">
      <header className="flex shrink-0 items-center justify-between gap-3 border-b border-border px-4 py-3">
        <div className="min-w-0">
          <h1 id="visual-edit-mode-title" className="text-base font-semibold text-foreground">
            {t("VisualEditModePanel.title")}
          </h1>
          <p className="truncate text-xs text-muted">{t("VisualEditModePanel.subtitle")}</p>
        </div>
        <IconButton size="sm" onClick={onClose} aria-label={t("VisualEditModePanel.close")}>
          <X size={16} />
        </IconButton>
      </header>

      <div className="min-h-0 flex-1 overflow-y-auto px-4 py-4">
        <div className="mx-auto flex max-w-3xl flex-col gap-4">
          <section className="rounded-lg border border-border bg-background p-4">
            <h2 className="text-sm font-semibold text-foreground">{t("VisualEditModePanel.pickElement.title")}</h2>
            <p className="mt-1 text-xs text-muted">{t("VisualEditModePanel.pickElement.description")}</p>

            {error && (
              <p role="alert" className="mt-3 rounded-md border border-danger/30 bg-danger-soft px-3 py-2 text-xs text-danger">
                {error}
              </p>
            )}

            <div className="mt-3 flex flex-col gap-2">
              <label className="text-xs font-medium text-muted" htmlFor="visual-edit-session">
                {t("VisualEditModePanel.pickElement.session")}
              </label>
              <div className="flex items-center gap-2">
                <select
                  id="visual-edit-session"
                  value={selectedSessionId}
                  onChange={(event) => setSelectedSessionId(event.target.value)}
                  className="min-w-0 flex-1 rounded-md border border-border bg-background px-2 py-2 text-xs text-foreground"
                  disabled={sessions.length === 0}
                >
                  {sessions.length === 0 ? (
                    <option value="">{t("VisualEditModePanel.pickElement.noSessions")}</option>
                  ) : (
                    sessions.map((session) => (
                      <option key={session.sessionId} value={session.sessionId}>
                        {session.currentUrl || session.sessionId}
                      </option>
                    ))
                  )}
                </select>
                <IconButton
                  size="sm"
                  onClick={() => void refreshSessions()}
                  aria-label={t("VisualEditModePanel.pickElement.refreshSessions")}
                  disabled={busy === "sessions"}
                >
                  <Loader2 size={14} className={busy === "sessions" ? "animate-spin" : "opacity-0"} />
                </IconButton>
              </div>

              <label className="text-xs font-medium text-muted" htmlFor="visual-edit-selector">
                {t("VisualEditModePanel.pickElement.selector")}
              </label>
              <div className="flex items-center gap-2">
                <input
                  id="visual-edit-selector"
                  value={selector}
                  onChange={(event) => setSelector(event.target.value)}
                  placeholder={t("VisualEditModePanel.pickElement.selectorPlaceholder")}
                  className="min-w-0 flex-1 rounded-md border border-border bg-background px-2 py-2 font-mono text-xs text-foreground"
                />
                <Button size="sm" variant="secondary" onClick={() => void handleCapture()} disabled={!canCapture}>
                  <Camera size={13} />
                  {t("VisualEditModePanel.pickElement.capture")}
                </Button>
              </div>
            </div>

            {annotation && (
              <div className="mt-3 flex flex-col gap-3 rounded-md border border-border bg-surface px-3 py-2 sm:flex-row">
                {beforeScreenshot && (
                  <img
                    src={beforeScreenshot.dataUrl}
                    alt={t("VisualEditModePanel.before")}
                    className="h-24 w-auto shrink-0 rounded-md border border-border object-contain"
                  />
                )}
                <div className="min-w-0 flex-1 text-xs text-muted">
                  <p className="truncate font-mono text-foreground">{annotation.selector}</p>
                  <p className="truncate">
                    &lt;{annotation.tag}&gt; {annotation.ariaLabel || annotation.text || ""}
                  </p>
                </div>
              </div>
            )}

            <div className="mt-3 flex flex-col gap-2">
              <label className="text-xs font-medium text-muted" htmlFor="visual-edit-description">
                {t("VisualEditModePanel.describeChange")}
              </label>
              <textarea
                id="visual-edit-description"
                value={description}
                onChange={(event) => setDescription(event.target.value)}
                placeholder={t("VisualEditModePanel.describeChangePlaceholder")}
                rows={2}
                className="w-full resize-none rounded-md border border-border bg-background px-2 py-2 text-xs text-foreground"
              />
              <Button size="sm" variant="primary" onClick={handleGenerate} disabled={!canGenerate} className="self-start">
                {t("VisualEditModePanel.generate")}
              </Button>
            </div>
          </section>

          {order.length === 0 ? (
            <p className="text-sm text-muted">{t("VisualEditModePanel.empty")}</p>
          ) : (
            <div className="flex flex-col gap-3">
              {order.map((id) => {
                const edit = edits[id];
                return edit ? <EditCard key={id} edit={edit} /> : null;
              })}
            </div>
          )}
        </div>
      </div>
    </section>
  );
}

export default VisualEditModePanel;
