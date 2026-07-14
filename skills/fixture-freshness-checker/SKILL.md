---
name: Fixture Freshness Checker
description: Flag test fixtures and snapshots older than a threshold
command: fixture-check
version: 1.0.0
requires:
  bins: [git]
  env: []
---
Find test fixture and snapshot files and check their last-modified commit date via git log. Flag anything older than a given threshold (default 180 days) for review.

Age alone isn't proof a fixture is wrong — flag it as a review candidate, not a confirmed problem.
