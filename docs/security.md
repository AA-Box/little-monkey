# Workspace and trust boundaries

What the permission and trust model guarantees. The enforcement gaps it does
*not* close are in [Limitations](limitations.md#enforcement-and-isolation).

Execution targets add a second, explicit trust boundary. A run freezes the
target identity and capability digest before submission; target changes are
reported as identity/protocol failures, not silently accepted. Workspace
transfers are content-addressed and materialized only below runner-owned data
directories. Paths, links, special files, transfer sizes, and normalization
collisions are validated on both sides. Docker receives no host socket,
privilege, or arbitrary mount, and SSH requires strict known-host verification.
Secrets resolve on the executor by default; only explicitly supplied
environment entries are forwarded, and the whole originating environment is
never copied. Remote loss is recorded separately from cancellation, so a
connection drop cannot claim that remote work stopped.

Little Monkey canonicalizes workspace paths and rejects traversal and symlink escapes. Read-only workspace operations do not mutate files; mutating file, shell, memory, MCP, browser, Git and GitHub, workflow, background, capture, and remote actions use their applicable permission or grant boundary. A remote server's `readOnlyHint`, model output, webpage text, package instructions, or imported archive can never approve its own operation.

Shell commands run inside the workspace with bounded time and cancellation. Scheduled and headless recipes require an explicit permission mode and cannot use unattended `bypass`. External mutations are recorded as pending, confirmed, or `needs_reconciliation`; ambiguous effects are not retried as if known safe. API keys, OAuth tokens, bearer secrets, remote device keys, and TLS private keys use the OS keychain where the feature supports credentials.

A learned skill cannot widen what a run may do. Candidates are opened only from a completed run's own durable events, never from model output, retrieved content, or tool output claiming a procedure should be remembered; the model's `manage_skill_learning` tool can propose and request, and cannot approve, publish, or write a file. Proposed content is size-bounded and rebuilt into `SKILL.md` by deterministic code, resource paths are validated relative paths inside an app-owned staging directory, and the installed digest is recomputed from the staged bytes at the moment it authorizes the install. A widened tool list, a new executable or environment requirement, global scope, and any process, shell, network or connector access a brand-new skill introduces all need approval even under unattended promotion, and unattended promotion additionally requires an evaluation that really executed the procedure and passed — a capture of the tools a model asked for can never authorize an install. Approval itself is the app's own permission decision, bound to a digest of exactly what you were shown; a candidate edited or re-staged afterwards recomputes to a different digest and the earlier approval stops authorizing it. Content that would weaken permission policy or bypass a permission mode is refused outright, and a command already provided by a skill this loop did not install is never overwritten. A skill's allowed-tools list can only narrow a run: the effective capability is the run's own permissions ∩ the invoked skills' allowed tools ∩ the normal tool policy, enforced by the tool layer rather than by the prompt. Evaluation arms run in disposable copies under the app's own data directory, marker-verified before any tool call or verification command is allowed to target them, so an evaluation can never mutate the live workspace. Promotion publishes atomically or not at all, so a failed or interrupted one leaves the previously active version intact, and provenance is keyed by installed content hash so a rollback restores a real previous version together with its own evidence.

An origin outside this machine — a message on a channel, a text or a caller, a paired device, a peer, an extension — is authenticated and then bounded by exactly the same permission policy as anything typed into the desktop app. Being allowed to send a message is authority to submit a message and nothing else: it never approves the run's own file writes, shell commands, network calls, device actions or replies, and provider metadata (a display name, a role, a channel topic, an extension's claim about a sender) is untrusted text rather than a grant. One deliberate carve-out exists so a resident agent can answer with nobody at the machine: a reply to the conversation a message came from does not raise a per-send prompt, because the operator already made that decision when routing the account — the reply grant is read from the frozen route recorded on the durable turn, and the destination is resolved from the durable event itself rather than from a tool argument, so a prompt-injected model has nowhere to redirect it. A named account, a different conversation, and any send carrying artifacts still prompt, and a run holding no reply grant is refused above the prompt. Credentials for those paths are the operator's own and live in the OS keychain; the databases hold only the name to look one up under, and each entry is written through the bundled CLI so it is created by the same executable the resident daemon reads it from. What each path needs from the operator, and which of it automated tests can prove, is in [Reaching an agent from outside this machine](messaging-devices-and-phones.md).

