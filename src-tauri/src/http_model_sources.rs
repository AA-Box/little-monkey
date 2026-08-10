//! Production inventory adapters for the unified HTTP model catalog.
//!
//! Every adapter is deliberately lazy: constructing a source stores immutable
//! configuration only.  Filesystem, runtime, keychain, and network work starts
//! in `list_models`, which [`UnifiedModelCatalog`](crate::http_model_catalog::UnifiedModelCatalog)
//! invokes only after authentication, scope checks, policy intersection, and a
//! quota debit have succeeded.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::{Client, StatusCode, Url};
use serde::Deserialize;

use crate::compatibility_hub::ApiBackend;
use crate::http_model_catalog::{
    CatalogBackend, CatalogModel, CatalogRequestContext, CatalogSourceError,
    CatalogSourceFailurePolicy, CatalogSourceQuery, ModelCatalogSource,
};
use crate::m3_runtime_hub::{M3OperationContext, M3RuntimeCapabilityView, M3RuntimeHub};

const MAX_CATALOG_HTTP_BYTES: usize = 4 * 1024 * 1024;

fn catalog_backend(backend: ApiBackend) -> CatalogBackend {
    match backend {
        ApiBackend::ManagedLocal => CatalogBackend::ManagedLocal,
        ApiBackend::Ollama => CatalogBackend::Ollama,
        ApiBackend::Mlx => CatalogBackend::Mlx,
        ApiBackend::CloudProvider => CatalogBackend::CloudProvider,
    }
}

pub fn catalog_backends(backends: &BTreeSet<ApiBackend>) -> BTreeSet<CatalogBackend> {
    backends.iter().copied().map(catalog_backend).collect()
}

fn runtime_owned_by(backend: ApiBackend) -> &'static str {
    match backend {
        ApiBackend::ManagedLocal => "little-monkey",
        ApiBackend::Ollama => "ollama",
        ApiBackend::Mlx => "mlx",
        ApiBackend::CloudProvider => "provider",
    }
}

