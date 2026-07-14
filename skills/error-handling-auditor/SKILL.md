---
name: Error Handling Auditor
description: Find empty catch blocks and silently swallowed errors
command: error-handling
version: 1.0.0
requires:
  bins: []
  env: []
---
Scan for `catch` blocks that are empty, only log to console, or re-throw a generic error that discards the original cause.

For each one, note what information is being lost and suggest whether it should propagate, be logged with context, or be handled explicitly. Don't guess at the right recovery behavior for business logic you can't see.
