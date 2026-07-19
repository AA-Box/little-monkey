---
name: API Surface Diff
description: Compare a package's exported symbols between two commits
command: api-diff
version: 1.0.0
requires:
  bins: [git]
  env: []
---
Given two commit refs, list the exported symbols (functions, types, constants) from the package's public entry point at each ref, then diff them.

Classify each change as additive (safe), removed (breaking), or signature-changed (breaking unless purely additive optional params). This is for release-note and semver-bump decisions, not a merge blocker.