fn remaining_timeout(context: &CatalogRequestContext) -> Result<Duration, CatalogSourceError> {
    if context.cancellation().is_cancelled() {
        return Err(CatalogSourceError::TimedOut);
    }
    context
        .deadline()
        .checked_duration_since(tokio::time::Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or(CatalogSourceError::TimedOut)
}

/// One source per registered M3 runtime.  It joins the durable M3-installed
/// inventory with the runtime's live inventory under a single runtime id, so
/// a model present in both is one public identity rather than an ambiguous
/// duplicate.  Durable metadata wins when the same id is observed twice.
pub struct M3RuntimeCatalogSource {
    hub: Arc<M3RuntimeHub>,
    runtime: M3RuntimeCapabilityView,
    source_id: String,
}

impl M3RuntimeCatalogSource {
    pub fn new(hub: Arc<M3RuntimeHub>, runtime: M3RuntimeCapabilityView) -> Self {
        let source_id = format!("m3-runtime:{}", runtime.descriptor.runtime_id);
        Self {
            hub,
            runtime,
            source_id,
        }
    }
}

#[async_trait]
impl ModelCatalogSource for M3RuntimeCatalogSource {
    fn source_id(&self) -> &str {
        &self.source_id
    }

    fn backend(&self) -> CatalogBackend {
        catalog_backend(self.runtime.descriptor.api_backend)
    }

    fn runtime_id(&self) -> Option<&str> {
        Some(&self.runtime.descriptor.runtime_id)
    }

    fn failure_policy(&self) -> CatalogSourceFailurePolicy {
        // Registered runtimes participate in best-effort union discovery.
        // Exact lookup remains strict when no other source has proved the
        // requested identity; the catalog only omits this failure after an
        // earlier exact match exists.
        CatalogSourceFailurePolicy::OmitUnavailable
    }

    async fn list_models(
        &self,
        context: &CatalogRequestContext,
        query: CatalogSourceQuery<'_>,
        max_models: usize,
    ) -> Result<Vec<CatalogModel>, CatalogSourceError> {
        let remaining = remaining_timeout(context)?;
        if max_models == 0 {
            return Ok(Vec::new());
        }
        let operation = M3OperationContext {
            cancellation: context.cancellation().clone(),
            timeout_ms: u64::try_from(remaining.as_millis())
                .unwrap_or(u64::MAX)
                .max(1),
        };
        let descriptor = &self.runtime.descriptor;
        let mut models = BTreeMap::<String, CatalogModel>::new();

        // This reads authenticated local state only.  Errors stay at the
        // source boundary and never carry paths into an HTTP response.
        let installed = self
            .hub
            .list_installed_models()
            .map_err(|_| CatalogSourceError::Unavailable)?;
        for model in installed
            .into_iter()
            .filter(|model| model.runtime == descriptor.kind)
        {
            models
                .entry(model.model_id.clone())
                .or_insert(CatalogModel {
                    model_id: model.model_id,
                    source_id: self.source_id.clone(),
                    runtime_id: Some(descriptor.runtime_id.clone()),
                    backend: catalog_backend(descriptor.api_backend),
                    owned_by: "little-monkey".to_string(),
                });
        }

        // Runtime inventory is best-effort for broad listing: an offline
        // Ollama daemon must not hide a healthy durable snapshot. Exact
        // resolution is stricter below so an uninspected live inventory cannot
        // be mistaken for proof that a model does not exist.
        match self
            .hub
            .runtime_inventory(&descriptor.runtime_id, &operation)
            .await
        {
            Ok(inventory) => {
                for model in inventory.models {
                    models
                        .entry(model.model_id.clone())
                        .or_insert(CatalogModel {
                            model_id: model.model_id,
                            source_id: self.source_id.clone(),
                            runtime_id: Some(descriptor.runtime_id.clone()),
                            backend: catalog_backend(descriptor.api_backend),
                            owned_by: runtime_owned_by(descriptor.api_backend).to_string(),
                        });
                }
            }
            Err(_) if context.cancellation().is_cancelled() => {
                return Err(CatalogSourceError::TimedOut)
            }
            Err(_) if tokio::time::Instant::now() >= context.deadline() => {
                return Err(CatalogSourceError::TimedOut)
            }
            // A list may safely retain the authenticated durable snapshot when
            // the live runtime is offline. An exact lookup may do so only when
            // that snapshot already proves the requested identity exists;
            // otherwise returning an empty inventory would manufacture a 404.
            Err(_)
                if query
                    .model_id()
                    .is_some_and(|model_id| !models.contains_key(model_id)) =>
            {
                return Err(CatalogSourceError::Unavailable)
            }
            Err(_) => {}
        }

        models.retain(|model_id, _| query.matches_model_id(model_id));
        Ok(models.into_values().take(max_models).collect())
    }
}

/// Snapshots current runtime registrations after the caller's authorization
/// gate.  Rebuilding this small vector per catalog operation prevents runtime
/// refreshes from leaving a long-lived source registry stale.
pub fn m3_runtime_catalog_sources(
    hub: Arc<M3RuntimeHub>,
) -> Result<Vec<Arc<dyn ModelCatalogSource>>, CatalogSourceError> {
    let runtimes = hub
        .list_runtimes()
        .map_err(|_| CatalogSourceError::Unavailable)?;
    Ok(runtimes
        .into_iter()
        .map(|runtime| {
            Arc::new(M3RuntimeCatalogSource::new(hub.clone(), runtime))
                as Arc<dyn ModelCatalogSource>
        })
        .collect())
}

/// App-independent snapshot seam for the legacy managed llama process.  Its
/// implementation may read an atomic/mutex-backed process status; it must not
/// perform network I/O.
pub trait LoadedLlamaSnapshot: Send + Sync {
    fn loaded_model_id(&self) -> Result<Option<String>, CatalogSourceError>;
}

#[derive(Clone)]
pub struct StaticLoadedLlamaSnapshot {
    pub model_id: Option<String>,
}

impl LoadedLlamaSnapshot for StaticLoadedLlamaSnapshot {
    fn loaded_model_id(&self) -> Result<Option<String>, CatalogSourceError> {
        Ok(self.model_id.clone())
    }
}

pub struct LegacyLlamaCatalogSource {
    snapshot: Arc<dyn LoadedLlamaSnapshot>,
    source_id: String,
    runtime_id: String,
}

/// Lazy OpenAI-compatible inventory for the headless CLI, where no Tauri
/// `LlamaState` snapshot exists. Construction stores only the endpoint;
/// `/v1/models` is fetched after the shared authorization gate.
pub struct OpenAiRuntimeCatalogSource {
    source_id: String,
    runtime_id: String,
    backend: CatalogBackend,
    owned_by: String,
    models_url: Url,
    client: Client,
}

impl OpenAiRuntimeCatalogSource {
    pub fn new(
        source_id: impl Into<String>,
        runtime_id: impl Into<String>,
        backend: CatalogBackend,
        owned_by: impl Into<String>,
        models_url: Url,
        client: Client,
    ) -> Self {
        Self {
            source_id: source_id.into(),
            runtime_id: runtime_id.into(),
            backend,
            owned_by: owned_by.into(),
            models_url,
            client,
        }
    }
}

#[derive(Deserialize)]
struct OpenAiModelsEnvelope {
    #[serde(default)]
    data: Vec<OpenAiModelRow>,
}

#[derive(Deserialize)]
struct OpenAiModelRow {
    id: String,
}

#[async_trait]
impl ModelCatalogSource for OpenAiRuntimeCatalogSource {
    fn source_id(&self) -> &str {
        &self.source_id
    }

    fn backend(&self) -> CatalogBackend {
        self.backend
    }

    fn runtime_id(&self) -> Option<&str> {
        Some(&self.runtime_id)
    }

    fn failure_policy(&self) -> CatalogSourceFailurePolicy {
        CatalogSourceFailurePolicy::OmitUnavailable
    }

    async fn list_models(
        &self,
        context: &CatalogRequestContext,
        query: CatalogSourceQuery<'_>,
        max_models: usize,
    ) -> Result<Vec<CatalogModel>, CatalogSourceError> {
        remaining_timeout(context)?;
        if max_models == 0 {
            return Ok(Vec::new());
        }
        let bytes = fetch_bounded_json(self.client.get(self.models_url.clone()), context).await?;
        let response: OpenAiModelsEnvelope =
            serde_json::from_slice(&bytes).map_err(|_| CatalogSourceError::InvalidResponse)?;
        let mut model_ids = response
            .data
            .into_iter()
            .map(|row| row.id.trim().to_string())
            .filter(|model_id| !model_id.is_empty())
            .filter(|model_id| query.matches_model_id(model_id))
            .collect::<Vec<_>>();
        model_ids.sort();
        model_ids.dedup();
        model_ids.truncate(max_models);
        Ok(model_ids
            .into_iter()
            .map(|model_id| CatalogModel {
                model_id,
                source_id: self.source_id.clone(),
                runtime_id: Some(self.runtime_id.clone()),
                backend: self.backend,
                owned_by: self.owned_by.clone(),
            })
            .collect())
    }
}

impl LegacyLlamaCatalogSource {
    pub fn new(
        snapshot: Arc<dyn LoadedLlamaSnapshot>,
        source_id: impl Into<String>,
        runtime_id: impl Into<String>,
    ) -> Self {
        Self {
            snapshot,
            source_id: source_id.into(),
            runtime_id: runtime_id.into(),
        }
    }
}

#[async_trait]
impl ModelCatalogSource for LegacyLlamaCatalogSource {
    fn source_id(&self) -> &str {
        &self.source_id
    }

    fn backend(&self) -> CatalogBackend {
        CatalogBackend::ManagedLocal
    }

    fn runtime_id(&self) -> Option<&str> {
        Some(&self.runtime_id)
    }

    async fn list_models(
        &self,
        context: &CatalogRequestContext,
        query: CatalogSourceQuery<'_>,
        max_models: usize,
    ) -> Result<Vec<CatalogModel>, CatalogSourceError> {
        remaining_timeout(context)?;
        if max_models == 0 {
            return Ok(Vec::new());
        }
        Ok(self
            .snapshot
            .loaded_model_id()?
            .into_iter()
            .filter(|model_id| query.matches_model_id(model_id))
            .map(|model_id| CatalogModel {
                model_id,
                source_id: self.source_id.clone(),
                runtime_id: Some(self.runtime_id.clone()),
                backend: CatalogBackend::ManagedLocal,
                owned_by: "llama.cpp".to_string(),
            })
            .collect())
    }
}

/// App-independent live Ollama inventory used by the legacy/headless side of
/// the shared server. M3 uses its registered runtime driver instead. An
/// unreachable daemon is omitted from broad listings but remains a typed
/// source failure for exact resolution, preventing guessed Ollama routing.
pub struct OllamaCatalogSource {
    source_id: String,
    runtime_id: String,
    tags_url: Url,
    client: Client,
}

impl OllamaCatalogSource {
    pub fn new(
        source_id: impl Into<String>,
        runtime_id: impl Into<String>,
        base_url: Url,
        client: Client,
    ) -> Result<Self, CatalogSourceError> {
        let tags_url = base_url
            .join("/api/tags")
            .map_err(|_| CatalogSourceError::InvalidResponse)?;
        Ok(Self {
            source_id: source_id.into(),
            runtime_id: runtime_id.into(),
            tags_url,
            client,
        })
    }
}

#[derive(Deserialize)]
struct OllamaTagsEnvelope {
    #[serde(default)]
    models: Vec<OllamaTagRow>,
}

#[derive(Deserialize)]
struct OllamaTagRow {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

#[async_trait]
impl ModelCatalogSource for OllamaCatalogSource {
    fn source_id(&self) -> &str {
        &self.source_id
    }

    fn backend(&self) -> CatalogBackend {
        CatalogBackend::Ollama
    }

    fn runtime_id(&self) -> Option<&str> {
        Some(&self.runtime_id)
    }

    fn failure_policy(&self) -> CatalogSourceFailurePolicy {
        CatalogSourceFailurePolicy::OmitUnavailable
    }

    async fn list_models(
        &self,
        context: &CatalogRequestContext,
        query: CatalogSourceQuery<'_>,
        max_models: usize,
    ) -> Result<Vec<CatalogModel>, CatalogSourceError> {
        remaining_timeout(context)?;
        if max_models == 0 {
            return Ok(Vec::new());
        }
        let bytes = fetch_bounded_json(self.client.get(self.tags_url.clone()), context).await?;
        let response: OllamaTagsEnvelope =
            serde_json::from_slice(&bytes).map_err(|_| CatalogSourceError::InvalidResponse)?;
        let mut model_ids = response
            .models
            .into_iter()
            .filter_map(|row| row.name.or(row.model))
            .map(|model_id| model_id.trim().to_string())
            .filter(|model_id| !model_id.is_empty())
            .filter(|model_id| query.matches_model_id(model_id))
            .collect::<Vec<_>>();
        model_ids.sort();
        model_ids.dedup();
        model_ids.truncate(max_models);
        Ok(model_ids
            .into_iter()
            .map(|model_id| CatalogModel {
                model_id,
                source_id: self.source_id.clone(),
                runtime_id: Some(self.runtime_id.clone()),
                backend: CatalogBackend::Ollama,
                owned_by: "ollama".to_string(),
            })
            .collect())
    }
}

/// Keychain seam for provider model discovery.  It is invoked from
/// `list_models`, never construction, so an unauthorized request cannot even
/// cause a credential lookup.
pub trait ProviderCredentialSource: Send + Sync {
    fn bearer_token(&self, provider_id: &str) -> Result<Option<String>, CatalogSourceError>;
}

pub struct ProviderCredentialResolver {
    resolver:
        Arc<dyn Fn(&str) -> Result<Option<String>, CatalogSourceError> + Send + Sync + 'static>,
}

impl ProviderCredentialResolver {
    pub fn new(
        resolver: impl Fn(&str) -> Result<Option<String>, CatalogSourceError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            resolver: Arc::new(resolver),
        }
    }
}

impl ProviderCredentialSource for ProviderCredentialResolver {
    fn bearer_token(&self, provider_id: &str) -> Result<Option<String>, CatalogSourceError> {
        (self.resolver)(provider_id)
    }
}

pub struct CloudProviderCatalogSource {
    provider_id: String,
    models_url: Url,
    client: Client,
    credentials: Arc<dyn ProviderCredentialSource>,
}

impl CloudProviderCatalogSource {
    pub fn new(
        provider_id: impl Into<String>,
        base_url: Url,
        client: Client,
        credentials: Arc<dyn ProviderCredentialSource>,
    ) -> Result<Self, CatalogSourceError> {
        let provider_id = provider_id.into();
        let mut models_url = base_url;
        let path = format!("{}/models", models_url.path().trim_end_matches('/'));
        models_url.set_path(&path);
        models_url.set_query(None);
        models_url.set_fragment(None);
        Ok(Self {
            provider_id,
            models_url,
            client,
            credentials,
        })
    }
}

#[derive(Deserialize)]
struct ProviderModelsEnvelope {
    #[serde(default)]
    data: Vec<ProviderModelRow>,
}

#[derive(Deserialize)]
struct ProviderModelRow {
    id: String,
}

#[async_trait]
impl ModelCatalogSource for CloudProviderCatalogSource {
    fn source_id(&self) -> &str {
        &self.provider_id
    }

    fn backend(&self) -> CatalogBackend {
        CatalogBackend::CloudProvider
    }

    fn runtime_id(&self) -> Option<&str> {
        None
    }

    fn failure_policy(&self) -> CatalogSourceFailurePolicy {
        CatalogSourceFailurePolicy::OmitUnavailable
    }

    async fn list_models(
        &self,
        context: &CatalogRequestContext,
        query: CatalogSourceQuery<'_>,
        max_models: usize,
    ) -> Result<Vec<CatalogModel>, CatalogSourceError> {
        remaining_timeout(context)?;
        if max_models == 0 {
            return Ok(Vec::new());
        }
        let token = self
            .credentials
            .bearer_token(&self.provider_id)?
            .filter(|token| !token.trim().is_empty())
            .ok_or(CatalogSourceError::PermissionDenied)?;
        let request = crate::providers::add_anthropic_headers(
            self.client.get(self.models_url.clone()).bearer_auth(&token),
            &self.provider_id,
            &token,
        );
        let bytes = fetch_bounded_json(request, context).await?;
        let response: ProviderModelsEnvelope =
            serde_json::from_slice(&bytes).map_err(|_| CatalogSourceError::InvalidResponse)?;
        let mut models = response
            .data
            .into_iter()
            .filter_map(|row| {
                let id = row.id.trim();
                let model_id = format!("{}/{}", self.provider_id, id);
                (!id.is_empty() && query.matches_model_id(&model_id)).then(|| CatalogModel {
                    model_id,
                    source_id: self.provider_id.clone(),
                    runtime_id: None,
                    backend: CatalogBackend::CloudProvider,
                    owned_by: self.provider_id.clone(),
                })
            })
            .collect::<Vec<_>>();
        models.sort_by(|left, right| left.model_id.cmp(&right.model_id));
        models.dedup_by(|left, right| left.model_id == right.model_id);
        models.truncate(max_models);
        Ok(models)
    }
}

async fn fetch_bounded_json(
    request: reqwest::RequestBuilder,
    context: &CatalogRequestContext,
) -> Result<Vec<u8>, CatalogSourceError> {
    let response = tokio::select! {
        biased;
        _ = context.cancellation().cancelled() => return Err(CatalogSourceError::TimedOut),
        result = tokio::time::timeout_at(context.deadline(), crate::egress::send(request)) => {
            result.map_err(|_| CatalogSourceError::TimedOut)?
                .map_err(|_| CatalogSourceError::Unavailable)?
        }
    };
    match response.status() {
        status if status.is_success() => {}
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            return Err(CatalogSourceError::PermissionDenied)
        }
        StatusCode::TOO_MANY_REQUESTS => return Err(CatalogSourceError::Overloaded),
        _ => return Err(CatalogSourceError::Unavailable),
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_CATALOG_HTTP_BYTES as u64)
    {
        return Err(CatalogSourceError::InvalidResponse);
    }
    let mut output = Vec::new();
    let mut stream = response.bytes_stream();
    loop {
        let next = tokio::select! {
            biased;
            _ = context.cancellation().cancelled() => return Err(CatalogSourceError::TimedOut),
            result = tokio::time::timeout_at(context.deadline(), stream.next()) => {
                result.map_err(|_| CatalogSourceError::TimedOut)?
            }
        };
        let Some(chunk) = next else { break };
        let chunk = chunk.map_err(|_| CatalogSourceError::Unavailable)?;
        if output.len().saturating_add(chunk.len()) > MAX_CATALOG_HTTP_BYTES {
            return Err(CatalogSourceError::InvalidResponse);
        }
        output.extend_from_slice(&chunk);
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::m3_runtime_hub::{
        M3CanonicalStreamSink, M3HardwareProbe, M3HubConfig, M3HubFuture, M3HubResult,
        M3ResolvedModel, M3RuntimeDescriptor, M3RuntimeDriver, M3RuntimeHubDependencies,
        M3RuntimeKind, M3RuntimeMetricsView, M3RuntimeStatusView, ReqwestM3DownloadTransport,
        SystemM3Clock,
    };
    use crate::runtime_adapter::{
        HardwareSnapshot, KeepAlive, ModelCapabilities, RuntimeInventory, RuntimeLogTail,
        RuntimeModel, SettingValue,
    };
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct Snapshot {
        calls: Arc<AtomicUsize>,
        model: Option<String>,
    }

    impl LoadedLlamaSnapshot for Snapshot {
        fn loaded_model_id(&self) -> Result<Option<String>, CatalogSourceError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.model.clone())
        }
    }

    struct Credentials {
        calls: Arc<AtomicUsize>,
        token: Option<String>,
    }

    impl ProviderCredentialSource for Credentials {
        fn bearer_token(&self, _provider_id: &str) -> Result<Option<String>, CatalogSourceError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.token.clone())
        }
    }

    struct InventoryTestRuntime;

    impl M3RuntimeDriver for InventoryTestRuntime {
        fn descriptor(&self) -> M3RuntimeDescriptor {
            M3RuntimeDescriptor {
                runtime_id: "inventory-runtime".to_string(),
                kind: M3RuntimeKind::Ollama,
                label: "Inventory runtime".to_string(),
                managed: false,
                api_backend: ApiBackend::Ollama,
            }
        }

        fn capabilities(&self) -> M3RuntimeCapabilityView {
            M3RuntimeCapabilityView {
                descriptor: self.descriptor(),
                can_load: false,
                can_unload: false,
                can_logs: false,
                can_metrics: false,
                can_infer: false,
                can_embed: false,
                settings: Vec::new(),
            }
        }

        fn validate_config(&self, _values: &BTreeMap<String, SettingValue>) -> M3HubResult<()> {
            Ok(())
        }

        fn status<'a>(
            &'a self,
            _context: &'a M3OperationContext,
        ) -> M3HubFuture<'a, M3RuntimeStatusView> {
            panic!("catalog inventory test must not request runtime status")
        }

        fn inventory<'a>(
            &'a self,
            _context: &'a M3OperationContext,
        ) -> M3HubFuture<'a, RuntimeInventory> {
            Box::pin(async move {
                Ok(RuntimeInventory {
                    schema_version: crate::runtime_adapter::RUNTIME_ADAPTER_SCHEMA_VERSION,
                    runtime_id: "inventory-runtime".to_string(),
                    models: ["alpha", "zeta"]
                        .into_iter()
                        .map(|model_id| RuntimeModel {
                            model_id: model_id.to_string(),
                            display_name: model_id.to_string(),
                            size_bytes: 1,
                            local_path: None,
                            digest: None,
                            modified_at: None,
                            capabilities: ModelCapabilities::default(),
                            metadata: BTreeMap::new(),
                        })
                        .collect(),
                    captured_at_ms: 1,
                })
            })
        }

        fn load<'a>(
            &'a self,
            _model: &'a M3ResolvedModel,
            _settings: &'a BTreeMap<String, SettingValue>,
            _keep_alive: Option<KeepAlive>,
            _replace_existing: bool,
            _context: &'a M3OperationContext,
        ) -> M3HubFuture<'a, ()> {
            panic!("catalog inventory test must not load a model")
        }

        fn unload<'a>(
            &'a self,
            _model_id: &'a str,
            _force_exact_owner: bool,
            _context: &'a M3OperationContext,
        ) -> M3HubFuture<'a, ()> {
            panic!("catalog inventory test must not unload a model")
        }

        fn logs<'a>(
            &'a self,
            _max_bytes: usize,
            _context: &'a M3OperationContext,
        ) -> M3HubFuture<'a, RuntimeLogTail> {
            panic!("catalog inventory test must not request logs")
        }

        fn metrics<'a>(
            &'a self,
            _context: &'a M3OperationContext,
        ) -> M3HubFuture<'a, M3RuntimeMetricsView> {
            panic!("catalog inventory test must not request metrics")
        }

        fn complete<'a>(
            &'a self,
            _request: &'a crate::compatibility_hub::CanonicalInferenceRequest,
            _context: &'a M3OperationContext,
        ) -> M3HubFuture<'a, crate::compatibility_hub::CanonicalInferenceResponse> {
            panic!("catalog inventory test must not run inference")
        }

        fn stream<'a>(
            &'a self,
            _request: &'a crate::compatibility_hub::CanonicalInferenceRequest,
            _sink: &'a mut dyn M3CanonicalStreamSink,
            _context: &'a M3OperationContext,
        ) -> M3HubFuture<'a, ()> {
            panic!("catalog inventory test must not stream inference")
        }

        fn cancel<'a>(
            &'a self,
            _request_id: &'a str,
            _context: &'a M3OperationContext,
        ) -> M3HubFuture<'a, bool> {
            panic!("catalog inventory test must not cancel inference")
        }
    }

    struct InventoryTestHardware;

    impl M3HardwareProbe for InventoryTestHardware {
        fn snapshot(&self) -> M3HubResult<HardwareSnapshot> {
            panic!("catalog inventory test must not inspect hardware")
        }
    }

    struct TemporaryHubRoot(PathBuf);

    impl TemporaryHubRoot {
        fn new() -> Self {
            static NEXT_ROOT: AtomicUsize = AtomicUsize::new(1);
            Self(std::env::temp_dir().join(format!(
                "http-model-source-inventory-{}-{}",
                std::process::id(),
                NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
            )))
        }
    }

    impl Drop for TemporaryHubRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn context() -> CatalogRequestContext {
        CatalogRequestContext::with_timeout(
            tokio_util::sync::CancellationToken::new(),
            Duration::from_secs(1),
        )
    }

    async fn one_response_server(status: &str, body: String) -> Url {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture server");
        let address = listener.local_addr().expect("fixture address");
        let status = status.to_string();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept fixture request");
            let mut request = vec![0u8; 8 * 1024];
            let _ = stream.read(&mut request).await.expect("read request");
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("write fixture response");
        });
        Url::parse(&format!("http://{address}/v1/")).expect("fixture URL")
    }

    async fn capturing_response_server(
        status: &str,
        body: String,
    ) -> (Url, tokio::sync::oneshot::Receiver<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture server");
        let address = listener.local_addr().expect("fixture address");
        let status = status.to_string();
        let (request_tx, request_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept fixture request");
            let mut request = vec![0u8; 8 * 1024];
            let read = stream.read(&mut request).await.expect("read request");
            let _ = request_tx.send(String::from_utf8_lossy(&request[..read]).into_owned());
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("write fixture response");
        });
        (
            Url::parse(&format!("http://{address}/v1/")).expect("fixture URL"),
            request_rx,
        )
    }

    async fn hanging_response_server(
        send_partial_body: bool,
    ) -> (
        Url,
        tokio::sync::oneshot::Receiver<()>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind hanging fixture server");
        let address = listener.local_addr().expect("hanging fixture address");
        let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept fixture request");
            let mut request = vec![0u8; 8 * 1024];
            let _ = stream
                .read(&mut request)
                .await
                .expect("read fixture request");
            if send_partial_body {
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 1024\r\nconnection: close\r\n\r\n{\"data\":[",
                    )
                    .await
                    .expect("write partial fixture response");
            }
            let _ = reached_tx.send(());
            std::future::pending::<()>().await;
        });
        (
            Url::parse(&format!("http://{address}/v1/")).expect("hanging fixture URL"),
            reached_rx,
            task,
        )
    }

    #[tokio::test]
    async fn legacy_source_is_lazy_bounded_and_runtime_stamped() {
        let calls = Arc::new(AtomicUsize::new(0));
        let source = LegacyLlamaCatalogSource::new(
            Arc::new(Snapshot {
                calls: calls.clone(),
                model: Some("local.gguf".to_string()),
            }),
            "legacy-llama",
            "legacy-llama-runtime",
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0, "constructor must be pure");
        assert!(source
            .list_models(&context(), CatalogSourceQuery::List, 0)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 0, "zero cap avoids snapshot");
        let models = source
            .list_models(&context(), CatalogSourceQuery::List, 1)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(models[0].model_id, "local.gguf");
        assert_eq!(
            models[0].runtime_id.as_deref(),
            Some("legacy-llama-runtime")
        );

        let missing = source
            .list_models(
                &context(),
                CatalogSourceQuery::Resolve {
                    model_id: "another.gguf",
                },
                1,
            )
            .await
            .unwrap();
        assert!(
            missing.is_empty(),
            "exact lookup must not return another model"
        );
    }

    #[tokio::test]
    async fn m3_runtime_exact_lookup_selects_target_before_cap() {
        let root = TemporaryHubRoot::new();
        let runtime = Arc::new(InventoryTestRuntime);
        let capabilities = runtime.capabilities();
        let hub = Arc::new(
            M3RuntimeHub::new(
                &root.0,
                M3HubConfig::default(),
                M3RuntimeHubDependencies {
                    clock: Arc::new(SystemM3Clock),
                    hardware: Arc::new(InventoryTestHardware),
                    download: Arc::new(
                        ReqwestM3DownloadTransport::new().expect("test download transport"),
                    ),
                    catalogs: Vec::new(),
                    runtimes: vec![runtime],
                    runtime_reconciler: None,
                    lan_factory: None,
                },
            )
            .expect("test M3 hub"),
        );
        let source = M3RuntimeCatalogSource::new(hub, capabilities);

        let models = source
            .list_models(
                &context(),
                CatalogSourceQuery::Resolve { model_id: "zeta" },
                1,
            )
            .await
            .unwrap();

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].model_id, "zeta");
    }

    #[tokio::test]
    async fn openai_runtime_exact_lookup_selects_target_before_cap() {
        let models_url = one_response_server(
            "200 OK",
            serde_json::json!({
                "data": [
                    {"id": "alpha"},
                    {"id": "zeta"}
                ]
            })
            .to_string(),
        )
        .await;
        let source = OpenAiRuntimeCatalogSource::new(
            "openai-runtime",
            "runtime-a",
            CatalogBackend::ManagedLocal,
            "local",
            models_url,
            Client::new(),
        );

        let models = source
            .list_models(
                &context(),
                CatalogSourceQuery::Resolve { model_id: "zeta" },
                1,
            )
            .await
            .unwrap();

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].model_id, "zeta");
    }

    #[tokio::test]
    async fn ollama_source_is_lazy_bounded_sorted_and_runtime_stamped() {
        let base_url = one_response_server(
            "200 OK",
            serde_json::json!({
                "models": [
                    {"name": "zeta:latest"},
                    {"model": "alpha:latest"},
                    {"name": "alpha:latest"},
                    {"name": "   "}
                ]
            })
            .to_string(),
        )
        .await;
        let source =
            OllamaCatalogSource::new("ollama-local", "ollama-runtime", base_url, Client::new())
                .unwrap();
        assert!(source.tags_url.path().ends_with("/api/tags"));
        assert_eq!(
            source.failure_policy(),
            CatalogSourceFailurePolicy::OmitUnavailable
        );

        let models = source
            .list_models(&context(), CatalogSourceQuery::List, 1)
            .await
            .unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].model_id, "alpha:latest");
        assert_eq!(models[0].source_id, "ollama-local");
        assert_eq!(models[0].runtime_id.as_deref(), Some("ollama-runtime"));
        assert_eq!(models[0].backend, CatalogBackend::Ollama);
    }

    #[tokio::test]
    async fn ollama_exact_lookup_selects_target_before_cap() {
        let base_url = one_response_server(
            "200 OK",
            serde_json::json!({
                "models": [
                    {"name": "alpha:latest"},
                    {"name": "zeta:latest"}
                ]
            })
            .to_string(),
        )
        .await;
        let source =
            OllamaCatalogSource::new("ollama-local", "ollama-runtime", base_url, Client::new())
                .unwrap();

        let models = source
            .list_models(
                &context(),
                CatalogSourceQuery::Resolve {
                    model_id: "zeta:latest",
                },
                1,
            )
            .await
            .unwrap();

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].model_id, "zeta:latest");
    }

    #[tokio::test]
    async fn cancelled_context_never_reads_local_snapshot_or_credentials() {
        let cancellation = tokio_util::sync::CancellationToken::new();
        cancellation.cancel();
        let context = CatalogRequestContext::with_timeout(cancellation, Duration::from_secs(1));
        let snapshot_calls = Arc::new(AtomicUsize::new(0));
        let legacy = LegacyLlamaCatalogSource::new(
            Arc::new(Snapshot {
                calls: snapshot_calls.clone(),
                model: Some("local.gguf".to_string()),
            }),
            "legacy",
            "legacy-runtime",
        );
        assert_eq!(
            legacy
                .list_models(&context, CatalogSourceQuery::List, 1)
                .await,
            Err(CatalogSourceError::TimedOut)
        );
        assert_eq!(snapshot_calls.load(Ordering::SeqCst), 0);

        let credential_calls = Arc::new(AtomicUsize::new(0));
        let provider = CloudProviderCatalogSource::new(
            "example",
            Url::parse("https://example.invalid/v1/").unwrap(),
            Client::new(),
            Arc::new(Credentials {
                calls: credential_calls.clone(),
                token: Some("secret".to_string()),
            }),
        )
        .unwrap();
        assert_eq!(
            provider
                .list_models(&context, CatalogSourceQuery::List, 10)
                .await,
            Err(CatalogSourceError::TimedOut)
        );
        assert_eq!(credential_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn provider_constructor_is_lazy_and_joins_models_endpoint() {
        let calls = Arc::new(AtomicUsize::new(0));
        let source = CloudProviderCatalogSource::new(
            "openai",
            Url::parse("https://api.openai.com/v1/").unwrap(),
            Client::new(),
            Arc::new(Credentials {
                calls: calls.clone(),
                token: None,
            }),
        )
        .unwrap();
        assert_eq!(
            source.models_url.as_str(),
            "https://api.openai.com/v1/models"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(source.source_id(), "openai");
        assert_eq!(source.backend(), CatalogBackend::CloudProvider);
        assert_eq!(source.runtime_id(), None);
    }

    #[tokio::test]
    async fn provider_fetch_prefixes_sorts_deduplicates_and_honors_cap() {
        let base_url = one_response_server(
            "200 OK",
            serde_json::json!({
                "data": [
                    {"id": "zeta"},
                    {"id": "alpha"},
                    {"id": "alpha"},
                    {"id": "   "}
                ]
            })
            .to_string(),
        )
        .await;
        let calls = Arc::new(AtomicUsize::new(0));
        let source = CloudProviderCatalogSource::new(
            "acme",
            base_url,
            Client::new(),
            Arc::new(Credentials {
                calls: calls.clone(),
                token: Some("one-time-secret".to_string()),
            }),
        )
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let models = source
            .list_models(&context(), CatalogSourceQuery::List, 1)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].model_id, "acme/alpha");
        assert_eq!(models[0].source_id, "acme");
        assert_eq!(models[0].backend, CatalogBackend::CloudProvider);
        assert_eq!(models[0].owned_by, "acme");
    }

    #[tokio::test]
    async fn provider_exact_lookup_selects_prefixed_target_before_cap() {
        let base_url = one_response_server(
            "200 OK",
            serde_json::json!({
                "data": [
                    {"id": "alpha"},
                    {"id": "zeta"}
                ]
            })
            .to_string(),
        )
        .await;
        let source = CloudProviderCatalogSource::new(
            "acme",
            base_url,
            Client::new(),
            Arc::new(Credentials {
                calls: Arc::new(AtomicUsize::new(0)),
                token: Some("one-time-secret".to_string()),
            }),
        )
        .unwrap();

        let models = source
            .list_models(
                &context(),
                CatalogSourceQuery::Resolve {
                    model_id: "acme/zeta",
                },
                1,
            )
            .await
            .unwrap();

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].model_id, "acme/zeta");
    }

    #[tokio::test]
    async fn anthropic_provider_discovery_sends_native_auth_headers() {
        let (base_url, request_rx) = capturing_response_server(
            "200 OK",
            serde_json::json!({"data": [{"id": "claude-test"}]}).to_string(),
        )
        .await;
        let source = CloudProviderCatalogSource::new(
            "anthropic",
            base_url,
            Client::new(),
            Arc::new(Credentials {
                calls: Arc::new(AtomicUsize::new(0)),
                token: Some("anthropic-secret".to_string()),
            }),
        )
        .unwrap();

        let models = source
            .list_models(&context(), CatalogSourceQuery::List, 10)
            .await
            .unwrap();
        let request = request_rx.await.expect("capture provider request");
        let request = request.to_ascii_lowercase();

        assert_eq!(models[0].model_id, "anthropic/claude-test");
        assert!(request.starts_with("get /v1/models http/1.1\r\n"));
        assert!(request.contains("authorization: bearer anthropic-secret\r\n"));
        assert!(request.contains("x-api-key: anthropic-secret\r\n"));
        assert!(request.contains("anthropic-version: 2023-06-01\r\n"));
    }

    #[tokio::test]
    async fn provider_auth_failure_is_safe_and_typed() {
        let base_url = one_response_server(
            "401 Unauthorized",
            serde_json::json!({"private": "upstream detail must not escape"}).to_string(),
        )
        .await;
        let source = CloudProviderCatalogSource::new(
            "acme",
            base_url,
            Client::new(),
            Arc::new(Credentials {
                calls: Arc::new(AtomicUsize::new(0)),
                token: Some("bad-secret".to_string()),
            }),
        )
        .unwrap();
        assert_eq!(
            source
                .list_models(&context(), CatalogSourceQuery::List, 10)
                .await,
            Err(CatalogSourceError::PermissionDenied)
        );
    }

    #[tokio::test]
    async fn provider_cancellation_interrupts_an_in_flight_request() {
        let (base_url, reached, server_task) = hanging_response_server(false).await;
        let credential_calls = Arc::new(AtomicUsize::new(0));
        let source = Arc::new(
            CloudProviderCatalogSource::new(
                "acme",
                base_url,
                Client::new(),
                Arc::new(Credentials {
                    calls: credential_calls.clone(),
                    token: Some("secret".to_string()),
                }),
            )
            .unwrap(),
        );
        let cancellation = tokio_util::sync::CancellationToken::new();
        let context =
            CatalogRequestContext::with_timeout(cancellation.clone(), Duration::from_secs(5));
        let request_task = tokio::spawn(async move {
            source
                .list_models(&context, CatalogSourceQuery::List, 10)
                .await
        });

        reached.await.expect("provider request reached fixture");
        cancellation.cancel();
        let result = tokio::time::timeout(Duration::from_secs(1), request_task)
            .await
            .expect("provider cancellation must be prompt")
            .expect("provider request task must not panic");
        server_task.abort();

        assert_eq!(result, Err(CatalogSourceError::TimedOut));
        assert_eq!(credential_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn provider_absolute_deadline_interrupts_a_streaming_body() {
        let (base_url, reached, server_task) = hanging_response_server(true).await;
        let credential_calls = Arc::new(AtomicUsize::new(0));
        let source = CloudProviderCatalogSource::new(
            "acme",
            base_url,
            Client::new(),
            Arc::new(Credentials {
                calls: credential_calls.clone(),
                token: Some("secret".to_string()),
            }),
        )
        .unwrap();
        let context = CatalogRequestContext::with_timeout(
            tokio_util::sync::CancellationToken::new(),
            Duration::from_millis(250),
        );

        let result = source
            .list_models(&context, CatalogSourceQuery::List, 10)
            .await;
        reached.await.expect("provider response started streaming");
        server_task.abort();

        assert_eq!(result, Err(CatalogSourceError::TimedOut));
        assert_eq!(credential_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn backend_and_owner_mapping_covers_every_runtime_kind() {
        assert_eq!(
            catalog_backend(ApiBackend::ManagedLocal),
            CatalogBackend::ManagedLocal
        );
        assert_eq!(catalog_backend(ApiBackend::Ollama), CatalogBackend::Ollama);
        assert_eq!(catalog_backend(ApiBackend::Mlx), CatalogBackend::Mlx);
        assert_eq!(
            catalog_backend(ApiBackend::CloudProvider),
            CatalogBackend::CloudProvider
        );
        assert_eq!(runtime_owned_by(ApiBackend::ManagedLocal), "little-monkey");
        assert_eq!(runtime_owned_by(ApiBackend::Ollama), "ollama");
        assert_eq!(runtime_owned_by(ApiBackend::Mlx), "mlx");
        let _all_runtime_kinds = [
            crate::m3_runtime_hub::M3RuntimeKind::Ollama,
            crate::m3_runtime_hub::M3RuntimeKind::LlamaCpp,
            crate::m3_runtime_hub::M3RuntimeKind::Mlx,
        ];
    }
}
