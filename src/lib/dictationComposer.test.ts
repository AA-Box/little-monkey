import { describe, expect, it } from 'vitest';

import {
  beginDictationInsertion,
  caretAfterDictation,
  commitDictationFinal,
  dictationInsertedText,
  joinDictationSegments,
  renderDictationInsertion,
  withDictationPartial,
} from './dictationComposer';

describe('dictation composer insertion', () => {
  it('inserts at the current caret and preserves the original suffix', () => {
    let state = beginDictationInsertion('session-1', 'alpha omega', 6, 6);
    state = withDictationPartial(state, 'beta');

    expect(renderDictationInsertion(state)).toBe('alpha beta omega');
    expect(caretAfterDictation(state)).toBe(10);

    state = commitDictationFinal(state, 'beta');
    expect(dictationInsertedText(state)).toBe('beta');
    expect(renderDictationInsertion(state)).toBe('alpha beta omega');
  });

  it('replaces a selection without losing text outside that selection', () => {
    const state = commitDictationFinal(
      beginDictationInsertion('session-2', 'say this please', 4, 8),
      'that',
    );

    expect(renderDictationInsertion(state)).toBe('say that please');
    expect(caretAfterDictation(state)).toBe(8);
  });

  it('leaves the original value and selection unchanged when no speech is recognized', () => {
    const state = beginDictationInsertion('session-empty', 'say this', 4, 8);

    expect(renderDictationInsertion(state)).toBe('say this');
    expect(caretAfterDictation(state)).toBe(4);
  });

  it('replaces a provisional partial instead of appending it', () => {
    let state = beginDictationInsertion('session-3', '', 0, 0);
    state = withDictationPartial(state, 'hello wor');
    expect(renderDictationInsertion(state)).toBe('hello wor');

    state = withDictationPartial(state, 'hello world');
    expect(renderDictationInsertion(state)).toBe('hello world');
  });

  it('joins finalized segments with word spacing but preserves explicit spacing', () => {
    expect(joinDictationSegments('', 'hello')).toBe('hello');
    expect(joinDictationSegments('hello', 'world')).toBe('hello world');
    expect(joinDictationSegments('hello ', 'world')).toBe('hello world');
    expect(joinDictationSegments('hello', ' world')).toBe('hello world');
    expect(joinDictationSegments('in', 'the login service')).toBe('in the login service');
    expect(joinDictationSegments('hello', ', world')).toBe('hello, world');
  });

  it('clamps an invalid native selection safely', () => {
    const state = beginDictationInsertion('session-4', 'hello', -20, 99);
    expect(state.selectionStart).toBe(0);
    expect(state.selectionEnd).toBe(5);
    expect(renderDictationInsertion(commitDictationFinal(state, 'bye'))).toBe('bye');
  });
});
