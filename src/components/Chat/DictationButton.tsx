import { forwardRef, useCallback, useEffect, useImperativeHandle, useRef, useState } from "react";
import type { RefObject } from "react";
import { LoaderCircle, Mic } from "lucide-react";

import { companionClient } from "../../lib/companionClient";
import {
  beginDictationInsertion,
  caretAfterDictation,
  commitDictationFinal,
  renderDictationInsertion,
  withDictationPartial,
  type DictationInsertionState,
} from "../../lib/dictationComposer";
import {
  dictationClient,
  type DictationCapabilities,
  type DictationState,
  type DictationUnlisten,
} from "../../lib/dictationClient";
import { errorMessage } from "../../lib/errors";
import { useT } from "../../lib/i18n";
import { Tooltip } from "./MessageActions";

interface ActiveDictation {
  insertion: DictationInsertionState;
  phase: DictationState;
}

interface SettleWaiter {
  resolve: (value: string) => void;
  reject: (reason: unknown) => void;
}

export interface DictationButtonHandle {
  isActive: () => boolean;
  settleForSend: () => Promise<string | null>;
}

export interface DictationButtonProps {
  sessionId: string;
  value: string;
  onChange: (value: string) => void;
  textareaRef: RefObject<HTMLTextAreaElement | null>;
  resizeTextarea?: () => void;
  disabled?: boolean;
}

function focusTextarea(textareaRef: RefObject<HTMLTextAreaElement | null>, caret: number): void {
  const textarea = textareaRef.current;
  if (!textarea) return;
  textarea.focus();
  textarea.setSelectionRange(caret, caret);
}

