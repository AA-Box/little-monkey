---
name: Model Benchmark Runner
description: Compare local models on a fixed prompt set and summarize tradeoffs
command: model-bench
version: 1.0.0
requires:
  bins: []
  env: []
---
Using Compare mode, run the requested prompt set across the requested local
targets. If no prompt set is given, use a default spread of five prompts covering
short factual Q&A, a small code task, and a longer-context summarization task.

For each prompt/target pair, record latency, token usage, and a plain pass/fail
against a stated success criterion — not a vague quality score. Only report
numbers Compare mode actually returned for this run; never extrapolate or quote
benchmark figures from outside this session.

Summarize as a table ranked by whatever the user said they care about more,
speed or quality. If they didn't say, present both rankings side by side instead
of picking one.
