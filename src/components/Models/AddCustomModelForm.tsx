import { useCallback, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { CheckCircle2, Download, FolderOpen, Search } from "lucide-react";
import { Button } from "../ui";
import { useModelStore } from "../../store/modelStore";
import type { ProjectorCandidate, ResolvedModelReference } from "../../store/modelStore";
import { formatBytes } from "../../lib/modelRegistry";
import { useT } from "../../lib/i18n";
import { errorMessage } from "../../lib/errors";

/**
 * Two ways to add a model outside the curated catalog: pick an already-
 * downloaded `.gguf` file from anywhere on disk (registered as an external
 * reference, never copied), or resolve and install a public GGUF bundle from
 * an Ollama-style tag or explicit Hugging Face reference.
 */
export function AddCustomModelForm() {
  const addExternalModel = useModelStore((s) => s.addExternalModel);
  const detectProjectors = useModelStore((s) => s.detectProjectors);
  const setProjector = useModelStore((s) => s.setProjector);
  const resolveModelReference = useModelStore((s) => s.resolveModelReference);
  const installModelReference = useModelStore((s) => s.installModelReference);
  const cancelDownload = useModelStore((s) => s.cancelDownload);
  const downloadProgress = useModelStore((s) => s.downloadProgress);
  const { t } = useT();

  const [pickError, setPickError] = useState<string | null>(null);
  const [picking, setPicking] = useState(false);
  const [localModelPath, setLocalModelPath] = useState<string | null>(null);
  const [projectorCandidates, setProjectorCandidates] = useState<ProjectorCandidate[]>([]);
  const [localProjector, setLocalProjector] = useState<string | null>(null);

  const [reference, setReference] = useState("");
  const [resolved, setResolved] = useState<ResolvedModelReference | null>(null);
  const [selectedProjectorSha, setSelectedProjectorSha] = useState<string | undefined>();
  const [resolveError, setResolveError] = useState<string | null>(null);
  const [installError, setInstallError] = useState<string | null>(null);
  const [installedName, setInstalledName] = useState<string | null>(null);
  const [resolving, setResolving] = useState(false);
  const [installing, setInstalling] = useState(false);

  const handlePickFile = useCallback(async () => {
    setPickError(null);
    setPicking(true);
    try {
      const selected = await open({
        multiple: false,
        filters: [{ name: "GGUF model", extensions: ["gguf"] }],
      });
      if (!selected || Array.isArray(selected)) return;
      const model = await addExternalModel(selected);
      setLocalModelPath(model.path);
      const candidates = await detectProjectors(selected);
      setProjectorCandidates(candidates);
      if (candidates.length === 1 && model.path) {
        await setProjector(model.path, candidates[0].path);
        setLocalProjector(candidates[0].file);
      }
    } catch (err) {
      setPickError(errorMessage(err));
    } finally {
      setPicking(false);
    }
  }, [addExternalModel, detectProjectors, setProjector]);

  const handleChooseProjector = useCallback(async () => {
    if (!localModelPath) return;
    const selected = await open({
      multiple: false,
      filters: [{ name: t("AddCustomModelForm.projectorFileFilter"), extensions: ["gguf"] }],
    });
    if (!selected || Array.isArray(selected)) return;
    try {
      const model = await setProjector(localModelPath, selected);
      setLocalProjector(model.components?.projector?.file ?? selected.split(/[\\/]/).pop() ?? selected);
    } catch (err) {
      setPickError(errorMessage(err));
    }
  }, [localModelPath, setProjector, t]);

  const trimmedReference = reference.trim();

  const handleResolve = useCallback(async () => {
    if (!trimmedReference) return;
    setResolveError(null);
    setInstallError(null);
    setInstalledName(null);
    setResolved(null);
    setSelectedProjectorSha(undefined);
    setResolving(true);
    try {
      const next = await resolveModelReference(trimmedReference);
      setResolved(next);
      setSelectedProjectorSha(next.projectorCandidates?.length === 1 ? next.projectorCandidates[0].sha256 : undefined);
    } catch (err) {
      setResolveError(errorMessage(err));
    } finally {
      setResolving(false);
    }
  }, [trimmedReference, resolveModelReference]);

  const handleInstall = useCallback(async () => {
    if (!resolved) return;
    setInstallError(null);
    setInstalledName(null);
    setInstalling(true);
    try {
      const installed = await installModelReference(
        trimmedReference,
        resolved.sha256,
        selectedProjectorSha,
      );
      setInstalledName(installed.name);
    } catch (err) {
      setInstallError(errorMessage(err));
    } finally {
      setInstalling(false);
    }
  }, [resolved, trimmedReference, installModelReference, selectedProjectorSha]);

  const progress = resolved
    ? downloadProgress[trimmedReference]
      ?? downloadProgress[resolved.canonicalReference]
      ?? downloadProgress[resolved.fileName]
    : undefined;
  const progressTotal = progress && progress.total > 0 ? progress.total : resolved?.sizeBytes ?? 0;
  const progressPct =
    installing && progress && progressTotal > 0
      ? Math.min(100, Math.round((progress.downloaded / progressTotal) * 100))
      : 0;

  return (
    <div className="flex flex-col gap-3 rounded-lg border border-border bg-background p-3">
      <div className="flex items-center justify-between gap-2">
        <p className="text-xs text-muted">{t("AddCustomModelForm.openGgufDescription")}</p>
        <Button variant="secondary" size="sm" onClick={() => void handlePickFile()} disabled={picking}>
          <FolderOpen size={14} />
          {picking ? t("AddCustomModelForm.openingButton") : t("AddCustomModelForm.openModelFileButton")}
        </Button>
      </div>
      <div className="flex items-center justify-between gap-2 text-xs text-muted">
        <span>
          {t("AddCustomModelForm.projectorLabel")}:{" "}
          {localProjector ?? (projectorCandidates.length > 0 ? `${t("AddCustomModelForm.detectedProjector")}: ${projectorCandidates.map((candidate) => candidate.file).join(", ")}` : t("AddCustomModelForm.noProjector"))}
        </span>
        <Button variant="ghost" size="sm" onClick={() => void handleChooseProjector()} disabled={!localModelPath || picking}>
          {t("AddCustomModelForm.chooseProjectorButton")}
        </Button>
      </div>
      {pickError && <p className="text-xs text-danger">{pickError}</p>}

      <div className="border-t border-border pt-3">
        <p className="text-xs text-muted">{t("AddCustomModelForm.referenceDescription")}</p>
        <p id="model-reference-help" className="mt-1 text-[11px] text-faint">
          {t("AddCustomModelForm.publicSingleFileOnly")}
        </p>
        <label htmlFor="model-reference" className="mt-3 block text-xs font-medium text-foreground">
          {t("AddCustomModelForm.referenceLabel")}
        </label>
        <div className="mt-1.5 flex flex-wrap items-center gap-2">
          <input
            id="model-reference"
            type="text"
            value={reference}
            onChange={(event) => {
              setReference(event.target.value);
              setResolved(null);
              setResolveError(null);
              setInstallError(null);
              setInstalledName(null);
            }}
            placeholder={t("AddCustomModelForm.referencePlaceholder")}
            aria-describedby="model-reference-help model-reference-examples"
            className="h-8 min-w-0 flex-1 rounded-md border border-border bg-surface px-2.5 font-mono text-sm text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
            disabled={resolving || installing}
          />
          <Button
            variant="secondary"
            size="sm"
            onClick={() => void handleResolve()}
            disabled={!trimmedReference || resolving || installing}
          >
            <Search size={14} />
            {resolving
              ? t("AddCustomModelForm.resolvingButton")
              : t("AddCustomModelForm.resolveButton")}
          </Button>
        </div>
        <p id="model-reference-examples" className="mt-1.5 text-[11px] text-faint">
          {t("AddCustomModelForm.examplesLabel")}{" "}
          <code>llama3.2:3b</code>
          {" · "}
          <code>hf.co/Qwen/Qwen2.5-Coder-0.5B-Instruct-GGUF:Q4_K_M</code>
        </p>
        {resolveError && <p className="mt-2 text-xs text-danger">{resolveError}</p>}

        {resolved && (
          <div className="mt-3 rounded-md border border-border bg-surface p-3">
            <ResolvedModelReferenceDetails
              resolved={resolved}
              selectedProjectorSha={selectedProjectorSha}
              onProjectorChange={setSelectedProjectorSha}
            />

            {installing && (
              <div className="mt-3">
                <div
                  className="h-1.5 overflow-hidden rounded-full bg-surface-2"
                  role="progressbar"
                  aria-valuemin={0}
                  aria-valuemax={100}
                  aria-valuenow={progressPct}
                >
                  <div
                    className="h-full rounded-full bg-accent transition-[width] duration-300"
                    style={{ width: `${progressPct}%` }}
                  />
                </div>
                <p className="mt-1 text-right text-[11px] text-muted">
                  {progress
                    ? t("AddCustomModelForm.installProgress", {
                        downloaded: formatBytes(progress.downloaded),
                        total: formatBytes(progressTotal),
                        pct: progressPct,
                      })
                    : t("AddCustomModelForm.preparingInstall")}
                </p>
                <div className="mt-2 flex justify-end">
                  <Button
                    variant="danger"
                    size="sm"
                    onClick={() => void cancelDownload(trimmedReference)}
                  >
                    {t("AddCustomModelForm.cancelInstallButton")}
                  </Button>
                </div>
              </div>
            )}

            <div className="mt-3 flex flex-wrap items-center justify-between gap-2">
              <div className="min-w-0">
                {installError && <p className="text-xs text-danger">{installError}</p>}
                {installedName && (
                  <p className="flex items-center gap-1 text-xs text-success">
                    <CheckCircle2 size={13} />
                    {t("AddCustomModelForm.installedMessage", { name: installedName })}
                  </p>
                )}
              </div>
              <Button
                variant="primary"
                size="sm"
                onClick={() => void handleInstall()}
                disabled={
                  installing ||
                  installedName !== null ||
                  (resolved.projectorCandidates?.length ?? 0) > 1 && !selectedProjectorSha
                }
              >
                <Download size={14} />
                {installing
                  ? t("AddCustomModelForm.installingButton")
                  : installedName
                    ? t("AddCustomModelForm.installedButton")
                    : t("AddCustomModelForm.installButton")}
              </Button>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}

export function resolvedModelSourceKey(source: string): string | null {
  const normalized = source.toLowerCase().replace(/-/g, "_");
  if (normalized.includes("ollama")) return "AddCustomModelForm.sourceOllama";
  if (normalized === "hf" || normalized.includes("hugging")) {
    return "AddCustomModelForm.sourceHuggingFace";
  }
  return null;
}

export function ResolvedModelReferenceDetails({
  resolved,
  selectedProjectorSha,
  onProjectorChange,
}: {
  resolved: ResolvedModelReference;
  selectedProjectorSha?: string;
  onProjectorChange?: (sha256: string | undefined) => void;
}) {
  const { t } = useT();
  const sourceKey = resolvedModelSourceKey(resolved.source);
  const licenseName = resolved.licenseName?.trim() || t("AddCustomModelForm.licenseUnknown");

  return (
    <>
      <div className="flex flex-wrap items-start justify-between gap-2">
        <div className="min-w-0">
          <p className="text-sm font-medium text-foreground">{resolved.displayName}</p>
          <p className="mt-0.5 truncate font-mono text-[11px] text-faint" title={resolved.canonicalReference}>
            {resolved.canonicalReference}
          </p>
        </div>
        <span className="rounded-full bg-surface-2 px-2 py-0.5 text-[10px] font-medium uppercase tracking-wide text-muted">
          {sourceKey ? t(sourceKey) : resolved.source}
        </span>
      </div>
      <dl className="mt-3 grid gap-x-4 gap-y-2 text-xs sm:grid-cols-2">
        <div>
          <dt className="text-faint">{t("AddCustomModelForm.fileLabel")}</dt>
          <dd className="mt-0.5 break-all font-mono text-foreground">{resolved.fileName}</dd>
        </div>
        <div>
          <dt className="text-faint">{t("AddCustomModelForm.sizeLabel")}</dt>
          <dd className="mt-0.5 text-foreground">{formatBytes(resolved.sizeBytes)}</dd>
        </div>
        <div>
          <dt className="text-faint">SHA-256</dt>
          <dd
            title={resolved.sha256}
            className="mt-0.5 break-all font-mono text-[10px] text-foreground"
          >
            {resolved.sha256}
          </dd>
        </div>
        <div>
          <dt className="text-faint">{t("AddCustomModelForm.licenseLabel")}</dt>
          <dd className="mt-0.5 text-foreground">
            {resolved.licenseUrl ? (
              <a
                href={resolved.licenseUrl}
                target="_blank"
                rel="noreferrer"
                className="text-accent hover:underline"
              >
                {licenseName}
              </a>
            ) : (
              licenseName
            )}
          </dd>
        </div>
        <div>
          <dt className="text-faint">{t("AddCustomModelForm.toolCallingLabel")}</dt>
          <dd className="mt-0.5 text-foreground">
            {resolved.toolCalling
              ? t("AddCustomModelForm.toolCallingSupported")
              : t("AddCustomModelForm.toolCallingNotAdvertised")}
          </dd>
        </div>
      </dl>
      {resolved.artifacts && resolved.artifacts.length > 1 && (
        <div className="mt-3 border-t border-border pt-2 text-xs">
          {resolved.artifacts.slice(1).map((artifact) => (
            <p key={artifact.sha256} className="text-muted">
              {t("AddCustomModelForm.projectorLabel")}: <span className="font-mono text-foreground">{artifact.fileName}</span> ({formatBytes(artifact.sizeBytes)})
            </p>
          ))}
        </div>
      )}
      {resolved.projectorCandidates && resolved.projectorCandidates.length > 1 && (
        <label className="mt-3 block text-xs text-muted">
          {t("AddCustomModelForm.projectorLabel")}
          <select className="mt-1 block w-full rounded-md border border-border bg-background p-1.5 text-foreground" value={selectedProjectorSha ?? ""} onChange={(event) => onProjectorChange?.(event.target.value || undefined)}>
            <option value="" disabled>{t("AddCustomModelForm.chooseProjectorOption")}</option>
            {resolved.projectorCandidates.map((candidate) => (
              <option key={candidate.sha256} value={candidate.sha256}>{candidate.fileName}</option>
            ))}
          </select>
        </label>
      )}
    </>
  );
}
