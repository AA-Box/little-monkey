//! Post-authentication model discovery and resolution for the unified HTTP server.
//!
//! D1 in `docs/agent-os-roadmap.md` collapses the legacy proxy and the M3
//! compatibility listener.  The two implementations currently disagree about
//! model identity: the legacy proxy guesses that every unknown non-empty id is
//! an Ollama tag, while M3 only trusts its installed-model registry or an
//! explicit runtime header.  More importantly, discovery must not happen until
//! authentication, scope checks, and rate limiting have succeeded; otherwise an
//! invalid bearer token can turn one request into a set of outbound probes and
//! distinguish installed models by `401` versus `404`.
//!
//! This module is deliberately AppHandle-free.  Callers inject sources for M3,
//! Ollama, and providers, then pass the result of their authentication gate to
//! [`UnifiedModelCatalog`].  Denied and rate-limited requests return before a
//! source future is polled.  That ordering is part of the API contract and is
//! covered by counter-based tests below.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// A target class understood by both HTTP surfaces.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogBackend {
    ManagedLocal,
    Ollama,
    Mlx,
    CloudProvider,
}

impl CatalogBackend {
    fn deterministic_rank(self) -> u8 {
        match self {
            Self::ManagedLocal => 0,
            Self::Mlx => 1,
            Self::Ollama => 2,
            Self::CloudProvider => 3,
        }
    }
}

/// One model as reported by an injected source.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CatalogModel {
    /// Public id accepted on the compatibility routes. Provider models are
    /// already prefixed (for example `openai/gpt-4o`) by their source.
    pub model_id: String,
    /// Stable source id used for diagnostics after authentication.
    pub source_id: String,
    /// Runtime id used by `x-little-monkey-runtime-id`, when the source is a
    /// concrete runtime rather than a provider catalog.
    pub runtime_id: Option<String>,
    pub backend: CatalogBackend,
    /// Legacy `/v1/models.data[].owned_by` compatibility value.
    pub owned_by: String,
}

/// Typed handoff from exact catalog resolution into shared request dispatch.
/// Consumers must route this value directly instead of guessing from the raw
/// request model id a second time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogDispatchTarget {
    Runtime {
        model_id: String,
        source_id: String,
        runtime_id: String,
        backend: CatalogBackend,
    },
    Provider {
        /// Public source-prefixed identity used on compatibility routes.
        model_id: String,
        provider_id: String,
        /// Identity sent to the provider after removing exactly one trusted
        /// source prefix.
        provider_model_id: String,
    },
}

impl CatalogModel {
    pub fn validate(&self) -> Result<(), CatalogError> {
        for (label, value) in [
            ("model id", self.model_id.as_str()),
            ("source id", self.source_id.as_str()),
            ("owned_by", self.owned_by.as_str()),
        ] {
            if value.trim().is_empty()
                || value.trim() != value
                || value.len() > 512
                || value.chars().any(char::is_control)
            {
                return Err(CatalogError::InvalidSource(format!(
                    "{label} must be 1..=512 trimmed non-control characters"
                )));
            }
        }
        if self.runtime_id.as_deref().is_some_and(|value| {
            value.trim().is_empty()
                || value.trim() != value
                || value.len() > 256
                || value.chars().any(char::is_control)
        }) {
            return Err(CatalogError::InvalidSource(
                "runtime id must be 1..=256 trimmed non-control characters when present"
                    .to_string(),
            ));
        }
        Ok(())
    }

    pub fn into_dispatch_target(self) -> Result<CatalogDispatchTarget, CatalogError> {
        self.validate()?;
        let Self {
            model_id,
            source_id,
            runtime_id,
            backend,
            owned_by: _,
        } = self;
        if backend == CatalogBackend::CloudProvider {
            if runtime_id.is_some() {
                return Err(CatalogError::InvalidSource(
                    "cloud provider models cannot declare runtime ids".to_string(),
                ));
            }
            let provider_model_id = model_id
                .strip_prefix(&source_id)
                .and_then(|rest| rest.strip_prefix('/'))
                .filter(|suffix| !suffix.is_empty() && !suffix.starts_with('/'))
                .ok_or_else(|| {
                    CatalogError::InvalidSource(format!(
                        "provider model ids from {source_id} must be source-prefixed"
                    ))
                })?
                .to_string();
            Ok(CatalogDispatchTarget::Provider {
                model_id,
                provider_id: source_id,
                provider_model_id,
            })
        } else {
            let runtime_id = runtime_id.ok_or_else(|| {
                CatalogError::InvalidSource(format!(
                    "non-provider model source {source_id} must declare a runtime id"
                ))
            })?;
            Ok(CatalogDispatchTarget::Runtime {
                model_id,
                source_id,
                runtime_id,
                backend,
            })
        }
    }
}

/// A deliberately non-extensible public error returned by a model source.
///
/// Source implementations must log private diagnostics at their own trust
/// boundary. The catalog and HTTP response only receive this safe category,
/// so provider bodies, URLs, credentials, and filesystem paths cannot leak
/// through `Display` by accident.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogSourceError {
    Unavailable,
    PermissionDenied,
    TimedOut,
    InvalidResponse,
    Overloaded,
}

/// Whether failure of one source invalidates the whole catalog operation.
/// Durable kernel-owned inventories default to `Required`; independently
/// configured network providers may opt into `OmitUnavailable` so one
/// missing key or offline vendor cannot hide healthy local models.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogSourceFailurePolicy {
    Required,
    OmitUnavailable,
}

impl fmt::Display for CatalogSourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "unavailable",
            Self::PermissionDenied => "permission denied",
            Self::TimedOut => "timed out",
            Self::InvalidResponse => "returned an invalid response",
            Self::Overloaded => "overloaded",
        })
    }
}

impl std::error::Error for CatalogSourceError {}

/// Request lifetime shared with source implementations. The absolute deadline
/// applies to the complete union operation rather than being reset per source.
#[derive(Clone, Debug)]
pub struct CatalogRequestContext {
    cancellation: CancellationToken,
    deadline: Instant,
}

impl CatalogRequestContext {
    pub fn new(cancellation: CancellationToken, deadline: Instant) -> Self {
        Self {
            cancellation,
            deadline,
        }
    }

