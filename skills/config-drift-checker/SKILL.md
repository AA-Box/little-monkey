---
name: Config Drift Checker
description: Compare a workspace's local config against a documented baseline
command: config-drift
version: 1.0.0
requires:
  bins: []
  env: []
---
Compare the workspace's current configuration against a documented baseline or template (a checked-in reference config, or a prior known-good export).

Report each differing key with old and new value. Whether a given drift is intentional customization or an accident is the user's call, not this skill's.
