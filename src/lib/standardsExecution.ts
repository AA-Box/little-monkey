import { standardsPromptSection, standardsSelectionProvenance, type StandardsSelectionProvenance } from "./standards";
import { primaryRoot, useWorkspaceStore } from "../store/workspaceStore";
import { useStandardsStore } from "../store/standardsStore";

/** Immutable Standards authority captured once for one concrete operation.
 * Callers keep this object for the whole run instead of re-selecting from
 * mutable stores on every model round trip. */
export interface FrozenStandardsContext {
  promptSection: string;
  checkerCommandIds: string[];
  provenance: StandardsSelectionProvenance | null;
}

export const EMPTY_FROZEN_STANDARDS_CONTEXT: FrozenStandardsContext = Object.freeze({
  promptSection: "",
  checkerCommandIds: Object.freeze([]) as unknown as string[],
  provenance: null,
});

/**
 * Selects Standards against the exact task text and resolved workspace-file
 * hints for one operation, then freezes both the prompt text and locally-bound
 * mechanical checker IDs. The workspace is refreshed only when the Standards
 * store is not already hydrated for the current primary root.
 */
export async function freezeStandardsForTask(
  taskText: string,
  fileHints: readonly string[] = [],
): Promise<FrozenStandardsContext> {
  const normalizedTask = taskText.trim();
  const workspacePath = primaryRoot(useWorkspaceStore.getState().roots)?.path ?? null;
  if (!workspacePath || !normalizedTask) return EMPTY_FROZEN_STANDARDS_CONTEXT;

  let state = useStandardsStore.getState();
  if (state.workspacePath !== workspacePath || state.document === null) {
    await state.refresh();
    state = useStandardsStore.getState();
  }
  // A workspace switch can race the async refresh. Never borrow Standards
  // from the newly active workspace for an operation that started in another.
  if (state.workspacePath !== workspacePath || state.document === null) {
    return EMPTY_FROZEN_STANDARDS_CONTEXT;
  }

  const selection = state.preview(normalizedTask, [...fileHints]);
  const checkerCommandIds = [...new Set(
    selection.selected.flatMap(({ standard }) => standard.checker_command_ids),
  )].sort();
  return {
    promptSection: standardsPromptSection(selection),
    checkerCommandIds,
    provenance: standardsSelectionProvenance(selection),
  };
}
