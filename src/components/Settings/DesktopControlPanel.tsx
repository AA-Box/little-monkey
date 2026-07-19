import { useCallback, useEffect, useMemo, useState } from "react";
import { Octagon, Power, Trash2 } from "lucide-react";

import { useSettingsStore } from "../../store/settingsStore";
import {
  useDesktopControlStore,
  type ControlAction,
  type MouseButtonKind,
} from "../../store/desktopControlStore";
import { useT } from "../../lib/i18n";
import { Button } from "../ui";

/**
 * Safe Desktop Control settings surface — a design-validation research
 * spike (ROADMAP.md Phase 5, Status: Research). See
 * `docs/safe-desktop-control-design.md` and `src-tauri/src/desktop_control.rs`
 * for the full threat model. This panel is the ONLY way to reach the
 * feature: there is no agent tool for it, so nothing here is exposed to the
 * model's own initiative.
 */

const INPUT =
  "w-full rounded-md border border-border bg-background px-2.5 py-2 text-sm text-foreground outline-none focus:ring-1 focus:ring-accent";

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** No shared toggle-switch component exists in `ui/` yet — mirrors
 * `AutomationPanel.tsx`'s local `Toggle` rather than promoting one prematurely. */
function Toggle({
  checked,
  onChange,
  label,
  description,
}: {
  checked: boolean;
  onChange: (value: boolean) => void;
  label: string;
  description?: string;
}) {
  return (
    <label className="flex flex-col gap-0.5 py-2.5">
      <span className="flex items-center justify-between gap-3">
        <span className="text-sm font-medium text-foreground">{label}</span>
        <button
          type="button"
          role="switch"
          aria-checked={checked}
          aria-label={label}
          onClick={() => onChange(!checked)}
          className={`relative h-5 w-9 shrink-0 cursor-pointer rounded-full transition-colors ${
            checked ? "bg-accent" : "border border-border bg-surface-2"
          }`}
        >
          <span
            className={`absolute top-0.5 h-4 w-4 rounded-full bg-white shadow transition-[left] ${
              checked ? "left-[18px]" : "left-0.5"
            }`}
          />
        </button>
      </span>
      {description && <p className="pr-12 text-xs text-muted">{description}</p>}
    </label>
  );
}

type ActionKind = ControlAction["kind"];

