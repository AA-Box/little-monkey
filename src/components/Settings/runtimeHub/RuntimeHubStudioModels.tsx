import { useCallback, useEffect, useState } from "react";
import { Download, PackagePlus, RefreshCw, Trash2 } from "lucide-react";

import { AddModelForm } from "../../Studio/AddModelForm";
import { Button, StatusPill } from "../../ui";
import { useT } from "../../../lib/i18n";
import {
  formatBytes,
  SectionHeading,
} from "./RuntimeHubShared";
import {
  studioClient,
  type GenerationModel,
} from "../../../lib/studioClient";

function StudioModelCard({
  model,
  onChanged,
}: {
  model: GenerationModel;
  onChanged: () => void;
}) {
  const { t } = useT();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const blockedByLicense = model.license.acceptanceRequired && !model.licenseAccepted;

  async function acceptLicense() {
    setError(null);
    setBusy(true);
    try {
      await studioClient.acceptLicense(model.license.id);
      onChanged();
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : String(reason));
    } finally {
      setBusy(false);
    }
  }

  async function download() {
    setError(null);
    setBusy(true);
    try {
      await studioClient.downloadModel(model.id);
      onChanged();
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : String(reason));
    } finally {
      setBusy(false);
    }
  }

  async function remove() {
    if (!window.confirm(t("RuntimeHub.studioModels.removeConfirm", { name: model.name }))) return;
    setError(null);
    setBusy(true);
    try {
      await studioClient.removeModel(model.id);
      onChanged();
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : String(reason));
    } finally {
      setBusy(false);
    }
  }

  return (
    <article className="rounded-lg border border-border bg-background p-4">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0">
          <div className="flex flex-wrap items-center gap-2">
            <h4 className="break-words text-sm font-semibold text-foreground">{model.name}</h4>
            <StatusPill tone={model.engine === "mlx_video" ? "success" : "neutral"}>
              {model.engine === "mlx_video"
                ? t("RuntimeHub.studioModels.engineMlxVideo")
                : t("RuntimeHub.studioModels.engineBundled")}
            </StatusPill>
          </div>
          <p className="mt-1 break-all font-mono text-xs text-muted">
            {model.family || t("RuntimeHub.studioModels.unclassified")} · {model.tasks.join(", ")}
          </p>
        </div>
        <StatusPill tone={model.installed ? "success" : "warning"}>
          {model.installed
            ? t("RuntimeHub.studioModels.installed")
            : t("RuntimeHub.studioModels.missing", { bytes: formatBytes(model.missingBytes) })}
        </StatusPill>
      </div>

      <div className="mt-3 grid gap-2 text-xs text-muted sm:grid-cols-2">
        <span>{t("RuntimeHub.studioModels.onDisk", { bytes: formatBytes(model.totalBytes) })}</span>
        <span>
          {model.fitsInMemory
            ? t("RuntimeHub.studioModels.fitsMemory")
            : t("RuntimeHub.studioModels.tooLarge")}
        </span>
      </div>

      {blockedByLicense && (
        <div className="mt-3 rounded-md border border-warning/30 bg-warning-soft p-3 text-xs text-warning">
          <p>{t("RuntimeHub.studioModels.licenseRequired", { name: model.license.name })}</p>
          <div className="mt-2 flex justify-end">
            <Button type="button" size="sm" onClick={() => void acceptLicense()} disabled={busy}>
              {t("RuntimeHub.studioModels.acceptLicense")}
            </Button>
          </div>
        </div>
      )}

      {error && <p className="mt-3 text-xs text-danger">{error}</p>}
      <div className="mt-4 flex flex-wrap justify-end gap-2">
        {!model.installed && !blockedByLicense && (
          <Button type="button" size="sm" variant="primary" onClick={() => void download()} disabled={busy || !model.fitsInMemory}>
            <Download size={14} aria-hidden="true" /> {t("RuntimeHub.studioModels.downloadMissing")}
          </Button>
        )}
        <Button
          type="button"
          size="sm"
          variant="secondary"
          onClick={() => void remove()}
          disabled={busy}
        >
          <Trash2 size={14} aria-hidden="true" /> {t("RuntimeHub.studioModels.remove")}
        </Button>
      </div>
    </article>
  );
}

