---
name: Redaction Preview Reviewer
description: Summarize what a PII/secret redaction pass would strip before ingest
command: redaction-preview
version: 1.0.0
requires:
  bins: []
  env: []
---
Before a document enters a Knowledge Stack, scan it for likely PII (emails, phone numbers, personal names in structured fields) and likely secrets (API key-shaped strings, tokens).

Report what would be redacted and where, so the user can decide before ingest rather than after. Don't ingest or modify the document — preview only.
