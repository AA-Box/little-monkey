import { useCallback, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { FolderOpen } from "lucide-react";
import { Button } from "../ui";
import { useModelStore } from "../../store/modelStore";
import { useT } from "../../lib/i18n";

/** Ollama tag charset (mirrors Rust `ollama::validate_tag`). */
function sanitizeName(raw: string): string {
  return raw
    .toLowerCase()
    .replace(/[^a-z0-9._:-]+/g, "-")
    .replace(/^-+|-+$/g, "");
}

function suggestName(path: string): string {
  const base = path.split(/[\\/]/).filter(Boolean).pop() ?? path;
  return sanitizeName(base.replace(/\.gguf$/i, ""));
}

/**
 * Import a local model file/directory into Ollama via `ollama create`,
 * covering both formats Ollama itself can ingest: a single `.gguf` file, or
 * a Safetensors directory (a Hugging Face-style checkout with
 * `config.json` + `*.safetensors` + tokenizer files) — Ollama performs any
 * Safetensors -> GGUF conversion internally. Reuses `ollamaPullProgress`/
 * `ollamaPullError` (keyed by the chosen name) since the backend streams
 * `ollama create` output over the same `ollama://pull-progress` event as
 * `ollama pull`.
 */
export function OllamaImportForm() {
  const { t } = useT();
  const importOllamaModel = useModelStore((s) => s.importOllamaModel);
  const ollamaPullProgress = useModelStore((s) => s.ollamaPullProgress);
  const ollamaPullError = useModelStore((s) => s.ollamaPullError);

  const [sourcePath, setSourcePath] = useState<string | null>(null);
  const [name, setName] = useState("");
  const [importing, setImporting] = useState(false);
  const [pickError, setPickError] = useState<string | null>(null);

  const pick = useCallback(async (kind: "gguf" | "safetensors") => {
    setPickError(null);
    try {
      const selected =
        kind === "gguf"
          ? await open({ multiple: false, filters: [{ name: "GGUF model", extensions: ["gguf"] }] })
          : await open({ multiple: false, directory: true });
      if (!selected || Array.isArray(selected)) return;
      setSourcePath(selected);
      setName((prev) => prev || suggestName(selected));
    } catch (err) {
      setPickError(err instanceof Error ? err.message : String(err));
    }
  }, []);

  const trimmedName = sanitizeName(name.trim());
  const disabled = !sourcePath || !trimmedName || importing;

  const handleImport = useCallback(async () => {
    if (!sourcePath || !trimmedName) return;
    setImporting(true);
    try {
      await importOllamaModel(trimmedName, sourcePath);
      setSourcePath(null);
      setName("");
    } catch {
      // Failure message is captured in `ollamaPullError[trimmedName]` by the store.
    } finally {
      setImporting(false);
    }
  }, [sourcePath, trimmedName, importOllamaModel]);

  const progressLine = trimmedName ? ollamaPullProgress[trimmedName] : undefined;
  const errorMessage = trimmedName ? ollamaPullError[trimmedName] : undefined;

  return (
    <div className="flex flex-col gap-2 rounded-lg border border-border bg-background p-3">
      <p className="text-xs text-muted">{t("OllamaImportForm.description")}</p>

      <div className="flex flex-wrap gap-2">
        <Button variant="secondary" size="sm" onClick={() => void pick("gguf")}>
          <FolderOpen size={14} />
          {t("OllamaImportForm.pickGgufButton")}
        </Button>
        <Button variant="secondary" size="sm" onClick={() => void pick("safetensors")}>
          <FolderOpen size={14} />
          {t("OllamaImportForm.pickSafetensorsButton")}
        </Button>
      </div>
      {pickError && <p className="text-xs text-danger">{pickError}</p>}

      {sourcePath && (
        <>
          <p className="truncate font-mono text-xs text-muted">{sourcePath}</p>
          <div className="flex items-center gap-2">
            <input
              type="text"
              value={name}
              onChange={(event) => setName(event.target.value)}
              placeholder={t("OllamaImportForm.namePlaceholder")}
              className="h-8 min-w-0 flex-1 rounded-md border border-border bg-surface px-2.5 font-mono text-sm text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
            />
            <Button variant="primary" size="sm" onClick={() => void handleImport()} disabled={disabled}>
              {importing ? t("OllamaImportForm.importingButton") : t("OllamaImportForm.importButton")}
            </Button>
          </div>
        </>
      )}

      {importing && progressLine && <p className="truncate font-mono text-xs text-muted">{progressLine}</p>}
      {errorMessage && <p className="text-xs text-danger">{errorMessage}</p>}
    </div>
  );
}
