---
name: Commit Message Writer
description: Draft a Conventional Commits message from the staged diff
command: commit-message
version: 1.0.0
requires:
  bins: [git]
  env: []
---
Read `git diff --staged`. If nothing is staged, say so and stop — do not guess at
unstaged changes.

Classify the dominant change (feat, fix, refactor, docs, test, chore, perf, build,
ci). If the diff mixes unrelated concerns, say so and suggest splitting the commit
rather than picking one type to cover everything.

Write a subject line in imperative mood, under 50 characters, using the
`type(scope): subject` form when a scope is obvious from the changed paths. Add a
body only when the "why" is not already obvious from the diff — a one-line body
beats a padded one. Never describe changes that are not actually present in the
diff.

Print the finished message in a fenced block, ready to pass to `git commit -m`.
