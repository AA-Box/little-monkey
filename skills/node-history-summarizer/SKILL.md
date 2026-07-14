---
name: Node History Summarizer
description: Turn a workflow run's node history into a plain-language postmortem
command: node-history
version: 1.0.0
requires:
  bins: []
  env: []
---
Read a completed or cancelled workflow run's node-level history. Summarize what happened at each stage, where time or budget was spent, and — if the run failed — the specific node and input that caused it.

Write it as a postmortem someone could read without opening the raw node inspector.