export function DesktopControlPanel() {
  const { t } = useT();
  const enabled = useSettingsStore((s) => s.desktopControlEnabled);
  const setEnabled = useSettingsStore((s) => s.setDesktopControlEnabled);

  const sessions = useDesktopControlStore((s) => s.sessions);
  const pendingActions = useDesktopControlStore((s) => s.pendingActions);
  const storeError = useDesktopControlStore((s) => s.error);
  const refreshSessions = useDesktopControlStore((s) => s.refreshSessions);
  const startSession = useDesktopControlStore((s) => s.startSession);
  const stopSession = useDesktopControlStore((s) => s.stopSession);
  const requestAction = useDesktopControlStore((s) => s.requestAction);
  const respondAction = useDesktopControlStore((s) => s.respondAction);
  const emergencyStop = useDesktopControlStore((s) => s.emergencyStop);

  const [allowlistInput, setAllowlistInput] = useState("");
  const [lifetimeMinutes, setLifetimeMinutes] = useState("5");
  const [approvedBatch, setApprovedBatch] = useState(false);
  const [busy, setBusy] = useState(false);
  const [status, setStatus] = useState<string | null>(null);
  const [localError, setLocalError] = useState<string | null>(null);

  const [testSessionId, setTestSessionId] = useState("");
  const [testTarget, setTestTarget] = useState("");
  const [testKind, setTestKind] = useState<ActionKind>("mouse_move");
  const [testX, setTestX] = useState("0");
  const [testY, setTestY] = useState("0");
  const [testButton, setTestButton] = useState<MouseButtonKind>("left");
  const [testKey, setTestKey] = useState("a");

  useEffect(() => {
    if (!enabled) return;
    void refreshSessions();
    const interval = window.setInterval(() => void refreshSessions(), 5_000);
    return () => window.clearInterval(interval);
  }, [enabled, refreshSessions]);

  const activeSessions = useMemo(() => sessions.filter((session) => session.active), [sessions]);

  useEffect(() => {
    if (testSessionId && activeSessions.some((session) => session.sessionId === testSessionId)) return;
    const first = activeSessions[0];
    setTestSessionId(first?.sessionId ?? "");
    setTestTarget(first?.allowedApplications[0] ?? "");
  }, [activeSessions, testSessionId]);

  const allowlist = useMemo(
    () =>
      allowlistInput
        .split(",")
        .map((entry) => entry.trim())
        .filter(Boolean),
    [allowlistInput],
  );

  const handleStart = useCallback(async () => {
    setBusy(true);
    setLocalError(null);
    try {
      const lifetimeMs = Math.max(1, Math.round(Number(lifetimeMinutes) || 0)) * 60_000;
      const session = await startSession(allowlist, lifetimeMs, approvedBatch);
      setStatus(t("DesktopControlPanel.sessionStarted"));
      setTestSessionId(session.sessionId);
      setTestTarget(session.allowedApplications[0] ?? "");
      setAllowlistInput("");
    } catch (reason) {
      setLocalError(errorText(reason));
    } finally {
      setBusy(false);
    }
  }, [allowlist, approvedBatch, lifetimeMinutes, startSession, t]);

  const handleStop = useCallback(
    async (sessionId: string) => {
      setLocalError(null);
      try {
        await stopSession(sessionId);
        setStatus(t("DesktopControlPanel.sessionStopped"));
      } catch (reason) {
        setLocalError(errorText(reason));
      }
    },
    [stopSession, t],
  );

  const handleEmergencyStop = useCallback(async () => {
    setLocalError(null);
    try {
      const result = await emergencyStop();
      setStatus(
        t("DesktopControlPanel.emergencyStopStatus", {
          sessions: result.sessionsDeactivated,
          actions: result.actionsCancelled,
        }),
      );
    } catch (reason) {
      setLocalError(errorText(reason));
    }
  }, [emergencyStop, t]);

  const handleSendTestAction = useCallback(async () => {
    if (!testSessionId || !testTarget) return;
    setLocalError(null);
    const action: ControlAction =
      testKind === "mouse_move"
        ? { kind: "mouse_move", x: Number(testX) || 0, y: Number(testY) || 0 }
        : testKind === "mouse_click"
          ? { kind: "mouse_click", button: testButton }
          : { kind: "key_press", key: testKey };
    try {
      const outcome = await requestAction(testSessionId, testTarget, action);
      setStatus(outcome.executed ? t("DesktopControlPanel.actionExecuted") : t("DesktopControlPanel.actionNotExecuted"));
    } catch (reason) {
      setLocalError(errorText(reason));
    }
  }, [requestAction, t, testButton, testKey, testKind, testSessionId, testTarget, testX, testY]);

  const describeAction = useCallback(
    (action: ControlAction): string => {
      switch (action.kind) {
        case "mouse_move":
          return t("DesktopControlPanel.actionDescriptionMouseMove", { x: action.x, y: action.y });
        case "mouse_click":
          return t("DesktopControlPanel.actionDescriptionMouseClick", { button: action.button });
        case "key_press":
          return t("DesktopControlPanel.actionDescriptionKeyPress", { key: action.key });
      }
    },
    [t],
  );

  return (
    <div className="flex flex-col gap-6">
      <section className="rounded-lg border border-border bg-surface p-4">
        <div className="flex items-start justify-between gap-3">
          <div>
            <h3 className="text-sm font-semibold">{t("DesktopControlPanel.title")}</h3>
            <p className="mt-1 text-xs leading-relaxed text-muted">
              {enabled ? t("DesktopControlPanel.enabledDescription") : t("DesktopControlPanel.disabledDescription")}
            </p>
          </div>
          {enabled && (
            <Button size="sm" variant="danger" onClick={() => void handleEmergencyStop()}>
              <Octagon size={14} />
              {t("DesktopControlPanel.emergencyStopButton")}
            </Button>
          )}
        </div>
        <div className="mt-3 rounded-lg border border-border bg-background px-3">
          <Toggle
            checked={enabled}
            onChange={setEnabled}
            label={t("DesktopControlPanel.enableLabel")}
            description={t("DesktopControlPanel.enableDescription")}
          />
        </div>
      </section>

      {enabled && (
        <>
          <section className="rounded-lg border border-border bg-surface p-4">
            <h3 className="text-sm font-semibold">{t("DesktopControlPanel.startSessionHeading")}</h3>
            <p className="mt-1 text-xs text-muted">{t("DesktopControlPanel.startSessionDescription")}</p>
            <label className="mt-3 block text-xs text-muted">
              {t("DesktopControlPanel.allowlistLabel")}
              <input
                className={`${INPUT} mt-1`}
                value={allowlistInput}
                onChange={(event) => setAllowlistInput(event.target.value)}
                placeholder={t("DesktopControlPanel.allowlistPlaceholder")}
              />
            </label>
            <div className="mt-2 grid grid-cols-2 gap-2">
              <label className="text-xs text-muted">
                {t("DesktopControlPanel.lifetimeLabel")}
                <input
                  className={`${INPUT} mt-1`}
                  value={lifetimeMinutes}
                  onChange={(event) => setLifetimeMinutes(event.target.value)}
                  aria-label={t("DesktopControlPanel.lifetimeLabel")}
                />
              </label>
              <label className="mt-5 flex items-center gap-2 text-xs text-muted">
                <input type="checkbox" checked={approvedBatch} onChange={(event) => setApprovedBatch(event.target.checked)} />
                {t("DesktopControlPanel.approvedBatchLabel")}
              </label>
            </div>
            <Button
              className="mt-3"
              size="sm"
              variant="primary"
              disabled={busy || allowlist.length === 0}
              onClick={() => void handleStart()}
            >
              <Power size={14} />
              {t("DesktopControlPanel.startSessionButton")}
            </Button>
          </section>

          <section className="rounded-lg border border-border bg-surface p-4">
            <h3 className="text-sm font-semibold">{t("DesktopControlPanel.sessionsHeading")}</h3>
            {sessions.length === 0 ? (
              <p className="mt-2 text-xs text-muted">{t("DesktopControlPanel.noSessions")}</p>
            ) : (
              <div className="mt-3 flex flex-col gap-2">
                {sessions.map((session) => (
                  <div
                    key={session.sessionId}
                    className="flex items-center justify-between rounded-md border border-border bg-background px-3 py-2 text-xs"
                  >
                    <div>
                      <p className="font-medium text-foreground">
                        {session.allowedApplications.join(", ")}
                        {session.approvedBatch && (
                          <span className="ml-2 rounded-full border border-border px-1.5 py-0.5 text-[10px] text-muted">
                            {t("DesktopControlPanel.approvedBatchBadge")}
                          </span>
                        )}
                      </p>
                      <p className="text-muted">
                        {session.active ? t("DesktopControlPanel.statusActive") : t("DesktopControlPanel.statusInactive")}
                      </p>
                    </div>
                    {session.active && (
                      <Button size="sm" variant="ghost" onClick={() => void handleStop(session.sessionId)}>
                        <Trash2 size={13} />
                        {t("DesktopControlPanel.stopSessionButton")}
                      </Button>
                    )}
                  </div>
                ))}
              </div>
            )}
          </section>

          <section className="rounded-lg border border-border bg-surface p-4">
            <h3 className="text-sm font-semibold">{t("DesktopControlPanel.pendingActionsHeading")}</h3>
            <p className="mt-1 text-xs text-muted">{t("DesktopControlPanel.pendingActionsDescription")}</p>
            {pendingActions.length === 0 ? (
              <p className="mt-2 text-xs text-muted">{t("DesktopControlPanel.noPendingActions")}</p>
            ) : (
              <div className="mt-3 flex flex-col gap-2">
                {pendingActions.map((pending) => (
                  <div
                    key={pending.actionId}
                    className="flex items-center justify-between rounded-md border border-border bg-background px-3 py-2 text-xs"
                  >
                    <span>{describeAction(pending.action)}</span>
                    <div className="flex gap-2">
                      <Button size="sm" variant="primary" onClick={() => void respondAction(pending.actionId, true)}>
                        {t("DesktopControlPanel.approveButton")}
                      </Button>
                      <Button size="sm" variant="ghost" onClick={() => void respondAction(pending.actionId, false)}>
                        {t("DesktopControlPanel.denyButton")}
                      </Button>
                    </div>
                  </div>
                ))}
              </div>
            )}
          </section>

          <section className="rounded-lg border border-border bg-surface p-4">
            <h3 className="text-sm font-semibold">{t("DesktopControlPanel.testActionHeading")}</h3>
            <p className="mt-1 text-xs text-muted">{t("DesktopControlPanel.testActionDescription")}</p>
            {activeSessions.length === 0 ? (
              <p className="mt-2 text-xs text-muted">{t("DesktopControlPanel.noActiveSessions")}</p>
            ) : (
              <>
                <div className="mt-3 grid grid-cols-2 gap-2">
                  <select
                    className={INPUT}
                    value={testSessionId}
                    onChange={(event) => {
                      const sessionId = event.target.value;
                      setTestSessionId(sessionId);
                      const session = activeSessions.find((candidate) => candidate.sessionId === sessionId);
                      setTestTarget(session?.allowedApplications[0] ?? "");
                    }}
                  >
                    {activeSessions.map((session) => (
                      <option key={session.sessionId} value={session.sessionId}>
                        {session.sessionId.slice(-8)}
                      </option>
                    ))}
                  </select>
                  <select className={INPUT} value={testTarget} onChange={(event) => setTestTarget(event.target.value)}>
                    {(activeSessions.find((session) => session.sessionId === testSessionId)?.allowedApplications ?? []).map(
                      (application) => (
                        <option key={application} value={application}>
                          {application}
                        </option>
                      ),
                    )}
                  </select>
                </div>
                <select className={`${INPUT} mt-2`} value={testKind} onChange={(event) => setTestKind(event.target.value as ActionKind)}>
                  <option value="mouse_move">{t("DesktopControlPanel.actionKindMouseMove")}</option>
                  <option value="mouse_click">{t("DesktopControlPanel.actionKindMouseClick")}</option>
                  <option value="key_press">{t("DesktopControlPanel.actionKindKeyPress")}</option>
                </select>
                {testKind === "mouse_move" && (
                  <div className="mt-2 grid grid-cols-2 gap-2">
                    <input className={INPUT} value={testX} onChange={(event) => setTestX(event.target.value)} aria-label="X" />
                    <input className={INPUT} value={testY} onChange={(event) => setTestY(event.target.value)} aria-label="Y" />
                  </div>
                )}
                {testKind === "mouse_click" && (
                  <select
                    className={`${INPUT} mt-2`}
                    value={testButton}
                    onChange={(event) => setTestButton(event.target.value as MouseButtonKind)}
                  >
                    <option value="left">{t("DesktopControlPanel.mouseButtonLeft")}</option>
                    <option value="right">{t("DesktopControlPanel.mouseButtonRight")}</option>
                    <option value="middle">{t("DesktopControlPanel.mouseButtonMiddle")}</option>
                  </select>
                )}
                {testKind === "key_press" && (
                  <input
                    className={`${INPUT} mt-2`}
                    value={testKey}
                    onChange={(event) => setTestKey(event.target.value)}
                    placeholder={t("DesktopControlPanel.keyPlaceholder")}
                  />
                )}
                <Button className="mt-3" size="sm" onClick={() => void handleSendTestAction()}>
                  {t("DesktopControlPanel.sendActionButton")}
                </Button>
              </>
            )}
          </section>
        </>
      )}

      {status && (
        <p role="status" className="text-xs text-success">
          {status}
        </p>
      )}
      {(localError ?? storeError) && (
        <p role="alert" className="rounded-md border border-danger/40 bg-danger/10 p-3 text-xs text-danger">
          {localError ?? storeError}
        </p>
      )}
    </div>
  );
}
