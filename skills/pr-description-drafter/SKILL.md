---
name: PR Description Drafter
description: Draft a pull request description from the branch's commit range
command: pr-description
version: 1.0.0
requires:
  bins: [git]
  env: []
---
Read the commits on the current branch not yet on the target branch. Draft a PR description with a short summary, a bulleted list of changes grouped by concern, and a test plan section listing what should be verified before merge.

Base the test plan on what actually changed — don't list generic checklist items that don't apply to this diff.
