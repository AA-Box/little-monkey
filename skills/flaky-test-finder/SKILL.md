---
name: Flaky Test Finder
description: Cross-reference CI logs for intermittently failing tests
command: flaky-tests
version: 1.0.0
requires:
  bins: []
  env: []
---
Given a set of CI run logs or a pasted test history, find tests that both passed and failed across recent runs on the same commit (a strong flakiness signal, as opposed to a real regression that fails consistently).

Report each flaky candidate with its failure rate and, if visible in the logs, the error message pattern — timing-sensitive assertions and shared state are the usual culprits.
