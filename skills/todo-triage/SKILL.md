---
name: TODO / FIXME Triage
description: Collect TODO/FIXME/HACK comments and rank them by age
command: todo-triage
version: 1.0.0
requires:
  bins: [git]
  env: []
---
Grep the workspace for TODO, FIXME, and HACK comments. For each, run `git blame` on that line to find its age and author.

Group into a table sorted oldest-first. A comment that's outlived several unrelated refactors nearby is worth flagging as possibly stale or forgotten, not just old.
