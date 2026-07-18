---
name: Release Notes Drafter
description: Turn a milestone's merged changes into user-facing release notes
command: release-notes
version: 1.0.0
requires:
  bins: [git]
  env: []
---
Read the commits or merged PRs since the last release tag. Filter out internal-only changes (tests, CI, refactors with no behavior change) and group the rest into New, Improved, and Fixed.

Write each entry from the user's perspective — what changed for them, not which files changed. Skip anything you can't describe honestly from the diff alone.
