---
name: Permission Mode Advisor
description: Review a workspace's permission mode against its actual tool usage
command: permission-advisor
version: 1.0.0
requires:
  bins: []
  env: []
---
Look at which tools a workspace's recent turns have actually used (file writes, shell, web fetch) and compare that against its configured permission mode.

Flag a mismatch in either direction: a workspace running in a loose mode that never actually needed shell access, or one in a strict mode that keeps hitting friction for routine file edits.
