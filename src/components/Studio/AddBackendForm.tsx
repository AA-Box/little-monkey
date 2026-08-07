import { useState } from "react";

import { Button } from "../ui";
import { useT } from "../../lib/i18n";
import {
  studioClient,
  type RemoteBackend,
  type RemoteBackendKind,
} from "../../lib/studioClient";

const FIELD =
  "rounded border border-border bg-background px-2 py-1 text-xs text-foreground";

/** A starting graph that is obviously a template rather than a working one, so
 *  the user replaces it with their own exported workflow instead of trying to
 *  run this. */
const WORKFLOW_EXAMPLE = `{
  "3": { "class_type": "KSampler", "inputs": { "steps": "{{steps}}", "cfg": "{{cfg_scale}}", "seed": "{{seed}}" } },
  "4": { "class_type": "CheckpointLoaderSimple", "inputs": { "ckpt_name": "{{model}}" } },
  "6": { "class_type": "CLIPTextEncode", "inputs": { "text": "{{prompt}}" } },
  "7": { "class_type": "CLIPTextEncode", "inputs": { "text": "{{negative_prompt}}" } }
}`;

/** A backend id the Rust side will accept: no colons, slashes or spaces, since
 *  the id is one segment of a `remote:<id>:<model>` picker id. */
function slugify(value: string): string {
  return value
    .toLowerCase()
    .replace(/[^a-z0-9._-]+/g, "-")
    .replace(/^[.-]+/, "")
    .slice(0, 64);
}

/**
 * Registers a remote generation backend.
 *
 * Neither backend ships with the app. ComfyUI is a Python server the user
 * installs and runs themselves, and an OpenAI-compatible endpoint is somebody
 * else's host — this form only writes down an address and, for the hosted case,
 * which already-saved provider key to authenticate with. No key is entered
 * here and none is stored by Studio.
 */
export function AddBackendForm({ onSaved }: { onSaved: () => void }) {
  const { t } = useT();
  const [kind, setKind] = useState<RemoteBackendKind>("comfy_ui");
  const [label, setLabel] = useState("");
  const [id, setId] = useState("");
  const [baseUrl, setBaseUrl] = useState("http://127.0.0.1:8188");
  const [providerId, setProviderId] = useState("");
  const [supportsEditing, setSupportsEditing] = useState(false);
  const [models, setModels] = useState("");
  const [workflow, setWorkflow] = useState(WORKFLOW_EXAMPLE);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const switchKind = (next: RemoteBackendKind) => {
    setKind(next);
    // The two defaults are nothing alike, and a leftover loopback URL on a
    // hosted backend is a confusing failure rather than an obvious one.
    setBaseUrl(next === "comfy_ui" ? "http://127.0.0.1:8188" : "https://api.openai.com/v1");
  };

  const save = async () => {
    setError(null);
    let workflowTemplate: unknown | null = null;
    if (kind === "comfy_ui") {
      try {
        workflowTemplate = JSON.parse(workflow) as unknown;
      } catch (reason) {
        setError(
          t("Studio.backend.badWorkflow", {
            detail: reason instanceof Error ? reason.message : String(reason),
          }),
        );
        return;
      }
    }
    const backend: RemoteBackend = {
      id: id.trim() || slugify(label),
      label: label.trim(),
      kind,
      baseUrl: baseUrl.trim(),
      providerId: kind === "open_ai_compatible" ? providerId.trim() || null : null,
      workflowTemplate,
      supportsEditing: kind === "open_ai_compatible" && supportsEditing,
      models: models
        .split(/[\n,]/)
        .map((entry) => entry.trim())
        .filter(Boolean),
    };
    setBusy(true);
    try {
      await studioClient.addBackend(backend);
      setLabel("");
      setId("");
      setModels("");
      onSaved();
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : String(reason));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="grid gap-3 rounded border border-border p-3">
      <p className="text-xs font-medium">{t("Studio.backend.title")}</p>
      <p className="text-[11px] text-faint">{t("Studio.backend.hint")}</p>

      {error && (
        <p className="rounded border border-danger/40 bg-danger/10 px-2 py-1 text-[11px] text-danger">
          {error}
        </p>
      )}

      <label className="grid gap-1 text-[11px] text-muted">
        {t("Studio.backend.kind")}
        <select
          className={FIELD}
          value={kind}
          onChange={(event) => switchKind(event.target.value as RemoteBackendKind)}
        >
          <option value="comfy_ui">{t("Studio.backend.kindComfy")}</option>
          <option value="open_ai_compatible">{t("Studio.backend.kindOpenAi")}</option>
        </select>
      </label>

      <label className="grid gap-1 text-[11px] text-muted">
        {t("Studio.backend.label")}
        <input
          className={FIELD}
          value={label}
          placeholder={t("Studio.backend.labelPlaceholder")}
          onChange={(event) => setLabel(event.target.value)}
        />
      </label>

      <label className="grid gap-1 text-[11px] text-muted">
        {t("Studio.backend.id")}
        <input
          className={FIELD}
          value={id}
          placeholder={slugify(label) || t("Studio.backend.idPlaceholder")}
          onChange={(event) => setId(event.target.value)}
        />
      </label>

      <label className="grid gap-1 text-[11px] text-muted">
        {t("Studio.backend.baseUrl")}
        <input
          className={FIELD}
          value={baseUrl}
          onChange={(event) => setBaseUrl(event.target.value)}
        />
      </label>

      {kind === "open_ai_compatible" && (
        <>
          <label className="grid gap-1 text-[11px] text-muted">
            {t("Studio.backend.provider")}
            <input
              className={FIELD}
              value={providerId}
              placeholder="openai"
              onChange={(event) => setProviderId(event.target.value)}
            />
          </label>
          <label className="flex items-center gap-2 text-[11px] text-muted">
            <input
              type="checkbox"
              checked={supportsEditing}
              onChange={(event) => setSupportsEditing(event.target.checked)}
            />
            {t("Studio.backend.editing")}
          </label>
        </>
      )}

      <label className="grid gap-1 text-[11px] text-muted">
        {t("Studio.backend.models")}
        <textarea
          className={`${FIELD} min-h-16 font-mono`}
          value={models}
          placeholder={
            kind === "comfy_ui" ? "sd_xl_base_1.0.safetensors" : "gpt-image-1"
          }
          onChange={(event) => setModels(event.target.value)}
        />
      </label>

      {kind === "comfy_ui" && (
        <label className="grid gap-1 text-[11px] text-muted">
          {t("Studio.backend.workflow")}
          <textarea
            className={`${FIELD} min-h-32 font-mono`}
            value={workflow}
            onChange={(event) => setWorkflow(event.target.value)}
            aria-label={t("Studio.backend.workflow")}
          />
        </label>
      )}

      <div>
        <Button
          size="sm"
          variant="primary"
          disabled={busy || !label.trim() || !models.trim()}
          onClick={() => void save()}
        >
          {t("Studio.backend.save")}
        </Button>
      </div>
    </div>
  );
}
