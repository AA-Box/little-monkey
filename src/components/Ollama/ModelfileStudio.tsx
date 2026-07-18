import { useCallback, useEffect, useRef, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { FileText, FolderOpen, ScrollText } from "lucide-react";
import { Button, StatusPill } from "../ui";
import { useModelStore } from "../../store/modelStore";
import { modelfileClient, type DetectedFormat, type ModelfileDryRunReport } from "../../lib/modelfileClient";
import { useT } from "../../lib/i18n";

/** Mirrors Rust `modelfile::validate_short_name`'s accepted charset — used
 * only to suggest a short name from a picked file's basename, not to
 * enforce it (the real check happens server-side in `modelfile_dry_run`/
 * `ollama_create_from_modelfile`, which is the source of truth). */
function suggestShortName(path: string): string {
  const base = path.split(/[\\/]/).filter(Boolean).pop() ?? path;
  return base
    .replace(/\.(gguf|safetensors)$/i, "")
    .toLowerCase()
    .replace(/[^a-z0-9._:-]+/g, "-")
    .replace(/^-+|-+$/g, "");
}

/** Appends `line` on its own line, adding a separating blank line first only
 * when `text` already has content that doesn't already end in a newline. */
function appendBlock(text: string, block: string): string {
  if (text.length === 0) return `${block}\n`;
  return text.endsWith("\n") ? `${text}${block}\n` : `${text}\n${block}\n`;
}

const FORMAT_KEY: Record<DetectedFormat, string> = {
  gguf: "ModelfileStudio.format.gguf",
  safetensorsFile: "ModelfileStudio.format.safetensorsFile",
  safetensorsDirectory: "ModelfileStudio.format.safetensorsDirectory",
  existingModelReference: "ModelfileStudio.format.existingModelReference",
};

function formatBytes(bytes: number): string {
  if (bytes <= 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  const exponent = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1);
  const value = bytes / 1024 ** exponent;
  return `${exponent === 0 ? value : value.toFixed(1)} ${units[exponent]}`;
}

/** Live grammar-only feedback debounce, in ms — cheap enough per the Rust
 * `modelfile_parse` doc comment ("cheap enough to call on every keystroke
 * debounce"), but still debounced so a fast typist doesn't fire one call per
 * character. */
const LIVE_PARSE_DEBOUNCE_MS = 400;

/**
 * Modelfile Studio (Phase 8 "Modelfile Studio and Import Hardening"): a real
 * Modelfile text editor with live grammar feedback, a full preview/validate
 * dry run (short-name check, semantic validation, GGUF/safetensors source
 * sniffing), and a hardened create step that's only enabled once a dry run
 * has succeeded for the *exact* short name + text currently in the editor —
 * editing either after a successful preview requires re-previewing before
 * "Create Model" re-enables, satisfying the acceptance criterion that a
 * custom model package is previewed and validated before it enters the
 * model library.
 *
 * Reuses `modelStore`'s existing `ollamaPullProgress`/`ollamaPullError`
 * (keyed by short name) for the create step, since
 * `ollama_create_from_modelfile` streams progress over the same
 * `ollama://pull-progress` event as `ollama pull`/`ollama create` already do.
 */
export function ModelfileStudio() {
  const { t } = useT();
  const createModelfileModel = useModelStore((s) => s.createModelfileModel);
  const ollamaPullProgress = useModelStore((s) => s.ollamaPullProgress);
  const ollamaPullError = useModelStore((s) => s.ollamaPullError);

  const [shortName, setShortName] = useState("");
  const [modelfileText, setModelfileText] = useState("");
  const [pickError, setPickError] = useState<string | null>(null);

  const [grammarIssue, setGrammarIssue] = useState<string | null>(null);
  const debounceRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  const [validating, setValidating] = useState(false);
  const [dryRunError, setDryRunError] = useState<string | null>(null);
  const [dryRunReport, setDryRunReport] = useState<ModelfileDryRunReport | null>(null);
  /** The exact (shortName, modelfileText) pair the last successful dry run
   * covered — "Create Model" only ever enables when the editor still
   * matches this pair exactly. */
  const [validatedFor, setValidatedFor] = useState<{ shortName: string; modelfileText: string } | null>(null);

  const [creating, setCreating] = useState(false);
  const [createdName, setCreatedName] = useState<string | null>(null);

  // Live grammar-only feedback: debounced, and only ever reports a
  // structural parse error (not the "missing FROM"/semantic checks the dry
  // run covers) — an empty editor is simply not flagged.
  useEffect(() => {
    if (debounceRef.current) clearTimeout(debounceRef.current);
    if (modelfileText.trim().length === 0) {
      setGrammarIssue(null);
      return;
    }
    debounceRef.current = setTimeout(() => {
      modelfileClient
        .parse(modelfileText)
        .then(() => setGrammarIssue(null))
        .catch((err: unknown) => setGrammarIssue(err instanceof Error ? err.message : String(err)));
    }, LIVE_PARSE_DEBOUNCE_MS);
    return () => {
      if (debounceRef.current) clearTimeout(debounceRef.current);
    };
  }, [modelfileText]);

  const invalidateValidation = useCallback(() => {
    setDryRunReport(null);
    setDryRunError(null);
    setValidatedFor(null);
    setCreatedName(null);
  }, []);

  const handleShortNameChange = useCallback(
    (value: string) => {
      setShortName(value);
      invalidateValidation();
    },
    [invalidateValidation],
  );

  const handleTextChange = useCallback(
    (value: string) => {
      setModelfileText(value);
      invalidateValidation();
    },
    [invalidateValidation],
  );

  const pickFromFile = useCallback(async () => {
    setPickError(null);
    try {
      const selected = await open({ multiple: false, filters: [{ name: "GGUF model", extensions: ["gguf"] }] });
      if (!selected || Array.isArray(selected)) return;
      setModelfileText((prev) => {
        const next = appendBlock(prev, `FROM ${selected}`);
        invalidateValidation();
        return next;
      });
      setShortName((prev) => prev || suggestShortName(selected));
    } catch (err) {
      setPickError(err instanceof Error ? err.message : String(err));
    }
  }, [invalidateValidation]);

  const pickFromFolder = useCallback(async () => {
    setPickError(null);
    try {
      const selected = await open({ multiple: false, directory: true });
      if (!selected || Array.isArray(selected)) return;
      setModelfileText((prev) => {
        const next = appendBlock(prev, `FROM ${selected}`);
        invalidateValidation();
        return next;
      });
      setShortName((prev) => prev || suggestShortName(selected));
    } catch (err) {
      setPickError(err instanceof Error ? err.message : String(err));
    }
  }, [invalidateValidation]);

  const insertTextFileAs = useCallback(
    async (instruction: "SYSTEM" | "LICENSE") => {
      setPickError(null);
      try {
        const selected = await open({ multiple: false });
        if (!selected || Array.isArray(selected)) return;
        const content = await modelfileClient.readTextFile(selected);
        setModelfileText((prev) => {
          const next = appendBlock(prev, `${instruction} """\n${content}\n"""`);
          invalidateValidation();
          return next;
        });
      } catch (err) {
        setPickError(err instanceof Error ? err.message : String(err));
      }
    },
    [invalidateValidation],
  );

  const trimmedShortName = shortName.trim();
  const canValidate = trimmedShortName.length > 0 && modelfileText.trim().length > 0 && !validating;

  const handleValidate = useCallback(async () => {
    if (!canValidate) return;
    setValidating(true);
    setDryRunError(null);
    try {
      const report = await modelfileClient.dryRun({ shortName: trimmedShortName, modelfileText });
      setDryRunReport(report);
      setValidatedFor({ shortName: trimmedShortName, modelfileText });
    } catch (err) {
      setDryRunReport(null);
      setValidatedFor(null);
      setDryRunError(err instanceof Error ? err.message : String(err));
    } finally {
      setValidating(false);
    }
  }, [canValidate, trimmedShortName, modelfileText]);

  const isValidated =
    validatedFor !== null &&
    validatedFor.shortName === trimmedShortName &&
    validatedFor.modelfileText === modelfileText;
  const canCreate = isValidated && !creating;

  const handleCreate = useCallback(async () => {
    if (!canCreate) return;
    setCreating(true);
    setCreatedName(null);
    try {
      await createModelfileModel(trimmedShortName, modelfileText);
      setCreatedName(trimmedShortName);
      setShortName("");
      setModelfileText("");
      setDryRunReport(null);
      setValidatedFor(null);
    } catch {
      // Failure message is captured in `ollamaPullError[trimmedShortName]` by the store.
    } finally {
      setCreating(false);
    }
  }, [canCreate, trimmedShortName, modelfileText, createModelfileModel]);

  const progressLine = trimmedShortName ? ollamaPullProgress[trimmedShortName] : undefined;
  const createError = trimmedShortName ? ollamaPullError[trimmedShortName] : undefined;

  return (
    <div className="flex flex-col gap-2 rounded-lg border border-border bg-background p-3">
      <p className="text-xs text-muted">{t("ModelfileStudio.description")}</p>

      <div className="flex flex-wrap gap-2">
        <Button variant="secondary" size="sm" onClick={() => void pickFromFile()}>
          <FolderOpen size={14} />
          {t("ModelfileStudio.addFromFileButton")}
        </Button>
        <Button variant="secondary" size="sm" onClick={() => void pickFromFolder()}>
          <FolderOpen size={14} />
          {t("ModelfileStudio.addFromFolderButton")}
        </Button>
        <Button variant="secondary" size="sm" onClick={() => void insertTextFileAs("SYSTEM")}>
          <FileText size={14} />
          {t("ModelfileStudio.addSystemFileButton")}
        </Button>
        <Button variant="secondary" size="sm" onClick={() => void insertTextFileAs("LICENSE")}>
          <ScrollText size={14} />
          {t("ModelfileStudio.addLicenseFileButton")}
        </Button>
      </div>
      {pickError && <p className="text-xs text-danger">{pickError}</p>}

      <input
        type="text"
        value={shortName}
        onChange={(event) => handleShortNameChange(event.target.value)}
        placeholder={t("ModelfileStudio.shortNamePlaceholder")}
        className="h-8 min-w-0 rounded-md border border-border bg-surface px-2.5 font-mono text-sm text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
      />

      <textarea
        value={modelfileText}
        onChange={(event) => handleTextChange(event.target.value)}
        placeholder={t("ModelfileStudio.textareaPlaceholder")}
        rows={10}
        spellCheck={false}
        className="min-h-[10rem] w-full resize-y rounded-md border border-border bg-surface px-2.5 py-2 font-mono text-xs text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
      />
      {grammarIssue && <p className="text-xs text-danger">{grammarIssue}</p>}

      <div className="flex flex-wrap items-center gap-2">
        <Button variant="secondary" size="sm" onClick={() => void handleValidate()} disabled={!canValidate}>
          {validating ? t("ModelfileStudio.validatingButton") : t("ModelfileStudio.validateButton")}
        </Button>
        <Button variant="primary" size="sm" onClick={() => void handleCreate()} disabled={!canCreate}>
          {creating ? t("ModelfileStudio.creatingButton") : t("ModelfileStudio.createButton")}
        </Button>
        {dryRunReport && !isValidated && (
          <span className="text-xs text-warning">{t("ModelfileStudio.staleValidationHint")}</span>
        )}
      </div>

      {dryRunError && <p className="text-xs text-danger">{dryRunError}</p>}

      {dryRunReport && (
        <div className="flex flex-col gap-1.5 rounded-md border border-border bg-surface p-2.5 text-xs">
          <p className="font-mono text-foreground">
            {t("ModelfileStudio.fromLabel", { value: dryRunReport.from ?? "—" })}
          </p>
          {dryRunReport.source && (
            <p className="text-muted">
              {t(FORMAT_KEY[dryRunReport.source.format])}
              {dryRunReport.source.sizeBytes > 0 ? ` · ${formatBytes(dryRunReport.source.sizeBytes)}` : ""}
            </p>
          )}
          {dryRunReport.requires && (
            <p className="text-muted">{t("ModelfileStudio.requiresLabel", { version: dryRunReport.requires })}</p>
          )}
          <div className="flex flex-wrap gap-1.5">
            <StatusPill tone={dryRunReport.templatePresent ? "success" : "neutral"}>
              {t("ModelfileStudio.templateLabel")}
            </StatusPill>
            <StatusPill tone={dryRunReport.systemPresent ? "success" : "neutral"}>
              {t("ModelfileStudio.systemLabel")}
            </StatusPill>
            <StatusPill tone={dryRunReport.licensePresent ? "success" : "neutral"}>
              {t("ModelfileStudio.licenseLabel")}
            </StatusPill>
            <StatusPill tone="neutral">
              {t("ModelfileStudio.parametersCountLabel", { count: dryRunReport.parameters.length })}
            </StatusPill>
            <StatusPill tone="neutral">
              {t("ModelfileStudio.adaptersCountLabel", { count: dryRunReport.adapters.length })}
            </StatusPill>
            <StatusPill tone="neutral">
              {t("ModelfileStudio.messagesCountLabel", { count: dryRunReport.messagesCount })}
            </StatusPill>
          </div>
          {dryRunReport.warnings.length > 0 && (
            <ul className="list-inside list-disc text-warning">
              {dryRunReport.warnings.map((warning) => (
                <li key={warning}>{warning}</li>
              ))}
            </ul>
          )}
        </div>
      )}

      {creating && progressLine && <p className="truncate font-mono text-xs text-muted">{progressLine}</p>}
      {createError && <p className="text-xs text-danger">{createError}</p>}
      {createdName && !createError && (
        <StatusPill tone="success">{t("ModelfileStudio.createdMessage", { name: createdName })}</StatusPill>
      )}
    </div>
  );
}
