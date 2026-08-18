/** ROADMAP.md Phase 5 — Privacy Firewall. Strings for the per-workspace
 * policy editor (`PrivacyFirewallPanel.tsx`) and the pre-turn approval gate
 * (`PrivacyFirewallGate.tsx`). Only `en.ts` is required to be complete — see
 * `keyLint.test.ts`'s doc comment. */
export const privacyFirewallLocale: Record<string, string> = {
  "SettingsModal.tabPrivacyFirewall": "Privacy Firewall",

  "PrivacyFirewallPanel.title": "Privacy Firewall",
  "PrivacyFirewallPanel.description":
    "A visible boundary before a chat turn leaves this machine for a cloud model: likely secrets, credentials, and personal data are detected and handled per the policy below — allowed, redacted, blocked, or paused for your explicit approval.",
  "PrivacyFirewallPanel.workspaceLabel": "Policy for: {{workspace}}",
  "PrivacyFirewallPanel.globalWorkspace": "no workspace open (global policy)",
  "PrivacyFirewallPanel.loading": "Loading policy…",
  "PrivacyFirewallPanel.notLoaded": "Could not load a policy for this workspace.",
  "PrivacyFirewallPanel.localOnlyFallbackLabel": "Offer a local-only fallback",
  "PrivacyFirewallPanel.localOnlyFallbackDescription":
    "When a send is blocked or paused, offer to switch this turn to a configured Ollama model instead of cancelling outright.",
  "PrivacyFirewallPanel.kindsHeading": "Detected data kinds",
  "PrivacyFirewallPanel.actionPickerLabel": "Action:",
  "PrivacyFirewallPanel.kindPrivateKey": "Private keys",
  "PrivacyFirewallPanel.kindPrivateKeyDescription": "PEM-encoded RSA/EC/OpenSSH/DSA private key blocks.",
  "PrivacyFirewallPanel.kindApiCredential": "API credentials",
  "PrivacyFirewallPanel.kindApiCredentialDescription": "API keys, access tokens, secrets, and bearer authorization values.",
  "PrivacyFirewallPanel.kindEmail": "Email addresses",
  "PrivacyFirewallPanel.kindEmailDescription": "Email addresses in standard local@domain form.",
  "PrivacyFirewallPanel.kindCreditCard": "Credit card numbers",
  "PrivacyFirewallPanel.kindCreditCardDescription": "Digit sequences carrying a card-network prefix and an issued length that also pass a Luhn checksum, the same validation the scanner uses.",
  "PrivacyFirewallPanel.kindPhone": "Phone numbers",
  "PrivacyFirewallPanel.kindPhoneDescription": "Phone numbers in common international or local formats.",
  "PrivacyFirewallPanel.kindIpAddress": "IP addresses",
  "PrivacyFirewallPanel.kindIpAddressDescription": "IPv4 addresses that could identify a specific machine or network.",
  "PrivacyFirewallPanel.actionAllow": "Allow",
  "PrivacyFirewallPanel.actionRedact": "Redact",
  "PrivacyFirewallPanel.actionBlock": "Block",
  "PrivacyFirewallPanel.actionRequireApproval": "Require approval",
  "PrivacyFirewallPanel.exceptionsHeading": "Exceptions",
  "PrivacyFirewallPanel.exceptionsDescription":
    "Exact text matches listed here are always allowed through, regardless of their kind's action above — useful for a known-safe shared address or a test value.",
  "PrivacyFirewallPanel.exceptionsPlaceholder": "Exact text to always allow…",
  "PrivacyFirewallPanel.exceptionsAddButton": "Add",
  "PrivacyFirewallPanel.exceptionsEmpty": "No exceptions yet.",
  "PrivacyFirewallPanel.exceptionsRemoveAriaLabel": "Remove exception {{value}}",
  "PrivacyFirewallPanel.scopeNote":
    "This policy currently gates only cloud-model chat turns. Connector writes, MCP tool results, and paired devices are not gated by this yet.",

  "PrivacyFirewallGate.blockedTitle": "Send blocked by the Privacy Firewall",
  "PrivacyFirewallGate.approvalTitle": "This turn needs your approval",
  "PrivacyFirewallGate.findingsSummary": "{{count}} sensitive item(s) found in this turn before it was sent.",
  "PrivacyFirewallGate.redactedPreviewLabel": "What would be sent if redacted",
  "PrivacyFirewallGate.cancelButton": "Cancel",
  "PrivacyFirewallGate.switchLocalButton": "Switch to local model",
  "PrivacyFirewallGate.sendRedactedButton": "Send redacted",
  "PrivacyFirewallGate.sendUnredactedButton": "Send unredacted",
  "PrivacyFirewallGate.kind.private_key": "Private key",
  "PrivacyFirewallGate.kind.api_credential": "API credential",
  "PrivacyFirewallGate.kind.email": "Email",
  "PrivacyFirewallGate.kind.credit_card": "Credit card",
  "PrivacyFirewallGate.kind.phone": "Phone",
  "PrivacyFirewallGate.kind.ip_address": "IP address",
};
