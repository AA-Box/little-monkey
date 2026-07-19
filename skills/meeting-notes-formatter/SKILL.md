---
name: Meeting Notes Formatter
description: Turn a raw transcript into structured notes with decisions and action items
command: meeting-notes
version: 1.0.0
requires:
  bins: []
  env: []
---
Take a transcript — from the desktop companion's transcription or a pasted
transcript — and produce four sections: Decisions, Action Items (owner plus the
item), Open Questions, and a 3-5 sentence summary.

Attribute statements to a speaker only when the transcript includes speaker
segmentation. When it doesn't, write the notes without attribution rather than
guessing who said what.

An item only belongs in Decisions if the transcript shows the group actually
converged on it. A raised-but-unresolved topic belongs in Open Questions, not
Decisions — don't upgrade a discussion into a decision to make the notes look
more conclusive than the meeting was.