    pub fn with_timeout(cancellation: CancellationToken, timeout: Duration) -> Self {
        Self::new(cancellation, Instant::now() + timeout)
    }

    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    /// Verifies that source construction or polling may still begin.
    ///
    /// Public orchestration layers call this after authentication but before
    /// constructing dynamic sources. The catalog repeats it before every
    /// source poll so a single absolute request lifetime governs the union.
    pub fn ensure_active(&self) -> Result<(), CatalogError> {
        if self.cancellation.is_cancelled() {
            Err(CatalogError::Cancelled)
        } else if Instant::now() >= self.deadline {
            Err(CatalogError::DeadlineExceeded)
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogLimitKind {
    Sources,
    ModelsPerSource,
    TotalModels,
}

/// Hard bounds enforced even when an injected source ignores the hint passed
/// to `list_models`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CatalogLimits {
    pub max_sources: usize,
    pub max_models_per_source: usize,
    pub max_models_total: usize,
}

impl Default for CatalogLimits {
    fn default() -> Self {
        Self {
            max_sources: 64,
            max_models_per_source: 10_000,
            max_models_total: 25_000,
        }
    }
}

impl CatalogLimits {
    fn validate(self) -> Result<Self, CatalogError> {
        if self.max_sources == 0 || self.max_models_per_source == 0 || self.max_models_total == 0 {
            return Err(CatalogError::InvalidRequest(
                "catalog limits must all be greater than zero".to_string(),
            ));
        }
        Ok(self)
    }
}

/// Source boundary for model discovery. Implementations may read local hub
/// state or perform a live HTTP request; the catalog never assumes which.
#[async_trait]
pub trait ModelCatalogSource: Send + Sync {
    fn source_id(&self) -> &str;
    fn backend(&self) -> CatalogBackend;
    /// Stable runtime selected by `x-little-monkey-runtime-id`, when this
    /// source represents one concrete runtime. Provider catalogs return
    /// `None`. Keeping this outside `list_models` lets an explicit override
    /// select its source before any unrelated source is probed.
    fn runtime_id(&self) -> Option<&str>;
    fn failure_policy(&self) -> CatalogSourceFailurePolicy {
        CatalogSourceFailurePolicy::Required
    }
    /// `max_models` is a fetch-side bound; the catalog independently enforces
    /// the same bound after the future resolves. For [`CatalogSourceQuery::Resolve`],
    /// implementations must select the exact requested model before applying
    /// this bound so list caps cannot manufacture a false not-found result.
    async fn list_models(
        &self,
        context: &CatalogRequestContext,
        query: CatalogSourceQuery<'_>,
        max_models: usize,
    ) -> Result<Vec<CatalogModel>, CatalogSourceError>;
}

/// Operation intent passed to an adapter after authorization. Sources that
/// combine durable and live inventories can keep partial data for listings,
/// while refusing to manufacture a false 404 when an exact live lookup could
/// not be completed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogSourceQuery<'a> {
    List,
    Resolve { model_id: &'a str },
}

impl<'a> CatalogSourceQuery<'a> {
    pub fn model_id(self) -> Option<&'a str> {
        match self {
            Self::List => None,
            Self::Resolve { model_id } => Some(model_id),
        }
    }

    pub fn matches_model_id(self, model_id: &str) -> bool {
        self.model_id()
            .is_none_or(|requested_model_id| requested_model_id == model_id)
    }
}

/// Result of the authentication/scope/rate-limit gate.  The catalog accepts
/// the whole result rather than only an `Authorized` value so it owns the
/// security-critical ordering: failures are returned before source iteration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogAuthorization {
    Authorized {
        allowed_backends: BTreeSet<CatalogBackend>,
    },
    Unauthorized,
    Forbidden,
    RateLimited {
        retry_after_ms: u64,
    },
}

impl CatalogAuthorization {
    pub fn all_local() -> Self {
        Self::Authorized {
            allowed_backends: BTreeSet::from([
                CatalogBackend::ManagedLocal,
                CatalogBackend::Mlx,
                CatalogBackend::Ollama,
            ]),
        }
    }

    fn allowed_backends(self) -> Result<BTreeSet<CatalogBackend>, CatalogError> {
        match self {
            Self::Authorized { allowed_backends } => Ok(allowed_backends),
            Self::Unauthorized => Err(CatalogError::Unauthorized),
            Self::Forbidden => Err(CatalogError::Forbidden),
            Self::RateLimited { retry_after_ms } => {
                Err(CatalogError::RateLimited { retry_after_ms })
            }
        }
    }
}

/// Server-side feature policy. Authorization is intersected with this set;
/// neither routing nor a source implementation can widen it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogPolicy {
    pub enabled_backends: BTreeSet<CatalogBackend>,
}

impl CatalogPolicy {
    pub fn all() -> Self {
        Self {
            enabled_backends: BTreeSet::from([
                CatalogBackend::ManagedLocal,
                CatalogBackend::Mlx,
                CatalogBackend::Ollama,
                CatalogBackend::CloudProvider,
            ]),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogError {
    Unauthorized,
    Forbidden,
    RateLimited {
        retry_after_ms: u64,
    },
    Cancelled,
    DeadlineExceeded,
    LimitExceeded {
        kind: CatalogLimitKind,
        limit: usize,
    },
    InvalidRequest(String),
    InvalidSource(String),
    SourceUnavailable {
        source_id: String,
        failure: CatalogSourceError,
    },
    NotFound {
        model_id: String,
        /// Deterministic source ids whose inventories were successfully
        /// inspected for this exact lookup. This is safe to expose because
        /// resolution only runs after authorization and policy intersection.
        searched_sources: Vec<String>,
    },
    Conflict(String),
}

impl fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unauthorized => formatter.write_str("authentication failed"),
            Self::Forbidden => formatter.write_str("the token is not allowed to discover models"),
            Self::RateLimited { retry_after_ms } => {
                write!(
                    formatter,
                    "model discovery is rate limited for {retry_after_ms} ms"
                )
            }
            Self::Cancelled => formatter.write_str("model discovery was cancelled"),
            Self::DeadlineExceeded => formatter.write_str("model discovery deadline exceeded"),
            Self::LimitExceeded { kind, limit } => {
                let resource = match kind {
                    CatalogLimitKind::Sources => "sources",
                    CatalogLimitKind::ModelsPerSource => "models from one source",
                    CatalogLimitKind::TotalModels => "total model observations",
                };
                write!(
                    formatter,
                    "catalog limit exceeded for {resource} (maximum {limit})"
                )
            }
            Self::InvalidRequest(detail) | Self::InvalidSource(detail) | Self::Conflict(detail) => {
                formatter.write_str(detail)
            }
            Self::SourceUnavailable { source_id, failure } => {
                write!(
                    formatter,
                    "model source {source_id} is unavailable: {failure}"
                )
            }
            Self::NotFound {
                model_id,
                searched_sources,
            } => {
                if searched_sources.is_empty() {
                    write!(
                        formatter,
                        "model {model_id} was not found; no eligible model sources were available"
                    )
                } else {
                    write!(
                        formatter,
                        "model {model_id} was not found after checking {}",
                        searched_sources.join(", ")
                    )
                }
            }
        }
    }
}

impl std::error::Error for CatalogError {}

/// Union catalog shared by the merged listener and headless CLI.
pub struct UnifiedModelCatalog {
    sources: Vec<RegisteredCatalogSource>,
    limits: CatalogLimits,
}

/// Source declarations are snapshotted once. Output provenance is stamped
/// from this registration, never copied from an untrusted inventory row.
struct RegisteredCatalogSource {
    source: Arc<dyn ModelCatalogSource>,
    source_id: String,
    backend: CatalogBackend,
    runtime_id: Option<String>,
    failure_policy: CatalogSourceFailurePolicy,
}

struct CollectedCatalog {
    models: Vec<CatalogModel>,
    searched_sources: Vec<String>,
}

impl UnifiedModelCatalog {
    pub fn new(sources: Vec<Arc<dyn ModelCatalogSource>>) -> Result<Self, CatalogError> {
        Self::with_limits(sources, CatalogLimits::default())
    }

