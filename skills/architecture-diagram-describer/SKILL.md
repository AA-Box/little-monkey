---
name: Architecture Diagram Describer
description: Produce a Mermaid diagram of module dependencies
command: arch-diagram
version: 1.0.0
requires:
  bins: []
  env: []
---
Trace the import graph starting from the given entry point, one or two levels deep. Produce a Mermaid flowchart showing the major modules and the direction of their dependencies.

Keep it to the modules that matter for understanding the architecture — a diagram with every file is not a diagram anyone can read.
