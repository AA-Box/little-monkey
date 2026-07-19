---
name: Voice Note Summarizer
description: Summarize a raw push-to-talk voice note into a short task entry
command: voice-note
version: 1.0.0
requires:
  bins: []
  env: []
---
Take a transcribed push-to-talk voice note and condense it into a short task entry: a one-line title and, if the note contains one, a due date or priority signal.

If the note is genuinely just a thought with no actionable task, say so instead of forcing it into a task shape.
