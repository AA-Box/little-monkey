//! The conformance suite (roadmap K21).
//!
//! # What this is for
//!
//! The M3 compatibility harness certifies *this* implementation: it spins up
//! the real server and exercises every advertised route, and it lives in
//! `tests/`, so it is unrunnable by anyone who is not building this crate.
//! K21 asks for the other thing — something a third-party node, runtime
//! driver, or package author can run against a live implementation to claim
//! compatibility, with a named revision behind the claim.
//!
//! So this module is two halves that never share code with the handlers they
//! judge:
//!
//! * **The attestation** ([`ConformanceAttestation`]) — what a node publishes
//!   about itself at `GET /v1/conformance`: the contract it implements, which
//!   optional sections it claims, and the *live* evidence a run needs to check
//!   those claims (the isolation mechanism this machine can actually apply,
//!   the limits it actually enforces, the current head of its subsystem
//!   chain).
//! * **The runner** ([`run_suite`]) — a real HTTP client that talks to a live
//!   listener over a socket. It never imports a handler, never constructs a
//!   hub, and never reads the ledger directly. If it passes against a mirror
//!   of the pipeline rather than the pipeline, that is a bug in the mirror's
//!   deployment, not something this file can arrange.
//!
//! # Required and optional, and why a skip is reported rather than hidden
//!
//! [`SectionId::Contract`] is required: an implementation that cannot answer
//! for its own route surface is not implementing the contract at all. The
//! other three are optional because they name guarantees a node may honestly
//! not offer — a runtime driver embedded in someone else's process has no
//! sandbox of its own to attest, and a node with no ledger has nothing to
//! prove append-only about. An optional section that is skipped is **named in
//! the report**, so "compatible" never quietly means "compatible with the
//! three sections we chose to run".
//!
//! # What a "compatible" claim means
//!
//! Exactly this: a named [`SUITE_REVISION`] ran against a live listener, every
//! required section passed, and no optional section that was attempted failed.
//! The revision is part of the verdict because a suite that can be edited
//! after the claim is not evidence of anything — the same reason K12's chain
//! exists.

use std::collections::BTreeSet;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::compatibility_hub::{
    compatibility_conformance_manifest, CompatibilityConformanceManifest,
};
use crate::contract::{ContractManifest, DeniedSurfaceEntry};
use crate::run_ledger::ChainVerification;
use crate::sandbox::SandboxEnforcement;
use crate::subsystem_audit::ChainEvidence;

/// The revision a "compatible" claim names.
///
/// Date-stamped rather than semver'd on purpose: this is not a dependency
/// anyone resolves, it is a thing that ran on a day. Bump it whenever a check
/// is added, removed, or made stricter — a claim against an older revision
/// stays exactly as true as it was, and stays visibly older.
pub const SUITE_REVISION: &str = "little-monkey-conformance-2026-08-09";

/// Where the attestation lives. Published so a client does not have to
/// hard-code it from the docs.
pub const ATTESTATION_PATH: &str = "/v1/conformance";

/// K19's contract introspection endpoint — the ABI a running instance says it
/// implements. The suite reads it as a *client* would, and cross-checks it
/// against the attestation.
pub const CONTRACT_PATH: &str = "/v1/contract";

/// Query parameter asking the attestation to include the chain links after a
/// sequence the caller already saw. See [`SectionId::Ledger`].
pub const LEDGER_AFTER_PARAM: &str = "ledgerAfter";

/// How many chain links one attestation will return. A conformance run needs
/// a handful; nothing is served by letting a caller page the whole stream's
/// linkage out of a node one request at a time.
pub const MAX_LEDGER_LINKS: u32 = 64;

// ---------------------------------------------------------------------------
// Sections
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SectionId {
    /// The published route surface and its wire behaviour.
    Contract,
    /// K3 — the isolation guarantees.
    Isolation,
    /// K4/K5 — the limit semantics.
    Limits,
    /// K12 — the ledger obligations.
    Ledger,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Requirement {
    Required,
    Optional,
}

impl SectionId {
    pub const ALL: &'static [SectionId] = &[
        SectionId::Contract,
        SectionId::Isolation,
        SectionId::Limits,
        SectionId::Ledger,
    ];

    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Contract => "contract",
            Self::Isolation => "isolation",
            Self::Limits => "limits",
            Self::Ledger => "ledger",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|section| section.code() == value)
    }

    #[must_use]
    pub const fn requirement(self) -> Requirement {
        match self {
            Self::Contract => Requirement::Required,
            Self::Isolation | Self::Limits | Self::Ledger => Requirement::Optional,
        }
    }

    /// The roadmap item this section is evidence for.
    #[must_use]
    pub const fn covers(self) -> &'static str {
        match self {
            Self::Contract => "K19",
            Self::Isolation => "K3",
            Self::Limits => "K4/K5",
            Self::Ledger => "K12",
        }
    }
}