    pub fn with_limits(
        sources: Vec<Arc<dyn ModelCatalogSource>>,
        limits: CatalogLimits,
    ) -> Result<Self, CatalogError> {
        let limits = limits.validate()?;
        if sources.len() > limits.max_sources {
            return Err(CatalogError::LimitExceeded {
                kind: CatalogLimitKind::Sources,
                limit: limits.max_sources,
            });
        }

        let mut registrations = Vec::with_capacity(sources.len());
        let mut source_ids = BTreeSet::new();
        let mut runtime_ids = BTreeSet::new();
        for source in sources {
            let source_id = source.source_id().to_string();
            validate_source_identifier(&source_id, "source id", 256)?;
            if !source_ids.insert(source_id.clone()) {
                return Err(CatalogError::Conflict(format!(
                    "duplicate model source id {source_id}"
                )));
            }

            let backend = source.backend();
            let runtime_id = source.runtime_id().map(str::to_string);
            let failure_policy = source.failure_policy();
            if backend == CatalogBackend::CloudProvider && source_id.contains('/') {
                return Err(CatalogError::InvalidSource(
                    "cloud provider source ids cannot contain '/'".to_string(),
                ));
            }
            if let Some(runtime_id) = runtime_id.as_deref() {
                validate_source_identifier(runtime_id, "runtime id", 256)?;
                if !runtime_ids.insert(runtime_id.to_string()) {
                    return Err(CatalogError::Conflict(format!(
                        "duplicate declared runtime id {runtime_id}"
                    )));
                }
                if backend == CatalogBackend::CloudProvider {
                    return Err(CatalogError::InvalidSource(
                        "cloud provider sources cannot declare runtime ids".to_string(),
                    ));
                }
            } else if backend != CatalogBackend::CloudProvider {
                return Err(CatalogError::InvalidSource(format!(
                    "non-provider model source {source_id} must declare a runtime id"
                )));
            }

            registrations.push(RegisteredCatalogSource {
                source,
                source_id,
                backend,
                runtime_id,
                failure_policy,
            });
        }

        registrations.sort_by(|left, right| {
            left.backend
                .deterministic_rank()
                .cmp(&right.backend.deterministic_rank())
                .then_with(|| left.source_id.cmp(&right.source_id))
        });
        Ok(Self {
            sources: registrations,
            limits,
        })
    }

    pub fn limits(&self) -> CatalogLimits {
        self.limits
    }

    /// Lists the policy/auth intersection in deterministic order.
    pub async fn list(
        &self,
        authorization: CatalogAuthorization,
        policy: &CatalogPolicy,
        context: &CatalogRequestContext,
    ) -> Result<Vec<CatalogModel>, CatalogError> {
        let effective = effective_backends(authorization, policy)?;
        context.ensure_active()?;
        Ok(self
            .collect_effective(&effective, None, None, context)
            .await?
            .models)
    }

    /// Lists one explicitly selected runtime without polling unrelated
    /// sources. An unknown runtime is a post-authentication 404, matching
    /// exact model resolution and the historical M3 header contract.
    pub async fn list_for_runtime(
        &self,
        authorization: CatalogAuthorization,
        policy: &CatalogPolicy,
        runtime_id: &str,
        context: &CatalogRequestContext,
    ) -> Result<Vec<CatalogModel>, CatalogError> {
        let effective = effective_backends(authorization, policy)?;
        context.ensure_active()?;
        validate_source_identifier(runtime_id, "runtime id", 256)?;
        let eligible = self.sources.iter().any(|source| {
            source.runtime_id.as_deref() == Some(runtime_id) && effective.contains(&source.backend)
        });
        if !eligible {
            return Err(CatalogError::NotFound {
                model_id: format!("runtime:{runtime_id}"),
                searched_sources: Vec::new(),
            });
        }
        Ok(self
            .collect_effective(&effective, None, Some(runtime_id), context)
            .await?
            .models)
    }

    /// Resolves only exact identities. An unknown id is not guessed to be an
    /// Ollama tag; a `NotFound` is returned after every effective source has
    /// been inspected successfully.
    pub async fn resolve(
        &self,
        authorization: CatalogAuthorization,
        policy: &CatalogPolicy,
        model_id: &str,
        runtime_override: Option<&str>,
        context: &CatalogRequestContext,
    ) -> Result<CatalogModel, CatalogError> {
        // Authentication and policy intersection deliberately precede both
        // request validation and all source selection/probing.
        let effective = effective_backends(authorization, policy)?;
        context.ensure_active()?;
        validate_resolution_request(model_id, runtime_override)?;
        let collected = self
            .collect_effective(&effective, Some(model_id), runtime_override, context)
            .await?;

        let mut matches = collected.models.into_iter().filter(|model| {
            model.model_id == model_id
                && runtime_override
                    .is_none_or(|runtime_id| model.runtime_id.as_deref() == Some(runtime_id))
        });
        let Some(first) = matches.next() else {
            return Err(CatalogError::NotFound {
                model_id: model_id.to_string(),
                searched_sources: collected.searched_sources,
            });
        };
        if matches.next().is_some() {
            return Err(CatalogError::Conflict(format!(
                "model id {model_id} is ambiguous; provide x-little-monkey-runtime-id"
            )));
        }
        Ok(first)
    }

