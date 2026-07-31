import { useEffect, useState } from "react";
import {
  AlertTriangle,
  CheckCircle2,
  ClipboardList,
  FileUp,
  Loader2,
  ShieldAlert,
  Sparkles,
  Trash2,
  X,
} from "lucide-react";

import { useT } from "../../lib/i18n";
import { useSopCompilerStore, type SopCompilationDraft } from "../../store/sopCompilerStore";
import { Button, IconButton, StatusPill, type PillTone } from "../ui";
import { errorMessage } from "../../lib/errors";

interface SopCompilerPanelProps {
  onClose: () => void;
  /** Navigates to Settings → Prompts, the existing quarantined skill-proposal
   * review surface — mirrors `ChatWindow.tsx`'s `/learn` command, which jumps
   * there right after `createProposal` for the exact same reason. */
  onOpenSkillProposals: () => void;
}

function errorText(error: unknown): string {
  return errorMessage(error);
}

function draftStatusTone(status: SopCompilationDraft["status"]): PillTone {
  return status === "sent_for_review" ? "success" : "neutral";
}

export function SopCompilerPanel({ onClose, onOpenSkillProposals }: SopCompilerPanelProps) {
  const { t } = useT();
  const store = useSopCompilerStore();
  const [sendError, setSendError] = useState<string | null>(null);
  const [sendingId, setSendingId] = useState<string | null>(null);

  const selected = store.drafts.find((entry) => entry.id === store.selectedDraftId) ?? null;

  useEffect(() => {
    setSendError(null);
  }, [store.selectedDraftId]);

  const handleSendToReview = async (id: string) => {
    setSendError(null);
    setSendingId(id);
    try {
      await store.sendToReview(id);
      onOpenSkillProposals();
    } catch (err) {
      setSendError(errorText(err));
    } finally {
      setSendingId(null);
    }
  };

  return (
    <section className="flex h-full min-h-0 flex-col" aria-labelledby="sop-compiler-title">
      <header className="flex shrink-0 items-start justify-between gap-3 border-b border-border px-5 py-4">
        <div>
          <h2 id="sop-compiler-title" className="text-sm font-semibold text-foreground">
            {t("SopCompiler.title")}
          </h2>
          <p className="mt-1 max-w-2xl text-xs leading-5 text-muted">{t("SopCompiler.subtitle")}</p>
        </div>
        <IconButton size="sm" aria-label={t("SopCompiler.close")} title={t("SopCompiler.close")} onClick={onClose}>
          <X size={15} />
        </IconButton>
      </header>

      <div className="shrink-0 space-y-2.5 border-b border-border px-5 py-3">
        <div className="flex items-center justify-between gap-2">
          <label htmlFor="sop-compiler-source" className="text-xs font-medium text-foreground">
            {t("SopCompiler.sourceLabel")}
          </label>
          <div className="flex items-center gap-2">
            {store.sourceFileName && (
              <span className="max-w-40 truncate font-mono text-[11px] text-faint">{store.sourceFileName}</span>
            )}
            <Button size="sm" disabled={store.importing} onClick={() => void store.importFromFile()}>
              {store.importing ? <Loader2 className="animate-spin" size={13} /> : <FileUp size={13} />}
              {t("SopCompiler.importButton")}
            </Button>
          </div>
        </div>
        <textarea
          id="sop-compiler-source"
          className="h-28 w-full resize-y rounded-md border border-border bg-background px-2.5 py-2 font-mono text-xs text-foreground outline-none focus:border-accent focus:ring-1 focus:ring-accent"
          placeholder={t("SopCompiler.sourcePlaceholder")}
          value={store.sourceText}
          onChange={(event) => store.setSourceText(event.target.value)}
        />
        <div className="flex items-center justify-between gap-2">
          <p className="text-[11px] text-faint">{t("SopCompiler.inactiveNotice")}</p>
          <Button
            variant="primary"
            size="sm"
            disabled={store.compiling || !store.sourceText.trim()}
            onClick={() => void store.compile()}
          >
            {store.compiling ? <Loader2 className="animate-spin" size={13} /> : <Sparkles size={13} />}
            {t("SopCompiler.compileButton")}
          </Button>
        </div>
        {store.error && (
          <div role="alert" className="rounded-md border border-danger/40 bg-danger/10 p-2.5 text-xs text-danger">
            {store.error}
          </div>
        )}
      </div>

      <div className="grid min-h-0 flex-1 gap-4 overflow-hidden p-5 xl:grid-cols-[minmax(16rem,.9fr)_minmax(0,1.3fr)]">
        <div className="min-h-0 overflow-y-auto rounded-lg border border-border bg-surface p-3">
          <h3 className="text-xs font-semibold text-foreground">{t("SopCompiler.draftsHeading")}</h3>
          <div className="mt-2 space-y-1.5">
            {store.drafts.length === 0 && (
              <p className="rounded-md border border-dashed border-border p-5 text-center text-xs text-faint">
                {t("SopCompiler.emptyDrafts")}
              </p>
            )}
            {store.drafts.map((entry) => (
              <button
                key={entry.id}
                type="button"
                onClick={() => store.selectDraft(entry.id)}
                className={`w-full rounded-md border p-2.5 text-left transition-colors ${
                  entry.id === store.selectedDraftId
                    ? "border-accent bg-accent/10"
                    : "border-border bg-background hover:border-border-strong"
                }`}
              >
                <p className="truncate text-xs font-medium text-foreground">{entry.draft.name}</p>
                <p className="mt-0.5 truncate text-[11px] text-muted">{entry.draft.summary}</p>
                <div className="mt-1.5">
                  <StatusPill tone={draftStatusTone(entry.status)}>
                    {entry.status === "sent_for_review"
                      ? t("SopCompiler.statusSentForReview")
                      : t("SopCompiler.statusDraft")}
                  </StatusPill>
                </div>
              </button>
            ))}
          </div>
        </div>

        <div className="min-h-0 overflow-y-auto rounded-lg border border-border bg-surface p-4">
          {!selected ? (
            <p className="p-8 text-center text-xs text-faint">{t("SopCompiler.emptyDrafts")}</p>
          ) : (
            <div className="space-y-4">
              <div className="flex flex-wrap items-start justify-between gap-2">
                <div>
                  <h3 className="text-sm font-semibold text-foreground">{selected.draft.name}</h3>
                  <p className="mt-1 max-w-xl text-xs text-muted">{selected.draft.summary}</p>
                  <p className="mt-1 font-mono text-[11px] text-faint">/{selected.draft.suggestedCommand}</p>
                </div>
                <StatusPill tone={draftStatusTone(selected.status)}>
                  {selected.status === "sent_for_review"
                    ? t("SopCompiler.statusSentForReview")
                    : t("SopCompiler.statusDraft")}
                </StatusPill>
              </div>

              <section>
                <h4 className="flex items-center gap-1.5 text-xs font-semibold text-foreground">
                  <ClipboardList size={13} /> {t("SopCompiler.stepsHeading")}
                </h4>
                <ol className="mt-2 space-y-1 pl-4 text-[11px] text-foreground marker:text-faint" style={{ listStyleType: "decimal" }}>
                  {selected.draft.steps.length === 0 && <li className="text-faint">{t("SopCompiler.noStepsExtracted")}</li>}
                  {selected.draft.steps
                    .slice()
                    .sort((a, b) => a.order - b.order)
                    .map((step) => (
                      <li key={`${step.order}-${step.action}`}>{step.action}</li>
                    ))}
                </ol>
              </section>

              <section>
                <h4 className="text-xs font-semibold text-foreground">{t("SopCompiler.inputsHeading")}</h4>
                <div className="mt-2 space-y-1.5">
                  {selected.draft.inputs.map((input) => (
                    <div key={input.name} className="rounded-md border border-border bg-background p-2 text-[11px]">
                      <p className="font-mono text-foreground">
                        {input.name}
                        {" — "}
                        <span className="text-faint">{input.required ? t("SopCompiler.required") : t("SopCompiler.optional")}</span>
                      </p>
                      {input.description && <p className="mt-0.5 text-muted">{input.description}</p>}
                    </div>
                  ))}
                </div>
              </section>

              <section>
                <h4 className="flex items-center gap-1.5 text-xs font-semibold text-foreground">
                  <ShieldAlert size={13} /> {t("SopCompiler.gatesHeading")}
                </h4>
                <div className="mt-2 space-y-1.5">
                  {selected.draft.policyGates.map((gate) => (
                    <div key={gate.label} className="flex items-start gap-2 rounded-md border border-warning/40 bg-warning/5 p-2 text-[11px]">
                      <AlertTriangle size={13} className="mt-0.5 shrink-0 text-warning" />
                      <div className="min-w-0">
                        <p className="font-medium text-foreground">
                          {gate.label} <span className="text-faint">({gate.riskLevel})</span>
                        </p>
                        {gate.description && <p className="mt-0.5 text-muted">{gate.description}</p>}
                      </div>
                    </div>
                  ))}
                </div>
              </section>

              <section>
                <h4 className="flex items-center gap-1.5 text-xs font-semibold text-foreground">
                  <CheckCircle2 size={13} /> {t("SopCompiler.testsHeading")}
                </h4>
                <div className="mt-2 space-y-1.5">
                  {selected.draft.tests.map((test) => (
                    <div key={test.label} className="rounded-md border border-border bg-background p-2 text-[11px]">
                      <p className="text-foreground">☐ {test.label}</p>
                      {test.expected && <p className="mt-0.5 text-faint">{t("SopCompiler.expectedPrefix")} {test.expected}</p>}
                    </div>
                  ))}
                </div>
              </section>

              <section>
                <h4 className="text-xs font-semibold text-foreground">{t("SopCompiler.evidenceHeading")}</h4>
                <div className="mt-2 space-y-1.5">
                  {selected.draft.evidence.map((item) => (
                    <div key={item.label} className="rounded-md border border-border bg-background p-2 text-[11px]">
                      <p className="text-foreground">{item.label}</p>
                      {item.description && <p className="mt-0.5 text-faint">{item.description}</p>}
                    </div>
                  ))}
                </div>
              </section>

              <p className="rounded-md border border-dashed border-border p-2.5 text-[10px] leading-4 text-faint">
                {t("SopCompiler.nonGoalsNote")}
              </p>

              {sendError && (
                <div role="alert" className="rounded-md border border-danger/40 bg-danger/10 p-2.5 text-xs text-danger">
                  {sendError}
                </div>
              )}

              <div className="flex flex-wrap gap-2">
                <Button
                  variant="primary"
                  size="sm"
                  disabled={selected.status === "sent_for_review" || sendingId === selected.id}
                  onClick={() => void handleSendToReview(selected.id)}
                >
                  {sendingId === selected.id ? <Loader2 className="animate-spin" size={13} /> : <ClipboardList size={13} />}
                  {selected.status === "sent_for_review"
                    ? t("SopCompiler.alreadySentButton")
                    : t("SopCompiler.sendToReviewButton")}
                </Button>
                <Button size="sm" variant="ghost" onClick={() => store.discardDraft(selected.id)}>
                  <Trash2 size={13} /> {t("SopCompiler.discardButton")}
                </Button>
              </div>
            </div>
          )}
        </div>
      </div>
    </section>
  );
}

export default SopCompilerPanel;
