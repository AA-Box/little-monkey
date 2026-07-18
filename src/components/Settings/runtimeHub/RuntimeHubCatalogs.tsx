import { useEffect, useMemo, useState } from "react";
import { Plus, RotateCcw, Save, Trash2 } from "lucide-react";
import { Button, StatusPill } from "../../ui";
import type { M3CatalogSourceConfig } from "../../../lib/runtimeHubClient";
import { useRuntimeHubStore } from "../../../store/runtimeHubStore";
import { BusyButton, CONTROL_CLASS, ErrorNotice, Field, SectionHeading, SuccessNotice } from "./RuntimeHubShared";

export function validateCatalogDraft(sources: M3CatalogSourceConfig[]): string | null {
  if (sources.length > 32) return "At most 32 catalog sources can be configured.";
  const ids = new Set<string>();
  for (const source of sources) {
    const id = source.sourceId.trim();
    if (!id || id.length > 256 || /[\u0000-\u001f\u007f]/.test(id) || id === "." || id === "..") {
      return "Every source needs a bounded identifier without control characters.";
    }
    if (ids.has(id)) return `Source id “${id}” is duplicated.`;
    ids.add(id);
    try {
      const endpoint = new URL(source.endpoint);
      const loopback = endpoint.hostname === "localhost" || endpoint.hostname === "127.0.0.1" || endpoint.hostname === "[::1]";
      if (endpoint.username || endpoint.password || endpoint.hash || (endpoint.protocol !== "https:" && !(endpoint.protocol === "http:" && loopback))) {
        return `Endpoint for “${id}” must use HTTPS; HTTP is accepted only on loopback.`;
      }
    } catch {
      return `Endpoint for “${id}” is not a valid URL.`;
    }
  }
  return null;
}

function blankSource(index: number): M3CatalogSourceConfig {
  return { sourceId: `catalog-${index}`, endpoint: "https://" };
}

export function RuntimeHubCatalogs() {
  const persisted = useRuntimeHubStore((state) => state.catalogSources);
  const replaceSources = useRuntimeHubStore((state) => state.replaceCatalogSources);
  const busy = useRuntimeHubStore((state) => state.busy["catalog-sources"]);
  const error = useRuntimeHubStore((state) => state.errors["catalog-sources"]);
  const [draft, setDraft] = useState<M3CatalogSourceConfig[]>(persisted);
  const [saved, setSaved] = useState(false);

  useEffect(() => {
    setDraft(persisted);
  }, [persisted]);

  const validation = useMemo(() => validateCatalogDraft(draft), [draft]);
  const changed = JSON.stringify(draft) !== JSON.stringify(persisted);

  function update(index: number, field: keyof M3CatalogSourceConfig, value: string) {
    setSaved(false);
    setDraft((current) => current.map((source, sourceIndex) => sourceIndex === index ? { ...source, [field]: value } : source));
  }

  function save() {
    if (validation) return;
    const normalized = draft.map((source) => ({ sourceId: source.sourceId.trim(), endpoint: source.endpoint.trim() }));
    void replaceSources(normalized).then(() => setSaved(true)).catch(() => setSaved(false));
  }

  return (
    <div role="tabpanel" id="runtime-hub-panel-catalogs" aria-labelledby="runtime-hub-tab-catalogs" className="flex flex-col gap-5">
      <SectionHeading
        title="Catalog sources"
        description="Configure bounded HTTPS model catalogs. Changes are validated, atomically persisted, and used by the next search without restarting the app."
        action={
          <Button
            type="button"
            className="min-h-11"
            disabled={draft.length >= 32}
            onClick={() => { setSaved(false); setDraft((current) => [...current, blankSource(current.length + 1)]); }}
          >
            <Plus size={15} aria-hidden="true" /> Add source
          </Button>
        }
      />

      {draft.length ? (
        <div className="flex flex-col gap-3">
          {draft.map((source, index) => (
            <article key={`${index}:${source.sourceId}`} className="rounded-lg border border-border bg-background p-4">
              <div className="grid gap-3 lg:grid-cols-[minmax(12rem,0.7fr)_minmax(18rem,1.3fr)_auto] lg:items-end">
                <Field label={`Source ${index + 1} id`} hint="Stable id returned by every entry from this source.">
                  <input value={source.sourceId} onChange={(event) => update(index, "sourceId", event.target.value)} className={CONTROL_CLASS} />
                </Field>
                <Field label="Search endpoint" hint="Little Monkey adds bounded q and limit query parameters.">
                  <input type="url" value={source.endpoint} onChange={(event) => update(index, "endpoint", event.target.value)} className={CONTROL_CLASS} />
                </Field>
                <Button
                  type="button"
                  variant="danger"
                  className="min-h-11"
                  aria-label={`Remove catalog source ${source.sourceId || index + 1}`}
                  onClick={() => { setSaved(false); setDraft((current) => current.filter((_, sourceIndex) => sourceIndex !== index)); }}
                >
                  <Trash2 size={15} aria-hidden="true" /> Remove
                </Button>
              </div>
            </article>
          ))}
        </div>
      ) : (
        <div className="rounded-lg border border-dashed border-border p-6 text-center text-sm text-muted">
          No remote catalogs are configured. Installed models remain available; add a source to browse verified remote model cards.
        </div>
      )}

      {validation && <ErrorNotice message={validation} />}
      <ErrorNotice message={error} />
      {saved && !changed && <SuccessNotice>Catalog sources saved and activated for live search.</SuccessNotice>}
      <div className="flex flex-wrap justify-end gap-2">
        <Button type="button" className="min-h-11" disabled={!changed || busy} onClick={() => { setDraft(persisted); setSaved(false); }}>
          <RotateCcw size={15} aria-hidden="true" /> Reset
        </Button>
        <BusyButton type="button" variant="primary" busy={busy} disabled={!changed || Boolean(validation)} onClick={save}>
          <Save size={15} aria-hidden="true" /> Save and activate
        </BusyButton>
      </div>

      <div className="rounded-lg border border-border bg-surface p-4">
        <div className="flex flex-wrap items-center gap-2">
          <StatusPill tone="success">Fail closed</StatusPill>
          <p className="text-xs leading-5 text-muted">
            Redirects, credentials in URLs, cross-source entries, oversized responses, and non-HTTPS remote origins are rejected by the backend.
          </p>
        </div>
      </div>
    </div>
  );
}