    async fn collect_effective(
        &self,
        effective: &BTreeSet<CatalogBackend>,
        lookup_model_id: Option<&str>,
        runtime_override: Option<&str>,
        context: &CatalogRequestContext,
    ) -> Result<CollectedCatalog, CatalogError> {
        if let Some(runtime_id) = runtime_override {
            if let Some(source) = self
                .sources
                .iter()
                .find(|source| source.runtime_id.as_deref() == Some(runtime_id))
            {
                if !effective.contains(&source.backend) {
                    // A known but disabled override is a policy denial, not a
                    // reason to probe every other runtime or synthesize a 404.
                    return Err(CatalogError::Forbidden);
                }
            }
        }

        let targeted_provider = if runtime_override.is_none() {
            lookup_model_id.and_then(|model_id| {
                self.sources.iter().find(|source| {
                    source.backend == CatalogBackend::CloudProvider
                        && provider_id_has_source_prefix(model_id, &source.source_id)
                })
            })
        } else {
            None
        };
        if targeted_provider.is_some_and(|provider| !effective.contains(&provider.backend)) {
            return Err(CatalogError::Forbidden);
        }
        let mut by_identity = BTreeMap::<(String, Option<String>), CatalogModel>::new();
        let mut searched_sources = Vec::new();
        let mut retained_optional_failure = None;
        let mut observations = 0usize;
        for source in &self.sources {
            if !effective.contains(&source.backend) {
                continue;
            }
            if runtime_override
                .is_some_and(|runtime_id| source.runtime_id.as_deref() != Some(runtime_id))
            {
                continue;
            }
            if targeted_provider.is_some_and(|provider| provider.source_id != source.source_id) {
                // A configured provider prefix owns its namespace. Exact
                // lookup does not probe unrelated local runtimes or providers.
                continue;
            }
            if lookup_model_id.is_some_and(|model_id| {
                source.backend == CatalogBackend::CloudProvider
                    && !provider_id_has_source_prefix(model_id, &source.source_id)
            }) {
                // Provider namespaces are exact and source-prefixed. A request
                // for another namespace cannot be satisfied by this source, so
                // do not probe it or let its optional outage mask a valid local
                // resolution.
                continue;
            }

            context.ensure_active()?;
            if observations >= self.limits.max_models_total {
                return Err(CatalogError::LimitExceeded {
                    kind: CatalogLimitKind::TotalModels,
                    limit: self.limits.max_models_total,
                });
            }
            let source_hint = self
                .limits
                .max_models_per_source
                .min(self.limits.max_models_total - observations);
            searched_sources.push(source.source_id.clone());
            let query = lookup_model_id.map_or(CatalogSourceQuery::List, |model_id| {
                CatalogSourceQuery::Resolve { model_id }
            });
            let source_future = source.source.list_models(context, query, source_hint);
            let models = tokio::select! {
                biased;
                _ = context.cancellation.cancelled() => return Err(CatalogError::Cancelled),
                result = tokio::time::timeout_at(context.deadline, source_future) => {
                    match result {
                        Ok(Ok(models)) => models,
                        Ok(Err(failure)) => {
                            // Cancellation/deadline always outrank an optional
                            // source policy. Otherwise a provider that notices
                            // the request lifetime first could turn cancellation
                            // into a successful partial list.
                            context.ensure_active()?;
                            if source.failure_policy
                                == CatalogSourceFailurePolicy::OmitUnavailable
                            {
                                // Broad discovery is explicitly best-effort.
                                // Exact resolution retains the first safe,
                                // deterministic failure but keeps looking: a
                                // later source may still prove the requested
                                // identity. If none does, the retained failure
                                // prevents manufacturing a false 404.
                                if lookup_model_id.is_some() {
                                    retained_optional_failure.get_or_insert_with(|| {
                                        CatalogError::SourceUnavailable {
                                            source_id: source.source_id.clone(),
                                            failure,
                                        }
                                    });
                                }
                                continue;
                            }
                            return Err(CatalogError::SourceUnavailable {
                                source_id: source.source_id.clone(),
                                failure,
                            });
                        }
                        Err(_) => return Err(CatalogError::DeadlineExceeded),
                    }
                }
            };
            context.ensure_active()?;
            if models.len() > self.limits.max_models_per_source {
                return Err(CatalogError::LimitExceeded {
                    kind: CatalogLimitKind::ModelsPerSource,
                    limit: self.limits.max_models_per_source,
                });
            }
            observations = observations.checked_add(models.len()).unwrap_or(usize::MAX);
            if observations > self.limits.max_models_total {
                return Err(CatalogError::LimitExceeded {
                    kind: CatalogLimitKind::TotalModels,
                    limit: self.limits.max_models_total,
                });
            }

            for reported in models {
                let model = stamp_model_provenance(source, reported)?;
                let key = (model.model_id.clone(), model.runtime_id.clone());
                if let Some(existing) = by_identity.get(&key) {
                    if existing == &model {
                        // Inventory adapters can legitimately observe one
                        // physical model more than once. Identical stamped
                        // observations merge deterministically.
                        continue;
                    }
                    return Err(CatalogError::Conflict(format!(
                        "model identity {} has incompatible provenance from {} and {}",
                        model.model_id, existing.source_id, model.source_id
                    )));
                }
                by_identity.insert(key, model);
            }
        }
        let mut output = by_identity.into_values().collect::<Vec<_>>();
        output.sort_by(|left, right| {
            left.backend
                .deterministic_rank()
                .cmp(&right.backend.deterministic_rank())
                .then_with(|| left.owned_by.cmp(&right.owned_by))
                .then_with(|| left.model_id.cmp(&right.model_id))
                .then_with(|| left.runtime_id.cmp(&right.runtime_id))
        });
        if lookup_model_id
            .is_some_and(|model_id| !output.iter().any(|model| model.model_id == model_id))
        {
            if let Some(error) = retained_optional_failure {
                return Err(error);
            }
        }
        Ok(CollectedCatalog {
            models: output,
            searched_sources,
        })
    }
}

fn effective_backends(
    authorization: CatalogAuthorization,
    policy: &CatalogPolicy,
) -> Result<BTreeSet<CatalogBackend>, CatalogError> {
    let allowed = authorization.allowed_backends()?;
    let effective = allowed
        .intersection(&policy.enabled_backends)
        .copied()
        .collect::<BTreeSet<_>>();
    if effective.is_empty() {
        Err(CatalogError::Forbidden)
    } else {
        Ok(effective)
    }
}

fn stamp_model_provenance(
    source: &RegisteredCatalogSource,
    reported: CatalogModel,
) -> Result<CatalogModel, CatalogError> {
    validate_source_identifier(&reported.model_id, "model id", 512)?;
    validate_source_identifier(&reported.owned_by, "owned_by", 512)?;

    if let Some(reported_runtime_id) = reported.runtime_id.as_deref() {
        match source.runtime_id.as_deref() {
            Some(declared_runtime_id) if reported_runtime_id == declared_runtime_id => {}
            Some(_) => {
                return Err(CatalogError::InvalidSource(format!(
                    "model source {} reported a runtime other than its declaration",
                    source.source_id
                )));
            }
            None => {
                return Err(CatalogError::InvalidSource(format!(
                    "model source {} cannot inject a runtime id",
                    source.source_id
                )));
            }
        }
    }

    if source.backend == CatalogBackend::CloudProvider
        && !provider_id_has_source_prefix(&reported.model_id, &source.source_id)
    {
        return Err(CatalogError::InvalidSource(format!(
            "provider model ids from {} must be source-prefixed",
            source.source_id
        )));
    }

    let stamped = CatalogModel {
        model_id: reported.model_id,
        source_id: source.source_id.clone(),
        runtime_id: source.runtime_id.clone(),
        backend: source.backend,
        owned_by: reported.owned_by,
    };
    stamped.validate()?;
    Ok(stamped)
}

fn provider_id_has_source_prefix(model_id: &str, source_id: &str) -> bool {
    model_id
        .strip_prefix(source_id)
        .and_then(|rest| rest.strip_prefix('/'))
        .is_some_and(|suffix| !suffix.is_empty() && !suffix.starts_with('/'))
}

fn validate_source_identifier(value: &str, label: &str, max: usize) -> Result<(), CatalogError> {
    if value.trim().is_empty()
        || value.trim() != value
        || value.len() > max
        || value.chars().any(char::is_control)
    {
        Err(CatalogError::InvalidSource(format!(
            "{label} must be 1..={max} trimmed non-control characters"
        )))
    } else {
        Ok(())
    }
}

fn validate_lookup_value(value: &str, label: &str, max: usize) -> Result<(), CatalogError> {
    if value.trim().is_empty()
        || value.trim() != value
        || value.len() > max
        || value.chars().any(char::is_control)
    {
        Err(CatalogError::InvalidRequest(format!(
            "{label} must be 1..={max} trimmed non-control characters"
        )))
    } else {
        Ok(())
    }
}

