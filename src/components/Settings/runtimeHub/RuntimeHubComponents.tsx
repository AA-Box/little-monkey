import { useRef, useState } from "react";
import { ArchiveRestore, Download, Info, PackagePlus, RefreshCw, ShieldCheck } from "lucide-react";
import { Button, StatusPill, type PillTone } from "../../ui";
import type {
  M3ComponentCatalogEntry,
  M3ComponentChannel,
  M3ComponentUpdateCheck,
  M3InstalledComponent,
} from "../../../lib/runtimeHubClient";
import { useRuntimeHubStore } from "../../../store/runtimeHubStore";
import { BusyButton, ErrorNotice, formatBytes, formatDate, labelize, SectionHeading } from "./RuntimeHubShared";

/** What a registry entry's "Install"/"Update" button should say and do,
 * given what (if anything) is already installed for that component id. */
export type ComponentRegistryAction = "install" | "update" | "current";

export function describeRegistryAction(
  entry: M3ComponentCatalogEntry,
  installedComponents: M3InstalledComponent[],
): ComponentRegistryAction {
  const installed = installedComponents.find((component) => component.componentId === entry.componentId);
  if (!installed) return "install";
  const active = installed.versions.find((version) => version.active);
  if (active && active.version === entry.version && active.sha256 === entry.sha256) {
    return "current";
  }
  return "update";
}

const CHANNEL_TONE: Record<M3ComponentChannel, PillTone> = {
  stable: "success",
  beta: "neutral",
  pinned: "warning",
};

function ChannelPill({ channel }: { channel: M3ComponentChannel }) {
  return <StatusPill tone={CHANNEL_TONE[channel]}>{labelize(channel)}</StatusPill>;
}

function CompatibilityNote({ note }: { note: string | null }) {
  if (!note) return null;
  return (
    <p className="mt-2 flex items-start gap-1.5 rounded-md border border-border bg-surface-2 p-2 text-xs leading-5 text-muted">
      <Info size={13} className="mt-0.5 shrink-0" aria-hidden="true" />
      <span className="min-w-0 break-words">{note}</span>
    </p>
  );
}

function InstalledComponentCard({
  component,
  updateCheck,
}: {
  component: M3InstalledComponent;
  updateCheck?: M3ComponentUpdateCheck;
}) {
  const activateVersion = useRuntimeHubStore((state) => state.activateComponentVersion);
  const installComponent = useRuntimeHubStore((state) => state.installComponent);
  const busy = useRuntimeHubStore((state) => state.busy);
  const errors = useRuntimeHubStore((state) => state.errors);
  const active = component.versions.find((version) => version.active);
  const activateKey = `component-activate:${component.componentId}`;
  const installKey = `component-install:${component.componentId}`;

  return (
    <article className="rounded-lg border border-border bg-background p-4">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0">
          <div className="flex flex-wrap items-center gap-2">
            <h4 className="break-words text-sm font-semibold text-foreground">{component.displayName}</h4>
            <ChannelPill channel={component.channel} />
            {component.accelerator && (
              <span className="rounded-md border border-border px-2 py-0.5 font-mono text-xs text-muted">
                {labelize(component.accelerator)}
              </span>
            )}
          </div>
          <p className="mt-1 break-all font-mono text-xs text-muted">
            {component.componentId} · {labelize(component.kind)}
          </p>
        </div>
        <StatusPill tone="success">Verified</StatusPill>
      </div>

      <div className="mt-3 grid gap-2 text-xs text-muted sm:grid-cols-2">
        <span>{component.versions.length} installed version{component.versions.length === 1 ? "" : "s"}</span>
        <span>Version {active?.version ?? "unknown"}</span>
        <span>{formatBytes(active?.sizeBytes)}</span>
        <span>Installed {formatDate(active?.installedAtMs)}</span>
      </div>

      <CompatibilityNote note={active?.compatibilityNote ?? null} />

      {updateCheck?.updateAvailable && updateCheck.latestAvailable && (
        <div className="mt-3 rounded-md border border-warning/30 bg-warning-soft p-3">
          <p className="text-xs leading-5 text-warning">
            Version {updateCheck.latestAvailable.version} is available on the {labelize(updateCheck.channel)} channel.
          </p>
          <div className="mt-2 flex justify-end">
            <BusyButton
              type="button"
              variant="primary"
              busy={busy[installKey]}
              onClick={() => void installComponent(updateCheck.latestAvailable!).catch(() => {})}
            >
              <Download size={15} aria-hidden="true" /> Install update
            </BusyButton>
          </div>
        </div>
      )}
      {component.channel === "pinned" && (
        <p className="mt-3 text-xs text-muted">Pinned components never auto-upgrade. Install a different version to change it.</p>
      )}

      <div className="mt-4 flex flex-col gap-2" aria-label={`Installed versions of ${component.displayName}`}>
        {component.versions
          .slice()
          .sort((left, right) => right.installedAtMs - left.installedAtMs)
          .map((version) => (
            <div key={version.versionKey} className="flex flex-wrap items-center justify-between gap-3 rounded-md border border-border bg-surface-2 p-3">
              <div className="min-w-0">
                <div className="flex flex-wrap items-center gap-2">
                  <p className="text-xs font-medium text-foreground">Version {version.version}</p>
                  {version.active && <StatusPill tone="success">Active</StatusPill>}
                </div>
                <p className="mt-1 break-all font-mono text-[11px] text-muted">
                  {version.versionKey.slice(0, 16)}… · {formatBytes(version.sizeBytes)} · {formatDate(version.installedAtMs)}
                </p>
              </div>
              {!version.active && (
                <BusyButton
                  type="button"
                  busy={busy[activateKey]}
                  onClick={() => void activateVersion(component.componentId, version.versionKey).catch(() => {})}
                >
                  <ArchiveRestore size={15} aria-hidden="true" /> Roll back to this version
                </BusyButton>
              )}
            </div>
          ))}
      </div>
      <ErrorNotice message={errors[activateKey]} />
      <ErrorNotice message={errors[installKey]} />
    </article>
  );
}

