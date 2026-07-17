import { useCallback, useEffect, useState } from "react";
import { ChevronDown, Plus, SlidersHorizontal } from "lucide-react";
import { Button, StatusPill } from "../ui";
import { useModelStore } from "../../store/modelStore";
import { OllamaModelList } from "./OllamaModelList";
import { OllamaPullForm } from "./OllamaPullForm";
import { OllamaImportForm } from "./OllamaImportForm";
import { ModelfileStudio } from "./ModelfileStudio";
import { useT } from "../../lib/i18n";

/**
 * Top-level status row for the Ollama provider (a second, sibling model
 * provider alongside local llama.cpp): reachability + version + sign-in
 * state in one row, a "Start Ollama" action when it's installed but not
 * running, the pulled-model list, and pull/import forms tucked behind a
 * disclosure (most visits are "pick a model I already pulled", not "add a
 * new one").
 */
export function OllamaPanel() {
  const ollamaReachable = useModelStore((s) => s.ollamaReachable);
  const ollamaVersion = useModelStore((s) => s.ollamaVersion);
  const ollamaBinaryFound = useModelStore((s) => s.ollamaBinaryFound);
  const ollamaSigninMessage = useModelStore((s) => s.ollamaSigninMessage);
  const ollamaSignedInUser = useModelStore((s) => s.ollamaSignedInUser);
  const refreshOllama = useModelStore((s) => s.refreshOllama);
  const startOllama = useModelStore((s) => s.startOllama);
  const signinOllama = useModelStore((s) => s.signinOllama);

  const [starting, setStarting] = useState(false);
  const [startError, setStartError] = useState<string | null>(null);
  const [signingIn, setSigningIn] = useState(false);
  const [signinError, setSigninError] = useState<string | null>(null);
  const { t } = useT();

  useEffect(() => {
    void refreshOllama();
  }, [refreshOllama]);

  const handleStart = useCallback(async () => {
    setStarting(true);
    setStartError(null);
    try {
      await startOllama();
    } catch (err) {
      setStartError(err instanceof Error ? err.message : String(err));
    } finally {
      setStarting(false);
    }
  }, [startOllama]);

  const handleSignin = useCallback(async () => {
    setSigningIn(true);
    setSigninError(null);
    try {
      await signinOllama();
    } catch (err) {
      setSigninError(err instanceof Error ? err.message : String(err));
    } finally {
      setSigningIn(false);
    }
  }, [signinOllama]);

  return (
    <div className="flex flex-col gap-2 py-2">
      <div className="flex flex-wrap items-center gap-2 px-1">
        <StatusPill tone={ollamaReachable ? "success" : "neutral"}>
          {ollamaReachable ? t("OllamaPanel.statusConnected") : t("OllamaPanel.statusNotRunning")}
        </StatusPill>
        {ollamaVersion && (
          <span className="font-mono text-xs text-muted">
            {t("OllamaPanel.versionLabel", { version: ollamaVersion })}
          </span>
        )}

        <div className="ml-auto flex items-center gap-2">
          {!ollamaReachable && ollamaBinaryFound && (
            <Button variant="secondary" size="sm" onClick={() => void handleStart()} disabled={starting}>
              {starting ? t("OllamaPanel.startingButton") : t("OllamaPanel.startOllamaButton")}
            </Button>
          )}
          {ollamaSignedInUser ? (
            <StatusPill tone="success">
              {t("OllamaPanel.signedInAs", { user: ollamaSignedInUser })}
            </StatusPill>
          ) : (
            <Button variant="secondary" size="sm" onClick={() => void handleSignin()} disabled={signingIn}>
              {signingIn ? t("OllamaPanel.signingInButton") : t("OllamaPanel.signInButton")}
            </Button>
          )}
        </div>
      </div>

      {!ollamaReachable && !ollamaBinaryFound && (
        <p className="px-1 text-xs text-muted">{t("OllamaPanel.notFoundMessage")}</p>
      )}
      {startError && <p className="px-1 text-xs text-danger">{startError}</p>}
      {signinError && <p className="px-1 text-xs text-danger">{signinError}</p>}
      {ollamaSigninMessage && <p className="px-1 text-xs text-muted">{ollamaSigninMessage}</p>}

      <OllamaModelList />

      <details className="group rounded-lg border border-border">
        <summary className="flex cursor-pointer list-none items-center gap-1.5 px-3 py-2 text-sm text-muted [&::-webkit-details-marker]:hidden">
          <Plus size={14} />
          {t("OllamaPanel.addModelLabel")}
          <ChevronDown size={14} className="ml-auto transition-transform group-open:rotate-180" />
        </summary>
        <div className="flex flex-col gap-2 border-t border-border p-2">
          <OllamaPullForm />
          <OllamaImportForm />
        </div>
      </details>

      <details className="group rounded-lg border border-border">
        <summary className="flex cursor-pointer list-none items-center gap-1.5 px-3 py-2 text-sm text-muted [&::-webkit-details-marker]:hidden">
          <SlidersHorizontal size={14} />
          {t("OllamaPanel.modelfileStudioLabel")}
          <ChevronDown size={14} className="ml-auto transition-transform group-open:rotate-180" />
        </summary>
        <div className="flex flex-col gap-2 border-t border-border p-2">
          <ModelfileStudio />
        </div>
      </details>
    </div>
  );
}
