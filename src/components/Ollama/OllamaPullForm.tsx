import { useCallback, useState } from "react";
import { Button } from "../ui";
import { useModelStore } from "../../store/modelStore";
import { useT } from "../../lib/i18n";

/** Heuristic for detecting an auth-required failure in `ollama pull`'s output. */
const SIGNIN_HINT_PATTERN = /sign in|not signed in|unauthorized/i;

/**
 * Free-text "pull any tag" input plus quick-fill example chips, mirroring
 * using the real `ollama pull <tag>` CLI. Cloud model tags (e.g.
 * "gpt-oss:120b-cloud") are just ordinary tags once pulled — there's no
 * stable public catalog to enumerate, so the examples are illustrative only.
 */
export function OllamaPullForm() {
  const { t } = useT();
  const ollamaExampleTags = useModelStore((s) => s.ollamaExampleTags);
  const ollamaPullProgress = useModelStore((s) => s.ollamaPullProgress);
  const ollamaPullError = useModelStore((s) => s.ollamaPullError);
  const ollamaSigninMessage = useModelStore((s) => s.ollamaSigninMessage);
  const pullOllamaModel = useModelStore((s) => s.pullOllamaModel);
  const cancelOllamaPull = useModelStore((s) => s.cancelOllamaPull);
  const signinOllama = useModelStore((s) => s.signinOllama);

  const [tag, setTag] = useState("");
  const [submittedTag, setSubmittedTag] = useState<string | null>(null);
  const [isPulling, setIsPulling] = useState(false);
  const [isCancelling, setIsCancelling] = useState(false);

  const trimmedTag = tag.trim();
  const disabled = !trimmedTag || (isPulling && submittedTag === trimmedTag);

  const handlePull = useCallback(async () => {
    if (!trimmedTag || (isPulling && submittedTag === trimmedTag)) return;
    setSubmittedTag(trimmedTag);
    setIsPulling(true);
    setIsCancelling(false);
    try {
      await pullOllamaModel(trimmedTag);
    } catch {
      // Failure message is captured in `ollamaPullError[trimmedTag]` by the
      // store; nothing further to do here.
    } finally {
      setIsPulling(false);
      setIsCancelling(false);
    }
  }, [trimmedTag, isPulling, submittedTag, pullOllamaModel]);

  const handleCancel = useCallback(async () => {
    if (!submittedTag || isCancelling) return;
    setIsCancelling(true);
    try {
      await cancelOllamaPull(submittedTag);
    } catch {
      // Best-effort — if this fails the pull is still tracked server-side
      // and the user can retry cancelling.
    } finally {
      setIsCancelling(false);
    }
  }, [submittedTag, isCancelling, cancelOllamaPull]);

  const progressLine = submittedTag ? ollamaPullProgress[submittedTag] : undefined;
  const errorMessage = submittedTag ? ollamaPullError[submittedTag] : undefined;
  const needsSignin = errorMessage ? SIGNIN_HINT_PATTERN.test(errorMessage) : false;

  return (
    <div className="flex flex-col gap-2 rounded-lg border border-border bg-background p-3">
      <div className="flex items-center gap-2">
        <input
          type="text"
          value={tag}
          onChange={(event) => setTag(event.target.value)}
          placeholder={t("OllamaPullForm.tagInputPlaceholder")}
          className="h-8 min-w-0 flex-1 rounded-md border border-border bg-surface px-2.5 font-mono text-sm text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
        />
        <Button variant="primary" size="sm" onClick={() => void handlePull()} disabled={disabled}>
          {t("OllamaPullForm.pullButton")}
        </Button>
        {isPulling && (
          <Button
            variant="secondary"
            size="sm"
            onClick={() => void handleCancel()}
            disabled={isCancelling}
          >
            {t("OllamaPullForm.cancelButton")}
          </Button>
        )}
      </div>

      {ollamaExampleTags.length > 0 && (
        <div className="flex flex-wrap gap-1.5">
          {ollamaExampleTags.map((example) => (
            <Button
              key={example}
              variant="ghost"
              size="sm"
              onClick={() => setTag(example)}
              className="font-mono"
            >
              {example}
            </Button>
          ))}
        </div>
      )}

      <p className="text-xs text-faint">{t("OllamaPullForm.examplesHint")}</p>

      {isPulling && progressLine && (
        <p className="truncate font-mono text-xs text-muted">{progressLine}</p>
      )}

      {errorMessage && (
        <div className="flex flex-col items-start gap-1.5">
          <p className="text-xs text-danger">{errorMessage}</p>
          {needsSignin && (
            <>
              <Button variant="secondary" size="sm" onClick={() => void signinOllama()}>
                {t("OllamaPullForm.signInButton")}
              </Button>
              {ollamaSigninMessage && <p className="text-xs text-muted">{ollamaSigninMessage}</p>}
            </>
          )}
        </div>
      )}
    </div>
  );
}
