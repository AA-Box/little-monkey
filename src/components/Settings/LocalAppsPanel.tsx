import { useEffect, useMemo, useState } from "react";
import { Button, StatusPill } from "../ui";
import { useLocalAppsStore, LOCAL_APP_TEMPLATES, type LocalAppTemplate } from "../../store/localAppsStore";
import { useRecipeStore } from "../../store/recipeStore";
import { useT } from "../../lib/i18n";
import { errorMessage } from "../../lib/errors";

/**
 * Settings "Local Apps" tab (ROADMAP.md Phase 3): publishes a saved Recipe
 * as a small static local page (Form/Dashboard/Approval/Report/Chat
 * template), authenticated by a token scoped to run only that one recipe —
 * see `local_apps.rs`'s module doc for the full security model. This panel
 * never sees a token's plaintext (it's embedded directly into the generated
 * page by the Rust side, the same "never re-shown" convention
 * `ApiServerPanel`'s create-token flow already follows).
 */
export function LocalAppsPanel() {
  const { t } = useT();
  const apps = useLocalAppsStore((s) => s.apps);
  const loading = useLocalAppsStore((s) => s.loading);
  const error = useLocalAppsStore((s) => s.error);
  const refresh = useLocalAppsStore((s) => s.refresh);
  const publish = useLocalAppsStore((s) => s.publish);
  const unpublish = useLocalAppsStore((s) => s.unpublish);
  const open = useLocalAppsStore((s) => s.open);

  const recipes = useRecipeStore((s) => s.recipes);
  const refreshRecipes = useRecipeStore((s) => s.refresh);

  useEffect(() => {
    void refresh();
    void refreshRecipes();
  }, [refresh, refreshRecipes]);

  const availableRecipes = useMemo(
    () => recipes.filter((r) => r.recipe !== null).map((r) => r.recipe!),
    [recipes],
  );

  const [wizardOpen, setWizardOpen] = useState(false);
  const [selectedRecipeName, setSelectedRecipeName] = useState("");
  const [selectedTemplate, setSelectedTemplate] = useState<LocalAppTemplate>("form");
  const [paramLabels, setParamLabels] = useState<Record<string, string>>({});
  const [publishing, setPublishing] = useState(false);
  const [publishError, setPublishError] = useState<string | null>(null);

  const selectedRecipe = availableRecipes.find((r) => r.name === selectedRecipeName) ?? null;
  const declaredParams = selectedRecipe ? Object.keys(selectedRecipe.params) : [];

  function openWizard() {
    setPublishError(null);
    setSelectedRecipeName(availableRecipes[0]?.name ?? "");
    setSelectedTemplate("form");
    setParamLabels({});
    setWizardOpen(true);
  }

  async function handlePublish() {
    if (!selectedRecipeName) return;
    setPublishing(true);
    setPublishError(null);
    try {
      await publish(selectedRecipeName, selectedTemplate, paramLabels);
      setWizardOpen(false);
    } catch (err) {
      setPublishError(errorMessage(err));
    } finally {
      setPublishing(false);
    }
  }

  const [confirmingUnpublishId, setConfirmingUnpublishId] = useState<string | null>(null);
  const [copiedId, setCopiedId] = useState<string | null>(null);
  const [rowError, setRowError] = useState<string | null>(null);

  async function handleUnpublish(id: string) {
    try {
      await unpublish(id);
    } catch (err) {
      setRowError(errorMessage(err));
    } finally {
      setConfirmingUnpublishId(null);
    }
  }

  async function handleCopyLink(id: string) {
    try {
      const url = await open(id);
      await navigator.clipboard.writeText(url);
      setCopiedId(id);
      setTimeout(() => setCopiedId(null), 1500);
    } catch (err) {
      setRowError(errorMessage(err));
    }
  }

  async function handleOpen(id: string) {
    try {
      const url = await open(id);
      window.open(url, "_blank", "noopener,noreferrer");
    } catch (err) {
      setRowError(errorMessage(err));
    }
  }

  return (
    <div className="flex flex-col gap-4 py-2">
      <p className="text-xs text-muted">{t("LocalAppsPanel.description")}</p>

      <div className="flex items-center justify-between">
        <h3 className="text-xs font-semibold uppercase tracking-wide text-faint">{t("LocalAppsPanel.publishedHeading")}</h3>
        <Button
          variant="secondary"
          size="sm"
          onClick={openWizard}
          disabled={availableRecipes.length === 0}
          title={availableRecipes.length === 0 ? t("LocalAppsPanel.noRecipesHint") : undefined}
        >
          {t("LocalAppsPanel.publishButton")}
        </Button>
      </div>

      {(error || rowError) && <p className="text-xs text-danger">{error ?? rowError}</p>}

      {!loading && apps.length === 0 && (
        <p className="text-xs text-faint">{t("LocalAppsPanel.emptyState")}</p>
      )}

      <div className="flex flex-col gap-2">
        {apps.map((app) => (
          <div key={app.id} className="flex flex-col gap-1.5 rounded-lg border border-border bg-background p-3">
            <div className="flex items-center justify-between gap-2">
              <div className="min-w-0">
                <p className="truncate text-sm font-medium text-foreground">{app.name}</p>
                <p className="truncate text-xs text-muted">
                  {t("LocalAppsPanel.recipeAndTemplate", { recipe: app.recipe_name, template: t(`LocalAppsPanel.template.${app.template}`) })}
                </p>
              </div>
              <StatusPill tone={app.enabled ? "success" : "neutral"}>
                {app.enabled ? t("LocalAppsPanel.statusPublished") : t("LocalAppsPanel.statusDisabled")}
              </StatusPill>
            </div>
            <div className="flex flex-wrap gap-2">
              <Button variant="secondary" size="sm" onClick={() => void handleOpen(app.id)}>
                {t("LocalAppsPanel.openButton")}
              </Button>
              <Button variant="secondary" size="sm" onClick={() => void handleCopyLink(app.id)}>
                {copiedId === app.id ? t("LocalAppsPanel.copiedButton") : t("LocalAppsPanel.copyLinkButton")}
              </Button>
              {confirmingUnpublishId === app.id ? (
                <>
                  <Button variant="danger" size="sm" onClick={() => void handleUnpublish(app.id)}>
                    {t("LocalAppsPanel.confirmUnpublishButton")}
                  </Button>
                  <Button variant="secondary" size="sm" onClick={() => setConfirmingUnpublishId(null)}>
                    {t("LocalAppsPanel.cancelButton")}
                  </Button>
                </>
              ) : (
                <Button variant="secondary" size="sm" onClick={() => setConfirmingUnpublishId(app.id)}>
                  {t("LocalAppsPanel.unpublishButton")}
                </Button>
              )}
            </div>
          </div>
        ))}
      </div>

      {wizardOpen && (
        <div className="flex flex-col gap-3 rounded-lg border border-border bg-background p-3">
          <h3 className="text-sm font-semibold text-foreground">{t("LocalAppsPanel.wizardHeading")}</h3>

          <label className="flex flex-col gap-1 text-sm">
            <span className="text-foreground">{t("LocalAppsPanel.wizardRecipeLabel")}</span>
            <select
              value={selectedRecipeName}
              onChange={(event) => setSelectedRecipeName(event.target.value)}
              className="h-8 rounded-md border border-border bg-surface px-2 text-sm text-foreground focus:outline-none focus:ring-2 focus:ring-accent"
            >
              {availableRecipes.map((recipe) => (
                <option key={recipe.name} value={recipe.name}>
                  {recipe.name}
                </option>
              ))}
            </select>
          </label>

          <label className="flex flex-col gap-1 text-sm">
            <span className="text-foreground">{t("LocalAppsPanel.wizardTemplateLabel")}</span>
            <select
              value={selectedTemplate}
              onChange={(event) => setSelectedTemplate(event.target.value as LocalAppTemplate)}
              className="h-8 rounded-md border border-border bg-surface px-2 text-sm text-foreground focus:outline-none focus:ring-2 focus:ring-accent"
            >
              {LOCAL_APP_TEMPLATES.map((template) => (
                <option key={template} value={template}>
                  {t(`LocalAppsPanel.template.${template}`)}
                </option>
              ))}
            </select>
          </label>

          {declaredParams.length > 0 && (
            <div className="flex flex-col gap-2">
              <span className="text-sm text-foreground">{t("LocalAppsPanel.wizardParamsLabel")}</span>
              {declaredParams.map((paramName) => (
                <label key={paramName} className="flex items-center justify-between gap-2 text-xs">
                  <span className="font-mono text-muted">{paramName}</span>
                  <input
                    type="text"
                    value={paramLabels[paramName] ?? ""}
                    placeholder={paramName}
                    onChange={(event) =>
                      setParamLabels((prev) => ({ ...prev, [paramName]: event.target.value }))
                    }
                    className="h-7 w-48 rounded-md border border-border bg-surface px-2 text-xs text-foreground focus:outline-none focus:ring-2 focus:ring-accent"
                  />
                </label>
              ))}
            </div>
          )}

          {publishError && <p className="text-xs text-danger">{publishError}</p>}

          <div className="flex gap-2">
            <Button
              variant="primary"
              size="sm"
              onClick={() => void handlePublish()}
              disabled={publishing || !selectedRecipeName}
            >
              {publishing ? t("LocalAppsPanel.publishingButton") : t("LocalAppsPanel.confirmPublishButton")}
            </Button>
            <Button variant="secondary" size="sm" onClick={() => setWizardOpen(false)}>
              {t("LocalAppsPanel.cancelButton")}
            </Button>
          </div>
        </div>
      )}
    </div>
  );
}
