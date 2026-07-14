---
name: Issue Triage Assistant
description: Suggest labels and priority for a GitHub issue from its content and repo context
command: issue-triage
version: 1.0.0
requires:
  bins: [gh]
  env: []
---
Read the issue title, body, and any linked context via `gh`. Suggest candidate labels (bug/feature/docs/question), a rough priority, and — if the issue clearly maps to a specific area of the codebase — which files or modules are probably relevant.

This produces suggestions only. Applying labels, assigning owners, or commenting on the issue are separate explicit actions.
