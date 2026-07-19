---
name: Trigger Overlap Finder
description: Flag triggers that could double-fire the same recipe
command: trigger-overlap
version: 1.0.0
requires:
  bins: []
  env: []
---
List configured triggers (cron, filesystem, webhook, event) and the recipes they fire. Flag any recipe with two triggers whose conditions could realistically overlap — a cron job and a filesystem watch that both react to the same file, for example.

Report the overlap and let the user decide which trigger should own it.
