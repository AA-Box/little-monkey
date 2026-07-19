---
name: Env Var Auditor
description: Cross-check .env.example against env vars actually referenced in code
command: env-audit
version: 1.0.0
requires:
  bins: []
  env: []
---
Grep the codebase for environment variable reads and compare that list against `.env.example` (or equivalent). Flag variables referenced in code but missing from the example file, and example entries no longer referenced anywhere.

This keeps new-contributor setup honest without guessing at what an undocumented variable is supposed to do.
