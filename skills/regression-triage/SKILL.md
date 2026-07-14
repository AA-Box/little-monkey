---
name: Regression Triage
description: Narrow down the likely culprit commit for a failing test
command: regression-triage
version: 1.0.0
requires:
  bins: [git]
  env: []
---
Given a failing test and the commit range since it last passed, use `git log -p` on the files the test actually exercises to shortlist commits that touched relevant logic.

Rank candidates by relevance, not just recency, and explain the reasoning for the top candidate. This narrows the search — it doesn't replace running a real bisect.
