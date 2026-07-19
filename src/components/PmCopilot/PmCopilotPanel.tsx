import { Plus, Trash2, X } from "lucide-react";

import { useT } from "../../lib/i18n";
import type { PmRiskSeverity } from "../../lib/pmCopilot";
import { usePmCopilotStore } from "../../store/pmCopilotStore";
import { useWorkspaceStore, primaryRoot } from "../../store/workspaceStore";
import { Button, IconButton } from "../ui";

interface PmCopilotPanelProps {
  onClose: () => void;
}

const INPUT =
  "w-full rounded-md border border-border bg-surface px-2.5 py-1.5 text-sm text-foreground focus:outline-none focus:ring-2 focus:ring-accent";
const TEXTAREA = `${INPUT} resize-y`;
const SEVERITIES: PmRiskSeverity[] = ["low", "medium", "high"];

function SectionHeading({ title, hint, onAdd, addLabel }: { title: string; hint: string; onAdd: () => void; addLabel: string }) {
  return (
    <div className="mb-2 flex items-center justify-between gap-2">
      <div>
        <h3 className="text-sm font-semibold text-foreground">{title}</h3>
        <p className="text-xs text-muted">{hint}</p>
      </div>
      <Button type="button" variant="secondary" size="sm" onClick={onAdd}>
        <Plus size={13} />
        {addLabel}
      </Button>
    </div>
  );
}

function RemoveRowButton({ onClick, label }: { onClick: () => void; label: string }) {
  return (
    <IconButton size="sm" variant="ghost" onClick={onClick} aria-label={label} title={label}>
      <Trash2 size={14} />
    </IconButton>
  );
}

