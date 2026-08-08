/**
 * Studio's tool section: install a sidecar tool, then run it.
 *
 * Nothing in this file knows what any tool does. The form in the rail is drawn
 * entirely from the manifest the running tool served — that indirection is the
 * point, and it is what lets a new tool arrive as a download rather than as a
 * release of this app. See `src-tauri/src/studio_tools.rs` for the contract and
 * for why a tool is a process rather than a plugin.
 *
 * Installing goes through the Runtime Hub's component path, unchanged: it
 * already downloads, checks a published SHA-256, keeps versions and can roll
 * back, so a tool is never less verified than a runtime is.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { open } from "@tauri-apps/plugin-dialog";
import { Trash2 } from "lucide-react";

import { Button, IconButton, Listbox, StatusPill } from "../ui";
import { SettingsCard } from "./SettingsCard";
import { useT } from "../../lib/i18n";
import { formatBytes, studioClient, type GenerationEntry } from "../../lib/studioClient";
import {
  clampToolNumber,
  missingRequired,
  toolDefaults,
  toolsClient,
  type StudioTool,
  type ToolInput,
  type ToolInputs,
  type ToolManifest,
} from "../../lib/studioTools";
import {
  createM3OperationId,
  runtimeHubClient,
  type M3ComponentCatalogEntry,
} from "../../lib/runtimeHubClient";

interface Props {
  /** Sidebar node to render this tool's settings into, shared with the
   *  generation form so both sections use the same column. */
  railSlot: HTMLElement | null;
}

