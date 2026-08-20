/** Pure composer insertion rules for native dictation. */

export interface DictationInsertionState {
  sessionId: string;
  originalValue: string;
  selectionStart: number;
  selectionEnd: number;
  prefix: string;
  suffix: string;
  partial: string;
  committed: string;
}

export function beginDictationInsertion(
  sessionId: string,
  value: string,
  selectionStart: number,
  selectionEnd: number,
): DictationInsertionState {
  const start = Math.max(0, Math.min(selectionStart, value.length));
  const end = Math.max(start, Math.min(selectionEnd, value.length));
  return {
    sessionId,
    originalValue: value,
    selectionStart: start,
    selectionEnd: end,
    prefix: value.slice(0, start),
    suffix: value.slice(end),
    partial: "",
    committed: "",
  };
}

const NO_SPACE_BEFORE = /^[,.;:!?%)\]}]/;
const NO_SPACE_AFTER = /[([{\/]$/;

/** Join prose at a boundary without duplicating or dropping meaningful spaces. */
export function joinDictationSegments(left: string, right: string): string {
  if (!left || !right) return `${left}${right}`;
  if (/\s$/.test(left) || /^\s/.test(right)) {
    if (/\s$/.test(left) && /^\s/.test(right)) return `${left}${right.replace(/^\s+/, "")}`;
    return `${left}${right}`;
  }
  if (NO_SPACE_BEFORE.test(right) || NO_SPACE_AFTER.test(left)) return `${left}${right}`;
  return `${left} ${right}`;
}

export function dictationInsertedText(state: DictationInsertionState): string {
  return joinDictationSegments(state.committed, state.partial);
}

export function renderDictationInsertion(state: DictationInsertionState): string {
  const inserted = dictationInsertedText(state);
  if (!inserted) return state.originalValue;
  return joinDictationSegments(joinDictationSegments(state.prefix, inserted), state.suffix);
}

export function caretAfterDictation(state: DictationInsertionState): number {
  const inserted = dictationInsertedText(state);
  return inserted ? joinDictationSegments(state.prefix, inserted).length : state.selectionStart;
}

export function withDictationPartial(
  state: DictationInsertionState,
  partial: string,
): DictationInsertionState {
  return { ...state, partial };
}

export function commitDictationFinal(
  state: DictationInsertionState,
  finalText: string,
): DictationInsertionState {
  return {
    ...state,
    committed: joinDictationSegments(state.committed, finalText),
    partial: "",
  };
}
