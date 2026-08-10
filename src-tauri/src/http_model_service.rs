//! Authentication-ordered orchestration for the unified HTTP model catalog.
//!
//! Source *construction* can itself observe runtime registrations, so calling
//! `UnifiedModelCatalog::list` is not enough to guarantee auth-before-probe.
//! This layer validates the authorization/policy/model intersection first and
//! only then asks a source factory for the current runtime adapters.  Both the
//! desktop listener and the AppHandle-free CLI use this transport-free seam.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::{json, Value};

use crate::http_model_catalog::{
    validate_resolution_request, CatalogAuthorization, CatalogBackend, CatalogError, CatalogModel,
    CatalogPolicy, CatalogRequestContext, ModelCatalogSource, UnifiedModelCatalog,
};
use crate::http_model_sources::m3_runtime_catalog_sources;
use crate::m3_runtime_hub::M3RuntimeHub;

pub trait ModelCatalogSourceFactory: Send + Sync {
    fn current_sources(&self) -> Result<Vec<Arc<dyn ModelCatalogSource>>, CatalogError>;
}

#[derive(Clone)]
pub struct M3RuntimeCatalogSourceFactory {
    hub: Arc<M3RuntimeHub>,
}

impl M3RuntimeCatalogSourceFactory {
    pub fn new(hub: Arc<M3RuntimeHub>) -> Self {
        Self { hub }
    }
}

impl ModelCatalogSourceFactory for M3RuntimeCatalogSourceFactory {
    fn current_sources(&self) -> Result<Vec<Arc<dyn ModelCatalogSource>>, CatalogError> {
        m3_runtime_catalog_sources(self.hub.clone()).map_err(|failure| {
            CatalogError::SourceUnavailable {
                source_id: "m3-runtime-registry".to_string(),
                failure,
            }
        })
    }
}

/// A request-independent source set for the AppHandle-free/headless server.
///
/// Source constructors supplied here must be pure and lazy: runtime snapshots,
/// credential lookup, and network I/O belong in `list_models`. Keeping
/// registration cheap lets the service preserve auth-before-observation.
#[derive(Clone, Default)]
pub struct StaticModelCatalogSourceFactory {
    sources: Vec<Arc<dyn ModelCatalogSource>>,
}

impl StaticModelCatalogSourceFactory {
    pub fn new(sources: Vec<Arc<dyn ModelCatalogSource>>) -> Self {
        Self { sources }
    }
}

impl ModelCatalogSourceFactory for StaticModelCatalogSourceFactory {
    fn current_sources(&self) -> Result<Vec<Arc<dyn ModelCatalogSource>>, CatalogError> {
        Ok(self.sources.clone())
    }
}

#[derive(Clone)]
pub struct HttpModelService {
    source_factory: Arc<dyn ModelCatalogSourceFactory>,
}

impl HttpModelService {
    pub fn new(source_factory: Arc<dyn ModelCatalogSourceFactory>) -> Self {
        Self { source_factory }
    }

    pub fn for_m3_hub(hub: Arc<M3RuntimeHub>) -> Self {
        Self::new(Arc::new(M3RuntimeCatalogSourceFactory::new(hub)))
    }

    pub fn from_sources(sources: Vec<Arc<dyn ModelCatalogSource>>) -> Self {
        Self::new(Arc::new(StaticModelCatalogSourceFactory::new(sources)))
    }

    pub async fn list(
        &self,
        request: ModelListRequest<'_>,
    ) -> Result<Vec<CatalogModel>, CatalogError> {
        let authorization = preflight_gate(request.authorization, request.policy)?;
        request.context.ensure_active()?;
        let catalog = self.catalog_after_authorization(request.extra_sources)?;
        let mut models = catalog
            .list(authorization, request.policy, request.context)
            .await?;
        if !request.allowed_models.is_empty() {
            models.retain(|model| request.allowed_models.contains(&model.model_id));
        }
        Ok(models)
    }

    pub async fn list_for_runtime(
        &self,
        request: ModelListRequest<'_>,
        runtime_id: &str,
    ) -> Result<Vec<CatalogModel>, CatalogError> {
        let authorization = preflight_gate(request.authorization, request.policy)?;
        request.context.ensure_active()?;
        let catalog = self.catalog_after_authorization(request.extra_sources)?;
        let mut models = catalog
            .list_for_runtime(authorization, request.policy, runtime_id, request.context)
            .await?;
        if !request.allowed_models.is_empty() {
            models.retain(|model| request.allowed_models.contains(&model.model_id));
        }
        Ok(models)
    }

