---
name: Branch Cleanup Advisor
description: List merged and stale local branches safe to delete
command: branch-cleanup
version: 1.0.0
requires:
  bins: [git]
  env: []
---
List local branches already merged into the default branch, and branches with no commits in the last 90 days that aren't the current branch.

Report them as a cleanup candidate list with last-commit date and author. This skill never deletes a branch itself — it hands you the list to review.
