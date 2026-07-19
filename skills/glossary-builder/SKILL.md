---
name: Glossary Builder
description: Extract and define domain-specific terms used across the codebase
command: glossary
version: 1.0.0
requires:
  bins: []
  env: []
---
Scan identifiers, comments, and docs for domain terms that a newcomer wouldn't know from general programming knowledge (project-specific nouns, internal abbreviations, business concepts).

For each term, write a one-sentence definition grounded in how the code actually uses it, and note the file where it's most clearly defined or used.
