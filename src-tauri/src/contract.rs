//! The versioned syscall ABI (roadmap K19).
//!
//! **What this module is.** One generated description of everything a third
//! party can build against: every externally reachable HTTP route, every
//! signed remote-plane route, the ACP stdio methods, and the agent tool
//! schemas. It is *generated*, not written: the HTTP section reads
//! [`crate::http_route_registry::ROUTES`] — the same table the listener
//! dispatches from — the tool section reads [`crate::agent_tools`] — the same
//! functions the agent loop hands to the model — and the remote-plane section
//! is checked against `monkey-cli`'s own dispatch match by a test that scans
//! that source, so a route added there and not declared here fails CI rather
//! than silently leaving the published contract.
//!
//! **Why a contract version separate from the app version.** `little-monkey`
//! ships a new build whenever anything changes; a package or a third-party
//! node cares about one question — "does the surface I compiled against still
//! exist?" [`CONTRACT_VERSION`] answers only that, and moves only when the
//! surface does: major on a removal or a tightened requirement, minor on an
//! addition, patch on wording. K20's package gate and K21's conformance suite
//! both name this number, which is why it is a value in this crate rather
//! than a line in a document.
//!
//! **The gate.** `contract/baseline.json` is the last *published* manifest.
//! `tests/contract_abi.rs` regenerates the current one, diffs it against that
//! baseline, and fails when [`CONTRACT_VERSION`] has not moved far enough for
//! the changes it finds. Publishing is the deliberate act of copying the
//! current manifest over the baseline and bumping the version —
//! `docs/contract-abi.md` is the procedure and the support window.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::http_route_registry::{
    AllowedMethods, DeniedCapability, DeniedPathMatcher, HttpMethod, PathMatcher, RouteFamily,
    RouteOwner, DENIED_SURFACES, ROUTES,
};

/// Semantic version of the whole published contract.
///
/// Bump rules — enforced by `tests/contract_abi.rs`, not by convention:
/// * **major**: anything an existing caller can lose — a route, a method, a
///   tool, a tool parameter, or a parameter that becomes required.
/// * **minor**: anything additive — a new route, method, tool, or optional
///   parameter.
/// * **patch**: descriptions and other non-structural wording.
pub const CONTRACT_VERSION: &str = "1.2.0";

/// How long a surface stays after it is announced deprecated.
///
/// A number rather than prose because K20's resolver has to be able to answer
/// "is this still here next quarter?" without a human reading a policy page.
/// The window runs from the release that first ships the deprecation, and a
/// removal is a major bump on top of the window — the window is a floor, not
/// permission to remove on day 181 in a minor.
pub const SUPPORT_WINDOW_DAYS: u32 = 180;

/// ACP protocol version implemented over stdio. Mirrors `monkey-cli`'s
/// `acp::ACP_PROTOCOL_VERSION`; the sync test in `acp.rs` fails if they part.
pub const ACP_PROTOCOL_VERSION: u64 = 1;

/// Signed remote-plane protocol version. Mirrors `monkey-cli`'s
/// `daemon::remote::protocol::REMOTE_PROTOCOL_VERSION`, checked by the same
/// source-scanning test that checks [`REMOTE_ROUTES`].
pub const REMOTE_PROTOCOL_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// The remote plane's route table
// ---------------------------------------------------------------------------