export const DictationButton = forwardRef<DictationButtonHandle, DictationButtonProps>(function DictationButton(
  { sessionId, value, onChange, textareaRef, resizeTextarea, disabled = false },
  ref,
) {
  const { t } = useT();
  const tRef = useRef(t);
  tRef.current = t;
  const [capabilities, setCapabilities] = useState<DictationCapabilities | null>(null);
  const [capabilityError, setCapabilityError] = useState<string | null>(null);
  const [active, setActive] = useState<ActiveDictation | null>(null);
  const [starting, setStarting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const activeRef = useRef<ActiveDictation | null>(null);
  const pendingStartRef = useRef<{
    value: string;
    selectionStart: number;
    selectionEnd: number;
  } | null>(null);
  const mountedRef = useRef(true);
  const listenersReadyRef = useRef<Promise<void> | null>(null);
  const unlistenRef = useRef<DictationUnlisten[]>([]);
  const settleWaitersRef = useRef<SettleWaiter[]>([]);

  const setActiveSession = useCallback((next: ActiveDictation | null) => {
    activeRef.current = next;
    setActive(next);
  }, []);

  const updateComposer = useCallback((insertion: DictationInsertionState) => {
    onChange(renderDictationInsertion(insertion));
    resizeTextarea?.();
  }, [onChange, resizeTextarea]);

  const resolveWaiters = useCallback((text: string) => {
    const waiters = settleWaitersRef.current.splice(0);
    for (const waiter of waiters) waiter.resolve(text);
  }, []);

  const rejectWaiters = useCallback((reason: unknown) => {
    const waiters = settleWaitersRef.current.splice(0);
    for (const waiter of waiters) waiter.reject(reason);
  }, []);

  const finishActive = useCallback((session: ActiveDictation) => {
    const finalValue = renderDictationInsertion(session.insertion);
    setActiveSession(null);
    updateComposer(session.insertion);
    requestAnimationFrame(() => {
      focusTextarea(textareaRef, caretAfterDictation(session.insertion));
      resizeTextarea?.();
    });
    resolveWaiters(finalValue);
  }, [resolveWaiters, resizeTextarea, setActiveSession, textareaRef, updateComposer]);

  const failActive = useCallback((reason: unknown, restore = true) => {
    const session = activeRef.current;
    if (!session) {
      setError(errorMessage(reason));
      return;
    }
    setActiveSession(null);
    if (restore) {
      onChange(session.insertion.originalValue);
      resizeTextarea?.();
      requestAnimationFrame(() => focusTextarea(textareaRef, session.insertion.selectionStart));
    }
    const message = errorMessage(reason);
    setError(message);
    rejectWaiters(reason);
    void dictationClient.cancel(session.insertion.sessionId).catch(() => undefined);
  }, [onChange, rejectWaiters, resizeTextarea, setActiveSession, textareaRef]);

  const cancelActive = useCallback((restore: boolean) => {
    const session = activeRef.current;
    if (!session) return Promise.resolve();
    setActiveSession(null);
    if (restore) {
      onChange(session.insertion.originalValue);
      resizeTextarea?.();
      requestAnimationFrame(() => focusTextarea(textareaRef, session.insertion.selectionStart));
    }
    rejectWaiters(new Error(t("DictationButton.cancelled")));
    return dictationClient.cancel(session.insertion.sessionId).catch((reason) => {
      setError(errorMessage(reason));
    });
  }, [onChange, rejectWaiters, resizeTextarea, setActiveSession, t, textareaRef]);

  const stopActive = useCallback(() => {
    const session = activeRef.current;
    if (!session) return Promise.resolve();
    setActiveSession({ ...session, phase: "stopping" });
    return dictationClient.stop(session.insertion.sessionId).catch((reason) => failActive(reason));
  }, [failActive, setActiveSession]);

  useImperativeHandle(ref, () => ({
    isActive: () => activeRef.current !== null,
    settleForSend: () => {
      const session = activeRef.current;
      if (!session) return Promise.resolve(null);
      setActiveSession({ ...session, phase: "stopping" });
      return new Promise<string>((resolve, reject) => {
        settleWaitersRef.current.push({ resolve, reject });
        void dictationClient.stop(session.insertion.sessionId).catch((reason) => failActive(reason));
      });
    },
  }), [failActive, setActiveSession]);

  useEffect(() => {
    let disposed = false;
    const ready = dictationClient.capabilities()
      .then((next) => {
        if (!disposed) setCapabilities(next);
      })
      .catch((reason: unknown) => {
        if (!disposed) {
          const message = errorMessage(reason);
          setCapabilityError(message);
          setCapabilities({
            supported: false,
            platform: "unsupported",
            engine: "",
            supportsPartialResults: false,
            supportsOnDevice: false,
            languages: [],
          });
        }
      });
    listenersReadyRef.current = ready.then(async () => {
      if (disposed) return;
      const [stateUnlisten, partialUnlisten, finalUnlisten, errorUnlisten] = await Promise.all([
        dictationClient.onState((event) => {
          const current = activeRef.current;
          if (!current || current.insertion.sessionId !== event.sessionId) return;
          if (event.state === "idle") {
            finishActive(current);
            return;
          }
          setActiveSession({ ...current, phase: event.state });
        }),
        dictationClient.onPartial((event) => {
          const current = activeRef.current;
          if (!current || current.insertion.sessionId !== event.sessionId) return;
          const insertion = withDictationPartial(current.insertion, event.text);
          updateComposer(insertion);
          setActiveSession({ ...current, insertion });
        }),
        dictationClient.onFinal((event) => {
          const current = activeRef.current;
          if (!current || current.insertion.sessionId !== event.sessionId) return;
          const insertion = commitDictationFinal(current.insertion, event.text);
          updateComposer(insertion);
          setActiveSession({ ...current, insertion });
        }),
        dictationClient.onError((event) => {
          const current = activeRef.current;
          if (!current || current.insertion.sessionId !== event.sessionId) return;
          failActive(new Error(event.message));
        }),
      ]);
      if (disposed) {
        stateUnlisten();
        partialUnlisten();
        finalUnlisten();
        errorUnlisten();
      } else {
        unlistenRef.current = [stateUnlisten, partialUnlisten, finalUnlisten, errorUnlisten];
      }
    });
    return () => {
      disposed = true;
      for (const unlisten of unlistenRef.current.splice(0)) unlisten();
    };
  }, [failActive, finishActive, setActiveSession, updateComposer]);

  useEffect(() => {
    const textarea = textareaRef.current;
    if (!textarea) return;
    const onManualInput = () => {
      const current = activeRef.current;
      if (!current) {
        // A click can be awaiting the OS permission/configuration prompt. Do
        // not let that in-flight start overwrite text the user typed meanwhile.
        pendingStartRef.current = null;
        return;
      }
      const editedValue = textarea.value;
      setActiveSession(null);
      setError(null);
      void dictationClient.cancel(current.insertion.sessionId).catch(() => undefined);
      resolveWaiters(editedValue);
    };
    textarea.addEventListener("input", onManualInput);
    return () => textarea.removeEventListener("input", onManualInput);
  }, [resolveWaiters, setActiveSession, textareaRef]);

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key !== "Escape" || !activeRef.current) return;
      event.preventDefault();
      event.stopPropagation();
      void cancelActive(true);
    };
    window.addEventListener("keydown", onKeyDown, true);
    return () => window.removeEventListener("keydown", onKeyDown, true);
  }, [cancelActive]);

  useEffect(() => {
    return () => {
      const current = activeRef.current;
      activeRef.current = null;
      pendingStartRef.current = null;
      setStarting(false);
      setActive(null);
      if (current) void dictationClient.cancel(current.insertion.sessionId).catch(() => undefined);
      rejectWaiters(new Error(tRef.current("DictationButton.unmounted")));
    };
  }, [rejectWaiters, sessionId]);

  useEffect(() => () => {
    mountedRef.current = false;
  }, []);

  const start = useCallback(async () => {
    if (disabled || !capabilities?.supported || activeRef.current || pendingStartRef.current) return;
    const textarea = textareaRef.current;
    const selectionStart = textarea?.selectionStart ?? value.length;
    const selectionEnd = textarea?.selectionEnd ?? selectionStart;
    const snapshot = { value, selectionStart, selectionEnd };
    pendingStartRef.current = snapshot;
    setStarting(true);
    setError(null);
    try {
      await (listenersReadyRef.current ?? Promise.resolve());
      const config = await companionClient.config();
      const started = await dictationClient.start({
        language: config.voice.dictationLanguage,
        requireOnDevice: capabilities.platform === "macos" && config.voice.dictationRequireOnDevice,
      });
      if (!mountedRef.current || pendingStartRef.current !== snapshot) {
        void dictationClient.cancel(started.sessionId).catch(() => undefined);
        return;
      }
      const insertion = beginDictationInsertion(
        started.sessionId,
        snapshot.value,
        snapshot.selectionStart,
        snapshot.selectionEnd,
      );
      setActiveSession({ insertion, phase: "starting" });
    } catch (reason) {
      if (mountedRef.current && pendingStartRef.current === snapshot) {
        setError(errorMessage(reason));
        requestAnimationFrame(() => focusTextarea(textareaRef, snapshot.value.length));
      }
    } finally {
      if (pendingStartRef.current === snapshot) pendingStartRef.current = null;
      if (mountedRef.current) setStarting(false);
    }
  }, [capabilities, disabled, setActiveSession, textareaRef, value]);

  const isActive = active !== null;
  const isStarting = starting || active?.phase === "starting";
  const unavailable = capabilityError ?? (!capabilities?.supported ? t("DictationButton.unavailable") : null);
  const tooltipText = error ?? unavailable ?? (isStarting ? t("DictationButton.starting") : isActive ? t("DictationButton.stop") : t("DictationButton.dictate"));
  const ariaLabel = isActive ? t("DictationButton.stop") : t("DictationButton.startAriaLabel");

  return (
    <span className="group/action relative shrink-0">
      <button
        type="button"
        onClick={() => void (isActive ? stopActive() : start())}
        disabled={disabled || isStarting || !capabilities?.supported}
        aria-label={isStarting ? t("DictationButton.starting") : ariaLabel}
        aria-pressed={isActive || undefined}
        className={`flex h-7 w-7 shrink-0 cursor-pointer items-center justify-center rounded-full text-faint transition-colors duration-150 hover:bg-surface-2 hover:text-foreground disabled:cursor-not-allowed disabled:opacity-40 disabled:hover:bg-transparent focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2 focus-visible:ring-offset-background ${isActive ? "bg-accent-soft text-accent hover:bg-accent-soft hover:text-accent" : ""}`}
      >
        {isStarting ? <LoaderCircle size={16} className="animate-spin" /> : <Mic size={16} className={isActive ? "animate-pulse" : ""} />}
      </button>
      <Tooltip text={tooltipText} />
      {error && <span role="status" className="sr-only">{error}</span>}
    </span>
  );
});
