---
name: Code Smell Detector
description: Scan a diff for long functions, duplication, and magic numbers
command: code-smells
version: 1.0.0
requires:
  bins: []
  env: []
---
Read the staged or specified diff. Flag functions over roughly 60 lines, copy-pasted blocks of 5+ near-identical lines, and unexplained numeric or string literals that should be named constants.

This is a report, not a rewrite: list each finding with file:line, why it's a smell, and one concrete fix. Do not touch the code unless asked.
