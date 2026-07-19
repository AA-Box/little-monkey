---
name: Duplicate Document Finder
description: Find near-duplicate ingested documents in a Knowledge Stack
command: dup-docs
version: 1.0.0
requires:
  bins: []
  env: []
---
Compare ingested documents for high textual overlap — same source re-ingested under a different path, or a near-identical revision of the same file.

Report duplicate clusters with a suggested canonical copy to keep. Never delete a document automatically; this is a merge proposal only.