/// One route on the signed remote plane (`monkey-cli daemon serve`).
///
/// Path segments that are captures are written `{name}`, matching the binding
/// names in the dispatch match so the sync test can compare the two directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemoteRouteSpec {
    pub plane: RemotePlane,
    pub method: &'static str,
    pub path: &'static str,
    /// The exact `RemoteAction::`/`DeviceCapability::` variant the dispatch
    /// arm requires, or [`RemoteGate::Unauthenticated`]. Asserted against the
    /// source, so this is a fact about the code rather than a claim about it.
    pub gate: RemoteGate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemotePlane {
    /// Pairing enrolment — the one route that runs before a device exists.
    Pairing,
    /// Acts on runs this machine already holds (K10/K11).
    Control,
    /// First-party mobile companion (K15).
    Mobile,
    /// Placement and live migration (K17/K18) — the only way a run authored
    /// elsewhere starts here.
    Node,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteGate {
    Unauthenticated,
    Action(&'static str),
    Capability(&'static str),
    /// A device may always sever itself; no grant beyond a valid signature.
    SelfService,
}

/// Every route the signed remote plane dispatches.
///
/// Kept here rather than in `monkey-cli` because the published contract is
/// generated in this library; `daemon::remote::api`'s test scans its own
/// dispatch match and fails if this table and that match disagree on a route,
/// a method or a gate.
pub const REMOTE_ROUTES: &[RemoteRouteSpec] = &[
    RemoteRouteSpec {
        plane: RemotePlane::Pairing,
        method: "POST",
        path: "/v1/remote/pairings/accept",
        gate: RemoteGate::Unauthenticated,
    },
    RemoteRouteSpec {
        plane: RemotePlane::Control,
        method: "GET",
        path: "/v1/remote/runs",
        gate: RemoteGate::Action("ViewRuns"),
    },
    RemoteRouteSpec {
        plane: RemotePlane::Control,
        method: "GET",
        path: "/v1/remote/runs/{run_id}",
        gate: RemoteGate::Action("ViewRuns"),
    },
    RemoteRouteSpec {
        plane: RemotePlane::Control,
        method: "GET",
        path: "/v1/remote/runs/{run_id}/events",
        gate: RemoteGate::Action("ViewEvents"),
    },
    RemoteRouteSpec {
        plane: RemotePlane::Control,
        method: "GET",
        path: "/v1/remote/runs/{run_id}/approvals",
        gate: RemoteGate::Action("ViewRuns"),
    },
    RemoteRouteSpec {
        plane: RemotePlane::Control,
        method: "GET",
        path: "/v1/remote/runs/{run_id}/artifacts/{artifact_id}",
        gate: RemoteGate::Action("ReadArtifacts"),
    },
    RemoteRouteSpec {
        plane: RemotePlane::Control,
        method: "POST",
        path: "/v1/remote/runs/{run_id}/approve",
        gate: RemoteGate::Action("Approve"),
    },
    RemoteRouteSpec {
        plane: RemotePlane::Control,
        method: "POST",
        path: "/v1/remote/runs/{run_id}/cancel",
        gate: RemoteGate::Action("Cancel"),
    },
    RemoteRouteSpec {
        plane: RemotePlane::Control,
        method: "POST",
        path: "/v1/remote/runs/{run_id}/pause",
        gate: RemoteGate::Action("Pause"),
    },
    RemoteRouteSpec {
        plane: RemotePlane::Control,
        method: "POST",
        path: "/v1/remote/runs/{run_id}/resume",
        gate: RemoteGate::Action("Pause"),
    },
    RemoteRouteSpec {
        plane: RemotePlane::Control,
        method: "POST",
        path: "/v1/remote/kill",
        gate: RemoteGate::Action("Kill"),
    },
    RemoteRouteSpec {
        plane: RemotePlane::Control,
        method: "POST",
        path: "/v1/remote/desktop-control/start",
        gate: RemoteGate::Action("ControlDesktop"),
    },
    RemoteRouteSpec {
        plane: RemotePlane::Control,
        method: "POST",
        path: "/v1/remote/desktop-control/action",
        gate: RemoteGate::Action("ControlDesktop"),
    },
    RemoteRouteSpec {
        plane: RemotePlane::Control,
        method: "POST",
        path: "/v1/remote/desktop-control/stop",
        gate: RemoteGate::Action("ControlDesktop"),
    },
    RemoteRouteSpec {
        plane: RemotePlane::Mobile,
        method: "GET",
        path: "/v1/remote/mobile/sessions",
        gate: RemoteGate::Capability("ViewSessions"),
    },
    RemoteRouteSpec {
        plane: RemotePlane::Mobile,
        method: "GET",
        path: "/v1/remote/mobile/sessions/{session_id}/messages",
        gate: RemoteGate::Capability("ViewSessions"),
    },
    RemoteRouteSpec {
        plane: RemotePlane::Mobile,
        method: "POST",
        path: "/v1/remote/mobile/sessions/{session_id}/messages",
        gate: RemoteGate::Capability("Chat"),
    },
    RemoteRouteSpec {
        plane: RemotePlane::Mobile,
        method: "GET",
        path: "/v1/remote/mobile/workflows",
        gate: RemoteGate::Capability("ViewTasks"),
    },
    RemoteRouteSpec {
        plane: RemotePlane::Mobile,
        method: "POST",
        path: "/v1/remote/mobile/workflows/{workflow_id}/runs",
        gate: RemoteGate::Capability("RunWorkflows"),
    },
    RemoteRouteSpec {
        plane: RemotePlane::Mobile,
        method: "POST",
        path: "/v1/remote/mobile/captures",
        gate: RemoteGate::Capability("Capture"),
    },
    RemoteRouteSpec {
        plane: RemotePlane::Mobile,
        method: "DELETE",
        path: "/v1/remote/mobile/devices/self",
        gate: RemoteGate::SelfService,
    },
    RemoteRouteSpec {
        plane: RemotePlane::Node,
        method: "GET",
        path: "/v1/remote/node",
        gate: RemoteGate::Capability("DescribeNode"),
    },
    RemoteRouteSpec {
        plane: RemotePlane::Node,
        method: "GET",
        path: "/v1/remote/node/health",
        gate: RemoteGate::Capability("DescribeNode"),
    },
    RemoteRouteSpec {
        plane: RemotePlane::Node,
        method: "POST",
        path: "/v1/remote/node/runs",
        gate: RemoteGate::Capability("PlaceRuns"),
    },
    RemoteRouteSpec {
        plane: RemotePlane::Node,
        method: "GET",
        path: "/v1/remote/node/runs/{submitted_run_id}",
        gate: RemoteGate::Capability("DescribeNode"),
    },
    RemoteRouteSpec {
        plane: RemotePlane::Node,
        method: "POST",
        path: "/v1/remote/node/migration/preflight",
        gate: RemoteGate::Capability("Migrate"),
    },
    RemoteRouteSpec {
        plane: RemotePlane::Node,
        method: "POST",
        path: "/v1/remote/node/migration/accept",
        gate: RemoteGate::Capability("Migrate"),
    },
];

/// Every ACP method `monkey-cli acp` dispatches. Checked against `acp.rs`'s
/// own match by the same kind of source scan as [`REMOTE_ROUTES`].
pub const ACP_METHODS: &[&str] = &[
    "initialize",
    "session/new",
    "session/load",
    "session/resume",
    "session/set_mode",
    "session/prompt",
    "session/cancel",
    "$/cancel_request",
];

// ---------------------------------------------------------------------------
// Deprecations
// ---------------------------------------------------------------------------

/// One announced deprecation. Empty is the honest state of a first published
/// contract: nothing has been announced, so nothing is inside its window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeprecationSpec {
    /// `http`, `remote`, `acp` or `tool`.
    pub surface: &'static str,
    /// The route path, ACP method or tool name being deprecated.
    pub id: &'static str,
    /// Contract version that announced it.
    pub announced_in: &'static str,
    /// Earliest contract version that may remove it. Always a major bump.
    pub removable_in: &'static str,
    /// What to use instead, or `""` when there is no replacement.
    pub replacement: &'static str,
}

pub const DEPRECATIONS: &[DeprecationSpec] = &[];

// ---------------------------------------------------------------------------
// The manifest
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpRouteEntry {
    pub id: String,
    pub path: String,
    pub family: String,
    /// Methods the legacy loopback listener answers on this path.
    pub legacy_methods: Vec<String>,
    /// Methods the M3 (LAN/paired) listener answers on this path.
    pub m3_methods: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeniedSurfaceEntry {
    pub capability: String,
    pub path: String,
    /// `exact_or_descendant` or `prefix`.
    pub matcher: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteRouteEntry {
    pub plane: RemotePlane,
    pub method: String,
    pub path: String,
    pub gate: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpEntry {
    pub protocol_version: u64,
    pub methods: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolEntry {
    pub name: String,
    pub description: String,
    /// When the agent loop offers this tool: `base`, `plan_mode`,
    /// `stack_attached` or `subagents_enabled`.
    pub availability: String,
    pub parameters: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeprecationEntry {
    pub surface: String,
    pub id: String,
    pub announced_in: String,
    pub removable_in: String,
    pub replacement: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractManifest {
    pub contract_version: String,
    pub support_window_days: u32,
    pub http_routes: Vec<HttpRouteEntry>,
    pub denied_surfaces: Vec<DeniedSurfaceEntry>,
    pub remote_protocol_version: u32,
    pub remote_routes: Vec<RemoteRouteEntry>,
    pub acp: AcpEntry,
    pub tools: Vec<ToolEntry>,
    pub deprecations: Vec<DeprecationEntry>,
}

fn method_name(method: HttpMethod) -> &'static str {
    match method {
        HttpMethod::Get => "GET",
        HttpMethod::Post => "POST",
        HttpMethod::Options => "OPTIONS",
        HttpMethod::Other => "OTHER",
    }
}

fn methods(methods: AllowedMethods, owner: RouteOwner) -> Vec<String> {
    methods
        .for_owner(owner)
        .iter()
        .map(|method| method_name(*method).to_string())
        .collect()
}

/// The published path form of a matcher. Captures are `{param}` and a prefix
/// match is `…/*`, so a reader of the artifact sees the shape of the URL
/// rather than the name of a Rust enum variant.
fn path_pattern(matcher: PathMatcher) -> String {
    match matcher {
        PathMatcher::Exact(path) => path.to_string(),
        PathMatcher::NonEmptyRemainder { prefix } => format!("{prefix}{{path}}"),
        PathMatcher::SegmentWithSuffix { prefix, suffix } => format!("{prefix}{{id}}{suffix}"),
        PathMatcher::FirstSegmentWithRemainder { prefix } => format!("{prefix}{{id}}/{{path}}"),
        PathMatcher::Prefix(prefix) => format!("{prefix}*"),
    }
}

fn family_name(family: RouteFamily) -> &'static str {
    match family {
        RouteFamily::LegacyHost => "legacy_host",
        RouteFamily::Shared => "shared",
        RouteFamily::M3Compatibility => "m3_compatibility",
        RouteFamily::M3Lifecycle => "m3_lifecycle",
        RouteFamily::LegacyPreflight => "legacy_preflight",
    }
}

fn denied_capability_name(capability: DeniedCapability) -> &'static str {
    match capability {
        DeniedCapability::AgentExecution => "agent_execution",
        DeniedCapability::WorkspaceAccess => "workspace_access",
        DeniedCapability::ToolExecution => "tool_execution",
        DeniedCapability::FileAccess => "file_access",
        DeniedCapability::GitAccess => "git_access",
        DeniedCapability::McpAccess => "mcp_access",
        DeniedCapability::RecipeExecution => "recipe_execution",
    }
}

fn gate_name(gate: RemoteGate) -> String {
    match gate {
        RemoteGate::Unauthenticated => "unauthenticated".to_string(),
        RemoteGate::Action(action) => format!("action:{action}"),
        RemoteGate::Capability(capability) => format!("capability:{capability}"),
        RemoteGate::SelfService => "self_service".to_string(),
    }
}

/// Turns one OpenAI-style tool definition into a contract entry.
fn tool_entry(definition: &serde_json::Value, availability: &str) -> ToolEntry {
    let function = &definition["function"];
    ToolEntry {
        name: function["name"].as_str().unwrap_or_default().to_string(),
        description: function["description"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        availability: availability.to_string(),
        parameters: function["parameters"].clone(),
    }
}

/// Every tool in the contract, generated from [`crate::agent_tools`].
///
/// `search_docs` is offered with the attached stack names spliced into its
/// description; the contract publishes it with the literal `{stack_names}`
/// placeholder, because the *shape* is the contract and the names are one
/// user's configuration.
fn tool_entries() -> Vec<ToolEntry> {
    let mut entries: Vec<ToolEntry> = crate::agent_tools::tool_definitions()
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .map(|tool| tool_entry(tool, "base"))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    entries.push(tool_entry(
        &crate::agent_tools::present_plan_tool_def(),
        "plan_mode",
    ));
    entries.push(tool_entry(
        &crate::agent_tools::search_docs_tool_def(&["{stack_names}".to_string()]),
        "stack_attached",
    ));
    entries.push(tool_entry(
        &crate::agent_tools::task_tool_def(),
        "subagents_enabled",
    ));
    entries
}

/// The whole published contract, generated from the tables the running code
/// dispatches from.
pub fn manifest() -> ContractManifest {
    ContractManifest {
        contract_version: CONTRACT_VERSION.to_string(),
        support_window_days: SUPPORT_WINDOW_DAYS,
        http_routes: ROUTES
            .iter()
            .map(|route| HttpRouteEntry {
                id: format!("{:?}", route.id),
                path: path_pattern(route.path),
                family: family_name(route.family).to_string(),
                legacy_methods: methods(route.methods, RouteOwner::Legacy),
                m3_methods: methods(route.methods, RouteOwner::M3),
            })
            .collect(),
        denied_surfaces: DENIED_SURFACES
            .iter()
            .map(|surface| DeniedSurfaceEntry {
                capability: denied_capability_name(surface.capability).to_string(),
                path: match surface.path {
                    DeniedPathMatcher::ExactOrDescendant(path) => path.to_string(),
                    DeniedPathMatcher::Prefix(prefix) => prefix.to_string(),
                },
                matcher: match surface.path {
                    DeniedPathMatcher::ExactOrDescendant(_) => "exact_or_descendant".to_string(),
                    DeniedPathMatcher::Prefix(_) => "prefix".to_string(),
                },
            })
            .collect(),
        remote_protocol_version: REMOTE_PROTOCOL_VERSION,
        remote_routes: REMOTE_ROUTES
            .iter()
            .map(|route| RemoteRouteEntry {
                plane: route.plane,
                method: route.method.to_string(),
                path: route.path.to_string(),
                gate: gate_name(route.gate),
            })
            .collect(),
        acp: AcpEntry {
            protocol_version: ACP_PROTOCOL_VERSION,
            methods: ACP_METHODS.iter().map(|m| m.to_string()).collect(),
        },
        tools: tool_entries(),
        deprecations: DEPRECATIONS
            .iter()
            .map(|entry| DeprecationEntry {
                surface: entry.surface.to_string(),
                id: entry.id.to_string(),
                announced_in: entry.announced_in.to_string(),
                removable_in: entry.removable_in.to_string(),
                replacement: entry.replacement.to_string(),
            })
            .collect(),
    }
}

/// The manifest as the published artifact's exact bytes: pretty-printed with
/// a trailing newline, so `contract/agent-os-contract.json` diffs line by line
/// in review instead of as one 40 KB line.
pub fn manifest_json_text() -> String {
    let mut text = serde_json::to_string_pretty(&manifest()).unwrap_or_default();
    text.push('\n');
    text
}

/// SHA-256 over the published bytes. What a client pins when it wants to know
/// that the surface it tested against is byte-for-byte the surface it is
/// talking to now — a version says "compatible", a digest says "identical".
pub fn digest() -> String {
    let mut hasher = Sha256::new();
    hasher.update(manifest_json_text().as_bytes());
    format!("{:x}", hasher.finalize())
}

/// The `GET /v1/contract` body: the version a running instance implements,
/// the digest of the exact manifest it was built from, and the manifest
/// itself so a client needs no second request and no shipped copy.
pub fn introspection() -> serde_json::Value {
    serde_json::json!({
        "contract_version": CONTRACT_VERSION,
        "digest": digest(),
        "support_window_days": SUPPORT_WINDOW_DAYS,
        "implementation": {
            "name": "little-monkey",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "manifest": manifest(),
    })
}

// ---------------------------------------------------------------------------
// Version arithmetic and the breaking-change gate
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

/// Three dot-separated numbers. Deliberately not a semver crate: the contract
/// version has no pre-release or build metadata, and a dependency that could
/// accept `1.0.0-rc.1` here would let one into the published artifact.
pub fn parse_version(value: &str) -> Option<Version> {
    let mut parts = value.split('.');
    let mut next = || parts.next()?.parse::<u32>().ok();
    let (major, minor, patch) = (next()?, next()?, next()?);
    parts.next().is_none().then_some(Version {
        major,
        minor,
        patch,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ChangeKind {
    /// Wording only. No caller can break on it.
    Patch,
    /// Something new. Old callers keep working.
    Additive,
    /// Something an existing caller can lose.
    Breaking,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContractChange {
    pub kind: ChangeKind,
    pub detail: String,
}

fn required_parameters(parameters: &serde_json::Value) -> BTreeSet<String> {
    parameters["required"]
        .as_array()
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn parameter_names(parameters: &serde_json::Value) -> BTreeSet<String> {
    parameters["properties"]
        .as_object()
        .map(|properties| properties.keys().cloned().collect())
        .unwrap_or_default()
}

fn diff_tools(baseline: &ContractManifest, current: &ContractManifest) -> Vec<ContractChange> {
    let mut changes = Vec::new();
    for old in &baseline.tools {
        let Some(new) = current.tools.iter().find(|tool| tool.name == old.name) else {
            changes.push(ContractChange {
                kind: ChangeKind::Breaking,
                detail: format!("tool removed: {}", old.name),
            });
            continue;
        };
        if new.availability != old.availability {
            changes.push(ContractChange {
                kind: ChangeKind::Breaking,
                detail: format!(
                    "tool {} availability changed: {} -> {}",
                    old.name, old.availability, new.availability
                ),
            });
        }
        let (old_params, new_params) = (
            parameter_names(&old.parameters),
            parameter_names(&new.parameters),
        );
        for lost in old_params.difference(&new_params) {
            changes.push(ContractChange {
                kind: ChangeKind::Breaking,
                detail: format!("tool {} lost parameter {lost}", old.name),
            });
        }
        for added in new_params.difference(&old_params) {
            changes.push(ContractChange {
                kind: ChangeKind::Additive,
                detail: format!("tool {} gained parameter {added}", old.name),
            });
        }
        let (old_required, new_required) = (
            required_parameters(&old.parameters),
            required_parameters(&new.parameters),
        );
        for tightened in new_required.difference(&old_required) {
            changes.push(ContractChange {
                kind: ChangeKind::Breaking,
                detail: format!("tool {} now requires {tightened}", old.name),
            });
        }
        for relaxed in old_required.difference(&new_required) {
            changes.push(ContractChange {
                kind: ChangeKind::Additive,
                detail: format!("tool {} no longer requires {relaxed}", old.name),
            });
        }
        if new.description != old.description {
            changes.push(ContractChange {
                kind: ChangeKind::Patch,
                detail: format!("tool {} description changed", old.name),
            });
        }
    }
    for new in &current.tools {
        if !baseline.tools.iter().any(|tool| tool.name == new.name) {
            changes.push(ContractChange {
                kind: ChangeKind::Additive,
                detail: format!("tool added: {}", new.name),
            });
        }
    }
    changes
}

fn diff_http(baseline: &ContractManifest, current: &ContractManifest) -> Vec<ContractChange> {
    let mut changes = Vec::new();
    for old in &baseline.http_routes {
        let Some(new) = current.http_routes.iter().find(|route| route.id == old.id) else {
            changes.push(ContractChange {
                kind: ChangeKind::Breaking,
                detail: format!("http route removed: {} {}", old.id, old.path),
            });
            continue;
        };
        if new.path != old.path {
            changes.push(ContractChange {
                kind: ChangeKind::Breaking,
                detail: format!("http route {} moved: {} -> {}", old.id, old.path, new.path),
            });
        }
        for (listener, old_methods, new_methods) in [
            ("legacy", &old.legacy_methods, &new.legacy_methods),
            ("m3", &old.m3_methods, &new.m3_methods),
        ] {
            for lost in old_methods.iter().filter(|m| !new_methods.contains(m)) {
                changes.push(ContractChange {
                    kind: ChangeKind::Breaking,
                    detail: format!("http route {} lost {listener} method {lost}", old.id),
                });
            }
            for added in new_methods.iter().filter(|m| !old_methods.contains(m)) {
                changes.push(ContractChange {
                    kind: ChangeKind::Additive,
                    detail: format!("http route {} gained {listener} method {added}", old.id),
                });
            }
        }
    }
    for new in &current.http_routes {
        if !baseline.http_routes.iter().any(|route| route.id == new.id) {
            changes.push(ContractChange {
                kind: ChangeKind::Additive,
                detail: format!("http route added: {} {}", new.id, new.path),
            });
        }
    }
    changes
}

fn diff_remote(baseline: &ContractManifest, current: &ContractManifest) -> Vec<ContractChange> {
    let key = |route: &RemoteRouteEntry| format!("{} {}", route.method, route.path);
    let mut changes = Vec::new();
    for old in &baseline.remote_routes {
        match current
            .remote_routes
            .iter()
            .find(|new| key(new) == key(old))
        {
            None => changes.push(ContractChange {
                kind: ChangeKind::Breaking,
                detail: format!("remote route removed: {}", key(old)),
            }),
            // A route that starts demanding a different grant breaks every
            // pairing that held the old one, so it is not an additive change
            // even though the route is still there.
            Some(new) if new.gate != old.gate => changes.push(ContractChange {
                kind: ChangeKind::Breaking,
                detail: format!(
                    "remote route {} gate changed: {} -> {}",
                    key(old),
                    old.gate,
                    new.gate
                ),
            }),
            Some(_) => {}
        }
    }
    for new in &current.remote_routes {
        if !baseline
            .remote_routes
            .iter()
            .any(|old| key(old) == key(new))
        {
            changes.push(ContractChange {
                kind: ChangeKind::Additive,
                detail: format!("remote route added: {}", key(new)),
            });
        }
    }
    if current.remote_protocol_version != baseline.remote_protocol_version {
        changes.push(ContractChange {
            kind: ChangeKind::Breaking,
            detail: format!(
                "remote protocol version changed: {} -> {}",
                baseline.remote_protocol_version, current.remote_protocol_version
            ),
        });
    }
    changes
}

fn diff_acp(baseline: &ContractManifest, current: &ContractManifest) -> Vec<ContractChange> {
    let mut changes = Vec::new();
    for old in &baseline.acp.methods {
        if !current.acp.methods.contains(old) {
            changes.push(ContractChange {
                kind: ChangeKind::Breaking,
                detail: format!("acp method removed: {old}"),
            });
        }
    }
    for new in &current.acp.methods {
        if !baseline.acp.methods.contains(new) {
            changes.push(ContractChange {
                kind: ChangeKind::Additive,
                detail: format!("acp method added: {new}"),
            });
        }
    }
    if current.acp.protocol_version != baseline.acp.protocol_version {
        changes.push(ContractChange {
            kind: ChangeKind::Breaking,
            detail: format!(
                "acp protocol version changed: {} -> {}",
                baseline.acp.protocol_version, current.acp.protocol_version
            ),
        });
    }
    changes
}

fn diff_denied(baseline: &ContractManifest, current: &ContractManifest) -> Vec<ContractChange> {
    // A surface that stops being denied is a *widening* of what this machine
    // exposes. It cannot break a caller, so it is not "Breaking" in the
    // compatibility sense — but it is never a patch either, and it must show
    // up in the diff a reviewer reads.
    let mut changes = Vec::new();
    for old in &baseline.denied_surfaces {
        if !current
            .denied_surfaces
            .iter()
            .any(|new| new.path == old.path && new.matcher == old.matcher)
        {
            changes.push(ContractChange {
                kind: ChangeKind::Additive,
                detail: format!("denied surface no longer declared: {}", old.path),
            });
        }
    }
    for new in &current.denied_surfaces {
        if !baseline
            .denied_surfaces
            .iter()
            .any(|old| old.path == new.path && old.matcher == new.matcher)
        {
            changes.push(ContractChange {
                kind: ChangeKind::Additive,
                detail: format!("denied surface declared: {}", new.path),
            });
        }
    }
    changes
}

/// Every difference between two manifests, classified by what it can do to a
/// caller that was built against the baseline.
pub fn diff(baseline: &ContractManifest, current: &ContractManifest) -> Vec<ContractChange> {
    let mut changes = diff_http(baseline, current);
    changes.extend(diff_denied(baseline, current));
    changes.extend(diff_remote(baseline, current));
    changes.extend(diff_acp(baseline, current));
    changes.extend(diff_tools(baseline, current));
    if current.support_window_days < baseline.support_window_days {
        changes.push(ContractChange {
            kind: ChangeKind::Breaking,
            detail: format!(
                "support window shortened: {} -> {} days",
                baseline.support_window_days, current.support_window_days
            ),
        });
    }
    changes
}

/// The smallest version the changes allow, given the baseline's version.
pub fn required_version(baseline: Version, changes: &[ContractChange]) -> Version {
    match changes.iter().map(|change| change.kind).max() {
        Some(ChangeKind::Breaking) => Version {
            major: baseline.major + 1,
            minor: 0,
            patch: 0,
        },
        Some(ChangeKind::Additive) => Version {
            major: baseline.major,
            minor: baseline.minor + 1,
            patch: 0,
        },
        Some(ChangeKind::Patch) => Version {
            major: baseline.major,
            minor: baseline.minor,
            patch: baseline.patch + 1,
        },
        None => baseline,
    }
}

/// The gate itself: `Ok(changes)` when [`CONTRACT_VERSION`] has moved far
/// enough for what changed, `Err(explanation)` when it has not. The error
/// names the required version and every change that forced it — a reviewer
/// should never have to re-derive which edit was the breaking one.
pub fn check_against_baseline(baseline_json: &str) -> Result<Vec<ContractChange>, String> {
    let baseline: ContractManifest = serde_json::from_str(baseline_json)
        .map_err(|error| format!("contract/baseline.json is not a manifest: {error}"))?;
    let current = manifest();
    let baseline_version = parse_version(&baseline.contract_version).ok_or_else(|| {
        format!(
            "baseline version {} is not x.y.z",
            baseline.contract_version
        )
    })?;
    let current_version = parse_version(&current.contract_version)
        .ok_or_else(|| format!("CONTRACT_VERSION {} is not x.y.z", current.contract_version))?;
    let changes = diff(&baseline, &current);
    let required = required_version(baseline_version, &changes);
    if current_version >= required {
        return Ok(changes);
    }
    let breaking = changes
        .iter()
        .filter(|change| change.kind == ChangeKind::Breaking)
        .map(|change| format!("  BREAKING  {}", change.detail))
        .collect::<Vec<_>>();
    let additive = changes
        .iter()
        .filter(|change| change.kind == ChangeKind::Additive)
        .map(|change| format!("  additive  {}", change.detail))
        .collect::<Vec<_>>();
    Err(format!(
        "The published contract changed but CONTRACT_VERSION did not keep up.\n\
         baseline {}.{}.{} -> requires at least {}.{}.{}, found {}.\n{}\n{}\n\
         Bump CONTRACT_VERSION, regenerate, and republish the baseline \
         (docs/contract-abi.md).",
        baseline_version.major,
        baseline_version.minor,
        baseline_version.patch,
        required.major,
        required.minor,
        required.patch,
        current.contract_version,
        breaking.join("\n"),
        additive.join("\n"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn baseline() -> ContractManifest {
        manifest()
    }

    #[test]
    fn the_manifest_is_generated_from_the_route_table_the_listener_dispatches_from() {
        let manifest = manifest();
        assert_eq!(manifest.http_routes.len(), ROUTES.len());
        let contract_route = manifest
            .http_routes
            .iter()
            .find(|route| route.path == "/v1/contract")
            .expect("the introspection endpoint is part of the contract it publishes");
        assert!(contract_route.legacy_methods.contains(&"GET".to_string()));
        assert!(contract_route.m3_methods.contains(&"GET".to_string()));
    }

    #[test]
    fn every_tool_the_agent_loop_can_offer_is_published_with_its_availability() {
        let manifest = manifest();
        let names = manifest
            .tools
            .iter()
            .map(|tool| (tool.name.as_str(), tool.availability.as_str()))
            .collect::<Vec<_>>();
        assert!(names.contains(&("read_file", "base")));
        assert!(names.contains(&("present_plan", "plan_mode")));
        assert!(names.contains(&("search_docs", "stack_attached")));
        assert!(names.contains(&("task", "subagents_enabled")));
        // The per-turn description is parameterized; the contract publishes
        // the placeholder, never one machine's stack names.
        let search_docs = manifest
            .tools
            .iter()
            .find(|tool| tool.name == "search_docs")
            .expect("search_docs");
        assert!(search_docs.description.contains("{stack_names}"));
    }

    #[test]
    fn an_unchanged_contract_needs_no_version_bump() {
        let changes = diff(&baseline(), &manifest());
        assert!(changes.is_empty(), "{changes:?}");
        assert_eq!(
            required_version(parse_version(CONTRACT_VERSION).unwrap(), &changes),
            parse_version(CONTRACT_VERSION).unwrap()
        );
    }

    #[test]
    fn a_removed_route_demands_a_major_bump_and_a_new_one_only_a_minor() {
        let mut removed = baseline();
        removed.http_routes.remove(0);
        let changes = diff(&removed, &manifest());
        assert_eq!(
            changes.iter().map(|c| c.kind).max(),
            Some(ChangeKind::Additive),
            "adding a route back is additive"
        );

        let changes = diff(&baseline(), &removed);
        assert_eq!(
            changes.iter().map(|c| c.kind).max(),
            Some(ChangeKind::Breaking)
        );
        assert_eq!(
            required_version(
                Version {
                    major: 1,
                    minor: 4,
                    patch: 2
                },
                &changes
            ),
            Version {
                major: 2,
                minor: 0,
                patch: 0
            }
        );
    }

    #[test]
    fn a_newly_required_tool_parameter_is_breaking_and_a_new_optional_one_is_not() {
        let mut tightened = baseline();
        let tool = tightened
            .tools
            .iter_mut()
            .find(|tool| tool.name == "web_fetch")
            .expect("web_fetch");
        tool.parameters["required"] = serde_json::json!(["url", "max_chars"]);
        let changes = diff(&baseline(), &tightened);
        assert_eq!(
            changes.iter().map(|c| c.kind).max(),
            Some(ChangeKind::Breaking)
        );

        let mut widened = baseline();
        let tool = widened
            .tools
            .iter_mut()
            .find(|tool| tool.name == "web_fetch")
            .expect("web_fetch");
        tool.parameters["properties"]["timeout_ms"] = serde_json::json!({ "type": "integer" });
        let changes = diff(&baseline(), &widened);
        assert_eq!(
            changes.iter().map(|c| c.kind).max(),
            Some(ChangeKind::Additive)
        );
    }

    #[test]
    fn a_remote_route_that_starts_demanding_a_different_grant_is_breaking() {
        let mut regrated = baseline();
        let route = regrated
            .remote_routes
            .iter_mut()
            .find(|route| route.path == "/v1/remote/node/runs")
            .expect("placement route");
        route.gate = "capability:DescribeNode".to_string();
        let changes = diff(&baseline(), &regrated);
        assert_eq!(
            changes.iter().map(|c| c.kind).max(),
            Some(ChangeKind::Breaking)
        );
    }

    #[test]
    fn the_gate_rejects_an_unversioned_breaking_change_and_names_it() {
        let mut published = baseline();
        published.tools.push(ToolEntry {
            name: "invented_tool".to_string(),
            description: "Only in the baseline, so the current manifest lost it.".to_string(),
            availability: "base".to_string(),
            parameters: serde_json::json!({ "type": "object", "properties": {} }),
        });
        let error = check_against_baseline(&serde_json::to_string(&published).unwrap())
            .expect_err("losing a tool without a major bump must fail");
        assert!(error.contains("tool removed: invented_tool"), "{error}");
        assert!(error.contains("requires at least 2.0.0"), "{error}");
    }

    #[test]
    fn the_gate_accepts_a_baseline_the_current_version_already_covers() {
        let mut published = baseline();
        published.contract_version = "0.9.0".to_string();
        published.tools.retain(|tool| tool.name != "web_search");
        let changes = check_against_baseline(&serde_json::to_string(&published).unwrap())
            .expect("1.0.0 covers an additive change over 0.9.0");
        assert!(changes
            .iter()
            .any(|change| change.detail == "tool added: web_search"));
    }

    #[test]
    fn the_digest_covers_the_published_bytes() {
        assert_eq!(digest().len(), 64);
        assert_eq!(introspection()["digest"], digest());
        assert_eq!(introspection()["contract_version"], CONTRACT_VERSION);
    }

    #[test]
    fn parse_version_refuses_anything_that_is_not_three_numbers() {
        assert_eq!(
            parse_version("1.2.3"),
            Some(Version {
                major: 1,
                minor: 2,
                patch: 3
            })
        );
        assert_eq!(parse_version("1.0.0-rc.1"), None);
        assert_eq!(parse_version("1.0"), None);
        assert_eq!(parse_version("1.0.0.0"), None);
    }
}
