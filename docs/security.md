# Workspace and trust boundaries

What the permission and trust model guarantees. The enforcement gaps it does
*not* close are in [Limitations](limitations.md#enforcement-and-isolation).

Little Monkey canonicalizes workspace paths and rejects traversal and symlink escapes. Read-only workspace operations do not mutate files; mutating file, shell, memory, MCP, browser, Git and GitHub, workflow, background, capture, and remote actions use their applicable permission or grant boundary. A remote server's `readOnlyHint`, model output, webpage text, package instructions, or imported archive can never approve its own operation.

Shell commands run inside the workspace with bounded time and cancellation. Scheduled and headless recipes require an explicit permission mode and cannot use unattended `bypass`. External mutations are recorded as pending, confirmed, or `needs_reconciliation`; ambiguous effects are not retried as if known safe. API keys, OAuth tokens, bearer secrets, remote device keys, and TLS private keys use the OS keychain where the feature supports credentials.

A learned skill cannot widen what a run may do. Candidates are opened only from a completed run's own durable events, never from model output, retrieved content, or tool output claiming a procedure should be remembered; the model's `manage_skill_learning` tool can propose and request, and cannot approve, publish, or write a file. Proposed content is size-bounded and rebuilt into `SKILL.md` by deterministic code, resource paths are validated relative paths inside an app-owned staging directory, and the installed digest is recomputed from the staged bytes at the moment it authorizes the install. A widened tool list, a new executable or environment requirement, or global scope needs approval even under unattended promotion; content that would weaken permission policy or bypass a permission mode is refused outright, and a command already provided by a skill this loop did not install is never overwritten. Promotion publishes atomically or not at all, so a failed or interrupted one leaves the previously active version intact, and provenance is keyed by installed content hash so a rollback restores a real previous version together with its own evidence.

Security Doctor is a posture aid, not a substitute for operating-system updates, endpoint security, or a release penetration test.
