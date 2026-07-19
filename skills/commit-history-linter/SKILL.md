---
name: Commit History Linter
description: Check recent commits against Conventional Commits format
command: commit-lint
version: 1.0.0
requires:
  bins: [git]
  env: []
---
Read the last N commits (or a given range) and check each subject line against Conventional Commits: valid type prefix, imperative mood, under 72 characters, no trailing period.

Report violations with the offending commit and what a compliant subject would look like. This is a lint pass, not a history rewrite.
