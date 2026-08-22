# Computer Use E2E matrix

Use the native fixture from `computer-use.md` with a fresh profile. The driver
discovers the target first, creates a grant scoped to that exact window, proves
a second same-application window is refused, and exercises the real
per-action approval gate (`test-approved-through-real-gate`).

Generate the machine-readable evidence envelope before running the native
driver:

```sh
COMPUTER_USE_E2E_RUN=1 COMPUTER_USE_PRODUCTION_BACKEND=1 \
python3 src-tauri/fixtures/computer-use-e2e.py \
  --report /tmp/little-monkey-computer-use-e2e.json
```

The harness validates the fixture and, when `COMPUTER_USE_E2E_RUN=1`, launches
`computer-use-native-driver.py`. Set `COMPUTER_USE_PRODUCTION_BACKEND=1` for
the acceptance path used by CI; it invokes the Rust production backend, which
creates the scoped grant and approval gates before the OS accessibility
provider performs the semantic actions. The driver discovers the real
accessibility window, restarts the fixture, captures a screenshot, inspects
the audit, and proves the negative security cases. A fabricated trace
dictionary is rejected. CI runs this production path on macOS Accessibility,
Windows UIAutomation, and Linux/X11 AT-SPI under Xvfb; Wayland remains an
explicit fail-closed path.

1. Feature off: Computer Use schemas are absent.
2. Bypass mode: grant creation is refused.
3. Empty grant: refused.
4. Calculator grant: listing excludes Terminal, 1Password, and System Settings.
5. Expired grant: list and action fail.
6. Pause: action is refused; resume restores the grant.
7. Stop: session and pending approvals are revoked.
8. Emergency stop: every session/pending action is cancelled and indicator hides.
9. List targets: only visible allowlisted windows are returned.
10. Inspect: elements are bounded, deduplicated, ordered, and query-filtered.
11. Semantic click: button action is sent and target is reverified.
12. Coordinate fallback: outside-target coordinates are refused.
13. Double-click: exactly one bounded double-click action is sent.
14. Scroll: bounded deltas work; oversized deltas fail.
15. Type/key/hotkey: keyboard grant and per-action approval are enforced.
16. Screenshot: disabled grants fail; enabled screenshots create durable artifacts.
17. Verification: stale window/frontmost changes fail closed after approval.
18. Sensitive field: fake password is omitted/refused and no typed value is audited.
19. Dialog/destructive action: critical confirmation remains operator-controlled.
20. Dynamic/list/disabled controls: re-inspection handles changes and disabled controls do not mutate.
21. Browser routing: DOM/browser control is selected for web content; native control is not a pixel-browser bypass.
22. Remote Windows runner: local consent, target verification, lock, revocation, audit, and emergency stop remain mandatory.

Acceptance evidence is the JSON report plus the TestApp dark-mode/profile/save
state, the verification result, screenshot artifact id, redacted audit row,
and a negative test showing prompt injection cannot widen the grant.
