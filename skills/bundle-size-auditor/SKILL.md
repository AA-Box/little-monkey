---
name: Bundle Size Auditor
description: Inspect a production build and flag its largest contributors
command: bundle-size
version: 1.0.0
requires:
  bins: []
  env: []
---
Read the build output manifest (or run the project's existing build command if one is configured) and list the largest emitted chunks by size.

For each large chunk, identify the dependency or module driving its size and note if a lighter alternative or dynamic import would help. Don't change the build config — report the opportunity and let the user decide.