Security Doctor is a posture aid, not a substitute for operating-system updates, endpoint security, or a release penetration test. It covers storage, network listeners, MCP origins, extensions, skills, process isolation, browser and companion grants, voice, paired devices, messaging channels, telephony and peers; the desktop panel and `monkey security audit` run the same checks over the same state.

Desktop wake-word listening has a narrower audio boundary than transcription.
Before a native wake event, bounded PCM exists only in renderer memory and the
local sherpa-onnx stream. It has no application-log, support-bundle, analytics,
ledger, diagnostic, database, artifact, or crash-report field; Whisper and the
agent are not invoked. After wake, only the command portion is passed to the
built-in local Whisper path — and where the keyword's exact position cannot be
proven, that portion starts at the audio frame that revealed the keyword rather
than at a guess that could reach back over the phrase itself. Switching Always
Listening off is an act rather than a preference: the hook that owns the
devices re-reads the saved configuration and closes the microphone that setting
opened, so the finding below and the open device cannot disagree. Security
Doctor reports wake enabled, Always
Listening enabled, local/non-local processing, and passive off-device audio as
four independent findings; any passive network path is Critical. Details and
the executable boundary test are in [Local wake-word detection](local-wake-word.md).
Desktop realtime Talk is brokered natively: the WebView sends an SDP offer,
the native host reads the ordinary OpenAI key from the existing OS keychain and
contacts only the compiled-in OpenAI Realtime calls origin, then returns an SDP
answer. The credential is never exposed to JavaScript, a URL, logs, transcript,
or metrics. A privacy acknowledgement is required before the microphone opens,
and Security Doctor reports realtime voice as a separate configured/active
status. Provider function calls do not create a permission shortcut; they enter
the same schema validation, permission prompt, plan-mode, sandbox, workspace,
network, MCP, extension, and hook boundary as typed turns. Details and teardown
semantics are in [Desktop realtime voice](realtime-voice.md).

Routing a conversation's microphone or speaker to a paired device grants nothing by
being paired. Pairing is not microphone authority: an input endpoint requires
`voice_stream`, an output endpoint requires `audio_playback`, and a duplex endpoint
requires both — where *effective* means the operator's grant, the capability the
device's own build advertises, the operating-system permission where one exists, and
current readiness all agree, and a device's claim about itself can only narrow that,
never widen it. The route is conversation-scoped and host-enforced: a device never
chooses which conversation it is speaking into, and it cannot substitute one. The
admission ticket is random, single-use, short-lived, and bound to the device that
minted it, to that device's key generation, and — for a routed socket — to the exact
route, role and generation; every one of those is re-checked when the socket is
admitted, not only when the ticket was issued, and a stale generation is refused at
both points. Withdrawing a grant, revoking or re-keying the device, moving an
endpoint, or losing the required readiness closes the affected side immediately and
fail-closed, rather than at whatever moment the device next happens to send a frame.
Raw audio has no field in the route ledger; the tables hold route state and bounded
coordination metadata, and paired Realtime PCM lives only in bounded in-memory
queues and on a loopback-only socket authenticated by a process-local token that is
regenerated on every daemon start. Details are in
[Voice Everywhere](voice-everywhere.md).

A support bundle is built to be handed over: it carries a bounded trace of what the messaging, telephony, peer and device subsystems did, and no message text, transcript, audio, key, session or credential — those have no field in the format. Identifiers are pseudonymized with a salt generated per bundle and never recorded, so a party is consistent within one document and correlates with nothing outside it.
