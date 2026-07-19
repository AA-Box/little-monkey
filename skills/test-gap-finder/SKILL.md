---
name: Test Gap Finder
description: Compare changed files against touched test files to flag untested changes
command: test-gaps
version: 1.0.0
requires:
  bins: [git]
  env: []
---
For the current diff, list changed source files and check whether a corresponding test file was also touched in the same change.

Flag source changes with no matching test update as gaps — but skip files where a test genuinely doesn't make sense (pure config, generated files, type-only changes).
