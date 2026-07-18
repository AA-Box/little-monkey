---
name: Merge Conflict Explainer
description: Summarize both sides of a conflict in plain language before resolving
command: conflict-explain
version: 1.0.0
requires:
  bins: [git]
  env: []
---
For each conflicted file, read both sides of the conflict markers and summarize in plain language what each side was trying to accomplish — not just what the diff shows syntactically.

Suggest a resolution only when one side is a strict superset of the other or the intents are clearly compatible; otherwise say so and leave the call to the user.
