//! Pure route registry for the unified legacy/M3 HTTP listener.
//!
//! This module deliberately knows nothing about `AppHandle`, runtime models,
//! bearer-token plaintexts, credential stores, or handler state.  A caller
//! classifies the listener and the already-recognized authentication family;
//! the registry then makes the routing decision from those inputs alone.

use hyper::Method;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListenerExposure {
    Loopback,
    Lan,
}

/// Authentication family recognized before route dispatch.
///
/// `Internal` also represents a request with no bearer token.  Authentication
/// and scope checks remain handler responsibilities; this value only prevents
/// overlapping compatibility routes from probing models or credentials to
/// decide which handler should receive the request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthFamily {
    LegacyToken,
    PairedLanToken,
    Internal,
}

/// Syntactic migration classifier. It never reads either credential store,
/// probes a runtime, or decides whether a token is valid; the selected owner
/// performs the real authentication. Pairing must be checked first because
/// `lmk-lan-*` also starts with the legacy `lmk-` prefix.
pub fn classify_bearer_family(authorization: Option<&str>) -> AuthFamily {
    let token = authorization.and_then(|value| {
        let (scheme, token) = value.split_once(char::is_whitespace)?;
        scheme
            .eq_ignore_ascii_case("bearer")
            .then_some(token.trim())
            .filter(|token| !token.is_empty())
    });
    match token {
        Some(token) if token.starts_with("lmk-lan-") => AuthFamily::PairedLanToken,
        Some(_) => AuthFamily::LegacyToken,
        None => AuthFamily::Internal,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClassificationInput {
    pub exposure: ListenerExposure,
    pub auth_family: AuthFamily,
}

impl ClassificationInput {
    pub const fn new(exposure: ListenerExposure, auth_family: AuthFamily) -> Self {
        Self {
            exposure,
            auth_family,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
    Options,
    Other,
}

impl From<&Method> for HttpMethod {
    fn from(method: &Method) -> Self {
        if method == Method::GET {
            Self::Get
        } else if method == Method::POST {
            Self::Post
        } else if method == Method::OPTIONS {
            Self::Options
        } else {
            Self::Other
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteOwner {
    Legacy,
    M3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteFamily {
    LegacyHost,
    Shared,
    M3Compatibility,
    M3Lifecycle,
    LegacyPreflight,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RouteId {
    Health,
    Models,
    ChatCompletions,
    Embeddings,
    KnowledgeQuery,
    ArtifactRead,
    WorkflowRunStatus,
    LocalAppRun,
    LocalAppStatic,
    OpenAiResponses,
    AnthropicMessages,
    OllamaTags,
    OllamaChat,
    ModelDownload,
    ModelLoad,
    ModelUnload,
    ModelStatus,
    ModelDelete,
    RequestCancel,
    Contract,
    LegacyV1Preflight,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathMatcher {
    Exact(&'static str),
    /// The full remainder is one non-empty capture. Slashes are preserved.
    NonEmptyRemainder {
        prefix: &'static str,
    },
    /// One non-empty path segment between `prefix` and `suffix`.
    SegmentWithSuffix {
        prefix: &'static str,
        suffix: &'static str,
    },
    /// A non-empty first segment and an optional remaining relative path.
    FirstSegmentWithRemainder {
        prefix: &'static str,
    },
    Prefix(&'static str),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RouteCaptures<'path> {
    pub primary: Option<&'path str>,
    pub remainder: Option<&'path str>,
}

impl PathMatcher {
    fn captures<'path>(self, path: &'path str) -> Option<RouteCaptures<'path>> {
        match self {
            Self::Exact(expected) => (path == expected).then_some(RouteCaptures::default()),
            Self::NonEmptyRemainder { prefix } => {
                let value = path.strip_prefix(prefix)?;
                (!value.is_empty()).then_some(RouteCaptures {
                    primary: Some(value),
                    remainder: None,
                })
            }
            Self::SegmentWithSuffix { prefix, suffix } => {
                let value = path.strip_prefix(prefix)?.strip_suffix(suffix)?;
                (!value.is_empty() && !value.contains('/')).then_some(RouteCaptures {
                    primary: Some(value),
                    remainder: None,
                })
            }
            Self::FirstSegmentWithRemainder { prefix } => {
                let rest = path.strip_prefix(prefix)?;
                let mut parts = rest.splitn(2, '/');
                let primary = parts.next().unwrap_or_default();
                if primary.is_empty() {
                    return None;
                }
                Some(RouteCaptures {
                    primary: Some(primary),
                    remainder: Some(parts.next().unwrap_or_default()),
                })
            }
            Self::Prefix(prefix) => path.strip_prefix(prefix).map(|remainder| RouteCaptures {
                primary: None,
                remainder: Some(remainder),
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AllowedMethods {
    pub legacy: &'static [HttpMethod],
    pub m3: &'static [HttpMethod],
}

impl AllowedMethods {
    pub const fn for_owner(self, owner: RouteOwner) -> &'static [HttpMethod] {
        match owner {
            RouteOwner::Legacy => self.legacy,
            RouteOwner::M3 => self.m3,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouteSpec {
    pub id: RouteId,
    pub family: RouteFamily,
    pub path: PathMatcher,
    pub methods: AllowedMethods,
}

impl RouteSpec {
    pub fn allowed_methods(self, owner: RouteOwner) -> &'static [HttpMethod] {
        self.methods.for_owner(owner)
    }
}

const NONE: &[HttpMethod] = &[];
const GET: &[HttpMethod] = &[HttpMethod::Get];
const GET_OPTIONS: &[HttpMethod] = &[HttpMethod::Get, HttpMethod::Options];
const POST_OPTIONS: &[HttpMethod] = &[HttpMethod::Post, HttpMethod::Options];
const OPTIONS: &[HttpMethod] = &[HttpMethod::Options];

const fn legacy_methods(methods: &'static [HttpMethod]) -> AllowedMethods {
    AllowedMethods {
        legacy: methods,
        m3: NONE,
    }
}

const fn m3_methods(methods: &'static [HttpMethod]) -> AllowedMethods {
    AllowedMethods {
        legacy: NONE,
        m3: methods,
    }
}

const fn shared_methods(
    legacy: &'static [HttpMethod],
    m3: &'static [HttpMethod],
) -> AllowedMethods {
    AllowedMethods { legacy, m3 }
}

/// The only routes the unified listener may dispatch.
///
/// The final entry is the legacy `/v1/*` OPTIONS fallback. Classification
/// treats it as a method-scoped fallback so an unknown `GET /v1/...` remains
/// a 404 instead of becoming a 405.
pub const ROUTES: &[RouteSpec] = &[
    RouteSpec {
        id: RouteId::Health,
        family: RouteFamily::Shared,
        path: PathMatcher::Exact("/health"),
        methods: shared_methods(GET, GET_OPTIONS),
    },
    RouteSpec {
        id: RouteId::Models,
        family: RouteFamily::Shared,
        path: PathMatcher::Exact("/v1/models"),
        methods: shared_methods(GET_OPTIONS, GET_OPTIONS),
    },
    RouteSpec {
        id: RouteId::ChatCompletions,
        family: RouteFamily::Shared,
        path: PathMatcher::Exact("/v1/chat/completions"),
        methods: shared_methods(POST_OPTIONS, POST_OPTIONS),
    },
    RouteSpec {
        id: RouteId::Embeddings,
        family: RouteFamily::Shared,
        path: PathMatcher::Exact("/v1/embeddings"),
        methods: shared_methods(POST_OPTIONS, POST_OPTIONS),
    },
    RouteSpec {
        id: RouteId::KnowledgeQuery,
        family: RouteFamily::LegacyHost,
        path: PathMatcher::Exact("/v1/knowledge/query"),
        methods: legacy_methods(POST_OPTIONS),
    },
    RouteSpec {
        id: RouteId::ArtifactRead,
        family: RouteFamily::LegacyHost,
        path: PathMatcher::NonEmptyRemainder {
            prefix: "/v1/artifacts/",
        },
        methods: legacy_methods(GET_OPTIONS),
    },
    RouteSpec {
        id: RouteId::WorkflowRunStatus,
        family: RouteFamily::LegacyHost,
        path: PathMatcher::NonEmptyRemainder {
            prefix: "/v1/workflows/runs/",
        },
        methods: legacy_methods(GET_OPTIONS),
    },
    RouteSpec {
        id: RouteId::LocalAppRun,
        family: RouteFamily::LegacyHost,
        path: PathMatcher::SegmentWithSuffix {
            prefix: "/v1/local-apps/",
            suffix: "/run",
        },
        methods: legacy_methods(POST_OPTIONS),
    },
    RouteSpec {
        id: RouteId::LocalAppStatic,
        family: RouteFamily::LegacyHost,
        path: PathMatcher::FirstSegmentWithRemainder {
            prefix: "/local-apps/",
        },
        methods: legacy_methods(GET),
    },
    RouteSpec {
        id: RouteId::OpenAiResponses,
        family: RouteFamily::M3Compatibility,
        path: PathMatcher::Exact("/v1/responses"),
        methods: m3_methods(POST_OPTIONS),
    },
    RouteSpec {
        id: RouteId::AnthropicMessages,
        family: RouteFamily::M3Compatibility,
        path: PathMatcher::Exact("/v1/messages"),
        methods: m3_methods(POST_OPTIONS),
    },
    RouteSpec {
        id: RouteId::OllamaTags,
        family: RouteFamily::M3Compatibility,
        path: PathMatcher::Exact("/api/tags"),
        methods: m3_methods(GET_OPTIONS),
    },
    RouteSpec {
        id: RouteId::OllamaChat,
        family: RouteFamily::M3Compatibility,
        path: PathMatcher::Exact("/api/chat"),
        methods: m3_methods(POST_OPTIONS),
    },
    RouteSpec {
        id: RouteId::ModelDownload,
        family: RouteFamily::M3Lifecycle,
        path: PathMatcher::Exact("/v1/models/download"),
        methods: m3_methods(POST_OPTIONS),
    },
    RouteSpec {
        id: RouteId::ModelLoad,
        family: RouteFamily::M3Lifecycle,
        path: PathMatcher::Exact("/v1/models/load"),
        methods: m3_methods(POST_OPTIONS),
    },
    RouteSpec {
        id: RouteId::ModelUnload,
        family: RouteFamily::M3Lifecycle,
        path: PathMatcher::Exact("/v1/models/unload"),
        methods: m3_methods(POST_OPTIONS),
    },
    RouteSpec {
        id: RouteId::ModelStatus,
        family: RouteFamily::M3Lifecycle,
        path: PathMatcher::Exact("/v1/models/status"),
        methods: m3_methods(POST_OPTIONS),
    },
    RouteSpec {
        id: RouteId::ModelDelete,
        family: RouteFamily::M3Lifecycle,
        path: PathMatcher::Exact("/v1/models/delete"),
        methods: m3_methods(POST_OPTIONS),
    },
    RouteSpec {
        id: RouteId::RequestCancel,
        family: RouteFamily::M3Lifecycle,
        path: PathMatcher::Exact("/v1/requests/cancel"),
        methods: m3_methods(POST_OPTIONS),
    },
    // The K19 contract introspection endpoint. Shared, and unauthenticated on
    // both listeners for the same reason `/health` is: a client has to be able
    // to ask which ABI this instance implements *before* it knows whether its
    // credentials are still the right shape. It reports only what the built
    // binary would answer with anyway — no configuration, no model list, no
    // credential state — so it is a version negotiation surface, not a probe.
    RouteSpec {
        id: RouteId::Contract,
        family: RouteFamily::Shared,
        path: PathMatcher::Exact("/v1/contract"),
        methods: shared_methods(GET_OPTIONS, GET_OPTIONS),
    },
    RouteSpec {
        id: RouteId::LegacyV1Preflight,
        family: RouteFamily::LegacyPreflight,
        path: PathMatcher::Prefix("/v1/"),
        methods: legacy_methods(OPTIONS),
    },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeniedCapability {
    AgentExecution,
    WorkspaceAccess,
    ToolExecution,
    FileAccess,
    GitAccess,
    McpAccess,
    RecipeExecution,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeniedPathMatcher {
    ExactOrDescendant(&'static str),
    Prefix(&'static str),
}

impl DeniedPathMatcher {
    fn matches(self, path: &str) -> bool {
        match self {
            Self::ExactOrDescendant(root) => {
                path == root
                    || path
                        .strip_prefix(root)
                        .is_some_and(|remainder| remainder.starts_with('/'))
            }
            Self::Prefix(prefix) => path.starts_with(prefix),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeniedSurfaceSpec {
    pub capability: DeniedCapability,
    pub path: DeniedPathMatcher,
}

/// Explicit negative capabilities. These are checked before the legacy
/// `/v1/*` preflight fallback, so even OPTIONS cannot make an execution
/// surface appear exposed.
pub const DENIED_SURFACES: &[DeniedSurfaceSpec] = &[
    DeniedSurfaceSpec {
        capability: DeniedCapability::AgentExecution,
        path: DeniedPathMatcher::ExactOrDescendant("/v1/agent"),
    },
    DeniedSurfaceSpec {
        capability: DeniedCapability::AgentExecution,
        path: DeniedPathMatcher::ExactOrDescendant("/v1/agents"),
    },
    DeniedSurfaceSpec {
        capability: DeniedCapability::WorkspaceAccess,
        path: DeniedPathMatcher::ExactOrDescendant("/v1/workspace"),
    },
    DeniedSurfaceSpec {
        capability: DeniedCapability::WorkspaceAccess,
        path: DeniedPathMatcher::ExactOrDescendant("/v1/workspaces"),
    },
    DeniedSurfaceSpec {
        capability: DeniedCapability::ToolExecution,
        path: DeniedPathMatcher::ExactOrDescendant("/v1/tool"),
    },
    DeniedSurfaceSpec {
        capability: DeniedCapability::ToolExecution,
        path: DeniedPathMatcher::ExactOrDescendant("/v1/tools"),
    },
    DeniedSurfaceSpec {
        capability: DeniedCapability::ToolExecution,
        path: DeniedPathMatcher::Prefix("/v1/tool_"),
    },
    DeniedSurfaceSpec {
        capability: DeniedCapability::FileAccess,
        path: DeniedPathMatcher::ExactOrDescendant("/v1/files"),
    },
    DeniedSurfaceSpec {
        capability: DeniedCapability::GitAccess,
        path: DeniedPathMatcher::ExactOrDescendant("/v1/git"),
    },
    DeniedSurfaceSpec {
        capability: DeniedCapability::McpAccess,
        path: DeniedPathMatcher::ExactOrDescendant("/v1/mcp"),
    },
    DeniedSurfaceSpec {
        capability: DeniedCapability::RecipeExecution,
        path: DeniedPathMatcher::ExactOrDescendant("/v1/recipes"),
    },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteDenial {
    ExplicitCapability(DeniedCapability),
    LoopbackOnly(RouteId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouteMatch<'path> {
    pub route: &'static RouteSpec,
    pub owner: RouteOwner,
    pub captures: RouteCaptures<'path>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteDecision<'path> {
    Allowed(RouteMatch<'path>),
    MethodNotAllowed {
        route: RouteId,
        owner: RouteOwner,
        allowed: &'static [HttpMethod],
    },
    Denied(RouteDenial),
    NotFound,
}

/// Owner for routes implemented by both historical listeners.
///
/// LAN traffic always stays in M3's LAN security policy. On loopback, paired
/// callers opt into M3 while legacy/internal callers retain byte-compatible
/// legacy behavior.
///
/// # Why the four shared routes still have two implementations
///
/// `/health`, `/v1/models`, `/v1/chat/completions` and `/v1/embeddings` each
/// have a complete handler in `server.rs` *and* a complete handler in
/// `m3_http_server.rs`, and this function is the fork that picks one. That
/// duplication looks like exactly the kind of residue the "one HTTP server"
/// work exists to delete. **It is not.** The two sides answer the same request
/// in deliberately different bytes, and the difference is the security boundary:
///
/// * **CORS.** The legacy side stamps `Access-Control-Allow-Origin: *` onto
///   every response, including failures, because a bearer token — not the
///   browser's same-origin policy — is its gate (see `server.rs`'s `with_cors`
///   and `authenticate_credential`, which requires a real token from any
///   request carrying an `Origin` header even when `require_token` is off). The
///   M3 side is deny-by-default against `LanServerPolicy::cors_allowlist` and
///   answers `403 cors_denied` to an origin that is not listed. Collapsing the
///   two onto the M3 handler turns every browser client of the loopback API
///   into a 403; collapsing them onto the legacy handler hands a LAN listener
///   wildcard CORS.
/// * **Error envelope.** Legacy emits
///   `{"error":{"message","type":"invalid_request_error","code"}}`; M3 emits
///   `{"error":{"code","message","type":"little_monkey_m3_error"}}`. Real
///   OpenAI-compatible SDKs parse the former.
/// * **Streaming.** Legacy `/v1/chat/completions` is a reverse proxy and passes
///   upstream SSE bytes through unmodified; M3 re-frames every event through its
///   own translation layer.
/// * **Body shape.** `/health` differs by whole keys (`service`,
///   `schemaVersion`, `tls`, `conformance`); `/v1/models` differs by the
///   `extended` flag (`source_id`/`runtime_id`/`backend` rows) and by legacy's
///   rewrite of `owned_by` to `"local"` for managed-local models.
///
/// `tests/legacy_route_compatibility.rs` pins the legacy bytes for `/health`,
/// `/v1/models`, the wildcard CORS header, the error envelope and the raw SSE
/// passthrough precisely so that a well-intentioned merge here fails loudly
/// instead of shipping. Shared *mechanism* — admission, the body cap, the route
/// table, the clock — belongs in `http_policy.rs`/this module and has been moved
/// there. Shared *bytes* do not exist to be merged.
pub const fn shared_route_owner(input: ClassificationInput) -> RouteOwner {
    match (input.exposure, input.auth_family) {
        (ListenerExposure::Lan, _) | (_, AuthFamily::PairedLanToken) => RouteOwner::M3,
        (ListenerExposure::Loopback, AuthFamily::LegacyToken | AuthFamily::Internal) => {
            RouteOwner::Legacy
        }
    }
}

fn owner_for(route: &RouteSpec, input: ClassificationInput) -> Option<RouteOwner> {
    match route.family {
        RouteFamily::LegacyHost => {
            (input.exposure == ListenerExposure::Loopback).then_some(RouteOwner::Legacy)
        }
        RouteFamily::Shared => Some(shared_route_owner(input)),
        RouteFamily::M3Compatibility | RouteFamily::M3Lifecycle => Some(RouteOwner::M3),
        RouteFamily::LegacyPreflight => (input.exposure == ListenerExposure::Loopback
            && shared_route_owner(input) == RouteOwner::Legacy)
            .then_some(RouteOwner::Legacy),
    }
}

pub fn denied_capability(path: &str) -> Option<DeniedCapability> {
    DENIED_SURFACES
        .iter()
        .find(|surface| surface.path.matches(path))
        .map(|surface| surface.capability)
}

pub fn classify_request<'path>(
    method: &Method,
    path: &'path str,
    input: ClassificationInput,
) -> RouteDecision<'path> {
    classify_route(HttpMethod::from(method), path, input)
}

pub fn classify_route<'path>(
    method: HttpMethod,
    path: &'path str,
    input: ClassificationInput,
) -> RouteDecision<'path> {
    if let Some(capability) = denied_capability(path) {
        return RouteDecision::Denied(RouteDenial::ExplicitCapability(capability));
    }

    for route in ROUTES
        .iter()
        .filter(|route| route.family != RouteFamily::LegacyPreflight)
    {
        let Some(captures) = route.path.captures(path) else {
            continue;
        };
        let Some(owner) = owner_for(route, input) else {
            return RouteDecision::Denied(RouteDenial::LoopbackOnly(route.id));
        };
        let allowed = route.allowed_methods(owner);
        return if allowed.contains(&method) {
            RouteDecision::Allowed(RouteMatch {
                route,
                owner,
                captures,
            })
        } else {
            RouteDecision::MethodNotAllowed {
                route: route.id,
                owner,
                allowed,
            }
        };
    }

    // Preserve legacy's method-scoped dynamic route: OPTIONS on any /v1/*
    // path, but only when this request belongs to the legacy compatibility
    // side. It must not turn other methods on an unknown path into a 405.
    if method == HttpMethod::Options {
        let route = ROUTES
            .iter()
            .find(|route| route.family == RouteFamily::LegacyPreflight)
            .expect("the typed registry includes its legacy preflight fallback");
        if let (Some(owner), Some(captures)) = (owner_for(route, input), route.path.captures(path))
        {
            return RouteDecision::Allowed(RouteMatch {
                route,
                owner,
                captures,
            });
        }
    }

    RouteDecision::NotFound
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOOPBACK_LEGACY: ClassificationInput =
        ClassificationInput::new(ListenerExposure::Loopback, AuthFamily::LegacyToken);
    const LOOPBACK_INTERNAL: ClassificationInput =
        ClassificationInput::new(ListenerExposure::Loopback, AuthFamily::Internal);
    const LOOPBACK_PAIRED: ClassificationInput =
        ClassificationInput::new(ListenerExposure::Loopback, AuthFamily::PairedLanToken);
    const LAN_PAIRED: ClassificationInput =
        ClassificationInput::new(ListenerExposure::Lan, AuthFamily::PairedLanToken);

    fn allowed(method: HttpMethod, path: &str, input: ClassificationInput) -> RouteMatch<'_> {
        match classify_route(method, path, input) {
            RouteDecision::Allowed(route) => route,
            other => panic!("expected {method:?} {path} to be allowed, got {other:?}"),
        }
    }

    #[test]
    fn shared_routes_use_only_exposure_and_auth_family_to_select_an_owner() {
        for (input, owner) in [
            (LOOPBACK_LEGACY, RouteOwner::Legacy),
            (LOOPBACK_INTERNAL, RouteOwner::Legacy),
            (LOOPBACK_PAIRED, RouteOwner::M3),
            (LAN_PAIRED, RouteOwner::M3),
            (
                ClassificationInput::new(ListenerExposure::Lan, AuthFamily::LegacyToken),
                RouteOwner::M3,
            ),
            (
                ClassificationInput::new(ListenerExposure::Lan, AuthFamily::Internal),
                RouteOwner::M3,
            ),
        ] {
            assert_eq!(allowed(HttpMethod::Get, "/v1/models", input).owner, owner);
            assert_eq!(
                allowed(HttpMethod::Post, "/v1/chat/completions", input).owner,
                owner
            );
            assert_eq!(
                allowed(HttpMethod::Post, "/v1/embeddings", input).owner,
                owner
            );
        }
    }

    #[test]
    fn bearer_classifier_checks_overlapping_pairing_prefix_first() {
        assert_eq!(
            classify_bearer_family(Some("Bearer lmk-lan-abc")),
            AuthFamily::PairedLanToken
        );
        assert_eq!(
            classify_bearer_family(Some("bearer lmk-abc")),
            AuthFamily::LegacyToken
        );
        assert_eq!(
            classify_bearer_family(Some("Bearer unrecognized")),
            AuthFamily::LegacyToken
        );
        assert_eq!(classify_bearer_family(None), AuthFamily::Internal);
        assert_eq!(
            classify_bearer_family(Some("Basic credentials")),
            AuthFamily::Internal
        );
    }

    #[test]
    fn health_preserves_legacy_loopback_methods_but_m3_owns_lan_and_paired_requests() {
        assert_eq!(
            allowed(HttpMethod::Get, "/health", LOOPBACK_INTERNAL).owner,
            RouteOwner::Legacy
        );
        assert!(matches!(
            classify_route(HttpMethod::Options, "/health", LOOPBACK_INTERNAL),
            RouteDecision::MethodNotAllowed {
                route: RouteId::Health,
                owner: RouteOwner::Legacy,
                allowed: GET,
            }
        ));
        assert_eq!(
            allowed(HttpMethod::Options, "/health", LOOPBACK_PAIRED).owner,
            RouteOwner::M3
        );
        assert_eq!(
            allowed(HttpMethod::Get, "/health", LAN_PAIRED).owner,
            RouteOwner::M3
        );
    }

    #[test]
    fn legacy_host_dynamic_routes_keep_their_existing_capture_constraints() {
        let artifact = allowed(
            HttpMethod::Get,
            "/v1/artifacts/sha/with/slashes",
            LOOPBACK_LEGACY,
        );
        assert_eq!(artifact.route.id, RouteId::ArtifactRead);
        assert_eq!(artifact.captures.primary, Some("sha/with/slashes"));

        let workflow = allowed(
            HttpMethod::Get,
            "/v1/workflows/runs/run/child",
            LOOPBACK_LEGACY,
        );
        assert_eq!(workflow.route.id, RouteId::WorkflowRunStatus);
        assert_eq!(workflow.captures.primary, Some("run/child"));

        let run = allowed(
            HttpMethod::Post,
            "/v1/local-apps/app-1/run",
            LOOPBACK_LEGACY,
        );
        assert_eq!(run.route.id, RouteId::LocalAppRun);
        assert_eq!(run.captures.primary, Some("app-1"));

        for invalid in [
            "/v1/local-apps//run",
            "/v1/local-apps/app/child/run",
            "/v1/local-apps/app-1/run/extra",
        ] {
            assert_eq!(
                classify_route(HttpMethod::Post, invalid, LOOPBACK_LEGACY),
                RouteDecision::NotFound,
                "invalid local-app route matched: {invalid}"
            );
        }
        assert_eq!(
            classify_route(HttpMethod::Get, "/v1/artifacts/", LOOPBACK_LEGACY),
            RouteDecision::NotFound
        );
        assert_eq!(
            classify_route(HttpMethod::Get, "/v1/workflows/runs/", LOOPBACK_LEGACY),
            RouteDecision::NotFound
        );
    }

    #[test]
    fn local_app_static_preserves_first_segment_and_relative_path() {
        let index = allowed(HttpMethod::Get, "/local-apps/app-1", LOOPBACK_INTERNAL);
        assert_eq!(index.route.id, RouteId::LocalAppStatic);
        assert_eq!(index.captures.primary, Some("app-1"));
        assert_eq!(index.captures.remainder, Some(""));

        let asset = allowed(
            HttpMethod::Get,
            "/local-apps/app-1/assets/main.js",
            LOOPBACK_INTERNAL,
        );
        assert_eq!(asset.captures.primary, Some("app-1"));
        assert_eq!(asset.captures.remainder, Some("assets/main.js"));
        assert_eq!(
            classify_route(
                HttpMethod::Get,
                "/local-apps//index.html",
                LOOPBACK_INTERNAL
            ),
            RouteDecision::NotFound
        );
    }

    #[test]
    fn host_routes_are_never_dispatched_on_a_lan_listener() {
        for (method, path) in [
            (HttpMethod::Post, "/v1/knowledge/query"),
            (HttpMethod::Get, "/v1/artifacts/id"),
            (HttpMethod::Get, "/v1/workflows/runs/id"),
            (HttpMethod::Post, "/v1/local-apps/app/run"),
            (HttpMethod::Get, "/local-apps/app/index.html"),
        ] {
            assert!(matches!(
                classify_route(method, path, LAN_PAIRED),
                RouteDecision::Denied(RouteDenial::LoopbackOnly(_))
            ));
        }
    }

    #[test]
    fn m3_compatibility_and_lifecycle_routes_are_exact_and_m3_owned() {
        for (method, path, id) in [
            (HttpMethod::Post, "/v1/responses", RouteId::OpenAiResponses),
            (HttpMethod::Post, "/v1/messages", RouteId::AnthropicMessages),
            (HttpMethod::Get, "/api/tags", RouteId::OllamaTags),
            (HttpMethod::Post, "/api/chat", RouteId::OllamaChat),
            (
                HttpMethod::Post,
                "/v1/models/download",
                RouteId::ModelDownload,
            ),
            (HttpMethod::Post, "/v1/models/load", RouteId::ModelLoad),
            (HttpMethod::Post, "/v1/models/unload", RouteId::ModelUnload),
            (HttpMethod::Post, "/v1/models/status", RouteId::ModelStatus),
            (HttpMethod::Post, "/v1/models/delete", RouteId::ModelDelete),
            (
                HttpMethod::Post,
                "/v1/requests/cancel",
                RouteId::RequestCancel,
            ),
        ] {
            for input in [
                LOOPBACK_LEGACY,
                LOOPBACK_INTERNAL,
                LOOPBACK_PAIRED,
                LAN_PAIRED,
            ] {
                let matched = allowed(method, path, input);
                assert_eq!(matched.route.id, id);
                assert_eq!(matched.owner, RouteOwner::M3);
            }
            assert!(matches!(
                classify_route(HttpMethod::Other, path, LAN_PAIRED),
                RouteDecision::MethodNotAllowed { route, owner: RouteOwner::M3, .. }
                    if route == id
            ));
        }
        assert_eq!(
            classify_route(HttpMethod::Post, "/v1/models/load/extra", LOOPBACK_LEGACY),
            RouteDecision::NotFound
        );
    }

    #[test]
    fn legacy_v1_preflight_is_a_method_scoped_fallback_only() {
        let preflight = allowed(
            HttpMethod::Options,
            "/v1/future-compatibility-route",
            LOOPBACK_INTERNAL,
        );
        assert_eq!(preflight.route.id, RouteId::LegacyV1Preflight);
        assert_eq!(preflight.owner, RouteOwner::Legacy);
        assert_eq!(
            preflight.captures.remainder,
            Some("future-compatibility-route")
        );

        assert_eq!(
            classify_route(
                HttpMethod::Get,
                "/v1/future-compatibility-route",
                LOOPBACK_INTERNAL
            ),
            RouteDecision::NotFound
        );
        assert_eq!(
            classify_route(
                HttpMethod::Options,
                "/v1/future-compatibility-route",
                LOOPBACK_PAIRED
            ),
            RouteDecision::NotFound
        );
        assert_eq!(
            classify_route(
                HttpMethod::Options,
                "/v1/future-compatibility-route",
                LAN_PAIRED
            ),
            RouteDecision::NotFound
        );
        assert_eq!(
            classify_route(HttpMethod::Options, "/api/unknown", LOOPBACK_INTERNAL),
            RouteDecision::NotFound
        );
    }

    #[test]
    fn negative_capability_allowlist_cannot_expose_execution_surfaces() {
        let forbidden = [
            ("/v1/agent", DeniedCapability::AgentExecution),
            ("/v1/agents/run", DeniedCapability::AgentExecution),
            ("/v1/workspace", DeniedCapability::WorkspaceAccess),
            ("/v1/workspaces/repo", DeniedCapability::WorkspaceAccess),
            ("/v1/tools", DeniedCapability::ToolExecution),
            ("/v1/tool/run", DeniedCapability::ToolExecution),
            ("/v1/tool_run_shell", DeniedCapability::ToolExecution),
            ("/v1/files", DeniedCapability::FileAccess),
            ("/v1/git/status", DeniedCapability::GitAccess),
            ("/v1/mcp", DeniedCapability::McpAccess),
            ("/v1/recipes/run", DeniedCapability::RecipeExecution),
        ];
        for input in [
            LOOPBACK_LEGACY,
            LOOPBACK_INTERNAL,
            LOOPBACK_PAIRED,
            LAN_PAIRED,
        ] {
            for method in [
                HttpMethod::Get,
                HttpMethod::Post,
                HttpMethod::Options,
                HttpMethod::Other,
            ] {
                for (path, capability) in forbidden {
                    assert_eq!(
                        classify_route(method, path, input),
                        RouteDecision::Denied(RouteDenial::ExplicitCapability(capability)),
                        "{method:?} {path} leaked for {input:?}"
                    );
                }
            }
        }

        // Segment boundaries must not swallow unrelated future route names.
        for near_match in ["/v1/toolbox", "/v1/agentsmith", "/v1/workspaceship"] {
            assert_eq!(denied_capability(near_match), None);
        }
    }

    /// The literal text a matcher is built from, for the table-level guard
    /// below. `*` stands in for a capture so the pinned strings read like the
    /// routes they describe.
    fn matcher_literal(matcher: PathMatcher) -> String {
        match matcher {
            PathMatcher::Exact(path) => path.to_string(),
            PathMatcher::NonEmptyRemainder { prefix } => format!("{prefix}*"),
            PathMatcher::SegmentWithSuffix { prefix, suffix } => format!("{prefix}*{suffix}"),
            PathMatcher::FirstSegmentWithRemainder { prefix } => format!("{prefix}*"),
            PathMatcher::Prefix(prefix) => format!("{prefix}*"),
        }
    }

    /// Guards the *route table itself*, which no behavioural test over today's
    /// routes can do.
    ///
    /// `negative_capability_allowlist_cannot_expose_execution_surfaces` proves
    /// that the eleven paths currently in `DENIED_SURFACES` are refused for every
    /// exposure, auth family and method. That is a test over a fixed list of
    /// paths, so it is silent about the failure mode that actually worries us:
    /// somebody *adds a new entry to [`ROUTES`]* for an agent, workspace, tool,
    /// file, git, MCP or recipe surface that `DENIED_SURFACES` was never written
    /// to name. Such a route would classify as `Allowed` and no existing
    /// assertion would notice.
    ///
    /// So this is deliberately a source/table-level test rather than a request
    /// one. It does three things:
    ///
    /// 1. Pins the table as a closed allowlist. Adding, removing or broadening a
    ///    route means editing the literals below, which is the review checkpoint.
    /// 2. Refuses any route literal that names an execution surface, whatever
    ///    `DENIED_SURFACES` happens to cover on the day the route is added.
    /// 3. Refuses any matcher broad enough to swallow a currently-denied path,
    ///    so widening (say) `/v1/artifacts/*` into `/v1/*` cannot quietly put an
    ///    execution surface back inside the allowlist.
    #[test]
    fn route_allowlist_never_exposes_agent_or_workspace_tools() {
        const ALLOWED_ROUTE_PATHS: &[&str] = &[
            "/health",
            "/v1/models",
            "/v1/chat/completions",
            "/v1/embeddings",
            "/v1/knowledge/query",
            "/v1/artifacts/*",
            "/v1/workflows/runs/*",
            "/v1/local-apps/*/run",
            "/local-apps/*",
            "/v1/responses",
            "/v1/messages",
            "/api/tags",
            "/api/chat",
            "/v1/models/download",
            "/v1/models/load",
            "/v1/models/unload",
            "/v1/models/status",
            "/v1/models/delete",
            "/v1/requests/cancel",
            // Reviewed against DENIED_SURFACES: the K19 introspection endpoint
            // reads no state at all — its body is a pure function of the built
            // binary — so it exposes no agent, workspace, tool, file, git, MCP
            // or recipe surface. It publishes the fact that those are denied.
            "/v1/contract",
            "/v1/*",
        ];
        let actual: Vec<String> = ROUTES
            .iter()
            .map(|route| matcher_literal(route.path))
            .collect();
        assert_eq!(
            actual, ALLOWED_ROUTE_PATHS,
            "the route table changed: every addition must be reviewed against the \
             negative capabilities in DENIED_SURFACES before this list is updated"
        );

        // Substrings, not segments: a hypothetical `/v1/agent-run` or
        // `/v1/exec` must trip this even though `DENIED_SURFACES` matches on
        // segment boundaries and would let both through.
        const EXECUTION_SURFACE_WORDS: &[&str] = &[
            "agent",
            "workspace",
            "tool",
            "file",
            "git",
            "mcp",
            "recipe",
            "exec",
            "shell",
            "bash",
            "command",
            "terminal",
            "process",
            "spawn",
            "script",
            "eval",
            "sandbox",
        ];
        for route in ROUTES {
            let literal = matcher_literal(route.path).to_ascii_lowercase();
            for word in EXECUTION_SURFACE_WORDS {
                assert!(
                    !literal.contains(word),
                    "route {:?} ({literal}) names the execution surface {word:?}; \
                     the HTTP allowlist must never carry one",
                    route.id
                );
            }
        }

        // No matcher may reach a denied path. The `/v1/*` OPTIONS fallback is
        // exempt because the denial check runs before it and is method-scoped —
        // `negative_capability_allowlist_cannot_expose_execution_surfaces`
        // proves that for every method.
        for surface in DENIED_SURFACES {
            let probes = match surface.path {
                DeniedPathMatcher::ExactOrDescendant(root) => {
                    vec![root.to_string(), format!("{root}/probe")]
                }
                DeniedPathMatcher::Prefix(prefix) => {
                    vec![prefix.to_string(), format!("{prefix}probe")]
                }
            };
            for probe in probes {
                for route in ROUTES
                    .iter()
                    .filter(|route| route.family != RouteFamily::LegacyPreflight)
                {
                    assert!(
                        route.path.captures(&probe).is_none(),
                        "route {:?} matches the denied path {probe}",
                        route.id
                    );
                }
            }
        }
    }

    #[test]
    fn registry_ids_are_unique_and_every_non_fallback_route_has_real_methods() {
        let mut ids = std::collections::HashSet::new();
        for route in ROUTES {
            assert!(ids.insert(route.id), "duplicate route id: {:?}", route.id);
            match route.family {
                RouteFamily::LegacyHost | RouteFamily::LegacyPreflight => {
                    assert!(!route.methods.legacy.is_empty());
                    assert!(route.methods.m3.is_empty());
                }
                RouteFamily::Shared => {
                    assert!(!route.methods.legacy.is_empty());
                    assert!(!route.methods.m3.is_empty());
                }
                RouteFamily::M3Compatibility | RouteFamily::M3Lifecycle => {
                    assert!(route.methods.legacy.is_empty());
                    assert!(!route.methods.m3.is_empty());
                }
            }
        }
        assert_eq!(ids.len(), 21);
    }

    #[test]
    fn hyper_methods_convert_without_accepting_unregistered_verbs() {
        assert_eq!(HttpMethod::from(&Method::GET), HttpMethod::Get);
        assert_eq!(HttpMethod::from(&Method::POST), HttpMethod::Post);
        assert_eq!(HttpMethod::from(&Method::OPTIONS), HttpMethod::Options);
        assert_eq!(HttpMethod::from(&Method::PUT), HttpMethod::Other);
        assert!(matches!(
            classify_request(&Method::PUT, "/v1/models", LOOPBACK_INTERNAL),
            RouteDecision::MethodNotAllowed {
                route: RouteId::Models,
                owner: RouteOwner::Legacy,
                ..
            }
        ));
    }
}
