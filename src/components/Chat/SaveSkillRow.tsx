import { useState } from "react";
import { Brain, LoaderCircle } from "lucide-react";

import { captureAction } from "../../lib/skillLearning";
import { draftCandidate } from "../../lib/skillLearningReflection";
import { skillLearningClient } from "../../lib/skillLearningClient";
import type { SaveSkillNotice } from "../../lib/skillLearning";
import { errorMessage } from "../../lib/errors";
import { useSkillLearningFocusStore } from "../../store/skillLearningFocusStore";
import type { SettingsTab } from "../Settings/SettingsModal";

export function SaveSkillRow({
  notice,
  onOpenSettingsTab,
}: {
  notice: SaveSkillNotice;
  onOpenSettingsTab?: (tab: SettingsTab) => void;
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [alreadyInstalled, setAlreadyInstalled] = useState<string | null>(null);

  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      const action = captureAction(await skillLearningClient.capture(notice.runId, notice.userText));
      if (action.kind === "already_installed") {
        setAlreadyInstalled(action.candidate.proposed_command || "");
        return;
      }
      if (action.kind === "draft") {
        const outcome = await draftCandidate(action.candidate.candidate_id);
        if (outcome.error) throw new Error(outcome.error);
        if (outcome.declined) throw new Error("No reusable procedure was found in this run.");
      }
      useSkillLearningFocusStore.getState().focus(action.candidate.candidate_id);
      onOpenSettingsTab?.("prompts");
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex justify-center">
      <div className="flex max-w-[85%] min-w-0 items-center gap-2 overflow-hidden rounded-md border border-border bg-surface-2 px-3 py-1.5 text-xs text-muted">
        <Brain size={13} className="shrink-0 text-faint" />
        {alreadyInstalled !== null ? (
          <>
            <span className="truncate font-medium text-foreground">
              Already saved{alreadyInstalled && ` as /${alreadyInstalled}`}
            </span>
            {onOpenSettingsTab && (
              <button
                type="button"
                className="ml-auto shrink-0 cursor-pointer whitespace-nowrap underline decoration-dotted underline-offset-2 transition-colors duration-150 hover:text-foreground"
                onClick={() => onOpenSettingsTab("prompts")}
              >
                View skill
              </button>
            )}
          </>
        ) : (
          <>
            <span className="truncate font-medium text-foreground">Save this run as a reusable skill?</span>
            <button
              type="button"
              disabled={busy}
              className="ml-auto flex shrink-0 cursor-pointer items-center gap-1 whitespace-nowrap underline decoration-dotted underline-offset-2 transition-colors duration-150 hover:text-foreground disabled:cursor-wait disabled:opacity-60"
              onClick={() => void save()}
            >
              {busy && <LoaderCircle size={12} className="animate-spin" />}
              {busy ? "Generating draft…" : "Save as skill"}
            </button>
          </>
        )}
        {error && <span className="truncate text-danger">{error}</span>}
      </div>
    </div>
  );
}

