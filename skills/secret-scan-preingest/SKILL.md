---
name: Secret Scan (Pre-Ingest)
description: Scan a folder for likely secrets before it's added to a Knowledge Stack
command: secret-scan
version: 1.0.0
requires:
  bins: []
  env: []
---
Scan the given folder for API-key-shaped strings, private key blocks, and common `.env`-style secret patterns, independent of the ingest-time redaction pass.

Report file:line for each hit with the matched pattern type. This runs before ingest starts — it doesn't ingest, redact, or modify anything itself.