function RegistryEntryCard({ entry }: { entry: M3ComponentCatalogEntry }) {
  const installedComponents = useRuntimeHubStore((state) => state.installedComponents);
  const installComponent = useRuntimeHubStore((state) => state.installComponent);
  const busy = useRuntimeHubStore((state) => state.busy);
  const errors = useRuntimeHubStore((state) => state.errors);
  const action = describeRegistryAction(entry, installedComponents);
  const key = `component-install:${entry.componentId}`;

  return (
    <article className="rounded-lg border border-border bg-background p-4">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0">
          <div className="flex flex-wrap items-center gap-2">
            <h4 className="break-words text-sm font-semibold text-foreground">{entry.displayName}</h4>
            <ChannelPill channel={entry.channel} />
          </div>
          <p className="mt-1 break-all font-mono text-xs text-muted">
            {entry.componentId} · {labelize(entry.kind)} · v{entry.version}
          </p>
        </div>
        <span className="rounded-md border border-border px-2 py-1 font-mono text-xs text-muted">
          {formatBytes(entry.sizeBytes)}
        </span>
      </div>
      <CompatibilityNote note={entry.compatibilityNote} />
      <ErrorNotice message={errors[key]} />
      <div className="mt-4 flex justify-end">
        {action === "current" ? (
          <Button type="button" className="min-h-11" disabled>
            <ShieldCheck size={15} aria-hidden="true" /> Installed
          </Button>
        ) : (
          <BusyButton
            type="button"
            variant="primary"
            busy={busy[key]}
            onClick={() => void installComponent(entry).catch(() => {})}
          >
            <PackagePlus size={15} aria-hidden="true" /> {action === "update" ? "Install this version" : "Install"}
          </BusyButton>
        )}
      </div>
    </article>
  );
}

/** One registry row's identity. Two entries are the same known version when
 *  all three match — the same key `RegistryEntryCard` is listed by. */
function entryKey(entry: M3ComponentCatalogEntry): string {
  return `${entry.componentId}:${entry.version}:${entry.sha256}`;
}

/**
 * Folds an imported catalog into the registry the app already holds.
 *
 * A merge rather than a replace: `m3_component_replace_registry_entries` swaps
 * the whole file atomically, so importing an MLX catalog over a registry that
 * already lists a llama.cpp build would otherwise delete it. Imported entries
 * win on a key collision, which is how a publisher corrects a bad URL or note
 * for a version already registered.
 */
export function mergeRegistryEntries(
  existing: M3ComponentCatalogEntry[],
  imported: M3ComponentCatalogEntry[],
): M3ComponentCatalogEntry[] {
  const merged = new Map(existing.map((entry) => [entryKey(entry), entry]));
  for (const entry of imported) merged.set(entryKey(entry), entry);
  return [...merged.values()];
}

/**
 * Reads a published catalog file into entries.
 *
 * Only the shape is checked here, and only enough to tell "this is not a
 * catalog" from "this catalog is invalid": the backend re-validates every field
 * and is the authority on digests, URLs and channels. Duplicating that would be
 * two rulesets to keep in step.
 */
export function parseCatalogText(text: string): M3ComponentCatalogEntry[] {
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch {
    return raise("That file is not valid JSON.");
  }
  // A catalog is a bare array. The registry file the app writes wraps entries in
  // `{schemaVersion, entries}`, so accept that shape too — it is what someone
  // re-importing a backup of their own registry will have.
  const entries = Array.isArray(parsed)
    ? parsed
    : (parsed as { entries?: unknown })?.entries;
  if (!Array.isArray(entries)) {
    return raise("That file has no catalog entries in it.");
  }
  if (
    !entries.every(
      (entry) =>
        !!entry &&
        typeof entry === "object" &&
        typeof (entry as M3ComponentCatalogEntry).componentId === "string" &&
        typeof (entry as M3ComponentCatalogEntry).version === "string" &&
        typeof (entry as M3ComponentCatalogEntry).sha256 === "string",
    )
  ) {
    return raise("That file is not a component catalog.");
  }
  return entries as M3ComponentCatalogEntry[];
}

function raise(message: string): never {
  throw new Error(message);
}

