---
name: Dead Code Finder
description: Find exported functions and files with no remaining callers
command: dead-code
version: 1.0.0
requires:
  bins: []
  env: []
---
For each exported symbol in the given path, grep the rest of the codebase for references. An export with zero references outside its own file, and not re-exported from a public entry point, is a removal candidate.

Report candidates with confidence: high (no references anywhere, including tests) or low (only referenced from a test that might itself be dead). Never delete anything — this skill only reports.
