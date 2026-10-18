import test from 'node:test';
import assert from 'node:assert/strict';
import { normalizeUserId } from './normalize.mjs';

test('lowercases a basic name', () => {
  assert.equal(normalizeUserId('Ada Lovelace'), 'ada-lovelace');
});

test('collapses repeated whitespace into one separator', () => {
  assert.equal(normalizeUserId('  Ada   Lovelace  '), 'ada-lovelace');
});

test('rejects non-string names', () => {
  assert.throws(() => normalizeUserId(null), TypeError);
});