/**
 * The central model-management surface for Studio's local generation models.
 * Runtime Hub chat models and Studio models still have different formats, but
 * users no longer need to remember a second Settings area to add or download
 * either kind of MLX model.
 */
export function RuntimeHubStudioModels() {
  const { t } = useT();
  const [models, setModels] = useState<GenerationModel[]>([]);
  const [adding, setAdding] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [refreshing, setRefreshing] = useState(false);
  const [installingWan, setInstallingWan] = useState(false);

  const refresh = useCallback(async () => {
    setRefreshing(true);
    try {
      setModels(await studioClient.models());
      setError(null);
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : String(reason));
    } finally {
      setRefreshing(false);
    }
  }, []);

  async function installWan22() {
    if (!window.confirm(
      "Install native Wan 2.2 TI2V-5B? This downloads approximately 17.5 GB " +
      "of Apache-2.0 diffusion, VAE and text-encoder weights from Hugging Face. " +
      "It may require significant RAM, GPU memory, disk space and download time."
    )) return;
    setInstallingWan(true);
    setError(null);
    try {
      await studioClient.registerWan22();
      // Reuse the same backend download pipeline as every installed Studio
      // model: cancellation, per-file transfers and progress notification.
      await studioClient.downloadModel("wan22-ti2v-5b-native");
      await refresh();
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : String(reason));
      await refresh();
    } finally {
      setInstallingWan(false);
    }
  }

  useEffect(() => {
    void refresh();
  }, [refresh]);

  return (
    <section className="flex flex-col gap-3" aria-labelledby="studio-models-heading">
      <SectionHeading
        id="studio-models-heading"
        title={t("RuntimeHub.studioModels.title")}
        description={t("RuntimeHub.studioModels.description")}
        action={(
          <div className="flex flex-wrap gap-2">
            <Button type="button" size="sm" variant="secondary" onClick={() => void refresh()} disabled={refreshing}>
              <RefreshCw size={14} aria-hidden="true" /> {t("RuntimeHub.studioModels.refresh")}
            </Button>
            <Button type="button" size="sm" variant="secondary" onClick={() => setAdding((current) => !current)}>
              <PackagePlus size={14} aria-hidden="true" />
              {adding ? t("RuntimeHub.studioModels.closeAddForm") : t("RuntimeHub.studioModels.add")}
            </Button>
          </div>
        )}
      />

      <div className="flex flex-wrap items-center justify-between gap-3 rounded-lg border border-border bg-background p-3 text-xs">
        <div className="min-w-0">
          <div className="font-medium text-foreground">Wan 2.2 TI2V-5B — local text/image to video</div>
          <div className="text-muted">
            Worldwide Apache-2.0 model. Native sd-server, no ComfyUI or FreeVideo required.
            Approx. 17.5 GB of separate model weights. Adaptive VRAM profile enabled.
          </div>
        </div>
        <Button type="button" size="sm" variant="primary"
          disabled={installingWan}
          onClick={() => void installWan22()}>
          <Download size={14} aria-hidden="true" />
          {installingWan ? "Installing Wan 2.2…" : "Install Wan 2.2"}
        </Button>
      </div>

      {adding && (
        <div className="rounded-lg border border-border bg-background p-3">
          <AddModelForm
            onSaved={() => {
              setAdding(false);
              void refresh();
            }}
          />
        </div>
      )}

      {error && <p className="text-xs text-danger">{error}</p>}
      {models.length ? (
        <div className="grid gap-3 lg:grid-cols-2">
          {models.map((model) => (
            <StudioModelCard key={model.id} model={model} onChanged={() => void refresh()} />
          ))}
        </div>
      ) : (
        <div className="rounded-lg border border-dashed border-border p-6 text-center text-sm text-muted">
          {t("RuntimeHub.studioModels.empty")}
        </div>
      )}
    </section>
  );
}
