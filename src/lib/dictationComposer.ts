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

/** Join finalized recognition segments without creating `helloWorld` or gaps. */
export function joinDictationSegments(left: string, right: string): string {
  const next = right.trim();
  if (!left) return next;
  if (!next) return left;
  if (/\s$/.test(left)) return `${left}${next}`;
  return `${left} ${next}`;
}

export function dictationInsertedText(state: DictationInsertionState): string {
  return `${state.committed}${state.partial}`;
}

export function renderDictationInsertion(state: DictationInsertionState): string {
  return `${state.prefix}${dictationInsertedText(state)}${state.suffix}`;
}

export function caretAfterDictation(state: DictationInsertionState): number {
  return state.prefix.length + dictationInsertedText(state).length;
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
