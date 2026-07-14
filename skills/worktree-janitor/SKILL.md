---
name: Worktree Janitor
description: List Little Monkey-owned worktrees eligible for archive or clean
command: worktree-janitor
version: 1.0.0
requires:
  bins: [git]
  env: []
---
List owned Git worktrees and their last-activity timestamp. Flag worktrees with no uncommitted changes and no activity in 14+ days as archive candidates.

Never archive or clean a worktree with uncommitted changes, and never touch a worktree this skill doesn't recognize as owned.
