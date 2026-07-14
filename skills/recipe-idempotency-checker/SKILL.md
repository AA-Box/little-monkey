---
name: Recipe Idempotency Checker
description: Review a recipe or workflow definition for non-idempotent steps
command: idempotency-check
version: 1.0.0
requires:
  bins: []
  env: []
---
Read the given recipe/workflow definition and flag steps that would have a different or harmful effect if the recipe were retried after a partial failure — a step that appends rather than sets, an external side effect with no idempotency key.

For each flagged step, suggest the minimal change (an idempotency key, a check-before-write) rather than a redesign.