    pub async fn resolve(
        &self,
        request: ModelResolveRequest<'_>,
    ) -> Result<CatalogModel, CatalogError> {
        let authorization = preflight_gate(request.authorization, request.policy)?;
        request.context.ensure_active()?;
        validate_resolution_request(request.model_id, request.runtime_override)?;
        if !request.allowed_models.is_empty() && !request.allowed_models.contains(request.model_id)
        {
            return Err(CatalogError::Forbidden);
        }
        let catalog = self.catalog_after_authorization(request.extra_sources)?;
        catalog
            .resolve(
                authorization,
                request.policy,
                request.model_id,
                request.runtime_override,
                request.context,
            )
            .await
    }

    fn catalog_after_authorization(
        &self,
        extra_sources: &[Arc<dyn ModelCatalogSource>],
    ) -> Result<UnifiedModelCatalog, CatalogError> {
        let mut sources = self.source_factory.current_sources()?;
        for extra in extra_sources {
            let shadowed_legacy_fallback = extra.source_id().starts_with("legacy-")
                && extra.runtime_id().is_some()
                && sources
                    .iter()
                    .any(|source| source.runtime_id() == extra.runtime_id());
            if shadowed_legacy_fallback {
                continue;
            }
            sources.push(extra.clone());
        }
        UnifiedModelCatalog::new(sources)
    }
}

pub struct ModelListRequest<'a> {
    pub authorization: CatalogAuthorization,
    pub policy: &'a CatalogPolicy,
    /// An empty set means unrestricted model identities (legacy/headless
    /// compatibility); a non-empty set is an additional token-level allowlist.
    pub allowed_models: &'a BTreeSet<String>,
    pub extra_sources: &'a [Arc<dyn ModelCatalogSource>],
    pub context: &'a CatalogRequestContext,
}

pub struct ModelResolveRequest<'a> {
    pub authorization: CatalogAuthorization,
    pub policy: &'a CatalogPolicy,
    /// An empty set means unrestricted model identities (legacy/headless
    /// compatibility); a non-empty set is an additional token-level allowlist.
    pub allowed_models: &'a BTreeSet<String>,
    pub model_id: &'a str,
    pub runtime_override: Option<&'a str>,
    pub extra_sources: &'a [Arc<dyn ModelCatalogSource>],
    pub context: &'a CatalogRequestContext,
}

/// Repeat the catalog's pure gate before constructing runtime-backed sources.
/// `UnifiedModelCatalog` intentionally repeats this check as defense in depth.
fn preflight_gate(
    authorization: CatalogAuthorization,
    policy: &CatalogPolicy,
) -> Result<CatalogAuthorization, CatalogError> {
    let allowed_backends = match authorization {
        CatalogAuthorization::Authorized { allowed_backends } => allowed_backends,
        CatalogAuthorization::Unauthorized => return Err(CatalogError::Unauthorized),
        CatalogAuthorization::Forbidden => return Err(CatalogError::Forbidden),
        CatalogAuthorization::RateLimited { retry_after_ms } => {
            return Err(CatalogError::RateLimited { retry_after_ms })
        }
    };
    if allowed_backends.is_disjoint(&policy.enabled_backends) {
        return Err(CatalogError::Forbidden);
    }
    Ok(CatalogAuthorization::Authorized { allowed_backends })
}

pub fn openai_model_list(models: &[CatalogModel], extended: bool) -> Value {
    let data = models
        .iter()
        .map(|model| {
            let mut row = json!({
                "id": model.model_id,
                "object": "model",
                "owned_by": model.owned_by,
            });
            if extended {
                row["source_id"] = json!(model.source_id);
                row["runtime_id"] = json!(model.runtime_id);
                row["backend"] = json!(model.backend);
            }
            row
        })
        .collect::<Vec<_>>();
    json!({ "object": "list", "data": data })
}

