---
name: Rebase Planner
description: Outline a rebase plan for a branch against a moved base
command: rebase-plan
version: 1.0.0
requires:
  bins: [git]
  env: []
---
Compare the branch's commits against the new base and flag which commits are likely to conflict, based on overlapping changed files.

Suggest a commit order and note any commit that looks safe to `--fixup` or squash into an earlier one. Produce the plan only — don't run the rebase.
