import { useCallback, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { FolderOpen } from "lucide-react";
import { Button } from "../ui";
import { useModelStore } from "../../store/modelStore";
import type { ModelInfo } from "../../lib/modelRegistry";
import { useT } from "../../lib/i18n";

/**
 * Two ways to add a model outside the curated catalog: pick an already-
 * downloaded `.gguf` file from anywhere on disk (registered as an external
 * reference, never copied), or pull an arbitrary Hugging Face `<org>/<name>`
 * repo + filename (downloaded into the app's models directory, same as a
 * curated pull).
 */
export function AddCustomModelForm() {
  const addExternalModel = useModelStore((s) => s.addExternalModel);
  const download = useModelStore((s) => s.download);
  const { t } = useT();

  const [pickError, setPickError] = useState<string | null>(null);
  const [picking, setPicking] = useState(false);

  const [repo, setRepo] = useState("");
  const [file, setFile] = useState("");
  const [pullError, setPullError] = useState<string | null>(null);
  const [pulling, setPulling] = useState(false);

  const handlePickFile = useCallback(async () => {
    setPickError(null);
    setPicking(true);
    try {
      const selected = await open({
        multiple: false,
        filters: [{ name: "GGUF model", extensions: ["gguf"] }],
      });
      if (!selected || Array.isArray(selected)) return;
      await addExternalModel(selected);
    } catch (err) {
      setPickError(err instanceof Error ? err.message : String(err));
    } finally {
      setPicking(false);
    }
  }, [addExternalModel]);

  const trimmedRepo = repo.trim();
  const trimmedFile = file.trim();
  const pullDisabled = !trimmedRepo || !trimmedFile || pulling;

  const handlePull = useCallback(async () => {
    if (!trimmedRepo || !trimmedFile) return;
    setPullError(null);
    setPulling(true);
    const model: ModelInfo = {
      id: `${trimmedRepo}/${trimmedFile}`,
      name: trimmedFile,
      repo: trimmedRepo,
      file: trimmedFile,
      size_gb: 0,
      tool_calling: true,
      installed: false,
      path: null,
      is_external: false,
      kind: "chat",
    };
    try {
      await download(model);
      setRepo("");
      setFile("");
    } catch (err) {
      setPullError(err instanceof Error ? err.message : String(err));
    } finally {
      setPulling(false);
    }
  }, [trimmedRepo, trimmedFile, download]);

  return (
    <div className="flex flex-col gap-3 rounded-lg border border-border bg-background p-3">
      <div className="flex items-center justify-between gap-2">
        <p className="text-xs text-muted">{t("AddCustomModelForm.openGgufDescription")}</p>
        <Button variant="secondary" size="sm" onClick={() => void handlePickFile()} disabled={picking}>
          <FolderOpen size={14} />
          {picking ? t("AddCustomModelForm.openingButton") : t("AddCustomModelForm.openModelFileButton")}
        </Button>
      </div>
      {pickError && <p className="text-xs text-danger">{pickError}</p>}

      <div className="border-t border-border pt-3">
        <p className="mb-2 text-xs text-muted">{t("AddCustomModelForm.pullDescription")}</p>
        <div className="flex flex-wrap items-center gap-2">
          <input
            type="text"
            value={repo}
            onChange={(event) => setRepo(event.target.value)}
            placeholder={t("AddCustomModelForm.repoPlaceholder")}
            className="h-8 min-w-0 flex-1 rounded-md border border-border bg-surface px-2.5 font-mono text-sm text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
          />
          <input
            type="text"
            value={file}
            onChange={(event) => setFile(event.target.value)}
            placeholder={t("AddCustomModelForm.filePlaceholder")}
            className="h-8 min-w-0 flex-1 rounded-md border border-border bg-surface px-2.5 font-mono text-sm text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
          />
          <Button variant="primary" size="sm" onClick={() => void handlePull()} disabled={pullDisabled}>
            {pulling ? t("AddCustomModelForm.pullingButton") : t("AddCustomModelForm.pullButton")}
          </Button>
        </div>
        {pullError && <p className="mt-1.5 text-xs text-danger">{pullError}</p>}
      </div>
    </div>
  );
}
