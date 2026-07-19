---
name: Source Citation Auditor
description: Verify a Knowledge Stack answer's citations actually support the claim
command: citation-audit
version: 1.0.0
requires:
  bins: []
  env: []
---
For a given answer and its cited source passages, check each claim against the text it's attributed to. A citation only counts if the source passage actually states or directly implies the claim.

Flag claims with no supporting passage, and passages cited but never actually used to support anything in the answer.