export function RuntimeHubComponents() {
  const installedComponents = useRuntimeHubStore((state) => state.installedComponents);
  const componentRegistry = useRuntimeHubStore((state) => state.componentRegistry);
  const componentUpdateChecks = useRuntimeHubStore((state) => state.componentUpdateChecks);
  const refreshComponents = useRuntimeHubStore((state) => state.refreshComponents);
  const refreshing = useRuntimeHubStore((state) => state.busy.components);
  const error = useRuntimeHubStore((state) => state.errors.components);
  const replaceComponentRegistry = useRuntimeHubStore((state) => state.replaceComponentRegistry);
  const [showInstalledOnly, setShowInstalledOnly] = useState(false);
  const [importError, setImportError] = useState<string | null>(null);
  const [importing, setImporting] = useState(false);
  const catalogInput = useRef<HTMLInputElement | null>(null);

  const importCatalog = async (file: File) => {
    setImportError(null);
    setImporting(true);
    try {
      const imported = parseCatalogText(await file.text());
      await replaceComponentRegistry(mergeRegistryEntries(componentRegistry, imported));
    } catch (reason) {
      setImportError(reason instanceof Error ? reason.message : String(reason));
    } finally {
      setImporting(false);
    }
  };

  const checksByComponentId = new Map(componentUpdateChecks.map((check) => [check.componentId, check]));
  const notYetInstalled = componentRegistry.filter(
    (entry) => describeRegistryAction(entry, installedComponents) !== "current",
  );

  return (
    <div role="tabpanel" id="runtime-hub-panel-components" aria-labelledby="runtime-hub-tab-components" className="flex flex-col gap-6">
      <section className="flex flex-col gap-3" aria-labelledby="installed-components-heading">
        <SectionHeading
          title="Runtime components"
          description="Versioned llama.cpp, MLX, tokenizer, converter, projector, and accelerator-support components the app itself depends on — distinct from installed models. Installs are digest-verified before activation, and at least one prior version is always kept on disk for rollback."
          action={
            <BusyButton type="button" busy={refreshing} onClick={() => void refreshComponents().catch(() => {})}>
              <RefreshCw size={15} aria-hidden="true" /> Refresh
            </BusyButton>
          }
        />
        <ErrorNotice message={error} />
        <div id="installed-components-heading" className="grid gap-3 lg:grid-cols-2">
          {installedComponents.length ? (
            installedComponents.map((component) => (
              <InstalledComponentCard
                key={component.componentId}
                component={component}
                updateCheck={checksByComponentId.get(component.componentId)}
              />
            ))
          ) : (
            <div className="rounded-lg border border-dashed border-border p-6 text-center text-sm text-muted lg:col-span-2">
              No runtime components are installed yet. Install one from the known versions below.
            </div>
          )}
        </div>
      </section>

      <section className="flex flex-col gap-3" aria-labelledby="component-registry-heading">
        <SectionHeading
          title="Known component versions"
          description="A local, operator-editable registry of known versions — not a live upstream binary CDN. Populate it with source URLs and sha256 digests you have independently verified, or import a catalog file published alongside a component."
          action={
            <>
              <input
                ref={catalogInput}
                type="file"
                accept="application/json,.json"
                className="hidden"
                onChange={(event) => {
                  const file = event.target.files?.[0];
                  if (file) void importCatalog(file);
                  // Cleared so re-importing the same file still fires a change.
                  event.target.value = "";
                }}
              />
              <BusyButton
                type="button"
                busy={importing}
                onClick={() => catalogInput.current?.click()}
              >
                <PackagePlus size={15} aria-hidden="true" /> Import catalog
              </BusyButton>
            </>
          }
        />
        <ErrorNotice message={importError} />
        <p className="text-xs text-muted">
          Importing adds a catalog&apos;s versions to this registry; it does not download or
          install anything. Every entry is still digest-verified at install time, and a signed
          component is still checked against its pinned publisher key.
        </p>
        <label className="flex min-h-11 w-fit cursor-pointer items-center gap-2 text-xs text-muted">
          <input
            type="checkbox"
            checked={showInstalledOnly}
            onChange={(event) => setShowInstalledOnly(event.target.checked)}
            className="h-4 w-4 rounded border-border accent-[var(--color-accent)]"
          />
          Show only versions not yet installed
        </label>
        <div id="component-registry-heading" className="grid gap-3 lg:grid-cols-2" aria-live="polite">
          {(showInstalledOnly ? notYetInstalled : componentRegistry).length ? (
            (showInstalledOnly ? notYetInstalled : componentRegistry).map((entry) => (
              <RegistryEntryCard key={`${entry.componentId}:${entry.version}:${entry.sha256}`} entry={entry} />
            ))
          ) : (
            <div className="rounded-lg border border-dashed border-border p-6 text-center text-sm text-muted lg:col-span-2">
              {componentRegistry.length
                ? "Every known version is already installed."
                : "No component versions are registered yet. Add entries to the local component registry to make them installable."}
            </div>
          )}
        </div>
      </section>
    </div>
  );
}

export default RuntimeHubComponents;
