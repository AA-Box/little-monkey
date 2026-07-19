---
name: Prompt Regression Detector
description: Diff a prompt template's old and new versions for behavior shifts
command: prompt-regression
version: 1.0.0
requires:
  bins: [git]
  env: []
---
Diff the old and new version of a prompt or system message. Beyond the textual diff, describe what behavior change each edit is likely to cause — a removed constraint, a changed tone instruction, a new example that biases output a certain way.

Recommend running the affected prompt through Compare mode against the old version before shipping the change.