/// Pure validation shared with [`HttpModelService`](crate::http_model_service::HttpModelService)
/// so malformed authorized requests fail before dynamic source construction.
pub(crate) fn validate_resolution_request(
    model_id: &str,
    runtime_override: Option<&str>,
) -> Result<(), CatalogError> {
    validate_lookup_value(model_id, "model id", 512)?;
    if let Some(runtime_id) = runtime_override {
        validate_lookup_value(runtime_id, "runtime override", 256)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone)]
    enum FixtureBehavior {
        Ready(Result<Vec<CatalogModel>, CatalogSourceError>),
        Delayed {
            delay: Duration,
            result: Result<Vec<CatalogModel>, CatalogSourceError>,
        },
    }

    struct FixtureSource {
        id: String,
        backend: CatalogBackend,
        runtime_id: Option<String>,
        calls: Arc<AtomicUsize>,
        last_limit: Arc<AtomicUsize>,
        failure_policy: CatalogSourceFailurePolicy,
        behavior: FixtureBehavior,
    }

    struct ExactQueryFixtureSource {
        calls: Arc<AtomicUsize>,
        models: Vec<CatalogModel>,
    }

    #[async_trait]
    impl ModelCatalogSource for ExactQueryFixtureSource {
        fn source_id(&self) -> &str {
            "managed"
        }

        fn backend(&self) -> CatalogBackend {
            CatalogBackend::ManagedLocal
        }

        fn runtime_id(&self) -> Option<&str> {
            Some("runtime-a")
        }

        async fn list_models(
            &self,
            _context: &CatalogRequestContext,
            query: CatalogSourceQuery<'_>,
            max_models: usize,
        ) -> Result<Vec<CatalogModel>, CatalogSourceError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self
                .models
                .iter()
                .filter(|model| query.matches_model_id(&model.model_id))
                .take(max_models)
                .cloned()
                .collect())
        }
    }

    #[async_trait]
    impl ModelCatalogSource for FixtureSource {
        fn source_id(&self) -> &str {
            &self.id
        }

        fn backend(&self) -> CatalogBackend {
            self.backend
        }

        fn runtime_id(&self) -> Option<&str> {
            self.runtime_id.as_deref()
        }

        fn failure_policy(&self) -> CatalogSourceFailurePolicy {
            self.failure_policy
        }

        async fn list_models(
            &self,
            _context: &CatalogRequestContext,
            _query: CatalogSourceQuery<'_>,
            max_models: usize,
        ) -> Result<Vec<CatalogModel>, CatalogSourceError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.last_limit.store(max_models, Ordering::SeqCst);
            match &self.behavior {
                FixtureBehavior::Ready(result) => result.clone(),
                FixtureBehavior::Delayed { delay, result } => {
                    tokio::time::sleep(*delay).await;
                    result.clone()
                }
            }
        }
    }

    fn source(
        id: &str,
        backend: CatalogBackend,
        runtime_id: Option<&str>,
        calls: Arc<AtomicUsize>,
        models: Vec<(&str, Option<&str>, &str)>,
    ) -> Arc<dyn ModelCatalogSource> {
        Arc::new(FixtureSource {
            id: id.to_string(),
            backend,
            runtime_id: runtime_id.map(str::to_string),
            calls,
            last_limit: Arc::new(AtomicUsize::new(0)),
            failure_policy: CatalogSourceFailurePolicy::Required,
            behavior: FixtureBehavior::Ready(Ok(models
                .into_iter()
                .map(|(model_id, runtime_id, owned_by)| CatalogModel {
                    model_id: model_id.to_string(),
                    source_id: id.to_string(),
                    runtime_id: runtime_id.map(str::to_string),
                    backend,
                    owned_by: owned_by.to_string(),
                })
                .collect())),
        })
    }

    fn delayed_source(
        id: &str,
        backend: CatalogBackend,
        runtime_id: Option<&str>,
        calls: Arc<AtomicUsize>,
        delay: Duration,
    ) -> Arc<dyn ModelCatalogSource> {
        Arc::new(FixtureSource {
            id: id.to_string(),
            backend,
            runtime_id: runtime_id.map(str::to_string),
            calls,
            last_limit: Arc::new(AtomicUsize::new(0)),
            failure_policy: CatalogSourceFailurePolicy::Required,
            behavior: FixtureBehavior::Delayed {
                delay,
                result: Ok(Vec::new()),
            },
        })
    }

    fn failing_source(
        id: &str,
        backend: CatalogBackend,
        runtime_id: Option<&str>,
        calls: Arc<AtomicUsize>,
        failure: CatalogSourceError,
    ) -> Arc<dyn ModelCatalogSource> {
        Arc::new(FixtureSource {
            id: id.to_string(),
            backend,
            runtime_id: runtime_id.map(str::to_string),
            calls,
            last_limit: Arc::new(AtomicUsize::new(0)),
            failure_policy: CatalogSourceFailurePolicy::Required,
            behavior: FixtureBehavior::Ready(Err(failure)),
        })
    }

    fn optional_failing_source(
        id: &str,
        backend: CatalogBackend,
        runtime_id: Option<&str>,
        calls: Arc<AtomicUsize>,
        failure: CatalogSourceError,
    ) -> Arc<dyn ModelCatalogSource> {
        Arc::new(FixtureSource {
            id: id.to_string(),
            backend,
            runtime_id: runtime_id.map(str::to_string),
            calls,
            last_limit: Arc::new(AtomicUsize::new(0)),
            failure_policy: CatalogSourceFailurePolicy::OmitUnavailable,
            behavior: FixtureBehavior::Ready(Err(failure)),
        })
    }

    fn authorized_all() -> CatalogAuthorization {
        CatalogAuthorization::Authorized {
            allowed_backends: CatalogPolicy::all().enabled_backends,
        }
    }

    fn active_context() -> CatalogRequestContext {
        CatalogRequestContext::with_timeout(CancellationToken::new(), Duration::from_secs(5))
    }

    #[tokio::test]
    async fn authentication_and_rate_limit_failures_probe_nothing() {
        let calls = Arc::new(AtomicUsize::new(0));
        let catalog = UnifiedModelCatalog::new(vec![source(
            "managed",
            CatalogBackend::ManagedLocal,
            Some("runtime-a"),
            calls.clone(),
            vec![("private-model", Some("runtime-a"), "local")],
        )])
        .unwrap();
        let context = active_context();

        for authorization in [
            CatalogAuthorization::Unauthorized,
            CatalogAuthorization::Forbidden,
            CatalogAuthorization::RateLimited {
                retry_after_ms: 1_000,
            },
        ] {
            let error = catalog
                .resolve(
                    authorization,
                    &CatalogPolicy::all(),
                    "private-model",
                    None,
                    &context,
                )
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                CatalogError::Unauthorized
                    | CatalogError::Forbidden
                    | CatalogError::RateLimited { .. }
            ));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn invalid_and_duplicate_declared_runtime_ids_are_rejected() {
        let invalid = UnifiedModelCatalog::new(vec![source(
            "managed",
            CatalogBackend::ManagedLocal,
            Some("runtime\nsecret"),
            Arc::new(AtomicUsize::new(0)),
            Vec::new(),
        )])
        .err()
        .unwrap();
        assert!(matches!(invalid, CatalogError::InvalidSource(_)));

        let duplicate = UnifiedModelCatalog::new(vec![
            source(
                "managed-a",
                CatalogBackend::ManagedLocal,
                Some("same-runtime"),
                Arc::new(AtomicUsize::new(0)),
                Vec::new(),
            ),
            source(
                "managed-b",
                CatalogBackend::Mlx,
                Some("same-runtime"),
                Arc::new(AtomicUsize::new(0)),
                Vec::new(),
            ),
        ])
        .err()
        .unwrap();
        assert!(
            matches!(duplicate, CatalogError::Conflict(detail) if detail.contains("duplicate declared runtime id"))
        );
    }

    #[test]
    fn every_registered_source_has_an_unambiguous_dispatch_namespace() {
        let calls = Arc::new(AtomicUsize::new(0));
        let missing_runtime = UnifiedModelCatalog::new(vec![source(
            "local",
            CatalogBackend::ManagedLocal,
            None,
            calls.clone(),
            Vec::new(),
        )])
        .err()
        .unwrap();
        assert!(
            matches!(missing_runtime, CatalogError::InvalidSource(detail) if detail.contains("must declare a runtime id"))
        );

        let nested_provider = UnifiedModelCatalog::new(vec![source(
            "provider/nested",
            CatalogBackend::CloudProvider,
            None,
            calls.clone(),
            Vec::new(),
        )])
        .err()
        .unwrap();
        assert!(
            matches!(nested_provider, CatalogError::InvalidSource(detail) if detail.contains("cannot contain '/'"))
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn resolved_models_convert_to_typed_dispatch_targets() {
        let runtime = CatalogModel {
            model_id: "qwen".to_string(),
            source_id: "managed".to_string(),
            runtime_id: Some("runtime-a".to_string()),
            backend: CatalogBackend::ManagedLocal,
            owned_by: "local".to_string(),
        }
        .into_dispatch_target()
        .unwrap();
        assert_eq!(
            runtime,
            CatalogDispatchTarget::Runtime {
                model_id: "qwen".to_string(),
                source_id: "managed".to_string(),
                runtime_id: "runtime-a".to_string(),
                backend: CatalogBackend::ManagedLocal,
            }
        );

        let provider = CatalogModel {
            model_id: "openai/gpt-4o".to_string(),
            source_id: "openai".to_string(),
            runtime_id: None,
            backend: CatalogBackend::CloudProvider,
            owned_by: "openai".to_string(),
        }
        .into_dispatch_target()
        .unwrap();
        assert_eq!(
            provider,
            CatalogDispatchTarget::Provider {
                model_id: "openai/gpt-4o".to_string(),
                provider_id: "openai".to_string(),
                provider_model_id: "gpt-4o".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn source_without_runtime_cannot_inject_one() {
        let calls = Arc::new(AtomicUsize::new(0));
        let catalog = UnifiedModelCatalog::new(vec![source(
            "openai",
            CatalogBackend::CloudProvider,
            None,
            calls.clone(),
            vec![("openai/gpt-4o", Some("injected-runtime"), "openai")],
        )])
        .unwrap();
        let error = catalog
            .list(authorized_all(), &CatalogPolicy::all(), &active_context())
            .await
            .unwrap_err();
        assert!(
            matches!(error, CatalogError::InvalidSource(detail) if detail.contains("cannot inject"))
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn policy_and_scope_intersect_before_sources_are_polled() {
        let local_calls = Arc::new(AtomicUsize::new(0));
        let provider_calls = Arc::new(AtomicUsize::new(0));
        let catalog = UnifiedModelCatalog::new(vec![
            source(
                "local",
                CatalogBackend::ManagedLocal,
                Some("local"),
                local_calls.clone(),
                vec![("local-model", Some("local"), "local")],
            ),
            source(
                "openai",
                CatalogBackend::CloudProvider,
                None,
                provider_calls.clone(),
                vec![("openai/gpt", None, "openai")],
            ),
        ])
        .unwrap();
        let authorization = CatalogAuthorization::Authorized {
            allowed_backends: BTreeSet::from([CatalogBackend::ManagedLocal]),
        };
        let listed = catalog
            .list(authorization, &CatalogPolicy::all(), &active_context())
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(local_calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn empty_policy_intersection_is_forbidden_without_probes() {
        let calls = Arc::new(AtomicUsize::new(0));
        let catalog = UnifiedModelCatalog::new(vec![source(
            "managed",
            CatalogBackend::ManagedLocal,
            Some("runtime-a"),
            calls.clone(),
            Vec::new(),
        )])
        .unwrap();
        let authorization = CatalogAuthorization::Authorized {
            allowed_backends: BTreeSet::from([CatalogBackend::ManagedLocal]),
        };
        let policy = CatalogPolicy {
            enabled_backends: BTreeSet::from([CatalogBackend::CloudProvider]),
        };
        let error = catalog
            .list(authorization, &policy, &active_context())
            .await
            .unwrap_err();
        assert_eq!(error, CatalogError::Forbidden);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn disabled_runtime_override_is_forbidden_without_any_probe() {
        let local_calls = Arc::new(AtomicUsize::new(0));
        let mlx_calls = Arc::new(AtomicUsize::new(0));
        let catalog = UnifiedModelCatalog::new(vec![
            source(
                "managed",
                CatalogBackend::ManagedLocal,
                Some("runtime-a"),
                local_calls.clone(),
                vec![("shared", Some("runtime-a"), "local")],
            ),
            source(
                "mlx",
                CatalogBackend::Mlx,
                Some("runtime-b"),
                mlx_calls.clone(),
                vec![("shared", Some("runtime-b"), "mlx")],
            ),
        ])
        .unwrap();
        let authorization = CatalogAuthorization::Authorized {
            allowed_backends: BTreeSet::from([CatalogBackend::ManagedLocal]),
        };
        let error = catalog
            .resolve(
                authorization,
                &CatalogPolicy::all(),
                "shared",
                Some("runtime-b"),
                &active_context(),
            )
            .await
            .unwrap_err();
        assert_eq!(error, CatalogError::Forbidden);
        assert_eq!(local_calls.load(Ordering::SeqCst), 0);
        assert_eq!(mlx_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn listing_is_a_deterministic_union_with_legacy_provenance() {
        let catalog = UnifiedModelCatalog::new(vec![
            source(
                "openai",
                CatalogBackend::CloudProvider,
                None,
                Arc::new(AtomicUsize::new(0)),
                vec![("openai/gpt-4o", None, "openai")],
            ),
            source(
                "ollama",
                CatalogBackend::Ollama,
                Some("ollama"),
                Arc::new(AtomicUsize::new(0)),
                vec![("llama3:latest", Some("ollama"), "ollama")],
            ),
            source(
                "managed",
                CatalogBackend::ManagedLocal,
                Some("managed"),
                Arc::new(AtomicUsize::new(0)),
                vec![("qwen", Some("managed"), "local")],
            ),
        ])
        .unwrap();
        let listed = catalog
            .list(authorized_all(), &CatalogPolicy::all(), &active_context())
            .await
            .unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|model| (model.model_id.as_str(), model.owned_by.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("qwen", "local"),
                ("llama3:latest", "ollama"),
                ("openai/gpt-4o", "openai"),
            ]
        );
    }

    #[tokio::test]
    async fn expected_duplicate_observations_merge_after_catalog_stamps_provenance() {
        let calls = Arc::new(AtomicUsize::new(0));
        let reported = vec![
            CatalogModel {
                model_id: "qwen".to_string(),
                source_id: "spoofed-source".to_string(),
                runtime_id: None,
                backend: CatalogBackend::CloudProvider,
                owned_by: "local".to_string(),
            },
            CatalogModel {
                model_id: "qwen".to_string(),
                source_id: "another-spoof".to_string(),
                runtime_id: Some("runtime-a".to_string()),
                backend: CatalogBackend::Ollama,
                owned_by: "local".to_string(),
            },
        ];
        let catalog = UnifiedModelCatalog::new(vec![Arc::new(FixtureSource {
            id: "managed".to_string(),
            backend: CatalogBackend::ManagedLocal,
            runtime_id: Some("runtime-a".to_string()),
            calls,
            last_limit: Arc::new(AtomicUsize::new(0)),
            failure_policy: CatalogSourceFailurePolicy::Required,
            behavior: FixtureBehavior::Ready(Ok(reported)),
        })])
        .unwrap();
        let listed = catalog
            .list(authorized_all(), &CatalogPolicy::all(), &active_context())
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].source_id, "managed");
        assert_eq!(listed[0].runtime_id.as_deref(), Some("runtime-a"));
        assert_eq!(listed[0].backend, CatalogBackend::ManagedLocal);
    }

    #[tokio::test]
    async fn incompatible_duplicate_observations_conflict() {
        let catalog = UnifiedModelCatalog::new(vec![source(
            "managed",
            CatalogBackend::ManagedLocal,
            Some("runtime-a"),
            Arc::new(AtomicUsize::new(0)),
            vec![
                ("qwen", Some("runtime-a"), "owner-a"),
                ("qwen", Some("runtime-a"), "owner-b"),
            ],
        )])
        .unwrap();
        let error = catalog
            .list(authorized_all(), &CatalogPolicy::all(), &active_context())
            .await
            .unwrap_err();
        assert!(matches!(error, CatalogError::Conflict(_)));
    }

    #[tokio::test]
    async fn provider_model_ids_must_be_source_prefixed() {
        let catalog = UnifiedModelCatalog::new(vec![source(
            "openai",
            CatalogBackend::CloudProvider,
            None,
            Arc::new(AtomicUsize::new(0)),
            vec![("gpt-4o", None, "openai")],
        )])
        .unwrap();
        let error = catalog
            .list(authorized_all(), &CatalogPolicy::all(), &active_context())
            .await
            .unwrap_err();
        assert!(
            matches!(error, CatalogError::InvalidSource(detail) if detail.contains("source-prefixed"))
        );
    }

    #[tokio::test]
    async fn explicit_runtime_override_wins_and_unknown_ids_are_not_guessed() {
        let a_calls = Arc::new(AtomicUsize::new(0));
        let b_calls = Arc::new(AtomicUsize::new(0));
        let catalog = UnifiedModelCatalog::new(vec![
            source(
                "managed",
                CatalogBackend::ManagedLocal,
                Some("runtime-a"),
                a_calls.clone(),
                vec![("shared", Some("runtime-a"), "local")],
            ),
            source(
                "mlx",
                CatalogBackend::Mlx,
                Some("runtime-b"),
                b_calls.clone(),
                vec![("shared", Some("runtime-b"), "mlx")],
            ),
        ])
        .unwrap();

        let selected = catalog
            .resolve(
                authorized_all(),
                &CatalogPolicy::all(),
                "shared",
                Some("runtime-b"),
                &active_context(),
            )
            .await
            .unwrap();
        assert_eq!(selected.runtime_id.as_deref(), Some("runtime-b"));
        assert_eq!(a_calls.load(Ordering::SeqCst), 0);
        assert_eq!(b_calls.load(Ordering::SeqCst), 1);

        let ambiguous = catalog
            .resolve(
                authorized_all(),
                &CatalogPolicy::all(),
                "shared",
                None,
                &active_context(),
            )
            .await
            .unwrap_err();
        assert!(matches!(ambiguous, CatalogError::Conflict(_)));

        let missing = catalog
            .resolve(
                authorized_all(),
                &CatalogPolicy::all(),
                "not-an-ollama-guess",
                None,
                &active_context(),
            )
            .await
            .unwrap_err();
        assert_eq!(
            missing,
            CatalogError::NotFound {
                model_id: "not-an-ollama-guess".to_string(),
                searched_sources: vec!["managed".to_string(), "mlx".to_string()],
            }
        );
    }

    #[tokio::test]
    async fn deadline_and_cancellation_stop_in_flight_sources() {
        let deadline_calls = Arc::new(AtomicUsize::new(0));
        let deadline_catalog = UnifiedModelCatalog::new(vec![delayed_source(
            "managed",
            CatalogBackend::ManagedLocal,
            Some("runtime-a"),
            deadline_calls.clone(),
            Duration::from_secs(5),
        )])
        .unwrap();
        let deadline_context = CatalogRequestContext::with_timeout(
            CancellationToken::new(),
            Duration::from_millis(10),
        );
        let error = deadline_catalog
            .list(authorized_all(), &CatalogPolicy::all(), &deadline_context)
            .await
            .unwrap_err();
        assert_eq!(error, CatalogError::DeadlineExceeded);
        assert_eq!(deadline_calls.load(Ordering::SeqCst), 1);

        let cancellation_calls = Arc::new(AtomicUsize::new(0));
        let cancellation_catalog = Arc::new(
            UnifiedModelCatalog::new(vec![delayed_source(
                "managed",
                CatalogBackend::ManagedLocal,
                Some("runtime-a"),
                cancellation_calls.clone(),
                Duration::from_secs(5),
            )])
            .unwrap(),
        );
        let cancellation_context = active_context();
        let task_catalog = cancellation_catalog.clone();
        let task_context = cancellation_context.clone();
        let task = tokio::spawn(async move {
            task_catalog
                .list(authorized_all(), &CatalogPolicy::all(), &task_context)
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while cancellation_calls.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        cancellation_context.cancellation().cancel();
        let error = task.await.unwrap().unwrap_err();
        assert_eq!(error, CatalogError::Cancelled);
    }

    #[tokio::test]
    async fn source_and_result_caps_are_enforced() {
        let tiny_source_limit = CatalogLimits {
            max_sources: 1,
            max_models_per_source: 2,
            max_models_total: 2,
        };
        let too_many_sources = UnifiedModelCatalog::with_limits(
            vec![
                source(
                    "a",
                    CatalogBackend::ManagedLocal,
                    Some("runtime-a"),
                    Arc::new(AtomicUsize::new(0)),
                    Vec::new(),
                ),
                source(
                    "b",
                    CatalogBackend::Mlx,
                    Some("runtime-b"),
                    Arc::new(AtomicUsize::new(0)),
                    Vec::new(),
                ),
            ],
            tiny_source_limit,
        )
        .err()
        .unwrap();
        assert_eq!(
            too_many_sources,
            CatalogError::LimitExceeded {
                kind: CatalogLimitKind::Sources,
                limit: 1
            }
        );

        let per_source_limits = CatalogLimits {
            max_sources: 2,
            max_models_per_source: 1,
            max_models_total: 2,
        };
        let per_source_catalog = UnifiedModelCatalog::with_limits(
            vec![source(
                "a",
                CatalogBackend::ManagedLocal,
                Some("runtime-a"),
                Arc::new(AtomicUsize::new(0)),
                vec![
                    ("one", Some("runtime-a"), "local"),
                    ("two", Some("runtime-a"), "local"),
                ],
            )],
            per_source_limits,
        )
        .unwrap();
        let error = per_source_catalog
            .list(authorized_all(), &CatalogPolicy::all(), &active_context())
            .await
            .unwrap_err();
        assert_eq!(
            error,
            CatalogError::LimitExceeded {
                kind: CatalogLimitKind::ModelsPerSource,
                limit: 1
            }
        );

        let second_calls = Arc::new(AtomicUsize::new(0));
        let total_catalog = UnifiedModelCatalog::with_limits(
            vec![
                source(
                    "a",
                    CatalogBackend::ManagedLocal,
                    Some("runtime-a"),
                    Arc::new(AtomicUsize::new(0)),
                    vec![("one", Some("runtime-a"), "local")],
                ),
                source(
                    "b",
                    CatalogBackend::ManagedLocal,
                    Some("runtime-b"),
                    second_calls.clone(),
                    vec![("two", Some("runtime-b"), "local")],
                ),
            ],
            CatalogLimits {
                max_sources: 2,
                max_models_per_source: 2,
                max_models_total: 1,
            },
        )
        .unwrap();
        let error = total_catalog
            .list(authorized_all(), &CatalogPolicy::all(), &active_context())
            .await
            .unwrap_err();
        assert_eq!(
            error,
            CatalogError::LimitExceeded {
                kind: CatalogLimitKind::TotalModels,
                limit: 1
            }
        );
        assert_eq!(second_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn exact_resolution_selects_target_before_per_source_and_total_caps() {
        let calls = Arc::new(AtomicUsize::new(0));
        let catalog = UnifiedModelCatalog::with_limits(
            vec![Arc::new(ExactQueryFixtureSource {
                calls: calls.clone(),
                models: ["alpha", "zeta"]
                    .into_iter()
                    .map(|model_id| CatalogModel {
                        model_id: model_id.to_string(),
                        source_id: "managed".to_string(),
                        runtime_id: Some("runtime-a".to_string()),
                        backend: CatalogBackend::ManagedLocal,
                        owned_by: "local".to_string(),
                    })
                    .collect(),
            })],
            CatalogLimits {
                max_sources: 1,
                max_models_per_source: 1,
                max_models_total: 1,
            },
        )
        .unwrap();

        let resolved = catalog
            .resolve(
                authorized_all(),
                &CatalogPolicy::all(),
                "zeta",
                None,
                &active_context(),
            )
            .await
            .unwrap();

        assert_eq!(resolved.model_id, "zeta");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn safe_source_failure_short_circuits_and_never_becomes_a_false_404() {
        let first_calls = Arc::new(AtomicUsize::new(0));
        let later_calls = Arc::new(AtomicUsize::new(0));
        let catalog = UnifiedModelCatalog::new(vec![
            source(
                "z-later",
                CatalogBackend::ManagedLocal,
                Some("z-later"),
                later_calls.clone(),
                vec![("model", Some("z-later"), "local")],
            ),
            failing_source(
                "a-failing",
                CatalogBackend::ManagedLocal,
                Some("a-failing"),
                first_calls.clone(),
                CatalogSourceError::Unavailable,
            ),
        ])
        .unwrap();

        let error = catalog
            .resolve(
                authorized_all(),
                &CatalogPolicy::all(),
                "missing",
                None,
                &active_context(),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            &error,
            CatalogError::SourceUnavailable { source_id, failure: CatalogSourceError::Unavailable }
                if source_id == "a-failing"
        ));
        assert_eq!(
            error.to_string(),
            "model source a-failing is unavailable: unavailable"
        );
        assert_eq!(first_calls.load(Ordering::SeqCst), 1);
        assert_eq!(later_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn optional_provider_failure_does_not_hide_healthy_local_models() {
        let provider_calls = Arc::new(AtomicUsize::new(0));
        let local_calls = Arc::new(AtomicUsize::new(0));
        let catalog = UnifiedModelCatalog::new(vec![
            optional_failing_source(
                "provider",
                CatalogBackend::CloudProvider,
                None,
                provider_calls.clone(),
                CatalogSourceError::PermissionDenied,
            ),
            source(
                "managed",
                CatalogBackend::ManagedLocal,
                Some("runtime-a"),
                local_calls.clone(),
                vec![("local-model", Some("runtime-a"), "local")],
            ),
        ])
        .unwrap();

        let listed = catalog
            .list(authorized_all(), &CatalogPolicy::all(), &active_context())
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].model_id, "local-model");
        assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
        assert_eq!(local_calls.load(Ordering::SeqCst), 1);

        let local = catalog
            .resolve(
                authorized_all(),
                &CatalogPolicy::all(),
                "local-model",
                None,
                &active_context(),
            )
            .await
            .unwrap();
        assert_eq!(local.source_id, "managed");
        // Exact provider namespaces let an unrelated provider be skipped.
        assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
        assert_eq!(local_calls.load(Ordering::SeqCst), 2);

        let error = catalog
            .resolve(
                authorized_all(),
                &CatalogPolicy::all(),
                "provider/remote-model",
                None,
                &active_context(),
            )
            .await
            .unwrap_err();
        assert_eq!(
            error,
            CatalogError::SourceUnavailable {
                source_id: "provider".to_string(),
                failure: CatalogSourceError::PermissionDenied,
            }
        );
        assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
        assert_eq!(local_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn optional_exact_failure_is_deferred_across_source_order_and_input_permutations() {
        for (failing_id, healthy_id) in [("a-offline", "z-healthy"), ("z-offline", "a-healthy")] {
            for reverse_input in [false, true] {
                let failing_calls = Arc::new(AtomicUsize::new(0));
                let healthy_calls = Arc::new(AtomicUsize::new(0));
                let failing = optional_failing_source(
                    failing_id,
                    CatalogBackend::ManagedLocal,
                    Some(failing_id),
                    failing_calls.clone(),
                    CatalogSourceError::Unavailable,
                );
                let healthy = source(
                    healthy_id,
                    CatalogBackend::ManagedLocal,
                    Some(healthy_id),
                    healthy_calls.clone(),
                    vec![("shared-model", Some(healthy_id), "local")],
                );
                let sources = if reverse_input {
                    vec![healthy, failing]
                } else {
                    vec![failing, healthy]
                };
                let catalog = UnifiedModelCatalog::new(sources).unwrap();

                let resolved = catalog
                    .resolve(
                        authorized_all(),
                        &CatalogPolicy::all(),
                        "shared-model",
                        None,
                        &active_context(),
                    )
                    .await
                    .unwrap();

                assert_eq!(resolved.source_id, healthy_id);
                assert_eq!(resolved.runtime_id.as_deref(), Some(healthy_id));
                assert_eq!(failing_calls.load(Ordering::SeqCst), 1);
                assert_eq!(healthy_calls.load(Ordering::SeqCst), 1);
            }
        }
    }

    #[tokio::test]
    async fn optional_exact_failure_is_returned_only_after_every_eligible_source_misses() {
        let failing_calls = Arc::new(AtomicUsize::new(0));
        let later_calls = Arc::new(AtomicUsize::new(0));
        let catalog = UnifiedModelCatalog::new(vec![
            optional_failing_source(
                "a-offline",
                CatalogBackend::ManagedLocal,
                Some("a-offline"),
                failing_calls.clone(),
                CatalogSourceError::Unavailable,
            ),
            source(
                "z-empty",
                CatalogBackend::ManagedLocal,
                Some("z-empty"),
                later_calls.clone(),
                Vec::new(),
            ),
        ])
        .unwrap();

        let error = catalog
            .resolve(
                authorized_all(),
                &CatalogPolicy::all(),
                "missing-model",
                None,
                &active_context(),
            )
            .await
            .unwrap_err();

        assert_eq!(
            error,
            CatalogError::SourceUnavailable {
                source_id: "a-offline".to_string(),
                failure: CatalogSourceError::Unavailable,
            }
        );
        assert_eq!(failing_calls.load(Ordering::SeqCst), 1);
        assert_eq!(later_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancellation_outranks_optional_source_omission() {
        let calls = Arc::new(AtomicUsize::new(0));
        let source: Arc<dyn ModelCatalogSource> = Arc::new(FixtureSource {
            id: "provider".to_string(),
            backend: CatalogBackend::CloudProvider,
            runtime_id: None,
            calls: calls.clone(),
            last_limit: Arc::new(AtomicUsize::new(0)),
            failure_policy: CatalogSourceFailurePolicy::OmitUnavailable,
            behavior: FixtureBehavior::Delayed {
                delay: Duration::from_secs(5),
                result: Err(CatalogSourceError::TimedOut),
            },
        });
        let catalog = UnifiedModelCatalog::new(vec![source]).unwrap();
        let cancellation = CancellationToken::new();
        let context =
            CatalogRequestContext::with_timeout(cancellation.clone(), Duration::from_secs(1));
        let cancel_task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            cancellation.cancel();
        });

        let error = catalog
            .list(authorized_all(), &CatalogPolicy::all(), &context)
            .await
            .unwrap_err();
        cancel_task.await.unwrap();
        assert_eq!(error, CatalogError::Cancelled);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
