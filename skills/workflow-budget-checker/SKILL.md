---
name: Workflow Budget Checker
description: Review a workflow's token/cost budget against its node count and complexity
command: workflow-budget
version: 1.0.0
requires:
  bins: []
  env: []
---
Estimate a workflow DAG's likely token and cost consumption per run based on its node count, model choices, and any loop bounds, and compare that against its configured budget.

Flag workflows where the configured budget looks too tight to complete a normal run, or so loose it wouldn't catch a runaway loop.
