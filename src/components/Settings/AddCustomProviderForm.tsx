import { useCallback, useState } from "react";
import { Button } from "../ui";
import { useModelStore } from "../../store/modelStore";
import { useT } from "../../lib/i18n";
import { errorMessage } from "../../lib/errors";

/**
 * Mini-form for registering any other OpenAI-compatible endpoint (Groq,
 * Mistral, a self-hosted vLLM/TGI server, etc.) — just a label + base URL.
 * Adding a key for it happens afterwards via the `ProviderCard` this then
 * shows up as, same as any built-in preset.
 */
export function AddCustomProviderForm() {
  const addCustomProvider = useModelStore((s) => s.addCustomProvider);
  const { t } = useT();

  const [label, setLabel] = useState("");
  const [baseUrl, setBaseUrl] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const canSubmit = label.trim().length > 0 && baseUrl.trim().length > 0 && !submitting;

  const handleAdd = useCallback(async () => {
    if (!canSubmit) return;
    setSubmitting(true);
    setError(null);
    try {
      await addCustomProvider(label.trim(), baseUrl.trim());
      setLabel("");
      setBaseUrl("");
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setSubmitting(false);
    }
  }, [canSubmit, label, baseUrl, addCustomProvider]);

  return (
    <div className="flex flex-col gap-2 rounded-lg border border-dashed border-border p-3">
      <p className="text-xs font-semibold uppercase tracking-wider text-faint">{t("AddCustomProviderForm.heading")}</p>
      <div className="flex flex-col gap-2 sm:flex-row">
        <input
          type="text"
          value={label}
          onChange={(event) => setLabel(event.target.value)}
          placeholder={t("AddCustomProviderForm.labelPlaceholder")}
          className="h-8 min-w-0 flex-1 rounded-md border border-border bg-surface px-2.5 text-sm text-foreground placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
        />
        <input
          type="text"
          value={baseUrl}
          onChange={(event) => setBaseUrl(event.target.value)}
          placeholder={t("AddCustomProviderForm.baseUrlPlaceholder")}
          className="h-8 min-w-0 flex-[1.5] rounded-md border border-border bg-surface px-2.5 font-mono text-sm text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
        />
        <Button variant="secondary" size="sm" onClick={() => void handleAdd()} disabled={!canSubmit} className="shrink-0">
          {submitting ? t("AddCustomProviderForm.addingButton") : t("AddCustomProviderForm.addButton")}
        </Button>
      </div>
      {error && <p className="text-xs text-danger">{error}</p>}
      <p className="text-xs text-faint">{t("AddCustomProviderForm.helpText")}</p>
    </div>
  );
}
