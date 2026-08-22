# Computer Use

Computer Use is Little Monkey's model-facing native UI capability. It extends
the Safe Desktop Control substrate; it does not replace its session lock,
permission-mode check, allowlist, approval gate, emergency stop, expiry, or
visible indicator.

## Grant and tool contract

The operator enables Computer Use and creates a time-limited grant for one or
more exact application identifiers. A grant may additionally restrict window
identifiers and independently allows screenshots, keyboard input, and
clipboard reads (clipboard reads are currently off by default and have no
implicit relationship to typing). Bypass mode cannot create a grant. The
grant is local, expires after at most 30 minutes, can be paused or stopped,
and is revoked on emergency stop, app exit, or runner revocation.

The model-facing tools are `computer_list_targets`, `computer_screenshot`,
`computer_inspect`, `computer_focus`, `computer_click`,
`computer_double_click`, `computer_scroll`, `computer_type`, `computer_key`,
`computer_hotkey`, `computer_wait`, `computer_select`, and
`computer_set_value`. Every call supplies an active session and an
application/window target. The model must list and inspect first; semantic
element ids are preferred, with bounded coordinates as an explicit fallback.
Native semantic actions carry provider identity: macOS resolves `AXIdentifier`,
Windows resolves UIA AutomationId/runtime identity, and Linux/X11 resolves an
AT-SPI provider path. When an observed identity was available, a missing one
is a stale-target failure rather than an index fallback.
The backend re-resolves the target at execution time and after the action, so
stale ids, changed windows, inactive grants, and non-frontmost mutation
targets fail closed. Results distinguish input sent from state verified.

The approval levels are low for inspect/screenshot, medium for focus/scroll,
high for typing/clicking/value mutation, and critical for destructive or
external transactions, including a semantic element whose inspected role or
label is destructive. Critical actions always require their own approval,
even inside an approved batch. Approved-batch mode is an explicit grant
choice and never widens the allowlist or disables the kill switch. A shared
run budget atomically caps 50 actions, 12 screenshots, 5 retries, 20 model
calls, and a 15-minute deadline. Callers must not turn Computer Use into an
unbounded autonomous loop.

## Security boundary

Accessibility trees are normalized to target and element records containing
id, role, label, value, bounds, enabled/focused state, supported actions, and
sensitivity. Inspection is filtered, ordered, deduplicated, and capped. Audit
records contain run/session/target/action/approval/result/verification and
screenshot references. Typed text and value contents are redacted; secrets
are never written to the audit log.

Password managers, keychains, authentication dialogs, OS security and
permission dialogs, sudo/UAC, full-disk-encryption and biometric flows,
login/security agents, and password/secure elements are refused before input.
The model cannot grant an OS permission. Terminal work uses `run_shell`; the
computer tool must not be used to drive a terminal. A malicious web page,
file, MCP response, or model instruction cannot widen a grant or approve an
action.

For web content, the Universal Task Coordinator routes browser-capable tool
names to the browser route when that capability is available and rejects a
browser URL supplied to a native Computer Use call. Computer Use is for
native applications and does not offer a pixel-based browser bypass. Native
UI tasks use Computer Use, while the coordinator records the capability
choice and bounded observe/authorize/execute/verify phases.

## Platform adapters

| Platform | Semantic access | Input/screenshot boundary |
| --- | --- | --- |
| macOS | System Events / Accessibility, normalized and bounded | Accessibility is required for input/tree access; Screen Recording is required for screenshots; the target must remain visible/frontmost |
| Windows | UI Automation, normalized and bounded | Every target is checked with `GetWindowThreadProcessId` → `OpenProcessToken(TokenIntegrityLevel)` against the current process; native scripts use `SetThreadDpiAwarenessContext(PER_MONITOR_AWARE_V2)` and `GetDpiForWindow`; higher-integrity targets fail closed |
| Linux/X11 | AT-SPI when `pyatspi` is available | enigo plus bounded `scrot`/ImageMagick region capture; missing providers return a typed error |
| Linux/Wayland | No compositor bypass | Requires an approved xdg-desktop-portal RemoteDesktop/InputCapture/libei path; Little Monkey refuses to bypass Wayland security |

Remote desktop control uses the existing paired-runner path. The runner must
obtain local consent, hold the same machine-wide control lock, enforce the
same grant and revocation rules, and record the remote device/run identity.
The controller cannot approve an OS permission or escape the runner's local
allowlist.

The remote Computer Use surface mirrors the local observe/act lifecycle through
`/v1/remote/desktop-control/list-targets`, `/inspect`, `/screenshot`,
`/clipboard-read`, `/action`, `/pause`, `/resume`, and `/stop`. Read-only
observation still requires the owned session and the paired `ControlDesktop`
grant. Omitted remote screenshot and keyboard capabilities default to disabled;
clipboard reads require their own explicit grant and redact content from the
durable audit.

## Verification and recovery

The normal loop is semantic target listing → bounded inspection → screenshot
when needed → model decision → policy/approval → input → target/state
verification screenshot or semantic re-read. If input was sent but the
postcondition is not verified, the coordinator returns the typed
`INPUT_SENT_UNVERIFIED` failure instead of treating the last result as success.
The model should stop and ask the operator when recovery would require widening
the grant or bypassing a security boundary.

## Acceptance fixture

Run `python3 src-tauri/fixtures/computer-use-test-app.py` on a desktop session.
The native fixture contains a dark-mode toggle, profile input/save state,
menu, list and scroll region, dialog, disabled control, dynamic content,
destructive confirmation, and a fake password field. The fake password field
is intentionally a security test: it must not be typed into or inspected as a
secret-bearing element.
