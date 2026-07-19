---
name: Provider Failover Auditor
description: Review provider and failover configuration for coverage gaps
command: failover-audit
version: 1.0.0
requires:
  bins: []
  env: []
---
Read the configured model providers and their failover chains. Flag any primary provider with no fallback configured, and any fallback chain that ultimately depends on a single point of failure (same API key, same region).

This is a configuration review — it doesn't change any settings.
