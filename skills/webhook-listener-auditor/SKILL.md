---
name: Webhook Listener Auditor
description: Review configured webhook listeners for exposure risk
command: webhook-audit
version: 1.0.0
requires:
  bins: []
  env: []
---
List configured webhook and API listeners with their bind address, auth requirement, and rate limit. Flag anything bound beyond loopback without authentication configured.

Report findings in the same spirit as Security Doctor — informational, read-only, no listener is disabled by this skill.