// ---------------------------------------------------------------------------
// The attestation a node publishes
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConformanceAttestation {
    /// The revision this node was built against. A runner on a newer revision
    /// says so rather than silently grading an older node by newer rules.
    pub suite_revision: String,
    pub contract: ContractAttestation,
    pub sections: Vec<SectionSupport>,
    pub isolation: IsolationAttestation,
    pub limits: LimitsAttestation,
    /// `None` when this listener has no ledger behind it — a CLI-hosted server
    /// started outside a data directory, or a test context. Distinct from an
    /// empty chain, which is `Some` with no head.
    pub ledger: Option<LedgerAttestation>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SectionSupport {
    pub id: SectionId,
    pub requirement: Requirement,
    pub claimed: bool,
    /// Why a section is not claimed. Required whenever `claimed` is false, for
    /// the reason `SubsystemAudit::disabled` takes a reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContractAttestation {
    /// The K19 ABI version this instance implements, and the digest of the
    /// exact manifest it was built from.
    ///
    /// Carried here as well as at `GET /v1/contract` on purpose: two published
    /// surfaces that disagree about the same instance's version is itself a
    /// defect, and `contract.abi_version` is the check that catches it.
    pub contract_version: String,
    pub contract_digest: String,
    /// K19's generated manifest, verbatim. Not a second copy of the route
    /// table — `contract::manifest()` reads `ROUTES` and the tool definitions
    /// the running code dispatches from, and a conformance attestation that
    /// re-derived them would be exactly the believable-but-wrong artifact K19
    /// exists to prevent.
    pub manifest: ContractManifest,
    pub compatibility: CompatibilityConformanceManifest,
    /// Whether this listener refuses an unauthenticated request. A loopback
    /// listener may legitimately be configured without a token; the runner
    /// grades what the node claims rather than assuming.
    pub authentication_required: bool,
}

/// A concrete path a denied surface refuses — what a conformance run actually
/// sends. K19's manifest publishes the matcher rather than a request, and a
/// bare `prefix` (`/v1/tool_`) is not a path a client would ask for.
fn denied_probe_path(surface: &DeniedSurfaceEntry) -> String {
    if surface.matcher == "prefix" {
        format!("{}probe", surface.path)
    } else {
        surface.path.clone()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IsolationAttestation {
    pub platform: String,
    pub enforcement: SandboxEnforcement,
    /// The kernel mechanism, when there is one: `seatbelt`, `landlock+seccomp`,
    /// `appcontainer+job_object`. `None` alongside an `os_enforced` claim is
    /// itself a conformance failure, which is why it is reported rather than
    /// derived at read time.
    pub mechanism: Option<String>,
    /// Honest today, and the reason K3 is not closed: the sandbox is opt-in and
    /// the agent's own shell tool does not route through it. A node that
    /// claimed otherwise here would be claiming something this build does not
    /// do.
    pub applies_to_every_tool_call: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LimitsAttestation {
    pub max_request_body_bytes: u64,
    pub max_active_requests: u64,
    pub model_output_cap_bytes: u64,
    pub background_shell_output_cap_bytes: u64,
    /// Whether child processes are spawned under kernel-held `setrlimit`
    /// bounds. Platform-dependent: `os_limits` is a no-op off Unix.
    pub child_rlimits_enforced: bool,
    /// K5. Every credentialed outbound request goes through
    /// `egress::hardened`: a connect timeout, a silence budget, and a redirect
    /// policy that will not carry a credential to a host a `302` chose.
    pub egress_hardened: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LedgerAttestation {
    #[serde(flatten)]
    pub evidence: ChainEvidence,
}

/// The parts of an attestation only the listener knows.
///
/// Everything else is derived from this binary's own tables and probes, so it
/// cannot drift from what the code does.
pub struct AttestationInputs {
    pub authentication_required: bool,
    /// `None` when the listener has no ledger — see [`ConformanceAttestation::ledger`].
    pub ledger: Option<ChainEvidence>,
}

#[must_use]
pub fn build_attestation(inputs: AttestationInputs) -> ConformanceAttestation {
    let enforcement = crate::sandbox::sandbox_enforcement();
    let ledger = inputs.ledger.map(|evidence| LedgerAttestation { evidence });
    let sections = SectionId::ALL
        .iter()
        .map(|&id| match id {
            SectionId::Ledger if ledger.is_none() => SectionSupport {
                id,
                requirement: id.requirement(),
                claimed: false,
                reason: Some(
                    "this listener has no run ledger behind it, so it publishes no chain evidence"
                        .to_string(),
                ),
            },
            // Claimed only where a kernel actually holds the boundary. A host
            // whose kernel cannot enforce one is not a failing implementation
            // — it is an implementation that does not offer this optional
            // guarantee, and saying so is the whole point of the section
            // being optional. Claiming it here and failing the check would
            // report a Landlock-less kernel as a defect in the software
            // running on it.
            SectionId::Isolation
                if !matches!(
                    enforcement,
                    SandboxEnforcement::OsEnforced | SandboxEnforcement::ProcessContained
                ) =>
            {
                SectionSupport {
                    id,
                    requirement: id.requirement(),
                    claimed: false,
                    reason: Some(match enforcement {
                        SandboxEnforcement::Unavailable => {
                            "this platform has an enforcement mechanism and it is not usable here"
                                .to_string()
                        }
                        _ => format!(
                            "this {} kernel enforces no confinement boundary for this app to claim",
                            std::env::consts::OS
                        ),
                    }),
                }
            }
            _ => SectionSupport {
                id,
                requirement: id.requirement(),
                claimed: true,
                reason: None,
            },
        })
        .collect();

    ConformanceAttestation {
        suite_revision: SUITE_REVISION.to_string(),
        contract: ContractAttestation {
            contract_version: crate::contract::CONTRACT_VERSION.to_string(),
            contract_digest: crate::contract::digest(),
            manifest: crate::contract::manifest(),
            compatibility: compatibility_conformance_manifest(),
            authentication_required: inputs.authentication_required,
        },
        sections,
        isolation: IsolationAttestation {
            platform: std::env::consts::OS.to_string(),
            enforcement,
            mechanism: isolation_mechanism(enforcement),
            // Stated as a fact about this build, not an aspiration. See K3.
            applies_to_every_tool_call: false,
        },
        limits: LimitsAttestation {
            max_request_body_bytes: crate::http_policy::MAX_REQUEST_BODY_BYTES as u64,
            max_active_requests: crate::http_policy::MAX_ACTIVE_REQUESTS as u64,
            model_output_cap_bytes: crate::output_cap::MODEL_OUTPUT_CAP as u64,
            background_shell_output_cap_bytes: crate::background_shell::MAX_OUTPUT_BYTES as u64,
            child_rlimits_enforced: cfg!(unix),
            egress_hardened: true,
        },
        ledger,
    }
}

/// The kernel mechanism behind an enforcement state, or `None` when there is
/// none to name.
fn isolation_mechanism(enforcement: SandboxEnforcement) -> Option<String> {
    match enforcement {
        SandboxEnforcement::OsEnforced => Some(
            if cfg!(target_os = "macos") {
                "seatbelt"
            } else {
                "landlock+seccomp"
            }
            .to_string(),
        ),
        SandboxEnforcement::ProcessContained => Some("appcontainer+job_object".to_string()),
        SandboxEnforcement::ProcessOnly | SandboxEnforcement::Unavailable => None,
    }
}

// ---------------------------------------------------------------------------
// The report a run produces
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CheckStatus {
    Passed,
    Failed,
    /// Not run, and the reason says why. Never a silent pass.
    Skipped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SectionStatus {
    Passed,
    Failed,
    /// Every check that ran passed, but at least one could not run. A required
    /// section in this state does not support a compatibility claim: the
    /// claim would rest on checks nobody performed.
    Incomplete,
    /// The whole section was not attempted — the node did not claim it, or the
    /// caller did not select it.
    Skipped,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckResult {
    pub id: String,
    pub title: String,
    pub status: CheckStatus,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SectionReport {
    pub id: SectionId,
    pub requirement: Requirement,
    pub covers: String,
    pub status: SectionStatus,
    /// Why a section was skipped entirely. `None` for any other status.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<String>,
    pub checks: Vec<CheckResult>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum Verdict {
    /// Every required section passed, and no attempted optional section
    /// failed. The revision is part of the claim.
    Compatible {
        suite_revision: String,
    },
    NotCompatible {
        reasons: Vec<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConformanceReport {
    pub suite_revision: String,
    /// The revision the node itself was built against, when it answered at
    /// all. A mismatch is reported and does not by itself fail the run.
    pub node_suite_revision: Option<String>,
    pub target: String,
    pub sections: Vec<SectionReport>,
    /// Optional sections that did not run, by code. The headline number in
    /// every summary: a claim is only as broad as this list is short.
    pub skipped_optional_sections: Vec<String>,
    pub verdict: Verdict,
}

impl ConformanceReport {
    #[must_use]
    pub fn is_compatible(&self) -> bool {
        matches!(self.verdict, Verdict::Compatible { .. })
    }

    /// A terminal summary. Kept here rather than in the CLI so the desktop
    /// panel and the CLI never drift on what a status word means.
    #[must_use]
    pub fn to_summary(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "Conformance suite {} against {}\n",
            self.suite_revision, self.target
        ));
        if let Some(node) = &self.node_suite_revision {
            if node != &self.suite_revision {
                out.push_str(&format!(
                    "  note: the node was built against {node}; this runner is {}\n",
                    self.suite_revision
                ));
            }
        }
        for section in &self.sections {
            out.push_str(&format!(
                "\n[{}] {} ({}, {})\n",
                match section.status {
                    SectionStatus::Passed => "pass",
                    SectionStatus::Failed => "FAIL",
                    SectionStatus::Incomplete => "incomplete",
                    SectionStatus::Skipped => "skipped",
                },
                section.id.code(),
                match section.requirement {
                    Requirement::Required => "required",
                    Requirement::Optional => "optional",
                },
                section.covers,
            ));
            if let Some(reason) = &section.skip_reason {
                out.push_str(&format!("  {reason}\n"));
            }
            for check in &section.checks {
                out.push_str(&format!(
                    "  {} {} — {}\n",
                    match check.status {
                        CheckStatus::Passed => "ok  ",
                        CheckStatus::Failed => "FAIL",
                        CheckStatus::Skipped => "skip",
                    },
                    check.id,
                    check.detail
                ));
            }
        }
        out.push('\n');
        match &self.verdict {
            Verdict::Compatible { suite_revision } => {
                out.push_str(&format!("COMPATIBLE — {suite_revision} passed\n"));
            }
            Verdict::NotCompatible { reasons } => {
                out.push_str("NOT COMPATIBLE\n");
                for reason in reasons {
                    out.push_str(&format!("  - {reason}\n"));
                }
            }
        }
        if !self.skipped_optional_sections.is_empty() {
            out.push_str(&format!(
                "Optional sections not run: {}\n",
                self.skipped_optional_sections.join(", ")
            ));
        }
        out
    }
}

// ---------------------------------------------------------------------------
// The runner
// ---------------------------------------------------------------------------

pub struct SuiteOptions {
    /// Base URL of the live listener, e.g. `http://127.0.0.1:8756`.
    pub base_url: String,
    /// Bearer token, when the listener requires one.
    pub token: Option<String>,
    /// Sections to attempt. Empty means all of them.
    pub sections: Vec<SectionId>,
    /// Model to exercise the inference contract with. `None` takes the first
    /// model the node lists.
    pub model: Option<String>,
}

impl SuiteOptions {
    #[must_use]
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            token: None,
            sections: Vec::new(),
            model: None,
        }
    }

    fn wants(&self, section: SectionId) -> bool {
        self.sections.is_empty() || self.sections.contains(&section)
    }
}

/// The client a run should use.
///
/// Built from `egress::hardened` rather than a bare constructor, for the
/// reason that module's ratchet exists — and with a short connect budget
/// because the target is a listener the caller just named, not a slow remote.
pub fn client() -> Result<reqwest::Client, String> {
    crate::egress::hardened()
        .build()
        .map_err(|error| format!("Failed to build the conformance HTTP client: {error}"))
}

struct Runner<'a> {
    client: &'a reqwest::Client,
    options: &'a SuiteOptions,
}

/// One response, already reduced to what a check reasons about.
struct Fetched {
    status: u16,
    body: String,
}

impl Fetched {
    fn json(&self) -> Option<serde_json::Value> {
        serde_json::from_str(&self.body).ok()
    }
}

impl Runner<'_> {
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.options.base_url.trim_end_matches('/'))
    }

    async fn send(
        &self,
        request: reqwest::RequestBuilder,
        authenticated: bool,
    ) -> Result<Fetched, String> {
        let request = match (&self.options.token, authenticated) {
            (Some(token), true) => request.bearer_auth(token),
            _ => request,
        };
        let response = request
            .timeout(Duration::from_secs(120))
            .send()
            .await
            .map_err(|error| error.to_string())?;
        let status = response.status().as_u16();
        let body = response.text().await.map_err(|error| error.to_string())?;
        Ok(Fetched { status, body })
    }

    async fn get(&self, path: &str) -> Result<Fetched, String> {
        self.send(self.client.get(self.url(path)), true).await
    }

    async fn post_json(&self, path: &str, body: &serde_json::Value) -> Result<Fetched, String> {
        self.send(self.client.post(self.url(path)).json(body), true)
            .await
    }
}

fn passed(id: &str, title: &str, detail: impl Into<String>) -> CheckResult {
    CheckResult {
        id: id.to_string(),
        title: title.to_string(),
        status: CheckStatus::Passed,
        detail: detail.into(),
    }
}

fn failed(id: &str, title: &str, detail: impl Into<String>) -> CheckResult {
    CheckResult {
        id: id.to_string(),
        title: title.to_string(),
        status: CheckStatus::Failed,
        detail: detail.into(),
    }
}

fn skipped(id: &str, title: &str, detail: impl Into<String>) -> CheckResult {
    CheckResult {
        id: id.to_string(),
        title: title.to_string(),
        status: CheckStatus::Skipped,
        detail: detail.into(),
    }
}

/// Run the suite against a live listener.
///
/// Never returns `Err`: a node that refuses to answer is a conformance
/// *result*, not a runner failure, and reporting it as an error would let a
/// caller print "could not check" where the honest word is "not compatible".
pub async fn run_suite(client: &reqwest::Client, options: &SuiteOptions) -> ConformanceReport {
    let runner = Runner { client, options };

    // Everything downstream needs the attestation, so this is fetched once and
    // its failure is the first check of the required section rather than an
    // early return with no report at all.
    let attestation_fetch = runner.get(ATTESTATION_PATH).await;
    let (attestation, attestation_check) = match &attestation_fetch {
        Ok(fetched) if fetched.status == 200 => match serde_json::from_str::<ConformanceAttestation>(
            &fetched.body,
        ) {
            Ok(attestation) => {
                let detail = if attestation.suite_revision == SUITE_REVISION {
                    format!("the node attests to {SUITE_REVISION}")
                } else {
                    format!(
                        "the node attests to {}; this runner is {SUITE_REVISION}",
                        attestation.suite_revision
                    )
                };
                (Some(attestation), passed("contract.attestation", "publishes a conformance attestation", detail))
            }
            Err(error) => (
                None,
                failed(
                    "contract.attestation",
                    "publishes a conformance attestation",
                    format!("GET {ATTESTATION_PATH} answered 200 but the body is not an attestation: {error}"),
                ),
            ),
        },
        Ok(fetched) => (
            None,
            failed(
                "contract.attestation",
                "publishes a conformance attestation",
                format!("GET {ATTESTATION_PATH} answered {}", fetched.status),
            ),
        ),
        Err(error) => (
            None,
            failed(
                "contract.attestation",
                "publishes a conformance attestation",
                format!("GET {ATTESTATION_PATH} could not be reached: {error}"),
            ),
        ),
    };

    let mut sections = Vec::new();
    let mut skipped_optional = Vec::new();

    for &id in SectionId::ALL {
        let selected = options.wants(id);
        let claimed = attestation.as_ref().map(|attestation| {
            attestation
                .sections
                .iter()
                .find(|section| section.id == id)
                .map_or((true, None), |section| {
                    (section.claimed, section.reason.clone())
                })
        });

        let skip_reason = if !selected {
            Some("not selected by the caller".to_string())
        } else {
            match &claimed {
                Some((false, reason)) => Some(
                    reason
                        .clone()
                        .unwrap_or_else(|| "the node does not claim this section".to_string()),
                ),
                // The required section always runs: a node that could not be
                // read is exactly what it exists to report on.
                None if id.requirement() == Requirement::Optional => Some(
                    "no attestation was readable, so optional sections cannot be graded"
                        .to_string(),
                ),
                _ => None,
            }
        };

        if let Some(reason) = skip_reason {
            if id.requirement() == Requirement::Optional {
                skipped_optional.push(id.code().to_string());
            }
            sections.push(SectionReport {
                id,
                requirement: id.requirement(),
                covers: id.covers().to_string(),
                status: SectionStatus::Skipped,
                skip_reason: Some(reason),
                checks: Vec::new(),
            });
            continue;
        }

        let checks = match id {
            SectionId::Contract => {
                let mut checks = vec![attestation_check.clone()];
                checks.extend(contract_checks(&runner, attestation.as_ref()).await);
                checks
            }
            SectionId::Isolation => {
                isolation_checks(&runner, attestation.as_ref().expect("claimed section")).await
            }
            SectionId::Limits => {
                limits_checks(&runner, attestation.as_ref().expect("claimed section")).await
            }
            SectionId::Ledger => {
                ledger_checks(&runner, attestation.as_ref().expect("claimed section")).await
            }
        };

        sections.push(SectionReport {
            id,
            requirement: id.requirement(),
            covers: id.covers().to_string(),
            status: roll_up(&checks),
            skip_reason: None,
            checks,
        });
    }

    let verdict = verdict_for(&sections);
    ConformanceReport {
        suite_revision: SUITE_REVISION.to_string(),
        node_suite_revision: attestation
            .as_ref()
            .map(|attestation| attestation.suite_revision.clone()),
        target: options.base_url.clone(),
        sections,
        skipped_optional_sections: skipped_optional,
        verdict,
    }
}

fn roll_up(checks: &[CheckResult]) -> SectionStatus {
    if checks
        .iter()
        .any(|check| check.status == CheckStatus::Failed)
    {
        SectionStatus::Failed
    } else if checks
        .iter()
        .any(|check| check.status == CheckStatus::Skipped)
    {
        SectionStatus::Incomplete
    } else {
        SectionStatus::Passed
    }
}

fn verdict_for(sections: &[SectionReport]) -> Verdict {
    let mut reasons = Vec::new();
    for section in sections {
        match (section.requirement, section.status) {
            (Requirement::Required, SectionStatus::Failed) => reasons.push(format!(
                "required section '{}' failed: {}",
                section.id.code(),
                failed_check_ids(section).join(", ")
            )),
            (Requirement::Required, SectionStatus::Incomplete) => reasons.push(format!(
                "required section '{}' could not be completed: {} did not run",
                section.id.code(),
                skipped_check_ids(section).join(", ")
            )),
            (Requirement::Required, SectionStatus::Skipped) => reasons.push(format!(
                "required section '{}' was not run",
                section.id.code()
            )),
            (Requirement::Optional, SectionStatus::Failed) => reasons.push(format!(
                "optional section '{}' was attempted and failed: {}",
                section.id.code(),
                failed_check_ids(section).join(", ")
            )),
            _ => {}
        }
    }
    if reasons.is_empty() {
        Verdict::Compatible {
            suite_revision: SUITE_REVISION.to_string(),
        }
    } else {
        Verdict::NotCompatible { reasons }
    }
}

fn failed_check_ids(section: &SectionReport) -> Vec<String> {
    section
        .checks
        .iter()
        .filter(|check| check.status == CheckStatus::Failed)
        .map(|check| check.id.clone())
        .collect()
}

fn skipped_check_ids(section: &SectionReport) -> Vec<String> {
    section
        .checks
        .iter()
        .filter(|check| check.status == CheckStatus::Skipped)
        .map(|check| check.id.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// contract (required) — K19
// ---------------------------------------------------------------------------

async fn contract_checks(
    runner: &Runner<'_>,
    attestation: Option<&ConformanceAttestation>,
) -> Vec<CheckResult> {
    let mut checks = Vec::new();

    checks.push(match runner.get("/health").await {
        Ok(fetched) if fetched.status == 200 => match fetched.json() {
            Some(value)
                if value.get("status").and_then(serde_json::Value::as_str) == Some("ok") =>
            {
                passed(
                    "contract.health",
                    "answers a liveness probe",
                    "GET /health → 200 status=ok",
                )
            }
            _ => failed(
                "contract.health",
                "answers a liveness probe",
                format!(
                    "GET /health → 200 but the body is not {{\"status\":\"ok\"}}: {}",
                    truncate(&fetched.body)
                ),
            ),
        },
        Ok(fetched) => failed(
            "contract.health",
            "answers a liveness probe",
            format!("GET /health → {}", fetched.status),
        ),
        Err(error) => failed("contract.health", "answers a liveness probe", error),
    });

    let models = runner.get("/v1/models").await;
    let mut model_ids: Vec<String> = Vec::new();
    checks.push(match &models {
        Ok(fetched) if fetched.status == 200 => match fetched.json() {
            Some(value) => {
                let listed =
                    value.get("object").and_then(serde_json::Value::as_str) == Some("list");
                let data = value
                    .get("data")
                    .and_then(serde_json::Value::as_array)
                    .cloned();
                match (listed, data) {
                    (true, Some(entries)) => {
                        let malformed: Vec<String> = entries
                            .iter()
                            .filter(|entry| {
                                entry
                                    .get("id")
                                    .and_then(serde_json::Value::as_str)
                                    .is_none_or(str::is_empty)
                                    || entry.get("object").and_then(serde_json::Value::as_str)
                                        != Some("model")
                            })
                            .map(|entry| truncate(&entry.to_string()))
                            .collect();
                        model_ids = entries
                            .iter()
                            .filter_map(|entry| {
                                entry
                                    .get("id")
                                    .and_then(serde_json::Value::as_str)
                                    .map(str::to_string)
                            })
                            .collect();
                        if malformed.is_empty() {
                            passed(
                                "contract.models",
                                "lists models in the published shape",
                                format!("GET /v1/models → 200, {} model(s)", entries.len()),
                            )
                        } else {
                            failed(
                                "contract.models",
                                "lists models in the published shape",
                                format!(
                                    "entries missing a non-empty id or object=model: {}",
                                    malformed.join("; ")
                                ),
                            )
                        }
                    }
                    _ => failed(
                        "contract.models",
                        "lists models in the published shape",
                        format!(
                            "GET /v1/models → 200 but not {{object:list,data:[]}}: {}",
                            truncate(&fetched.body)
                        ),
                    ),
                }
            }
            None => failed(
                "contract.models",
                "lists models in the published shape",
                format!(
                    "GET /v1/models → 200 with a non-JSON body: {}",
                    truncate(&fetched.body)
                ),
            ),
        },
        Ok(fetched) => failed(
            "contract.models",
            "lists models in the published shape",
            format!("GET /v1/models → {}", fetched.status),
        ),
        Err(error) => failed(
            "contract.models",
            "lists models in the published shape",
            error.clone(),
        ),
    });

    let model = runner
        .options
        .model
        .clone()
        .or_else(|| model_ids.first().cloned());

    match &model {
        Some(model) => {
            checks.push(chat_completion_check(runner, model).await);
            checks.push(chat_stream_check(runner, model).await);
        }
        None => {
            let reason = "this node lists no models, so the inference contract could not be exercised — load a model and run the suite again";
            checks.push(skipped(
                "contract.chat_completion",
                "serves a chat completion",
                reason,
            ));
            checks.push(skipped(
                "contract.chat_stream",
                "streams a chat completion to [DONE]",
                reason,
            ));
        }
    }

    checks.push(match runner.get("/v1/this-route-does-not-exist").await {
        Ok(fetched) if fetched.status == 404 => passed(
            "contract.unknown_route",
            "refuses an unknown route",
            "GET /v1/this-route-does-not-exist → 404",
        ),
        Ok(fetched) => failed(
            "contract.unknown_route",
            "refuses an unknown route",
            format!(
                "an unknown /v1 path answered {} rather than 404",
                fetched.status
            ),
        ),
        Err(error) => failed("contract.unknown_route", "refuses an unknown route", error),
    });

    // The two listeners answer a wrong-method request differently on purpose:
    // the legacy side keeps its historical 404 so a migrating client sees no
    // wire change, the M3 side answers a typed 405. Both are contract-
    // conformant; serving the route anyway is not.
    checks.push(match runner.get("/v1/chat/completions").await {
        Ok(fetched) if fetched.status == 404 || fetched.status == 405 => passed(
            "contract.method_discipline",
            "refuses a wrong-method request",
            format!("GET /v1/chat/completions → {}", fetched.status),
        ),
        Ok(fetched) => failed(
            "contract.method_discipline",
            "refuses a wrong-method request",
            format!(
                "GET on a POST-only route answered {} rather than 404/405",
                fetched.status
            ),
        ),
        Err(error) => failed(
            "contract.method_discipline",
            "refuses a wrong-method request",
            error,
        ),
    });

    checks.push(
        match runner
            .send(runner.client.get(runner.url("/v1/models")), false)
            .await
        {
            Ok(fetched) => {
                let requires_auth = attestation
                    .map(|attestation| attestation.contract.authentication_required);
                match requires_auth {
                    Some(true) if fetched.status == 401 => passed(
                        "contract.authentication",
                        "refuses an unauthenticated request",
                        "an unauthenticated GET /v1/models → 401",
                    ),
                    Some(true) => failed(
                        "contract.authentication",
                        "refuses an unauthenticated request",
                        format!(
                            "the node attests that authentication is required, but an unauthenticated GET /v1/models answered {}",
                            fetched.status
                        ),
                    ),
                    Some(false) => skipped(
                        "contract.authentication",
                        "refuses an unauthenticated request",
                        "this listener attests that it serves loopback without a token",
                    ),
                    None => skipped(
                        "contract.authentication",
                        "refuses an unauthenticated request",
                        "no attestation was readable, so the node's auth claim is unknown",
                    ),
                }
            }
            Err(error) => failed(
                "contract.authentication",
                "refuses an unauthenticated request",
                error,
            ),
        },
    );

    checks.push(
        match runner
            .send(
                runner
                    .client
                    .post(runner.url("/v1/chat/completions"))
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body("{ this is not json"),
                true,
            )
            .await
        {
            Ok(fetched) if (400..500).contains(&fetched.status) => match fetched.json() {
                Some(value)
                    if value
                        .pointer("/error/message")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|message| !message.is_empty())
                        && value.pointer("/error/type").is_some() =>
                {
                    passed(
                        "contract.error_envelope",
                        "reports a bad request in the published envelope",
                        format!(
                            "a malformed body → {} with error.message and error.type",
                            fetched.status
                        ),
                    )
                }
                _ => failed(
                    "contract.error_envelope",
                    "reports a bad request in the published envelope",
                    format!(
                        "a malformed body → {} but the body is not {{error:{{message,type}}}}: {}",
                        fetched.status,
                        truncate(&fetched.body)
                    ),
                ),
            },
            Ok(fetched) => failed(
                "contract.error_envelope",
                "reports a bad request in the published envelope",
                format!(
                    "a malformed body answered {} rather than a 4xx",
                    fetched.status
                ),
            ),
            Err(error) => failed(
                "contract.error_envelope",
                "reports a bad request in the published envelope",
                error,
            ),
        },
    );

    if let Some(attestation) = attestation {
        let published: BTreeSet<&str> = attestation
            .contract
            .manifest
            .http_routes
            .iter()
            .map(|route| route.path.as_str())
            .collect();
        let missing: Vec<&str> = [
            "/health",
            "/v1/models",
            "/v1/chat/completions",
            CONTRACT_PATH,
            ATTESTATION_PATH,
        ]
        .into_iter()
        .filter(|path| !published.contains(path))
        .collect();
        checks.push(if missing.is_empty() {
            passed(
                "contract.route_table",
                "publishes its route table",
                format!(
                    "{} route(s) published",
                    attestation.contract.manifest.http_routes.len()
                ),
            )
        } else {
            failed(
                "contract.route_table",
                "publishes its route table",
                format!(
                    "the published table omits routes this suite requires: {}",
                    missing.join(", ")
                ),
            )
        });

        checks.push(abi_version_check(runner, attestation).await);
    }

    checks
}

/// The K19 ABI, cross-checked against the attestation that claims it.
///
/// `GET /v1/contract` is the version-negotiation surface a third party builds
/// against; the attestation restates the version and the manifest digest. Two
/// published surfaces of the same running instance disagreeing about which ABI
/// it implements is a defect a client would discover the hard way, and it is
/// invisible to any check that reads only one of them.
async fn abi_version_check(
    runner: &Runner<'_>,
    attestation: &ConformanceAttestation,
) -> CheckResult {
    const ID: &str = "contract.abi_version";
    const TITLE: &str = "reports one ABI version on both published surfaces";

    match runner.get(CONTRACT_PATH).await {
        Ok(fetched) if fetched.status == 200 => {
            let Some(value) = fetched.json() else {
                return failed(
                    ID,
                    TITLE,
                    format!(
                        "GET {CONTRACT_PATH} → 200 with a non-JSON body: {}",
                        truncate(&fetched.body)
                    ),
                );
            };
            let version = value
                .get("contract_version")
                .and_then(serde_json::Value::as_str);
            let digest = value.get("digest").and_then(serde_json::Value::as_str);
            let window = value
                .get("support_window_days")
                .and_then(serde_json::Value::as_u64);
            match (version, digest, window) {
                (Some(version), Some(digest), Some(window)) => {
                    let mut disagreements = Vec::new();
                    if version != attestation.contract.contract_version {
                        disagreements.push(format!(
                            "version: {CONTRACT_PATH} says {version}, the attestation says {}",
                            attestation.contract.contract_version
                        ));
                    }
                    if digest != attestation.contract.contract_digest {
                        disagreements.push(format!(
                            "manifest digest: {CONTRACT_PATH} says {digest}, the attestation says {}",
                            attestation.contract.contract_digest
                        ));
                    }
                    if window != u64::from(attestation.contract.manifest.support_window_days) {
                        disagreements.push(format!(
                            "support window: {CONTRACT_PATH} says {window} days, the manifest says {}",
                            attestation.contract.manifest.support_window_days
                        ));
                    }
                    if disagreements.is_empty() {
                        passed(
                            ID,
                            TITLE,
                            format!(
                                "contract {version}, manifest digest {}…, {window}-day support window",
                                &digest[..digest.len().min(12)]
                            ),
                        )
                    } else {
                        failed(ID, TITLE, disagreements.join("; "))
                    }
                }
                _ => failed(
                    ID,
                    TITLE,
                    format!(
                        "GET {CONTRACT_PATH} → 200 without contract_version, digest and support_window_days: {}",
                        truncate(&fetched.body)
                    ),
                ),
            }
        }
        Ok(fetched) => failed(
            ID,
            TITLE,
            format!("GET {CONTRACT_PATH} → {}", fetched.status),
        ),
        Err(error) => failed(ID, TITLE, error),
    }
}

fn probe_request(model: &str, stream: bool) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "stream": stream,
        "max_tokens": 16,
        "messages": [{ "role": "user", "content": "Reply with the single word: conformance." }],
    })
}

async fn chat_completion_check(runner: &Runner<'_>, model: &str) -> CheckResult {
    const ID: &str = "contract.chat_completion";
    const TITLE: &str = "serves a chat completion";
    match runner
        .post_json("/v1/chat/completions", &probe_request(model, false))
        .await
    {
        Ok(fetched) if fetched.status == 200 => {
            let Some(value) = fetched.json() else {
                return failed(
                    ID,
                    TITLE,
                    format!("200 with a non-JSON body: {}", truncate(&fetched.body)),
                );
            };
            let object = value.get("object").and_then(serde_json::Value::as_str);
            let content = value
                .pointer("/choices/0/message/content")
                .and_then(serde_json::Value::as_str);
            let role = value
                .pointer("/choices/0/message/role")
                .and_then(serde_json::Value::as_str);
            match (object, role, content) {
                (Some("chat.completion"), Some("assistant"), Some(_)) => passed(
                    ID,
                    TITLE,
                    format!("POST /v1/chat/completions with '{model}' → 200 chat.completion"),
                ),
                _ => failed(
                    ID,
                    TITLE,
                    format!(
                        "200 but not an OpenAI chat.completion with an assistant message: {}",
                        truncate(&value.to_string())
                    ),
                ),
            }
        }
        Ok(fetched) => failed(
            ID,
            TITLE,
            format!(
                "POST /v1/chat/completions with '{model}' → {}: {}",
                fetched.status,
                truncate(&fetched.body)
            ),
        ),
        Err(error) => failed(ID, TITLE, error),
    }
}

async fn chat_stream_check(runner: &Runner<'_>, model: &str) -> CheckResult {
    const ID: &str = "contract.chat_stream";
    const TITLE: &str = "streams a chat completion to [DONE]";
    match runner
        .post_json("/v1/chat/completions", &probe_request(model, true))
        .await
    {
        Ok(fetched) if fetched.status == 200 => {
            let frames = fetched
                .body
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .collect::<Vec<_>>();
            let terminated = frames.last().map(|frame| frame.trim()) == Some("[DONE]");
            let has_chunk = frames.iter().any(|frame| {
                serde_json::from_str::<serde_json::Value>(frame).is_ok_and(|value| {
                    value.get("object").and_then(serde_json::Value::as_str)
                        == Some("chat.completion.chunk")
                })
            });
            match (terminated, has_chunk) {
                (true, true) => passed(
                    ID,
                    TITLE,
                    format!("{} SSE frame(s), terminated by [DONE]", frames.len()),
                ),
                (false, _) => failed(
                    ID,
                    TITLE,
                    "the SSE stream did not end with a 'data: [DONE]' frame".to_string(),
                ),
                (_, false) => failed(
                    ID,
                    TITLE,
                    "no frame carried object=chat.completion.chunk".to_string(),
                ),
            }
        }
        Ok(fetched) => failed(
            ID,
            TITLE,
            format!(
                "a streaming request → {}: {}",
                fetched.status,
                truncate(&fetched.body)
            ),
        ),
        Err(error) => failed(ID, TITLE, error),
    }
}

// ---------------------------------------------------------------------------
// isolation (optional) — K3
// ---------------------------------------------------------------------------

async fn isolation_checks(
    runner: &Runner<'_>,
    attestation: &ConformanceAttestation,
) -> Vec<CheckResult> {
    let mut checks = Vec::new();
    let isolation = &attestation.isolation;

    checks.push(match (isolation.enforcement, &isolation.mechanism) {
        (SandboxEnforcement::OsEnforced, Some(mechanism)) => passed(
            "isolation.mechanism",
            "names the kernel mechanism it enforces with",
            format!(
                "{} enforces confinement with {mechanism}",
                isolation.platform
            ),
        ),
        (SandboxEnforcement::ProcessContained, Some(mechanism)) => passed(
            "isolation.mechanism",
            "names the kernel mechanism it enforces with",
            format!(
                "{} bounds the process tree with {mechanism}; no filesystem boundary is claimed",
                isolation.platform
            ),
        ),
        (SandboxEnforcement::OsEnforced | SandboxEnforcement::ProcessContained, None) => failed(
            "isolation.mechanism",
            "names the kernel mechanism it enforces with",
            "the node claims kernel enforcement but names no mechanism".to_string(),
        ),
        (SandboxEnforcement::ProcessOnly, _) => failed(
            "isolation.mechanism",
            "names the kernel mechanism it enforces with",
            format!(
                "{} applied no kernel boundary — a restricted cwd and environment only",
                isolation.platform
            ),
        ),
        (SandboxEnforcement::Unavailable, _) => failed(
            "isolation.mechanism",
            "names the kernel mechanism it enforces with",
            "this platform's enforcement mechanism is not usable here".to_string(),
        ),
    });

    // K3 is not closed, and the attestation says so. A node claiming the
    // opposite would be claiming something no build of this app does, so the
    // check reads the claim rather than assuming either answer.
    checks.push(passed(
        "isolation.scope_declared",
        "states honestly how far its confinement reaches",
        if isolation.applies_to_every_tool_call {
            "confinement is claimed for every tool call"
        } else {
            "confinement is opt-in per run; the node does not claim it covers every tool call"
        },
    ));

    let mut leaked = Vec::new();
    let mut probed = 0usize;
    for surface in &attestation.contract.manifest.denied_surfaces {
        let path = denied_probe_path(surface);
        probed += 1;
        match runner.get(&path).await {
            Ok(fetched) if fetched.status == 404 => {}
            Ok(fetched) => leaked.push(format!(
                "{path} ({}) → {}",
                surface.capability, fetched.status
            )),
            Err(error) => leaked.push(format!("{path} ({}): {error}", surface.capability)),
        }
    }
    checks.push(if leaked.is_empty() {
        passed(
            "isolation.denied_surfaces",
            "keeps every denied capability unreachable",
            format!("{probed} denied path(s) answered 404, indistinguishable from unknown"),
        )
    } else {
        failed(
            "isolation.denied_surfaces",
            "keeps every denied capability unreachable",
            format!(
                "denied paths that did not answer 404: {}",
                leaked.join("; ")
            ),
        )
    });

    checks.push(
        if attestation
            .contract
            .compatibility
            .workspace_tool_routes_exposed
        {
            failed(
                "isolation.no_tool_routes",
                "exposes no workspace or tool route",
                "the node's compatibility manifest advertises workspace/tool routes".to_string(),
            )
        } else {
            passed(
                "isolation.no_tool_routes",
                "exposes no workspace or tool route",
                "the compatibility manifest advertises no workspace or tool route",
            )
        },
    );

    checks
}

// ---------------------------------------------------------------------------
// limits (optional) — K4/K5
// ---------------------------------------------------------------------------

async fn limits_checks(
    runner: &Runner<'_>,
    attestation: &ConformanceAttestation,
) -> Vec<CheckResult> {
    let limits = &attestation.limits;
    let mut checks = Vec::new();

    let mut undeclared = Vec::new();
    if limits.max_request_body_bytes == 0 {
        undeclared.push("maxRequestBodyBytes");
    }
    if limits.max_active_requests == 0 {
        undeclared.push("maxActiveRequests");
    }
    if limits.model_output_cap_bytes == 0 {
        undeclared.push("modelOutputCapBytes");
    }
    if limits.background_shell_output_cap_bytes == 0 {
        undeclared.push("backgroundShellOutputCapBytes");
    }
    checks.push(if undeclared.is_empty() {
        passed(
            "limits.declared",
            "declares the bounds it enforces",
            format!(
                "body ≤ {} B, ≤ {} concurrent requests, model output ≤ {} B, shell tail ≤ {} B",
                limits.max_request_body_bytes,
                limits.max_active_requests,
                limits.model_output_cap_bytes,
                limits.background_shell_output_cap_bytes
            ),
        )
    } else {
        failed(
            "limits.declared",
            "declares the bounds it enforces",
            format!(
                "these bounds are declared as unbounded: {}",
                undeclared.join(", ")
            ),
        )
    });

    checks.push(if limits.egress_hardened {
        passed(
            "limits.egress_policy",
            "declares a hardened egress policy",
            "credentialed outbound requests carry a connect timeout, a silence budget and a same-origin redirect policy",
        )
    } else {
        failed(
            "limits.egress_policy",
            "declares a hardened egress policy",
            "the node declares no hardened egress policy (K5)".to_string(),
        )
    });

    checks.push(over_cap_body_check(runner, limits.max_request_body_bytes).await);

    checks
}

/// The one limit a caller can actually make a node demonstrate over the wire.
///
/// Sends one byte more than the declared cap and requires the declared
/// refusal. Deliberately not "any 4xx": a node that answered 400 would be
/// refusing the *content*, and a client could not tell a too-large body from a
/// malformed one.
async fn over_cap_body_check(runner: &Runner<'_>, cap: u64) -> CheckResult {
    const ID: &str = "limits.oversized_body";
    const TITLE: &str = "refuses a body past its declared cap";

    let Ok(size) = usize::try_from(cap.saturating_add(1)) else {
        return skipped(
            ID,
            TITLE,
            "the declared body cap does not fit this machine's address space",
        );
    };
    if cap == 0 || size > 256 * 1024 * 1024 {
        return skipped(
            ID,
            TITLE,
            format!("the declared cap of {cap} B is too large to probe from this runner"),
        );
    }

    match runner
        .send(
            runner
                .client
                .post(runner.url("/v1/chat/completions"))
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(vec![b'a'; size]),
            true,
        )
        .await
    {
        Ok(fetched) if fetched.status == 413 => passed(
            ID,
            TITLE,
            format!("a {size}-byte body against a {cap}-byte cap → 413"),
        ),
        Ok(fetched) => failed(
            ID,
            TITLE,
            format!(
                "a {size}-byte body against a {cap}-byte cap → {} rather than 413",
                fetched.status
            ),
        ),
        // A node may close the connection rather than answer; that is a
        // refusal too, but not the declared one, and a client cannot
        // distinguish it from a network fault.
        Err(error) => failed(
            ID,
            TITLE,
            format!("the oversized body produced no HTTP answer: {error}"),
        ),
    }
}

// ---------------------------------------------------------------------------
// ledger (optional) — K12
// ---------------------------------------------------------------------------

async fn ledger_checks(
    runner: &Runner<'_>,
    attestation: &ConformanceAttestation,
) -> Vec<CheckResult> {
    let mut checks = Vec::new();

    // Re-read rather than reuse the attestation the run opened with. That one
    // was this run's *first* request, so its head predates every request the
    // suite has made since — including, on a freshly started node, any
    // request at all. Grading append-only against a stale head would report
    // "the chain is empty" about a node whose chain the suite itself has
    // been filling for the last dozen checks.
    let current = match runner.get(ATTESTATION_PATH).await {
        Ok(fetched) if fetched.status == 200 => {
            serde_json::from_str::<ConformanceAttestation>(&fetched.body).ok()
        }
        _ => None,
    };
    let attestation = current.as_ref().unwrap_or(attestation);

    let Some(ledger) = &attestation.ledger else {
        return vec![failed(
            "ledger.chain_intact",
            "vouches for its own event chain",
            "the node claims the ledger section but published no chain evidence".to_string(),
        )];
    };

    let head = match &ledger.evidence.verification {
        ChainVerification::Intact {
            covered_from,
            covered_through,
            events_seen,
            ..
        } => {
            checks.push(passed(
                "ledger.chain_intact",
                "vouches for its own event chain",
                format!(
                    "{events_seen} event(s) recomputed, covering {}..{}",
                    covered_from.map_or("-".to_string(), |value| value.to_string()),
                    covered_through.map_or("-".to_string(), |value| value.to_string()),
                ),
            ));
            ledger.evidence.head.clone()
        }
        ChainVerification::Broken { sequence, detail } => {
            checks.push(failed(
                "ledger.chain_intact",
                "vouches for its own event chain",
                format!("the chain is broken at sequence {sequence}: {detail}"),
            ));
            None
        }
    };

    // Append-only, proved over the wire rather than asserted: take the head,
    // perform an action the node records, then ask for the links that followed
    // the head we saw. The first of them must name our head as its
    // predecessor, or the stream was rewritten between the two reads.
    const APPEND_ID: &str = "ledger.append_only";
    const APPEND_TITLE: &str = "extends its chain without rewriting it";
    let Some(head) = head else {
        checks.push(skipped(
            APPEND_ID,
            APPEND_TITLE,
            "the node's chain is empty, so there is no head to extend from",
        ));
        return checks;
    };

    // Reading the attestation is itself a recorded action, so the suite needs
    // no model loaded to make the stream move.
    let follow_up = runner
        .get(&format!(
            "{ATTESTATION_PATH}?{LEDGER_AFTER_PARAM}={}",
            head.sequence
        ))
        .await;
    checks.push(match follow_up {
        Ok(fetched) if fetched.status == 200 => {
            match serde_json::from_str::<ConformanceAttestation>(&fetched.body) {
                Ok(later) => match later.ledger.as_ref().map(|ledger| &ledger.evidence.links_after) {
                    Some(links) if links.is_empty() => failed(
                        APPEND_ID,
                        APPEND_TITLE,
                        format!(
                            "an attestation read is a recorded action, but nothing followed sequence {}",
                            head.sequence
                        ),
                    ),
                    Some(links) => {
                        let first = &links[0];
                        if first.previous_hash.as_deref() != Some(head.event_hash.as_str()) {
                            failed(
                                APPEND_ID,
                                APPEND_TITLE,
                                format!(
                                    "sequence {} does not link to the head this run saw at {}",
                                    first.sequence, head.sequence
                                ),
                            )
                        } else if let Some(gap) = first_linkage_gap(links) {
                            failed(APPEND_ID, APPEND_TITLE, gap)
                        } else {
                            passed(
                                APPEND_ID,
                                APPEND_TITLE,
                                format!(
                                    "{} new link(s) after sequence {}, each naming its predecessor's hash",
                                    links.len(),
                                    head.sequence
                                ),
                            )
                        }
                    }
                    None => failed(
                        APPEND_ID,
                        APPEND_TITLE,
                        "the follow-up attestation published no chain evidence".to_string(),
                    ),
                },
                Err(error) => failed(APPEND_ID, APPEND_TITLE, format!("the follow-up attestation could not be read: {error}")),
            }
        }
        Ok(fetched) => failed(
            APPEND_ID,
            APPEND_TITLE,
            format!("the follow-up attestation answered {}", fetched.status),
        ),
        Err(error) => failed(APPEND_ID, APPEND_TITLE, error),
    });

    checks
}

/// The first place a run of links stops naming its predecessor, described.
fn first_linkage_gap(links: &[crate::run_ledger::ChainLink]) -> Option<String> {
    links.windows(2).find_map(|pair| {
        let (earlier, later) = (&pair[0], &pair[1]);
        if later.sequence <= earlier.sequence {
            return Some(format!(
                "sequence {} does not follow {}",
                later.sequence, earlier.sequence
            ));
        }
        if later.previous_hash.as_deref() != Some(earlier.event_hash.as_str()) {
            return Some(format!(
                "sequence {} does not name sequence {}'s hash",
                later.sequence, earlier.sequence
            ));
        }
        None
    })
}

fn truncate(value: &str) -> String {
    const LIMIT: usize = 240;
    if value.len() <= LIMIT {
        return value.to_string();
    }
    let mut end = LIMIT;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section(id: SectionId, status: SectionStatus, checks: Vec<CheckResult>) -> SectionReport {
        SectionReport {
            id,
            requirement: id.requirement(),
            covers: id.covers().to_string(),
            status,
            skip_reason: None,
            checks,
        }
    }

    #[test]
    fn every_section_code_round_trips() {
        for &id in SectionId::ALL {
            assert_eq!(SectionId::parse(id.code()), Some(id));
        }
        assert_eq!(SectionId::parse("nope"), None);
    }

    #[test]
    fn only_the_contract_section_is_required() {
        let required: Vec<&str> = SectionId::ALL
            .iter()
            .filter(|id| id.requirement() == Requirement::Required)
            .map(|id| id.code())
            .collect();
        assert_eq!(required, vec!["contract"]);
    }

    #[test]
    fn a_skipped_optional_section_does_not_block_a_compatible_verdict() {
        let verdict = verdict_for(&[
            section(
                SectionId::Contract,
                SectionStatus::Passed,
                vec![passed("contract.health", "t", "ok")],
            ),
            section(SectionId::Ledger, SectionStatus::Skipped, Vec::new()),
        ]);
        assert!(matches!(verdict, Verdict::Compatible { .. }));
    }

    #[test]
    fn an_attempted_optional_section_that_fails_blocks_the_claim() {
        let verdict = verdict_for(&[
            section(
                SectionId::Contract,
                SectionStatus::Passed,
                vec![passed("contract.health", "t", "ok")],
            ),
            section(
                SectionId::Isolation,
                SectionStatus::Failed,
                vec![failed("isolation.mechanism", "t", "no boundary")],
            ),
        ]);
        match verdict {
            Verdict::NotCompatible { reasons } => {
                assert_eq!(reasons.len(), 1);
                assert!(reasons[0].contains("isolation.mechanism"), "{reasons:?}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// The failure mode the whole `Incomplete` state exists for: a required
    /// section whose checks all passed *because most of them never ran* must
    /// not read as a compatibility claim.
    #[test]
    fn a_required_section_with_an_unrun_check_is_not_compatible() {
        let verdict = verdict_for(&[section(
            SectionId::Contract,
            SectionStatus::Incomplete,
            vec![
                passed("contract.health", "t", "ok"),
                skipped("contract.chat_completion", "t", "no model"),
            ],
        )]);
        match verdict {
            Verdict::NotCompatible { reasons } => {
                assert!(
                    reasons[0].contains("contract.chat_completion"),
                    "{reasons:?}"
                );
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn roll_up_prefers_a_failure_over_a_skip() {
        assert_eq!(
            roll_up(&[
                skipped("a", "t", "s"),
                failed("b", "t", "f"),
                passed("c", "t", "p")
            ]),
            SectionStatus::Failed
        );
        assert_eq!(
            roll_up(&[skipped("a", "t", "s"), passed("c", "t", "p")]),
            SectionStatus::Incomplete
        );
        assert_eq!(roll_up(&[passed("c", "t", "p")]), SectionStatus::Passed);
    }

    #[test]
    fn linkage_gaps_are_found_at_the_first_break() {
        use crate::run_ledger::ChainLink;
        let links = vec![
            ChainLink {
                sequence: 1,
                event_hash: "aaa".into(),
                previous_hash: None,
            },
            ChainLink {
                sequence: 2,
                event_hash: "bbb".into(),
                previous_hash: Some("aaa".into()),
            },
            ChainLink {
                sequence: 3,
                event_hash: "ccc".into(),
                previous_hash: Some("not-bbb".into()),
            },
        ];
        assert!(first_linkage_gap(&links)
            .expect("a gap")
            .contains("sequence 3"));
        assert_eq!(first_linkage_gap(&links[..2]), None);
    }

    /// The attestation is a published artifact; a build that cannot serialize
    /// its own claim would fail every run with an unhelpful parse error.
    #[test]
    fn an_attestation_round_trips_through_its_own_wire_format() {
        let attestation = build_attestation(AttestationInputs {
            authentication_required: true,
            ledger: None,
        });
        let encoded = serde_json::to_string(&attestation).expect("encode");
        let decoded: ConformanceAttestation = serde_json::from_str(&encoded).expect("decode");
        assert_eq!(decoded, attestation);
        assert_eq!(decoded.suite_revision, SUITE_REVISION);

        // A ledger-less listener must say so rather than claim the section.
        let ledger = decoded
            .sections
            .iter()
            .find(|section| section.id == SectionId::Ledger)
            .expect("a ledger section");
        assert!(!ledger.claimed);
        assert!(ledger.reason.is_some());
    }

    /// The attestation carries K19's generated manifest rather than a second
    /// route table of its own — the whole reason K19 generates one.
    #[test]
    fn the_published_route_table_is_k19s_and_carries_what_the_suite_requires() {
        let attestation = build_attestation(AttestationInputs {
            authentication_required: true,
            ledger: None,
        });
        assert_eq!(attestation.contract.manifest, crate::contract::manifest());
        assert_eq!(
            attestation.contract.contract_version,
            crate::contract::CONTRACT_VERSION
        );
        assert_eq!(
            attestation.contract.contract_digest,
            crate::contract::digest()
        );
        for path in [
            "/health",
            "/v1/models",
            "/v1/chat/completions",
            CONTRACT_PATH,
            ATTESTATION_PATH,
        ] {
            assert!(
                attestation
                    .contract
                    .manifest
                    .http_routes
                    .iter()
                    .any(|route| route.path == path),
                "the published route table omits {path}"
            );
        }
    }

    /// A `prefix` denial publishes a matcher, not a request. The runner has to
    /// turn it into something a client would actually send, or the probe hits
    /// a path no route ever claimed for reasons unrelated to the denial.
    #[test]
    fn a_prefix_denial_is_probed_with_a_real_path() {
        assert_eq!(
            denied_probe_path(&DeniedSurfaceEntry {
                capability: "tool_execution".into(),
                path: "/v1/tool_".into(),
                matcher: "prefix".into(),
            }),
            "/v1/tool_probe"
        );
        assert_eq!(
            denied_probe_path(&DeniedSurfaceEntry {
                capability: "git_access".into(),
                path: "/v1/git".into(),
                matcher: "exact_or_descendant".into(),
            }),
            "/v1/git"
        );
    }
}
