---
name: README Freshness Checker
description: Compare README claims against the actual code and CLI
command: readme-check
version: 1.0.0
requires:
  bins: []
  env: []
---
Read the README's usage examples, CLI flags, and setup steps. Cross-check each claim against the actual code — does the flag still exist, does the example still run the way it's described, is the setup step still necessary.

Report drift as a list of stale claims with the actual current behavior next to each one. Don't rewrite the README yourself unless asked.
