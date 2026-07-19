---
name: Stale Source Flagger
description: Flag ingested sources that haven't refreshed in a given window
command: stale-sources
version: 1.0.0
requires:
  bins: []
  env: []
---
List ingested URLs and watched files with their last successful refresh timestamp. Flag anything past the given staleness window (default 30 days) along with the reason it stopped refreshing if one is recorded (auth expiry, 404, connector error).

Group by likely cause so the user can fix the root issue once instead of per source.
