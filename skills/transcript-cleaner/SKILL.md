---
name: Transcript Cleaner
description: Fix obvious ASR errors in a raw transcript before formatting
command: transcript-clean
version: 1.0.0
requires:
  bins: []
  env: []
---
Read a raw transcription and fix obvious speech-to-text errors: misheard homophones, misplaced punctuation, run-on sentences from missed pauses.

Do not change meaning, summarize, or restructure — this is a cleanup pass that produces corrected text in the same shape as the input, ready for a separate formatting or summarization step.
