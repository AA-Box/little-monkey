//! One logical HTTP service and lifecycle for the primary loopback API and an
//! optional policy/TLS endpoint.
//!
//! A service can legitimately own more than one socket: published Local Apps
//! must keep their loopback URL while a user may also opt into a TLS LAN
//! endpoint.  D1's invariant is therefore one route authority, admission
//! domain, endpoint plan, start/stop transaction, and status surface—not an
//! unsafe attempt to make one socket be plaintext loopback and TLS LAN at the
//! same time.

use std::net::IpAddr;
use std::sync::{Arc, Mutex, MutexGuard};

use serde::Serialize;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::compatibility_hub::{LanServerPolicy, TlsPolicy};
use crate::http_policy::{RequestAdmission, MAX_ACTIVE_REQUESTS};
use crate::http_route_registry::ListenerExposure;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndpointTransport {
    Plaintext,
    Tls,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PrimaryEndpointConfig {
    pub port: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PrimaryServiceConfig {
    pub port: u16,
    pub require_token: bool,
    pub expose_ollama: bool,
    pub expose_providers: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UnifiedGenerationSpec {
    pub primary: Option<PrimaryServiceConfig>,
    pub policy_endpoint: Option<LanServerPolicy>,
    /// Pairing remains accepted on the primary endpoint even when the
    /// separately exposed policy socket is disabled.
    pub pairing_policy: Option<LanServerPolicy>,
}

impl UnifiedGenerationSpec {
    pub fn endpoint_plan(&self) -> Result<UnifiedEndpointPlan, String> {
        UnifiedEndpointPlan::build(
            self.primary
                .as_ref()
                .map(|primary| PrimaryEndpointConfig { port: primary.port }),
            self.policy_endpoint.clone(),
        )
    }

    pub fn primary_enabled(&self) -> bool {
        self.primary.is_some()
    }

    pub fn policy_enabled(&self) -> bool {
        self.policy_endpoint.is_some()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnifiedEndpoint {
    pub key: String,
    pub bind_address: IpAddr,
    pub port: u16,
    pub exposure: ListenerExposure,
    pub transport: EndpointTransport,
    /// This socket serves legacy/shared primary routes.
    pub primary: bool,
    /// This socket applies the persisted M3 pairing/CORS/backend policy.
    pub policy: Option<LanServerPolicy>,
}

impl UnifiedEndpoint {
    pub fn address(&self) -> String {
        format!("{}:{}", self.bind_address, self.port)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UnifiedEndpointPlan {
    pub endpoints: Vec<UnifiedEndpoint>,
}

impl UnifiedEndpointPlan {
    pub fn build(
        primary: Option<PrimaryEndpointConfig>,
        policy: Option<LanServerPolicy>,
    ) -> Result<Self, String> {
        let mut endpoints = Vec::new();
        if let Some(primary) = primary {
            if primary.port == 0 {
                return Err("primary HTTP endpoint port must be non-zero".to_string());
            }
            let bind_address = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
            endpoints.push(UnifiedEndpoint {
                key: endpoint_key(bind_address, primary.port, EndpointTransport::Plaintext),
                bind_address,
                port: primary.port,
                exposure: ListenerExposure::Loopback,
                transport: EndpointTransport::Plaintext,
                primary: true,
                policy: None,
            });
        }

        if let Some(policy) = policy {
            policy.validate().map_err(|error| error.to_string())?;
            let bind_address = policy
                .bind_address
                .parse::<IpAddr>()
                .map_err(|error| format!("invalid policy bind address: {error}"))?;
            let exposure = if bind_address.is_loopback() {
                ListenerExposure::Loopback
            } else {
                ListenerExposure::Lan
            };
            let transport = match policy.tls {
                TlsPolicy::Disabled => EndpointTransport::Plaintext,
                TlsPolicy::Certificate { .. } => EndpointTransport::Tls,
            };

            if let Some(existing) = endpoints.iter_mut().find(|endpoint| {
                endpoint.bind_address == bind_address && endpoint.port == policy.port
            }) {
                if existing.transport != transport {
                    return Err(format!(
                        "HTTP endpoint {} cannot be both plaintext and TLS",
                        existing.address()
                    ));
                }
                existing.policy = Some(policy);
            } else {
                endpoints.push(UnifiedEndpoint {
                    key: endpoint_key(bind_address, policy.port, transport),
                    bind_address,
                    port: policy.port,
                    exposure,
                    transport,
                    primary: false,
                    policy: Some(policy),
                });
            }
        }

        endpoints.sort_by(|left, right| {
            right
                .primary
                .cmp(&left.primary)
                .then_with(|| left.bind_address.cmp(&right.bind_address))
                .then_with(|| left.port.cmp(&right.port))
                .then_with(|| transport_rank(left.transport).cmp(&transport_rank(right.transport)))
        });
        Ok(Self { endpoints })
    }
}

fn transport_rank(transport: EndpointTransport) -> u8 {
    match transport {
        EndpointTransport::Plaintext => 0,
        EndpointTransport::Tls => 1,
    }
}

fn endpoint_key(address: IpAddr, port: u16, transport: EndpointTransport) -> String {
    let scheme = match transport {
        EndpointTransport::Plaintext => "http",
        EndpointTransport::Tls => "https",
    };
    format!("{scheme}://{address}:{port}")
}

pub(crate) struct RunningEndpoint {
    pub endpoint: UnifiedEndpoint,
    pub task: JoinHandle<()>,
}

pub(crate) struct UnifiedServerInner {
    pub status: String,
    pub generation: u64,
    pub started_at_ms: Option<u64>,
    pub last_error: Option<String>,
    pub shutdown: Option<CancellationToken>,
    pub endpoints: Vec<RunningEndpoint>,
    /// One permit pool and one counter domain for every socket in this
    /// generation.  A LAN/TLS endpoint is not a second server.
    pub admission: Arc<RequestAdmission>,
    /// Desired surfaces are kept separately from bound sockets so the legacy
    /// command wrappers can independently enable/disable their historical
    /// surface while every change is still one serialized reconciliation.
    pub primary_enabled: bool,
    pub policy_enabled: bool,
    pub applied_spec: Option<UnifiedGenerationSpec>,
}

impl Default for UnifiedServerInner {
    fn default() -> Self {
        Self {
            status: "stopped".to_string(),
            generation: 0,
            started_at_ms: None,
            last_error: None,
            shutdown: None,
            endpoints: Vec::new(),
            admission: Arc::new(RequestAdmission::new(MAX_ACTIVE_REQUESTS)),
            primary_enabled: false,
            policy_enabled: false,
            applied_spec: None,
        }
    }
}

/// The only lifecycle lock and task owner for every HTTP endpoint.
pub struct UnifiedHttpServerState {
    pub(crate) lifecycle: tokio::sync::Mutex<()>,
    inner: Mutex<UnifiedServerInner>,
}

impl Default for UnifiedHttpServerState {
    fn default() -> Self {
        Self {
            lifecycle: tokio::sync::Mutex::new(()),
            inner: Mutex::new(UnifiedServerInner::default()),
        }
    }
}

impl UnifiedHttpServerState {
    pub(crate) fn lock(&self) -> Result<MutexGuard<'_, UnifiedServerInner>, String> {
        self.inner
            .lock()
            .map_err(|_| "unified HTTP server state lock is poisoned".to_string())
    }

    pub fn snapshot(&self) -> Result<UnifiedHttpServerStatus, String> {
        let inner = self.lock()?;
        Ok(UnifiedHttpServerStatus {
            status: inner.status.clone(),
            generation: inner.generation,
            started_at_ms: inner.started_at_ms,
            last_error: inner.last_error.clone(),
            primary_enabled: inner.primary_enabled,
            policy_enabled: inner.policy_enabled,
            endpoints: inner
                .endpoints
                .iter()
                .map(|running| EndpointStatus {
                    key: running.endpoint.key.clone(),
                    bind_address: running.endpoint.bind_address.to_string(),
                    port: running.endpoint.port,
                    exposure: match running.endpoint.exposure {
                        ListenerExposure::Loopback => "loopback".to_string(),
                        ListenerExposure::Lan => "lan".to_string(),
                    },
                    tls: running.endpoint.transport == EndpointTransport::Tls,
                    primary: running.endpoint.primary,
                })
                .collect(),
            request_count: inner.admission.request_count(),
            active_requests: inner.admission.active_requests(),
            last_request_at_ms: {
                let value = inner
                    .admission
                    .counters()
                    .last_request_at_ms
                    .load(std::sync::atomic::Ordering::Relaxed);
                (value != 0).then_some(value)
            },
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointStatus {
    pub key: String,
    pub bind_address: String,
    pub port: u16,
    pub exposure: String,
    pub tls: bool,
    pub primary: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnifiedHttpServerStatus {
    pub status: String,
    pub generation: u64,
    pub started_at_ms: Option<u64>,
    pub last_error: Option<String>,
    pub primary_enabled: bool,
    pub policy_enabled: bool,
    pub endpoints: Vec<EndpointStatus>,
    pub request_count: u64,
    pub active_requests: usize,
    pub last_request_at_ms: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compatibility_hub::ApiBackend;
    use std::collections::BTreeSet;

    #[test]
    fn identical_plaintext_loopback_endpoints_collapse_to_one_socket() {
        let mut policy = LanServerPolicy::default();
        policy.port = 4_321;
        let plan = UnifiedEndpointPlan::build(
            Some(PrimaryEndpointConfig { port: 4_321 }),
            Some(policy.clone()),
        )
        .expect("endpoint plan");
        assert_eq!(plan.endpoints.len(), 1);
        assert!(plan.endpoints[0].primary);
        assert_eq!(plan.endpoints[0].policy, Some(policy));
    }

    #[test]
    fn distinct_lan_policy_is_a_second_endpoint_in_the_same_plan() {
        let mut policy = LanServerPolicy::default();
        policy.bind_address = "192.168.1.20".to_string();
        policy.port = 8_443;
        policy.tls = TlsPolicy::Certificate {
            certificate_sha256: "11".repeat(32),
            private_key_reference: "keychain:server".to_string(),
            minimum_version: "1.3".to_string(),
        };
        policy.cors_allowlist = vec!["https://client.example".to_string()];
        policy.allowed_backends = BTreeSet::from([ApiBackend::ManagedLocal]);
        let plan =
            UnifiedEndpointPlan::build(Some(PrimaryEndpointConfig { port: 1_234 }), Some(policy))
                .expect("endpoint plan");
        assert_eq!(plan.endpoints.len(), 2);
        assert!(plan.endpoints[0].primary);
        assert_eq!(plan.endpoints[1].exposure, ListenerExposure::Lan);
        assert_eq!(plan.endpoints[1].transport, EndpointTransport::Tls);
    }

    #[test]
    fn same_socket_cannot_be_both_primary_plaintext_and_policy_tls() {
        let mut policy = LanServerPolicy::default();
        policy.port = 1_234;
        policy.tls = TlsPolicy::Certificate {
            certificate_sha256: "22".repeat(32),
            private_key_reference: "keychain:server".to_string(),
            minimum_version: "1.3".to_string(),
        };
        let error =
            UnifiedEndpointPlan::build(Some(PrimaryEndpointConfig { port: 1_234 }), Some(policy))
                .expect_err("transport collision must fail");
        assert!(error.contains("both plaintext and TLS"));
    }

    #[test]
    fn endpoint_order_and_keys_are_deterministic() {
        let mut policy = LanServerPolicy::default();
        policy.port = 9_999;
        let first = UnifiedEndpointPlan::build(
            Some(PrimaryEndpointConfig { port: 1_234 }),
            Some(policy.clone()),
        )
        .unwrap();
        let second =
            UnifiedEndpointPlan::build(Some(PrimaryEndpointConfig { port: 1_234 }), Some(policy))
                .unwrap();
        assert_eq!(first, second);
        assert_eq!(first.endpoints[0].key, "http://127.0.0.1:1234");
        assert_eq!(first.endpoints[1].key, "http://127.0.0.1:9999");
    }

    #[test]
    fn policy_only_plan_never_enables_primary_routes() {
        let mut policy = LanServerPolicy::default();
        policy.port = 7_777;
        let plan = UnifiedEndpointPlan::build(None, Some(policy)).expect("policy-only plan");
        assert_eq!(plan.endpoints.len(), 1);
        assert!(!plan.endpoints[0].primary);
        assert_eq!(plan.endpoints[0].exposure, ListenerExposure::Loopback);
    }

    #[test]
    fn every_endpoint_generation_observes_one_admission_counter_domain() {
        let state = UnifiedHttpServerState::default();
        let admission = state.lock().expect("state").admission.clone();
        let shutdown = CancellationToken::new();
        let guard = admission.try_admit(&shutdown).expect("admitted request");
        let active = state.snapshot().expect("active snapshot");
        assert_eq!(active.request_count, 0);
        assert_eq!(active.active_requests, 1);
        drop(guard);
        let completed = state.snapshot().expect("completed snapshot");
        assert_eq!(completed.request_count, 1);
        assert_eq!(completed.active_requests, 0);
        assert!(completed.last_request_at_ms.is_some());
    }
}
