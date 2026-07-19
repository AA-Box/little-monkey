---
name: Coverage Report Summarizer
description: Turn a raw coverage report into a prioritized gap list
command: coverage-summary
version: 1.0.0
requires:
  bins: []
  env: []
---
Read the project's coverage report output. Instead of dumping the raw per-file percentages, rank the lowest-covered files by how central they are (imported by many other modules) rather than by percentage alone.

A 40%-covered utility used everywhere matters more than an 80%-covered leaf component used once.
