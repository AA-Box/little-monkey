---
name: Rate Limit Watcher
description: Summarize recent rate-limit warnings and suggest pacing changes
command: rate-limit-watch
version: 1.0.0
requires:
  bins: []
  env: []
---
Read recent rate-limit warning events. Summarize which provider/model combinations are hitting limits, how often, and at what time patterns.

Suggest concrete pacing changes (batch size, concurrency, off-peak scheduling) grounded in the actual observed pattern, not generic advice.