export function PmCopilotPanel({ onClose }: PmCopilotPanelProps) {
  const { t } = useT();
  const hasWorkspace = useWorkspaceStore((state) => primaryRoot(state.roots) !== null);
  const state = usePmCopilotStore();

  const generating = state.status === "generating";
  const plan = state.plan;

  return (
    <section className="flex min-h-0 flex-1 flex-col" aria-labelledby="pm-copilot-title">
      <header className="flex shrink-0 items-center justify-between gap-3 border-b border-border px-4 py-3">
        <div className="min-w-0">
          <h1 id="pm-copilot-title" className="text-base font-semibold text-foreground">
            {t("PmCopilotPanel.title")}
          </h1>
          <p className="truncate text-xs text-muted">{t("PmCopilotPanel.subtitle")}</p>
        </div>
        <IconButton size="sm" onClick={onClose} aria-label={t("PmCopilotPanel.close")}>
          <X size={16} />
        </IconButton>
      </header>

      <div className="min-h-0 flex-1 overflow-y-auto px-4 py-4">
        <div className="mx-auto flex max-w-3xl flex-col gap-5">
          {!hasWorkspace && (
            <div role="alert" className="rounded-md border border-warning/30 bg-warning-soft px-3 py-2 text-xs text-warning">
              {t("PmCopilotPanel.workspaceRequired")}
            </div>
          )}

          <div className="rounded-lg border border-border bg-background p-4">
            <label className="block text-sm font-medium text-foreground" htmlFor="pm-copilot-goal">
              {t("PmCopilotPanel.goalLabel")}
            </label>
            <textarea
              id="pm-copilot-goal"
              className={`${TEXTAREA} mt-2`}
              rows={4}
              placeholder={t("PmCopilotPanel.goalPlaceholder")}
              value={state.goal}
              onChange={(event) => state.setGoal(event.target.value)}
              disabled={generating}
            />
            <div className="mt-3 flex items-center gap-2">
              <Button
                type="button"
                variant="primary"
                onClick={() => void state.generate()}
                disabled={generating || !state.goal.trim()}
              >
                {generating
                  ? t("PmCopilotPanel.generatingButton")
                  : plan
                    ? t("PmCopilotPanel.regenerateButton")
                    : t("PmCopilotPanel.generateButton")}
              </Button>
              {generating && (
                <Button type="button" variant="ghost" onClick={() => state.cancelGenerate()}>
                  {t("PmCopilotPanel.cancelButton")}
                </Button>
              )}
            </div>
            {state.status === "error" && state.error && (
              <p role="alert" className="mt-2 text-xs text-danger">
                {t("PmCopilotPanel.generationError", { error: state.error })}
              </p>
            )}
          </div>

          {!plan && state.status !== "generating" && (
            <p className="px-1 text-sm text-faint">{t("PmCopilotPanel.emptyState")}</p>
          )}

          {plan && (
            <>
              <div className="rounded-lg border border-border bg-background p-4">
                <label className="block text-sm font-medium text-foreground" htmlFor="pm-copilot-prd">
                  {t("PmCopilotPanel.prdSummaryLabel")}
                </label>
                <textarea
                  id="pm-copilot-prd"
                  className={`${TEXTAREA} mt-2`}
                  rows={4}
                  value={plan.prdSummary}
                  onChange={(event) => state.updatePrdSummary(event.target.value)}
                />
              </div>

              <div className="rounded-lg border border-border bg-background p-4">
                <SectionHeading
                  title={t("PmCopilotPanel.userStoriesLabel")}
                  hint={t("PmCopilotPanel.userStoriesHint")}
                  onAdd={() => state.addUserStory()}
                  addLabel={t("PmCopilotPanel.addStory")}
                />
                <div className="flex flex-col gap-2">
                  {plan.userStories.map((story, index) => (
                    <div key={index} className="flex flex-col gap-1.5 rounded-md border border-border bg-surface p-2.5 sm:flex-row sm:items-start">
                      <div className="grid flex-1 gap-1.5 sm:grid-cols-3">
                        <input
                          className={INPUT}
                          placeholder={t("PmCopilotPanel.storyAsAPlaceholder")}
                          value={story.asA}
                          onChange={(event) => state.updateUserStory(index, "asA", event.target.value)}
                        />
                        <input
                          className={INPUT}
                          placeholder={t("PmCopilotPanel.storyIWantPlaceholder")}
                          value={story.iWant}
                          onChange={(event) => state.updateUserStory(index, "iWant", event.target.value)}
                        />
                        <input
                          className={INPUT}
                          placeholder={t("PmCopilotPanel.storySoThatPlaceholder")}
                          value={story.soThat}
                          onChange={(event) => state.updateUserStory(index, "soThat", event.target.value)}
                        />
                      </div>
                      <RemoveRowButton onClick={() => state.removeUserStory(index)} label={t("PmCopilotPanel.removeStory")} />
                    </div>
                  ))}
                  {plan.userStories.length === 0 && <p className="text-xs text-faint">{t("PmCopilotPanel.userStoriesEmpty")}</p>}
                </div>
              </div>

              <div className="rounded-lg border border-border bg-background p-4">
                <SectionHeading
                  title={t("PmCopilotPanel.acceptanceCriteriaLabel")}
                  hint={t("PmCopilotPanel.acceptanceCriteriaHint")}
                  onAdd={() => state.addAcceptanceCriterion()}
                  addLabel={t("PmCopilotPanel.addCriterion")}
                />
                <div className="flex flex-col gap-2">
                  {plan.acceptanceCriteria.map((criterion, index) => (
                    <div key={index} className="flex items-center gap-1.5">
                      <input
                        className={`${INPUT} flex-1`}
                        value={criterion}
                        onChange={(event) => state.updateAcceptanceCriterion(index, event.target.value)}
                      />
                      <RemoveRowButton onClick={() => state.removeAcceptanceCriterion(index)} label={t("PmCopilotPanel.removeCriterion")} />
                    </div>
                  ))}
                  {plan.acceptanceCriteria.length === 0 && (
                    <p className="text-xs text-faint">{t("PmCopilotPanel.acceptanceCriteriaEmpty")}</p>
                  )}
                </div>
              </div>

              <div className="rounded-lg border border-border bg-background p-4">
                <SectionHeading
                  title={t("PmCopilotPanel.risksLabel")}
                  hint={t("PmCopilotPanel.risksHint")}
                  onAdd={() => state.addRisk()}
                  addLabel={t("PmCopilotPanel.addRisk")}
                />
                <div className="flex flex-col gap-2">
                  {plan.risks.map((risk, index) => (
                    <div key={index} className="flex flex-col gap-1.5 rounded-md border border-border bg-surface p-2.5 sm:flex-row sm:items-start">
                      <div className="grid flex-1 gap-1.5 sm:grid-cols-[2fr_1fr_2fr]">
                        <input
                          className={INPUT}
                          placeholder={t("PmCopilotPanel.riskDescriptionPlaceholder")}
                          value={risk.description}
                          onChange={(event) => state.updateRisk(index, "description", event.target.value)}
                        />
                        <select
                          className={INPUT}
                          value={risk.severity}
                          onChange={(event) => state.updateRisk(index, "severity", event.target.value)}
                        >
                          {SEVERITIES.map((severity) => (
                            <option key={severity} value={severity}>
                              {t(`PmCopilotPanel.severity.${severity}`)}
                            </option>
                          ))}
                        </select>
                        <input
                          className={INPUT}
                          placeholder={t("PmCopilotPanel.riskMitigationPlaceholder")}
                          value={risk.mitigation}
                          onChange={(event) => state.updateRisk(index, "mitigation", event.target.value)}
                        />
                      </div>
                      <RemoveRowButton onClick={() => state.removeRisk(index)} label={t("PmCopilotPanel.removeRisk")} />
                    </div>
                  ))}
                  {plan.risks.length === 0 && <p className="text-xs text-faint">{t("PmCopilotPanel.risksEmpty")}</p>}
                </div>
              </div>

              <div className="rounded-lg border border-border bg-background p-4">
                <SectionHeading
                  title={t("PmCopilotPanel.milestonesLabel")}
                  hint={t("PmCopilotPanel.milestonesHint")}
                  onAdd={() => state.addMilestone()}
                  addLabel={t("PmCopilotPanel.addMilestone")}
                />
                <div className="flex flex-col gap-2">
                  {plan.milestones.map((milestone, index) => (
                    <div key={index} className="flex flex-col gap-1.5 rounded-md border border-border bg-surface p-2.5 sm:flex-row sm:items-start">
                      <div className="grid flex-1 gap-1.5 sm:grid-cols-[1fr_2fr]">
                        <input
                          className={INPUT}
                          placeholder={t("PmCopilotPanel.milestoneNamePlaceholder")}
                          value={milestone.name}
                          onChange={(event) => state.updateMilestone(index, "name", event.target.value)}
                        />
                        <input
                          className={INPUT}
                          placeholder={t("PmCopilotPanel.milestoneSummaryPlaceholder")}
                          value={milestone.summary}
                          onChange={(event) => state.updateMilestone(index, "summary", event.target.value)}
                        />
                      </div>
                      <RemoveRowButton onClick={() => state.removeMilestone(index)} label={t("PmCopilotPanel.removeMilestone")} />
                    </div>
                  ))}
                  {plan.milestones.length === 0 && <p className="text-xs text-faint">{t("PmCopilotPanel.milestonesEmpty")}</p>}
                </div>
              </div>

              <div className="rounded-lg border border-border bg-background p-4">
                <h3 className="text-sm font-semibold text-foreground">{t("PmCopilotPanel.saveHeading")}</h3>
                <p className="mt-1 text-xs text-muted">{t("PmCopilotPanel.followUpNote")}</p>
                <div className="mt-3 flex flex-col gap-2 sm:flex-row sm:items-center">
                  <label className="flex flex-1 items-center gap-1.5 text-xs text-muted" htmlFor="pm-copilot-slug">
                    <span className="shrink-0 font-mono">docs/product/</span>
                    <input
                      id="pm-copilot-slug"
                      className={INPUT}
                      value={state.slug}
                      onChange={(event) => state.setSlug(event.target.value)}
                    />
                    <span className="shrink-0 font-mono">.md</span>
                  </label>
                  <Button
                    type="button"
                    variant="primary"
                    onClick={() => void state.save()}
                    disabled={!hasWorkspace || state.saveStatus === "saving"}
                  >
                    {state.saveStatus === "saving" ? t("PmCopilotPanel.savingButton") : t("PmCopilotPanel.saveButton")}
                  </Button>
                </div>
                {state.saveStatus === "saved" && state.savedPath && (
                  <p className="mt-2 text-xs text-success">{t("PmCopilotPanel.savedMessage", { path: state.savedPath })}</p>
                )}
                {state.saveStatus === "error" && state.saveError && (
                  <p role="alert" className="mt-2 text-xs text-danger">
                    {t("PmCopilotPanel.saveError", { error: state.saveError })}
                  </p>
                )}
              </div>
            </>
          )}
        </div>
      </div>
    </section>
  );
}

export default PmCopilotPanel;