export function ToolPanel({ railSlot }: Props) {
  const { t } = useT();
  const [tools, setTools] = useState<StudioTool[]>([]);
  const [available, setAvailable] = useState<M3ComponentCatalogEntry[]>([]);
  const [selectedId, setSelectedId] = useState<string>("");
  const [manifest, setManifest] = useState<ToolManifest | null>(null);
  const [values, setValues] = useState<ToolInputs>({});
  const [results, setResults] = useState<GenerationEntry[]>([]);
  const [previews, setPreviews] = useState<Record<string, string>>({});
  const [installing, setInstalling] = useState<string | null>(null);
  const [starting, setStarting] = useState(false);
  const [running, setRunning] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    try {
      setTools(await toolsClient.list());
    } catch (cause) {
      setError(String(cause));
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // The registry is the operator-editable component feed the Runtime Hub
  // already reads. A failure here is not worth an error banner: it only means
  // the "available" list stays empty, and the installed list — the half that
  // matters — is unaffected.
  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const entries = await runtimeHubClient.componentListRegistry({
          operationId: createM3OperationId("studio-tools"),
        });
        if (!cancelled) {
          setAvailable(entries.filter((entry) => entry.kind === "studio_tool"));
        }
      } catch {
        if (!cancelled) setAvailable([]);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  const selected = useMemo(
    () => tools.find((tool) => tool.id === selectedId) ?? null,
    [tools, selectedId],
  );

  // Selecting a tool starts it: the manifest is the only way to know what to
  // draw, and it is served by the running process. The tool is left running
  // afterwards so a second run does not reload its model — "Release memory"
  // is how the user takes that back, exactly as the engine works.
  useEffect(() => {
    if (!selectedId) {
      setManifest(null);
      return;
    }
    let cancelled = false;
    setStarting(true);
    setError(null);
    setManifest(null);
    void (async () => {
      try {
        const served = await toolsClient.manifest(selectedId);
        if (cancelled) return;
        setManifest(served);
        setValues(toolDefaults(served));
      } catch (cause) {
        if (!cancelled) setError(String(cause));
      } finally {
        if (!cancelled) setStarting(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [selectedId]);

  const install = async (entry: M3ComponentCatalogEntry) => {
    setInstalling(entry.componentId);
    setError(null);
    try {
      const component = await runtimeHubClient.componentInstall({
        operationId: createM3OperationId("studio-tool-install"),
        request: { entry },
      });
      // The hub owns the bytes and their versions; the library entry only
      // points at whichever version is active, so an upgrade is the same call
      // again with the new path.
      const active = component.versions.find(
        (version) => version.versionKey === component.activeVersionKey,
      );
      if (!active) throw new Error("The installed tool has no active version");
      setTools(
        await toolsClient.add({
          id: component.componentId,
          name: component.displayName,
          path: active.artifactPath,
          version: active.version,
          managed: true,
        }),
      );
    } catch (cause) {
      setError(String(cause));
    } finally {
      setInstalling(null);
    }
  };

  const addLocal = async () => {
    const picked = await open({ directory: false, multiple: false });
    if (typeof picked !== "string") return;
    setError(null);
    try {
      const name = picked.split(/[/\\]/).pop() ?? picked;
      setTools(
        await toolsClient.add({
          // Derived from the path so re-adding the same binary replaces its
          // entry instead of stacking duplicates.
          id: `local-${name.replace(/[^A-Za-z0-9._-]/g, "-")}`,
          name,
          path: picked,
          version: null,
          managed: false,
        }),
      );
    } catch (cause) {
      setError(String(cause));
    }
  };

  const remove = async (toolId: string) => {
    setError(null);
    try {
      setTools(await toolsClient.remove(toolId));
      if (selectedId === toolId) setSelectedId("");
    } catch (cause) {
      setError(String(cause));
    }
  };

  const setValue = (key: string, value: ToolInputs[string] | undefined) =>
    setValues((current) => {
      if (value === undefined) {
        const { [key]: _dropped, ...rest } = current;
        return rest;
      }
      return { ...current, [key]: value };
    });

  /** The same `FileReader` the generation form uses for its init and
   *  conditioning images: the tool contract wants bare base64, which is the
   *  data URL past its comma. */
  const readImageInto = (key: string, file: File) => {
    const reader = new FileReader();
    reader.onload = () => {
      const result = String(reader.result ?? "");
      const comma = result.indexOf(",");
      if (comma >= 0) setValue(key, result.slice(comma + 1));
    };
    reader.readAsDataURL(file);
  };

  const useNewestResult = async (key: string) => {
    setError(null);
    try {
      // The gallery is stored oldest-first, so the newest run is the last row.
      const gallery = await studioClient.gallery();
      const newest = gallery[gallery.length - 1];
      if (!newest) return;
      const dataUrl = await studioClient.mediaDataUrl(newest.artifactId);
      setValue(key, dataUrl.slice(dataUrl.indexOf(",") + 1));
    } catch (cause) {
      setError(String(cause));
    }
  };

  const missing = manifest ? missingRequired(manifest, values) : [];

  const run = async () => {
    if (!manifest || missing.length > 0) return;
    setRunning(true);
    setError(null);
    try {
      const entries = await toolsClient.run(selectedId, values);
      setResults(entries);
      const shown: Record<string, string> = {};
      for (const entry of entries) {
        shown[entry.artifactId] = await studioClient.mediaDataUrl(entry.artifactId);
      }
      setPreviews(shown);
    } catch (cause) {
      setError(String(cause));
    } finally {
      setRunning(false);
    }
  };

  const installedIds = new Set(tools.map((tool) => tool.id));

  return (
    <div className="flex min-h-0 flex-1 flex-col overflow-y-auto p-4">
      <header className="mb-4">
        <h1 className="text-sm font-medium">{t("Studio.tools.title")}</h1>
        <p className="mt-1 text-xs text-muted">{t("Studio.tools.subtitle")}</p>
      </header>

      {error && (
        <p className="mb-3 rounded border border-danger/40 bg-danger/10 px-3 py-2 text-xs text-danger">
          {error}
        </p>
      )}

      <section className="mb-4">
        <div className="mb-2 flex items-center justify-between">
          <h2 className="text-xs font-medium text-muted">{t("Studio.tools.library")}</h2>
          <Button size="sm" variant="secondary" onClick={() => void addLocal()}>
            {t("Studio.tools.addLocal")}
          </Button>
        </div>
        {tools.length === 0 ? (
          <p className="text-xs text-faint">{t("Studio.tools.empty")}</p>
        ) : (
          <div className="grid gap-2">
            {tools.map((tool) => (
              <div
                key={tool.id}
                className={`flex items-center justify-between gap-2 rounded-lg border p-2.5 ${
                  tool.id === selectedId ? "border-accent bg-accent/5" : "border-border bg-surface"
                }`}
              >
                <button
                  type="button"
                  className="min-w-0 flex-1 text-left"
                  onClick={() => setSelectedId(tool.id)}
                >
                  <span className="flex items-center gap-2">
                    <span className="truncate text-xs font-medium">{tool.name}</span>
                    {/* The pill carries no tooltip of its own, and the
                        difference between a digest-checked download and a
                        binary the user pointed at is worth explaining rather
                        than leaving to one word. */}
                    <span
                      title={
                        tool.managed
                          ? t("Studio.tools.managedHint")
                          : t("Studio.tools.unmanagedHint")
                      }
                    >
                      <StatusPill tone={tool.managed ? "success" : "warning"}>
                        {tool.managed ? t("Studio.tools.managed") : t("Studio.tools.unmanaged")}
                      </StatusPill>
                    </span>
                  </span>
                  {tool.version && (
                    <span className="mt-0.5 block text-[11px] text-faint">
                      {t("Studio.tools.version", { version: tool.version })}
                    </span>
                  )}
                </button>
                <IconButton
                  aria-label={t("Studio.tools.remove")}
                  title={t("Studio.tools.remove")}
                  onClick={() => void remove(tool.id)}
                >
                  <Trash2 size={14} />
                </IconButton>
              </div>
            ))}
          </div>
        )}
      </section>

      <section className="mb-4">
        <h2 className="mb-2 text-xs font-medium text-muted">{t("Studio.tools.available")}</h2>
        {available.length === 0 ? (
          <p className="text-xs text-faint">{t("Studio.tools.noneAvailable")}</p>
        ) : (
          <div className="grid gap-2">
            {available.map((entry) => (
              <div
                key={`${entry.componentId}-${entry.version}`}
                className="flex items-center justify-between gap-2 rounded-lg border border-border bg-surface p-2.5"
              >
                <span className="min-w-0">
                  <span className="block truncate text-xs font-medium">{entry.displayName}</span>
                  <span className="block text-[11px] text-faint">
                    {entry.version} · {formatBytes(entry.sizeBytes)}
                  </span>
                  {entry.compatibilityNote && (
                    <span className="block text-[11px] text-warning">
                      {entry.compatibilityNote}
                    </span>
                  )}
                </span>
                <Button
                  size="sm"
                  variant="secondary"
                  disabled={installing !== null}
                  onClick={() => void install(entry)}
                >
                  {installing === entry.componentId
                    ? t("Studio.tools.installing")
                    : installedIds.has(entry.componentId)
                      ? t("Studio.tools.installed")
                      : t("Studio.tools.install")}
                </Button>
              </div>
            ))}
          </div>
        )}
      </section>

      {results.length > 0 && (
        <section>
          <h2 className="mb-2 text-xs font-medium text-muted">{t("Studio.tools.results")}</h2>
          <div className="grid grid-cols-2 gap-2">
            {results.map((entry) => (
              <figure key={entry.entryId} className="overflow-hidden rounded-lg border border-border">
                {previews[entry.artifactId] && entry.mediaType.startsWith("image/") ? (
                  <img
                    src={previews[entry.artifactId]}
                    alt={entry.prompt}
                    className="block w-full"
                  />
                ) : (
                  <figcaption className="p-2 text-[11px] text-faint">{entry.mediaType}</figcaption>
                )}
              </figure>
            ))}
          </div>
        </section>
      )}

      {railSlot
        ? createPortal(
            <div className="grid content-start gap-3 [&>*]:min-w-0">
              {!selected ? (
                <p className="text-xs text-faint">{t("Studio.tools.select")}</p>
              ) : starting ? (
                <p className="text-xs text-faint">{t("Studio.tools.starting")}</p>
              ) : manifest ? (
                <>
                  <SettingsCard
                    title={manifest.name}
                    hint={manifest.description ?? undefined}
                  >
                    <div className="grid gap-3">
                      {manifest.inputs.map((input) => (
                        <ToolField
                          key={input.key}
                          input={input}
                          value={values[input.key]}
                          onChange={(next) => setValue(input.key, next)}
                          onPickImage={(file) => readImageInto(input.key, file)}
                          onUseNewest={() => void useNewestResult(input.key)}
                        />
                      ))}
                    </div>
                  </SettingsCard>
                  <Button
                    disabled={running || missing.length > 0}
                    onClick={() => void run()}
                    title={
                      missing.length > 0
                        ? t("Studio.tools.missing", { fields: missing.join(", ") })
                        : undefined
                    }
                  >
                    {running ? t("Studio.tools.running") : t("Studio.tools.run")}
                  </Button>
                  <Button
                    size="sm"
                    variant="secondary"
                    onClick={() => void toolsClient.stop()}
                  >
                    {t("Studio.tools.stop")}
                  </Button>
                </>
              ) : null}
            </div>,
            railSlot,
          )
        : null}
    </div>
  );
}

interface FieldProps {
  input: ToolInput;
  value: ToolInputs[string] | undefined;
  onChange: (value: ToolInputs[string] | undefined) => void;
  onPickImage: (file: File) => void;
  onUseNewest: () => void;
}

/**
 * One control, chosen by the kind the manifest declared.
 *
 * The `default` case is deliberate rather than exhaustive-by-type: a tool is
 * untrusted input, and although the backend refuses a manifest with an unknown
 * kind, a control that renders nothing is a better failure here than one that
 * throws inside the form.
 */
function ToolField({ input, value, onChange, onPickImage, onUseNewest }: FieldProps) {
  const { t } = useT();
  const fileInput = useRef<HTMLInputElement | null>(null);
  const label = (
    <span className="flex items-baseline justify-between gap-2">
      <span className="text-[11px] font-medium text-muted">{input.label}</span>
      {input.required && <span className="text-[11px] text-faint">*</span>}
    </span>
  );

  switch (input.kind) {
    case "image":
      return (
        <label className="grid gap-1" title={input.hint ?? undefined}>
          {label}
          {typeof value === "string" && value.length > 0 ? (
            <span className="grid gap-1">
              <img
                src={`data:image/png;base64,${value}`}
                alt={input.label}
                className="max-h-32 w-full rounded border border-border object-contain"
              />
              <Button size="sm" variant="secondary" onClick={() => onChange(undefined)}>
                {t("Studio.tools.clearImage")}
              </Button>
            </span>
          ) : (
            <span className="grid gap-1">
              <input
                ref={fileInput}
                type="file"
                accept="image/png,image/jpeg,image/webp"
                className="hidden"
                onChange={(event) => {
                  const file = event.target.files?.[0];
                  if (file) onPickImage(file);
                  // Cleared so re-picking the same file still fires a change.
                  event.target.value = "";
                }}
              />
              <Button size="sm" variant="secondary" onClick={() => fileInput.current?.click()}>
                {t("Studio.tools.pickImage")}
              </Button>
              <Button size="sm" variant="secondary" onClick={onUseNewest}>
                {t("Studio.tools.fromGallery")}
              </Button>
            </span>
          )}
        </label>
      );
    case "text":
      return (
        <label className="grid gap-1" title={input.hint ?? undefined}>
          {label}
          <input
            type="text"
            className="w-full rounded border border-border bg-surface px-2 py-1 text-xs"
            value={typeof value === "string" ? value : ""}
            onChange={(event) => onChange(event.target.value)}
          />
        </label>
      );
    case "number":
      return (
        <label className="grid gap-1" title={input.hint ?? undefined}>
          {label}
          <input
            type="number"
            className="w-full rounded border border-border bg-surface px-2 py-1 text-xs"
            value={typeof value === "number" ? value : ""}
            min={input.min ?? undefined}
            max={input.max ?? undefined}
            step={input.step ?? undefined}
            onChange={(event) => onChange(clampToolNumber(input, event.target.value))}
          />
        </label>
      );
    case "toggle":
      return (
        <label className="flex items-center gap-2" title={input.hint ?? undefined}>
          <input
            type="checkbox"
            checked={value === true}
            onChange={(event) => onChange(event.target.checked)}
          />
          <span className="text-[11px] font-medium text-muted">{input.label}</span>
        </label>
      );
    case "choice":
      return (
        <label className="grid gap-1" title={input.hint ?? undefined}>
          {label}
          <Listbox
            ariaLabel={input.label}
            value={typeof value === "string" ? value : ""}
            options={input.options.map((option) => ({
              value: option.value,
              label: option.label,
            }))}
            onChange={onChange}
          />
        </label>
      );
    default:
      return null;
  }
}
