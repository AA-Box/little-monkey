import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { History, ListOrdered, PlayCircle, RefreshCw } from "lucide-react";
import { Button, StatusPill } from "../ui";
import type { PillTone } from "../ui";
import { useT } from "../../lib/i18n";

/** Mirrors `src-tauri/src/approval_chains.rs`'s `ChainStage`. */
interface ChainStage {
  label: string;
  timeout_secs: number;
  escalate_after_secs?: number;
  escalate_message?: string;
}

/** Mirrors `src-tauri/src/approval_chains.rs`'s `ApprovalChainTemplate`. */
interface ApprovalChainTemplate {
  id: string;
  name: string;
  stages: ChainStage[];
}

type ChainStatus = "pending" | "approved" | "rejected" | "expired";
type StageDecisionKind = "allow" | "deny" | "expired";

/** Mirrors `src-tauri/src/approval_chains.rs`'s `StageDecision`. */
interface StageDecision {
  stage_index: number;
  label: string;
  decision: StageDecisionKind;
  decided_at_ms: number;
  escalated: boolean;
  decided_by?: { client_id: string; instance_id: string; kind: string; version: string };
}

/** Mirrors `src-tauri/src/approval_chains.rs`'s `ApprovalChainRun`. */
interface ApprovalChainRun {
  id: string;
  template_id: string;
  operation_digest: string;
  detail: string;
  current_stage: number;
  decisions: StageDecision[];
  status: ChainStatus;
}

const STATUS_TONE: Record<ChainStatus, PillTone> = {
  pending: "warning",
  approved: "success",
  rejected: "danger",
  expired: "neutral",
};

const DECISION_TONE: Record<StageDecisionKind, PillTone> = {
  allow: "success",
  deny: "danger",
  expired: "neutral",
};

function formatDate(ms: number): string {
  return new Date(ms).toLocaleString();
}

/**
 * Settings surface for Human Approval Chains (ROADMAP.md, Phase 3): shows
 * the built-in chain templates, lets the user run one as a manual test (the
 * reachable entry point for this stage — no other shipped feature calls
 * `run_approval_chain` yet, see `approval_chains.rs`'s module doc), and
 * displays the full audit history of past chain runs.
 */
export function ApprovalChainsPanel() {
  const { t } = useT();
  const [templates, setTemplates] = useState<ApprovalChainTemplate[]>([]);
  const [history, setHistory] = useState<ApprovalChainRun[]>([]);
  const [runningTemplateId, setRunningTemplateId] = useState<string | null>(null);
  const [loadingHistory, setLoadingHistory] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function refreshHistory() {
    setLoadingHistory(true);
    try {
      const rows = await invoke<ApprovalChainRun[]>("approval_chains_history", { limit: 50 });
      setHistory(rows);
    } catch (err) {
      setError(String(err));
    } finally {
      setLoadingHistory(false);
    }
  }

  useEffect(() => {
    void invoke<ApprovalChainTemplate[]>("approval_chains_list_templates")
      .then(setTemplates)
      .catch((err) => setError(String(err)));
    void refreshHistory();
  }, []);

  async function runTestChain(templateId: string) {
    setError(null);
    setRunningTemplateId(templateId);
    try {
      await invoke<boolean>("approval_chains_start", {
        templateId,
        detail: t("ApprovalChainsPanel.testRunDetail"),
      });
    } catch (err) {
      setError(String(err));
    } finally {
      setRunningTemplateId(null);
      void refreshHistory();
    }
  }

  return (
    <div className="flex flex-col gap-6">
      <div>
        <h3 className="text-sm font-semibold text-foreground">{t("ApprovalChainsPanel.templatesHeading")}</h3>
        <p className="mt-1 text-xs text-muted">{t("ApprovalChainsPanel.description")}</p>
      </div>

      {error && (
        <div className="rounded-md border border-danger/40 bg-danger-soft p-2.5 text-xs text-danger">{error}</div>
      )}

      <div className="flex flex-col gap-3">
        {templates.map((template) => (
          <div key={template.id} className="rounded-lg border border-border bg-surface p-3">
            <div className="flex items-center justify-between gap-3">
              <div className="flex items-center gap-2">
                <ListOrdered size={16} className="text-muted" />
                <span className="text-sm font-medium text-foreground">{template.name}</span>
              </div>
              <Button
                size="sm"
                variant="secondary"
                disabled={runningTemplateId !== null}
                onClick={() => void runTestChain(template.id)}
              >
                <PlayCircle size={14} className="mr-1.5" />
                {runningTemplateId === template.id
                  ? t("ApprovalChainsPanel.runningButton")
                  : t("ApprovalChainsPanel.runTestChainButton")}
              </Button>
            </div>
            <ol className="mt-2.5 flex flex-col gap-1">
              {template.stages.map((stage, index) => (
                <li key={index} className="flex items-center gap-2 text-xs text-muted">
                  <span className="flex h-4 w-4 shrink-0 items-center justify-center rounded-full bg-surface-2 text-[10px] font-medium text-foreground">
                    {index + 1}
                  </span>
                  <span>{stage.label}</span>
                  <span className="text-faint">
                    {t("ApprovalChainsPanel.stageTimeout", { seconds: stage.timeout_secs })}
                  </span>
                </li>
              ))}
            </ol>
          </div>
        ))}
        {templates.length === 0 && (
          <p className="text-xs text-faint">{t("ApprovalChainsPanel.noTemplatesState")}</p>
        )}
      </div>

      <div>
        <div className="flex items-center justify-between">
          <h3 className="flex items-center gap-2 text-sm font-semibold text-foreground">
            <History size={16} className="text-muted" />
            {t("ApprovalChainsPanel.historyHeading")}
          </h3>
          <Button size="sm" variant="secondary" onClick={() => void refreshHistory()} disabled={loadingHistory}>
            <RefreshCw size={14} className={`mr-1.5 ${loadingHistory ? "animate-spin" : ""}`} />
            {t("ApprovalChainsPanel.refreshButton")}
          </Button>
        </div>

        <div className="mt-2.5 flex flex-col gap-2">
          {history.map((run) => (
            <div key={run.id} className="rounded-lg border border-border bg-surface p-3">
              <div className="flex items-center justify-between gap-3">
                <span className="truncate text-xs font-mono text-muted">{run.detail}</span>
                <StatusPill tone={STATUS_TONE[run.status]}>
                  {t(`ApprovalChainsPanel.status.${run.status}`)}
                </StatusPill>
              </div>
              <ul className="mt-2 flex flex-col gap-1">
                {run.decisions.map((decision) => (
                  <li key={decision.stage_index} className="flex items-center justify-between gap-2 text-xs text-muted">
                    <span>{decision.label}</span>
                    <span className="flex items-center gap-2">
                      {decision.decided_by && (
                        <span className="text-faint">{decision.decided_by.client_id}</span>
                      )}
                      <span className="text-faint">{formatDate(decision.decided_at_ms)}</span>
                      <StatusPill tone={DECISION_TONE[decision.decision]}>
                        {t(`ApprovalChainsPanel.decision.${decision.decision}`)}
                      </StatusPill>
                    </span>
                  </li>
                ))}
              </ul>
            </div>
          ))}
          {history.length === 0 && !loadingHistory && (
            <p className="text-xs text-faint">{t("ApprovalChainsPanel.emptyHistoryState")}</p>
          )}
        </div>
      </div>
    </div>
  );
}

export default ApprovalChainsPanel;
