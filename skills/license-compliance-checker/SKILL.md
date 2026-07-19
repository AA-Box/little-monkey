---
name: License Compliance Checker
description: Scan dependency licenses for copyleft or incompatible terms
command: license-check
version: 1.0.0
requires:
  bins: []
  env: []
---
Read the dependency manifest and lockfile. For each direct dependency, note its declared license. Flag copyleft licenses (GPL, AGPL) and anything unlicensed or with a non-standard license string for manual review.

This is informational only — license compatibility with your own project's license is a legal judgment call, not something to auto-resolve.