pub fn ollama_tags(models: &[CatalogModel]) -> Value {
    let models = models
        .iter()
        .map(|model| {
            json!({
                "name": model.model_id,
                "model": model.model_id,
                "modified_at": "1970-01-01T00:00:00Z",
                "size": 0,
                "digest": "",
                "details": {
                    "format": "unknown",
                    "family": model.owned_by,
                    "families": [model.owned_by],
                    "parameter_size": "unknown",
                    "quantization_level": "unknown"
                }
            })
        })
        .collect::<Vec<_>>();
    json!({ "models": models })
}

pub fn catalog_policy(backends: impl IntoIterator<Item = CatalogBackend>) -> CatalogPolicy {
    CatalogPolicy {
        enabled_backends: backends.into_iter().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_model_catalog::{
        CatalogSourceError, CatalogSourceFailurePolicy, CatalogSourceQuery,
    };
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::time::Instant;
    use tokio_util::sync::CancellationToken;

    struct Factory {
        calls: Arc<AtomicUsize>,
        result: Result<Vec<Arc<dyn ModelCatalogSource>>, CatalogError>,
    }

    impl ModelCatalogSourceFactory for Factory {
        fn current_sources(&self) -> Result<Vec<Arc<dyn ModelCatalogSource>>, CatalogError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.result.clone()
        }
    }

    #[derive(Clone)]
    enum SourceBehavior {
        Ready(Result<Vec<CatalogModel>, CatalogSourceError>),
        Delayed {
            delay: Duration,
            result: Result<Vec<CatalogModel>, CatalogSourceError>,
        },
    }

    struct Source {
        id: String,
        backend: CatalogBackend,
        runtime_id: Option<String>,
        calls: Arc<AtomicUsize>,
        failure_policy: CatalogSourceFailurePolicy,
        behavior: SourceBehavior,
    }

    #[async_trait]
    impl ModelCatalogSource for Source {
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
            _max_models: usize,
        ) -> Result<Vec<CatalogModel>, CatalogSourceError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match &self.behavior {
                SourceBehavior::Ready(result) => result.clone(),
                SourceBehavior::Delayed { delay, result } => {
                    tokio::time::sleep(*delay).await;
                    result.clone()
                }
            }
        }
    }

    fn factory(
        calls: Arc<AtomicUsize>,
        sources: Vec<Arc<dyn ModelCatalogSource>>,
    ) -> Arc<dyn ModelCatalogSourceFactory> {
        Arc::new(Factory {
            calls,
            result: Ok(sources),
        })
    }

    fn model_source(
        id: &str,
        backend: CatalogBackend,
        runtime_id: Option<&str>,
        calls: Arc<AtomicUsize>,
        models: Vec<(&str, &str)>,
    ) -> Arc<dyn ModelCatalogSource> {
        Arc::new(Source {
            id: id.to_string(),
            backend,
            runtime_id: runtime_id.map(str::to_string),
            calls,
            failure_policy: CatalogSourceFailurePolicy::Required,
            behavior: SourceBehavior::Ready(Ok(models
                .into_iter()
                .map(|(model_id, owned_by)| CatalogModel {
                    model_id: model_id.to_string(),
                    source_id: "untrusted-source".to_string(),
                    runtime_id: runtime_id.map(str::to_string),
                    backend,
                    owned_by: owned_by.to_string(),
                })
                .collect())),
        })
    }

    fn optional_failing_provider(
        id: &str,
        calls: Arc<AtomicUsize>,
        failure: CatalogSourceError,
    ) -> Arc<dyn ModelCatalogSource> {
        Arc::new(Source {
            id: id.to_string(),
            backend: CatalogBackend::CloudProvider,
            runtime_id: None,
            calls,
            failure_policy: CatalogSourceFailurePolicy::OmitUnavailable,
            behavior: SourceBehavior::Ready(Err(failure)),
        })
    }

    fn delayed_source(
        id: &str,
        calls: Arc<AtomicUsize>,
        delay: Duration,
    ) -> Arc<dyn ModelCatalogSource> {
        Arc::new(Source {
            id: id.to_string(),
            backend: CatalogBackend::ManagedLocal,
            runtime_id: Some(format!("{id}-runtime")),
            calls,
            failure_policy: CatalogSourceFailurePolicy::Required,
            behavior: SourceBehavior::Delayed {
                delay,
                result: Ok(Vec::new()),
            },
        })
    }

    fn context() -> CatalogRequestContext {
        CatalogRequestContext::with_timeout(CancellationToken::new(), Duration::from_secs(1))
    }

    fn policy() -> CatalogPolicy {
        catalog_policy([CatalogBackend::ManagedLocal])
    }

    fn authorization(backends: impl IntoIterator<Item = CatalogBackend>) -> CatalogAuthorization {
        CatalogAuthorization::Authorized {
            allowed_backends: backends.into_iter().collect(),
        }
    }

    fn local_resolve_request<'a>(
        policy: &'a CatalogPolicy,
        allowed_models: &'a BTreeSet<String>,
        context: &'a CatalogRequestContext,
        model_id: &'a str,
        runtime_override: Option<&'a str>,
    ) -> ModelResolveRequest<'a> {
        ModelResolveRequest {
            authorization: authorization([CatalogBackend::ManagedLocal]),
            policy,
            allowed_models,
            model_id,
            runtime_override,
            extra_sources: &[],
            context,
        }
    }

    #[tokio::test]
    async fn denied_gate_never_constructs_sources_for_list_or_resolve() {
        for authorization in [
            CatalogAuthorization::Unauthorized,
            CatalogAuthorization::Forbidden,
            CatalogAuthorization::RateLimited {
                retry_after_ms: 100,
            },
            CatalogAuthorization::Authorized {
                allowed_backends: BTreeSet::from([CatalogBackend::CloudProvider]),
            },
        ] {
            let factory_calls = Arc::new(AtomicUsize::new(0));
            let service = HttpModelService::new(factory(factory_calls.clone(), Vec::new()));
            let policy = policy();
            let allowed_models = BTreeSet::new();
            let context = context();
            let list_error = service
                .list(ModelListRequest {
                    authorization: authorization.clone(),
                    policy: &policy,
                    allowed_models: &allowed_models,
                    extra_sources: &[],
                    context: &context,
                })
                .await
                .unwrap_err();
            let resolve_error = service
                .resolve(ModelResolveRequest {
                    authorization,
                    policy: &policy,
                    allowed_models: &allowed_models,
                    // Authentication deliberately outranks malformed input.
                    model_id: " ",
                    runtime_override: None,
                    extra_sources: &[],
                    context: &context,
                })
                .await
                .unwrap_err();
            assert!(matches!(
                list_error,
                CatalogError::Unauthorized
                    | CatalogError::Forbidden
                    | CatalogError::RateLimited { .. }
            ));
            assert!(matches!(
                resolve_error,
                CatalogError::Unauthorized
                    | CatalogError::Forbidden
                    | CatalogError::RateLimited { .. }
            ));
            assert_eq!(factory_calls.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn lifetime_request_and_allowlist_gates_precede_source_construction() {
        let factory_calls = Arc::new(AtomicUsize::new(0));
        let service = HttpModelService::new(factory(factory_calls.clone(), Vec::new()));
        let policy = policy();
        let allowed_models = BTreeSet::new();

        let cancelled_token = CancellationToken::new();
        cancelled_token.cancel();
        let cancelled =
            CatalogRequestContext::with_timeout(cancelled_token, Duration::from_secs(1));
        let error = service
            .list(ModelListRequest {
                authorization: authorization([CatalogBackend::ManagedLocal]),
                policy: &policy,
                allowed_models: &allowed_models,
                extra_sources: &[],
                context: &cancelled,
            })
            .await
            .unwrap_err();
        assert_eq!(error, CatalogError::Cancelled);

        let expired = CatalogRequestContext::new(CancellationToken::new(), Instant::now());
        let error = service
            .list(ModelListRequest {
                authorization: authorization([CatalogBackend::ManagedLocal]),
                policy: &policy,
                allowed_models: &allowed_models,
                extra_sources: &[],
                context: &expired,
            })
            .await
            .unwrap_err();
        assert_eq!(error, CatalogError::DeadlineExceeded);

        for (model_id, runtime_override) in [(" ", None), ("model", Some("\n"))] {
            let error = service
                .resolve(ModelResolveRequest {
                    authorization: authorization([CatalogBackend::ManagedLocal]),
                    policy: &policy,
                    allowed_models: &allowed_models,
                    model_id,
                    runtime_override,
                    extra_sources: &[],
                    context: &context(),
                })
                .await
                .unwrap_err();
            assert!(matches!(error, CatalogError::InvalidRequest(_)));
        }

        let allowlist = BTreeSet::from(["allowed".to_string()]);
        let error = service
            .resolve(ModelResolveRequest {
                authorization: authorization([CatalogBackend::ManagedLocal]),
                policy: &policy,
                allowed_models: &allowlist,
                model_id: "not-allowed",
                runtime_override: None,
                extra_sources: &[],
                context: &context(),
            })
            .await
            .unwrap_err();
        assert_eq!(error, CatalogError::Forbidden);
        assert_eq!(factory_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn list_and_resolve_share_current_sources_and_model_filter() {
        let factory_calls = Arc::new(AtomicUsize::new(0));
        let source_calls = Arc::new(AtomicUsize::new(0));
        let service = HttpModelService::new(factory(
            factory_calls.clone(),
            vec![model_source(
                "managed",
                CatalogBackend::ManagedLocal,
                Some("runtime-a"),
                source_calls.clone(),
                vec![("local-model", "little-monkey")],
            )],
        ));
        let authorization = || CatalogAuthorization::Authorized {
            allowed_backends: BTreeSet::from([CatalogBackend::ManagedLocal]),
        };
        let allowed_models = BTreeSet::from(["local-model".to_string()]);
        let context = context();
        let models = service
            .list(ModelListRequest {
                authorization: authorization(),
                policy: &policy(),
                allowed_models: &allowed_models,
                extra_sources: &[],
                context: &context,
            })
            .await
            .unwrap();
        assert_eq!(models.len(), 1);
        let resolved = service
            .resolve(ModelResolveRequest {
                authorization: authorization(),
                policy: &policy(),
                allowed_models: &allowed_models,
                model_id: "local-model",
                runtime_override: Some("runtime-a"),
                extra_sources: &[],
                context: &context,
            })
            .await
            .unwrap();
        assert_eq!(resolved.runtime_id.as_deref(), Some("runtime-a"));
        assert_eq!(factory_calls.load(Ordering::SeqCst), 2);
        assert_eq!(source_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn exact_resolution_honors_override_reports_ambiguity_and_names_search() {
        let factory_calls = Arc::new(AtomicUsize::new(0));
        let a_calls = Arc::new(AtomicUsize::new(0));
        let b_calls = Arc::new(AtomicUsize::new(0));
        let service = HttpModelService::new(factory(
            factory_calls.clone(),
            vec![
                model_source(
                    "managed-a",
                    CatalogBackend::ManagedLocal,
                    Some("runtime-a"),
                    a_calls.clone(),
                    vec![("shared", "local")],
                ),
                model_source(
                    "managed-b",
                    CatalogBackend::ManagedLocal,
                    Some("runtime-b"),
                    b_calls.clone(),
                    vec![("shared", "local")],
                ),
            ],
        ));
        let policy = policy();
        let allowed_models = BTreeSet::new();
        let request_context = context();

        let selected = service
            .resolve(local_resolve_request(
                &policy,
                &allowed_models,
                &request_context,
                "shared",
                Some("runtime-b"),
            ))
            .await
            .unwrap();
        assert_eq!(selected.runtime_id.as_deref(), Some("runtime-b"));
        assert_eq!(a_calls.load(Ordering::SeqCst), 0);
        assert_eq!(b_calls.load(Ordering::SeqCst), 1);

        let ambiguous = service
            .resolve(local_resolve_request(
                &policy,
                &allowed_models,
                &request_context,
                "shared",
                None,
            ))
            .await
            .unwrap_err();
        assert!(matches!(ambiguous, CatalogError::Conflict(_)));

        let unknown = service
            .resolve(local_resolve_request(
                &policy,
                &allowed_models,
                &request_context,
                "not-an-ollama-guess",
                None,
            ))
            .await
            .unwrap_err();
        assert_eq!(
            unknown,
            CatalogError::NotFound {
                model_id: "not-an-ollama-guess".to_string(),
                searched_sources: vec!["managed-a".to_string(), "managed-b".to_string()],
            }
        );
        assert!(unknown.to_string().contains("managed-a, managed-b"));

        let a_before = a_calls.load(Ordering::SeqCst);
        let b_before = b_calls.load(Ordering::SeqCst);
        let unknown_runtime = service
            .resolve(local_resolve_request(
                &policy,
                &allowed_models,
                &request_context,
                "shared",
                Some("runtime-missing"),
            ))
            .await
            .unwrap_err();
        assert_eq!(
            unknown_runtime,
            CatalogError::NotFound {
                model_id: "shared".to_string(),
                searched_sources: Vec::new(),
            }
        );
        assert_eq!(a_calls.load(Ordering::SeqCst), a_before);
        assert_eq!(b_calls.load(Ordering::SeqCst), b_before);
        assert_eq!(factory_calls.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn duplicate_source_and_runtime_declarations_fail_before_inventory_probes() {
        for duplicate_runtime in [false, true] {
            let factory_calls = Arc::new(AtomicUsize::new(0));
            let primary_calls = Arc::new(AtomicUsize::new(0));
            let extra_calls = Arc::new(AtomicUsize::new(0));
            let primary_id = if duplicate_runtime {
                "primary"
            } else {
                "duplicate"
            };
            let extra_id = if duplicate_runtime {
                "extra"
            } else {
                "duplicate"
            };
            let service = HttpModelService::new(factory(
                factory_calls.clone(),
                vec![model_source(
                    primary_id,
                    CatalogBackend::ManagedLocal,
                    Some("runtime-a"),
                    primary_calls.clone(),
                    Vec::new(),
                )],
            ));
            let extra = vec![model_source(
                extra_id,
                CatalogBackend::ManagedLocal,
                Some(if duplicate_runtime {
                    "runtime-a"
                } else {
                    "runtime-b"
                }),
                extra_calls.clone(),
                Vec::new(),
            )];
            let error = service
                .list(ModelListRequest {
                    authorization: authorization([CatalogBackend::ManagedLocal]),
                    policy: &policy(),
                    allowed_models: &BTreeSet::new(),
                    extra_sources: &extra,
                    context: &context(),
                })
                .await
                .unwrap_err();
            assert!(matches!(error, CatalogError::Conflict(_)));
            assert_eq!(factory_calls.load(Ordering::SeqCst), 1);
            assert_eq!(primary_calls.load(Ordering::SeqCst), 0);
            assert_eq!(extra_calls.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn optional_provider_is_omitted_only_for_lists_not_targeted_resolution() {
        let factory_calls = Arc::new(AtomicUsize::new(0));
        let local_calls = Arc::new(AtomicUsize::new(0));
        let provider_calls = Arc::new(AtomicUsize::new(0));
        let service = HttpModelService::new(factory(
            factory_calls,
            vec![
                model_source(
                    "managed",
                    CatalogBackend::ManagedLocal,
                    Some("runtime-a"),
                    local_calls.clone(),
                    vec![("local-model", "local")],
                ),
                optional_failing_provider(
                    "provider",
                    provider_calls.clone(),
                    CatalogSourceError::PermissionDenied,
                ),
            ],
        ));
        let policy = CatalogPolicy::all();
        let allowed_models = BTreeSet::new();

        let listed = service
            .list(ModelListRequest {
                authorization: authorization([
                    CatalogBackend::ManagedLocal,
                    CatalogBackend::CloudProvider,
                ]),
                policy: &policy,
                allowed_models: &allowed_models,
                extra_sources: &[],
                context: &context(),
            })
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(provider_calls.load(Ordering::SeqCst), 1);

        let local = service
            .resolve(ModelResolveRequest {
                authorization: authorization([
                    CatalogBackend::ManagedLocal,
                    CatalogBackend::CloudProvider,
                ]),
                policy: &policy,
                allowed_models: &allowed_models,
                model_id: "local-model",
                runtime_override: None,
                extra_sources: &[],
                context: &context(),
            })
            .await
            .unwrap();
        assert_eq!(local.source_id, "managed");
        assert_eq!(provider_calls.load(Ordering::SeqCst), 1);

        let provider_error = service
            .resolve(ModelResolveRequest {
                authorization: authorization([
                    CatalogBackend::ManagedLocal,
                    CatalogBackend::CloudProvider,
                ]),
                policy: &policy,
                allowed_models: &allowed_models,
                model_id: "provider/remote",
                runtime_override: None,
                extra_sources: &[],
                context: &context(),
            })
            .await
            .unwrap_err();
        assert_eq!(
            provider_error,
            CatalogError::SourceUnavailable {
                source_id: "provider".to_string(),
                failure: CatalogSourceError::PermissionDenied,
            }
        );
        assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
        assert_eq!(local_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn cancellation_and_deadline_stop_in_flight_service_sources() {
        let cancellation_calls = Arc::new(AtomicUsize::new(0));
        let cancellation_factory_calls = Arc::new(AtomicUsize::new(0));
        let cancellation_service = HttpModelService::new(factory(
            cancellation_factory_calls.clone(),
            vec![delayed_source(
                "cancelled",
                cancellation_calls.clone(),
                Duration::from_secs(10),
            )],
        ));
        let cancellation = CancellationToken::new();
        let context =
            CatalogRequestContext::with_timeout(cancellation.clone(), Duration::from_secs(1));
        let cancel_task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            cancellation.cancel();
        });
        let error = cancellation_service
            .list(ModelListRequest {
                authorization: authorization([CatalogBackend::ManagedLocal]),
                policy: &policy(),
                allowed_models: &BTreeSet::new(),
                extra_sources: &[],
                context: &context,
            })
            .await
            .unwrap_err();
        cancel_task.await.unwrap();
        assert_eq!(error, CatalogError::Cancelled);
        assert_eq!(cancellation_factory_calls.load(Ordering::SeqCst), 1);
        assert_eq!(cancellation_calls.load(Ordering::SeqCst), 1);

        let deadline_calls = Arc::new(AtomicUsize::new(0));
        let deadline_service = HttpModelService::from_sources(vec![delayed_source(
            "deadline",
            deadline_calls.clone(),
            Duration::from_secs(10),
        )]);
        let deadline_context = CatalogRequestContext::with_timeout(
            CancellationToken::new(),
            Duration::from_millis(10),
        );
        let error = deadline_service
            .list(ModelListRequest {
                authorization: authorization([CatalogBackend::ManagedLocal]),
                policy: &policy(),
                allowed_models: &BTreeSet::new(),
                extra_sources: &[],
                context: &deadline_context,
            })
            .await
            .unwrap_err();
        assert_eq!(error, CatalogError::DeadlineExceeded);
        assert_eq!(deadline_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn factory_failures_are_typed_and_still_auth_ordered() {
        let calls = Arc::new(AtomicUsize::new(0));
        let expected = CatalogError::SourceUnavailable {
            source_id: "runtime-registry".to_string(),
            failure: CatalogSourceError::Unavailable,
        };
        let service = HttpModelService::new(Arc::new(Factory {
            calls: calls.clone(),
            result: Err(expected.clone()),
        }));
        let policy = policy();
        let allowed_models = BTreeSet::new();
        let context = context();

        let denied = service
            .list(ModelListRequest {
                authorization: CatalogAuthorization::Unauthorized,
                policy: &policy,
                allowed_models: &allowed_models,
                extra_sources: &[],
                context: &context,
            })
            .await
            .unwrap_err();
        assert_eq!(denied, CatalogError::Unauthorized);
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        let error = service
            .list(ModelListRequest {
                authorization: authorization([CatalogBackend::ManagedLocal]),
                policy: &policy,
                allowed_models: &allowed_models,
                extra_sources: &[],
                context: &context,
            })
            .await
            .unwrap_err();
        assert_eq!(error, expected);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn wire_helpers_preserve_legacy_minimal_rows_and_m3_provenance() {
        let model = CatalogModel {
            model_id: "local-model".to_string(),
            source_id: "managed".to_string(),
            runtime_id: Some("runtime-a".to_string()),
            backend: CatalogBackend::ManagedLocal,
            owned_by: "little-monkey".to_string(),
        };
        let minimal = openai_model_list(std::slice::from_ref(&model), false);
        assert_eq!(
            minimal,
            json!({"object":"list","data":[{
                "id":"local-model","object":"model","owned_by":"little-monkey"
            }]})
        );
        let extended = openai_model_list(std::slice::from_ref(&model), true);
        assert_eq!(extended["data"][0]["runtime_id"], "runtime-a");
        assert_eq!(extended["data"][0]["source_id"], "managed");
        assert_eq!(ollama_tags(&[model])["models"][0]["name"], "local-model");
    }
}
