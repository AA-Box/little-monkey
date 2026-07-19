---
name: Context Budget Planner
description: Estimate token budget breakdown for a planned long-context task
command: context-budget
version: 1.0.0
requires:
  bins: []
  env: []
---
Given a planned task (files to attach, expected turns, system prompt size), estimate a rough token budget breakdown: fixed overhead, attached content, and remaining headroom for the conversation.

Flag if the plan is likely to hit context limits before the task completes, and suggest what to trim first (usually the largest single attachment, not the system prompt).
