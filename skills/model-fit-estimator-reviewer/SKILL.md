---
name: Model Fit Estimator Reviewer
description: Sanity-check the inputs behind a model hardware-fit estimate
command: model-fit-check
version: 1.0.0
requires:
  bins: []
  env: []
---
Given a model's fit estimate and the hardware profile it was computed against, check whether the inputs (available RAM, quantization, context length assumed) match the user's actual intended usage.

A fit estimate computed at a shorter context length than the user plans to actually use is misleading — flag that mismatch specifically.
