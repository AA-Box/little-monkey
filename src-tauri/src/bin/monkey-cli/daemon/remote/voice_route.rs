//! Host-authoritative Talk routing across local and paired-device endpoints.
//!
//! This module is deliberately a control plane, not an audio transport. Paired
//! endpoints reuse the existing physical capability grants, durable command
//! queue and authenticated Talk WebSocket. The route row carries only endpoint
//! identity, generation and bounded coordination events; raw microphone or TTS
//! audio never lands here.

use super::protocol::{
    capability_block, legacy_capabilities, validate_id, DeviceCapability, DeviceReadiness, OsPermission,
};
use super::store::{DeviceCommandRequest, RemoteStore, VoiceRouteEventRecord, VoiceRouteRecord};
use crate::daemon::store::DaemonPaths;

pub const LOCAL_INPUT_DEFAULT: &str = "local:input:default";
pub const LOCAL_OUTPUT_DEFAULT: &str = "local:output:default";
const ROUTE_COMMAND_TTL_MS: u64 = 60 * 60 * 1_000;
/// Paired controllers long-poll for at most 25 seconds and retry network failures
/// after 5 seconds. Past this bound the host no longer has evidence that the
/// endpoint is currently reachable, so VoiceRoute fails closed instead of
/// queueing a microphone start on an offline phone.
const ENDPOINT_ONLINE_WINDOW_MS: u64 = 45_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioEndpoint {
    LocalInput(String),
    LocalOutput(String),
    PairedInput(String),
    PairedOutput(String),
}

impl AudioEndpoint {
    pub fn parse(value: &str) -> Result<Self, String> {
        if let Some(id) = value.strip_prefix("local:input:") {
            if id.is_empty() || id.len() > 384 || id.chars().any(char::is_control) {
                return Err("Local input endpoint id is invalid".to_string());
            }
            return Ok(Self::LocalInput(id.to_string()));
        }
        if let Some(id) = value.strip_prefix("local:output:") {
            if id.is_empty() || id.len() > 384 || id.chars().any(char::is_control) {
                return Err("Local output endpoint id is invalid".to_string());
            }
            return Ok(Self::LocalOutput(id.to_string()));
        }
        if let Some(rest) = value.strip_prefix("paired:") {
            if let Some(device_id) = rest.strip_suffix(":input") {
                validate_id(device_id)?;
                return Ok(Self::PairedInput(device_id.to_string()));
            }
            if let Some(device_id) = rest.strip_suffix(":output") {
                validate_id(device_id)?;
                return Ok(Self::PairedOutput(device_id.to_string()));
            }
        }
        Err(format!("Unknown audio endpoint '{value}'"))
    }

    pub fn token(&self) -> String {
        match self {
            Self::LocalInput(id) => format!("local:input:{id}"),
            Self::LocalOutput(id) => format!("local:output:{id}"),
            Self::PairedInput(id) => format!("paired:{id}:input"),
            Self::PairedOutput(id) => format!("paired:{id}:output"),
        }
    }

    pub fn paired_device(&self) -> Option<&str> {
        match self {
            Self::PairedInput(id) | Self::PairedOutput(id) => Some(id),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EndpointDescriptor {
    pub id: String,
    pub label: String,
    pub direction: &'static str,
    pub locality: &'static str,
    pub device_id: Option<String>,
    /// Capability truth is advertised by the endpoint surface. It is never
    /// guessed from a platform or device name.
    pub input_supported: bool,
    pub output_supported: bool,
    pub voice_stream_supported: bool,
    pub os_permission: Option<OsPermission>,
    pub readiness: Option<DeviceReadiness>,
    pub foreground_required: bool,
    pub interaction_required: bool,
    /// Whether the host has recent signed contact from the paired controller.
    /// Local endpoints are process-local and therefore always online here;
    /// hardware/OS permission is still resolved by the WebView itself.
    pub online: bool,
    pub last_seen_at_ms: Option<u64>,
    /// The last round trip actually observed for this endpoint's device: how
    /// long its most recent completed command took from being queued to being
    /// reported back, through the same long-poll queue a route role travels.
    /// `None` until one has completed, and on local endpoints, which have no
    /// queue between the route and the hardware.
    pub latency_ms: Option<u64>,
    pub ready: bool,
    pub blocked_code: Option<String>,
    pub blocked_by: Option<String>,
}

fn device_online(last_seen_at_ms: Option<u64>, surface_seen_at_ms: Option<u64>, now_ms: u64) -> bool {
    last_seen_at_ms
        .into_iter()
        .chain(surface_seen_at_ms)
        .max()
        .is_some_and(|seen| now_ms.saturating_sub(seen) <= ENDPOINT_ONLINE_WINDOW_MS)
}

pub fn endpoints(paths: &DaemonPaths) -> Result<Vec<EndpointDescriptor>, String> {
    let store = RemoteStore::open(&paths.root)?;
    let now_ms = super::now_ms_public()?;
    let mut result = vec![
        EndpointDescriptor {
            id: LOCAL_INPUT_DEFAULT.to_string(),
            label: "This computer — default microphone".to_string(),
            direction: "input",
            locality: "local",
            device_id: None,
            input_supported: true,
            output_supported: false,
            voice_stream_supported: true,
            os_permission: None,
            readiness: None,
            foreground_required: false,
            interaction_required: false,
            online: true,
            last_seen_at_ms: None,
            latency_ms: None,
            ready: true,
            blocked_code: None,
            blocked_by: None,
        },
        EndpointDescriptor {
            id: LOCAL_OUTPUT_DEFAULT.to_string(),
            label: "This computer — default speaker".to_string(),
            direction: "output",
            locality: "local",
            device_id: None,
            input_supported: false,
            output_supported: true,
            voice_stream_supported: false,
            os_permission: None,
            readiness: None,
            foreground_required: false,
            interaction_required: false,
            online: true,
            last_seen_at_ms: None,
            latency_ms: None,
            ready: true,
            blocked_code: None,
            blocked_by: None,
        },
    ];
    for device in store.devices()?.into_iter().filter(|device| device.active()) {
        let surface = store.device_surface(&device.device_id)?;
        let granted = if device.capabilities.is_empty() {
            legacy_capabilities(&device.scopes)
        } else {
            device.capabilities.clone()
        };
        let surface_seen = surface.as_ref().map(|value| value.reported_at_ms);
        let last_seen = device.last_seen_at_ms.into_iter().chain(surface_seen).max();
        let online = device_online(device.last_seen_at_ms, surface_seen, now_ms);
        let latency_ms = store.last_device_round_trip_ms(&device.device_id)?;
        for (direction, capability) in [
            ("input", DeviceCapability::VoiceStream),
            ("output", DeviceCapability::AudioPlayback),
        ] {
            let endpoint = if direction == "input" {
                format!("paired:{}:input", device.device_id)
            } else {
                format!("paired:{}:output", device.device_id)
            };
            let permission = surface.as_ref().map(|value| value.permission(capability));
            let readiness = surface.as_ref().map(|value| value.readiness(capability));
            let block = capability_block(&granted, surface.as_ref(), capability);
            let (blocked_code, blocked_by) = if !online {
                (Some("offline".to_string()), Some("Paired device is offline or has not made signed contact recently.".to_string()))
            } else if let Some(block) = block {
                (Some(block.as_str().to_string()), Some(block.explain(capability)))
            } else {
                (None, None)
            };
            let input_supported = surface.as_ref().is_some_and(|value| value.capabilities.contains(&DeviceCapability::VoiceStream));
            let output_supported = surface.as_ref().is_some_and(|value| value.capabilities.contains(&DeviceCapability::AudioPlayback));
            result.push(EndpointDescriptor {
                id: endpoint,
                label: format!("{} — {}", device.device_name, if direction == "input" { "microphone" } else { "speaker" }),
                direction,
                locality: "paired",
                device_id: Some(device.device_id.clone()),
                input_supported,
                output_supported,
                voice_stream_supported: input_supported,
                os_permission: permission,
                readiness,
                foreground_required: matches!(readiness, Some(DeviceReadiness::ForegroundRequired)),
                interaction_required: matches!(readiness, Some(DeviceReadiness::InteractionRequired)),
                online,
                last_seen_at_ms: last_seen,
                latency_ms,
                ready: online && block.is_none(),
                blocked_code,
                blocked_by,
            });
        }
    }
    Ok(result)
}

fn validate_endpoint(store: &RemoteStore, endpoint: &AudioEndpoint, now_ms: u64) -> Result<(), String> {
    match endpoint {
        AudioEndpoint::LocalInput(_) | AudioEndpoint::LocalOutput(_) => Ok(()),
        AudioEndpoint::PairedInput(device_id) | AudioEndpoint::PairedOutput(device_id) => {
            let capability = if matches!(endpoint, AudioEndpoint::PairedInput(_)) {
                DeviceCapability::VoiceStream
            } else {
                DeviceCapability::AudioPlayback
            };
            super::device::resolve_target(store, capability, Some(device_id))?;
            let device = store
                .device(device_id)?
                .ok_or_else(|| format!("Paired device '{device_id}' no longer exists"))?;
            let surface_seen = store.device_surface(device_id)?.map(|value| value.reported_at_ms);
            if !device_online(device.last_seen_at_ms, surface_seen, now_ms) {
                return Err("Paired VoiceRoute endpoint is offline or has not made signed contact recently".to_string());
            }
            Ok(())
        }
    }
}

fn cancel_command(store: &mut RemoteStore, command_id: Option<&str>, now_ms: u64) {
    if let Some(command_id) = command_id {
        let _ = store.request_device_cancel(command_id, now_ms);
    }
}

fn retire_command(
    store: &mut RemoteStore,
    command_id: Option<&str>,
    now_ms: u64,
    role: &str,
) -> Result<(), String> {
    let Some(command_id) = command_id else { return Ok(()); };
    store.request_device_cancel(command_id, now_ms)?;
    // The physical executor watches `cancel_requested` while a routed role is
    // running. A handoff does not commit its next generation until the previous
    // owner has acknowledged termination. That keeps microphone ownership
    // exclusive and also prevents old speaker work from bleeding into the new
    // route after the UI already says the move completed.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let Some(command) = store.device_command(command_id)? else { return Ok(()); };
        if command.state.terminal() {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "The previous VoiceRoute {role} did not release within 5 seconds; the new route was not committed"
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn wait_for_prepare(store: &RemoteStore, command_id: &str, role: &str) -> Result<(), String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    loop {
        let Some(command) = store.device_command(command_id)? else {
            return Err(format!("VoiceRoute {role} preparation disappeared before acknowledgement"));
        };
        if command.state.terminal() {
            return if command.state == super::protocol::DeviceCommandState::Succeeded {
                Ok(())
            } else {
                Err(command.error.unwrap_or_else(|| format!("VoiceRoute {role} preparation was not accepted by the device")))
            };
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!("VoiceRoute {role} preparation was not acknowledged within 8 seconds"));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Whether the destination device is already carrying a live role of this very
/// route.
///
/// A paired controller executes its leased commands strictly one at a time, and
/// a routed role stays `running` for as long as the microphone or speaker is
/// open. So a prepare queued for a device that is already running a role of
/// this route can never be leased while that role lives, and waiting for its
/// acknowledgement would time out every time — which is what made a
/// same-device duplex handoff (moving the speaker onto the phone that already
/// holds the microphone) unreachable. It is also the one case where the prepare
/// has nothing to prove: the device is executing the capability right now, and
/// `validate_endpoint` still re-checks the grant, advertised support,
/// permission and readiness from the surface it reported.
fn already_carrying_route(
    store: &RemoteStore,
    previous: &VoiceRouteRecord,
    endpoint: &AudioEndpoint,
) -> Result<bool, String> {
    let Some(device_id) = endpoint.paired_device() else { return Ok(false); };
    for command_id in [
        previous.input_command_id.as_deref(),
        previous.output_command_id.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(command) = store.device_command(command_id)? {
            if command.device_id == device_id && !command.state.terminal() {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn prepare_paired_endpoint(
    store: &mut RemoteStore,
    endpoint: &AudioEndpoint,
    session_id: &str,
    route_id: &str,
    generation: u64,
    engine: &str,
    now_ms: u64,
) -> Result<(), String> {
    let (device_id, capability, role) = match endpoint {
        AudioEndpoint::PairedInput(device_id) => (device_id.as_str(), DeviceCapability::VoiceStream, "input"),
        AudioEndpoint::PairedOutput(device_id) => (device_id.as_str(), DeviceCapability::AudioPlayback, "output"),
        _ => return Ok(()),
    };
    validate_endpoint(store, endpoint, now_ms)?;
    let command = store.enqueue_device_command(
        &DeviceCommandRequest {
            device_id: device_id.to_string(),
            capability,
            arguments: serde_json::json!({
                "mode": "talk_route_prepare",
                "role": role,
                "session_id": session_id,
                "route_id": route_id,
                "route_generation": generation,
                "engine": engine,
            }),
            source_run_id: None,
            source_session_id: Some(session_id.to_string()),
            source_tool_call_id: None,
            invocation_id: Some(format!("voice-route-prepare:{session_id}:{generation}:{role}:{device_id}")),
            expires_at_ms: now_ms.saturating_add(15_000),
        },
        now_ms,
    )?;
    wait_for_prepare(store, &command.command_id, role)
}

fn enqueue_role(
    store: &mut RemoteStore,
    device_id: &str,
    capability: DeviceCapability,
    session_id: &str,
    route_id: &str,
    generation: u64,
    role: &str,
    engine: &str,
    now_ms: u64,
) -> Result<String, String> {
    let expires_at_ms = now_ms.saturating_add(ROUTE_COMMAND_TTL_MS);
    let command = store.enqueue_device_command(
        &DeviceCommandRequest {
            device_id: device_id.to_string(),
            capability,
            arguments: serde_json::json!({
                "mode": "talk_route",
                "role": role,
                "session_id": session_id,
                "route_id": route_id,
                "route_generation": generation,
                "engine": engine,
                "duration_ms": ROUTE_COMMAND_TTL_MS,
            }),
            source_run_id: None,
            source_session_id: Some(session_id.to_string()),
            source_tool_call_id: None,
            invocation_id: Some(format!("voice-route:{session_id}:{generation}:{role}:{device_id}")),
            expires_at_ms,
        },
        now_ms,
    )?;
    Ok(command.command_id)
}

/// Validates the destination first, commits a fresh generation, retires the old
/// roles and then activates the new roles. Validation is the prepare phase: it
/// checks the existing grant + advertised support + OS permission + readiness
/// without opening a second microphone. Therefore an input handoff never has two
/// live microphones at once.
pub fn set_route(
    paths: &DaemonPaths,
    session_id: &str,
    engine: &str,
    input: &str,
    output: &str,
    now_ms: u64,
) -> Result<VoiceRouteRecord, String> {
    validate_id(session_id)?;
    if !matches!(engine, "pipeline" | "realtime") {
        return Err("Voice route engine must be pipeline or realtime".to_string());
    }
    let input = AudioEndpoint::parse(input)?;
    let output = AudioEndpoint::parse(output)?;
    if !matches!(input, AudioEndpoint::LocalInput(_) | AudioEndpoint::PairedInput(_)) {
        return Err("Voice route input endpoint is not an input".to_string());
    }
    if !matches!(output, AudioEndpoint::LocalOutput(_) | AudioEndpoint::PairedOutput(_)) {
        return Err("Voice route output endpoint is not an output".to_string());
    }

    let mut store = RemoteStore::open(&paths.root)?;
    validate_endpoint(&store, &input, now_ms)?;
    validate_endpoint(&store, &output, now_ms)?;
    let previous = store.voice_route(session_id)?;
    // Prepare -> retire -> commit. The new endpoints are validated above while
    // the old route is untouched. We then wait for the old microphone command
    // to acknowledge cancellation before minting the new generation, so there
    // is never an intentional two-microphone ownership window.
    if let Some(previous) = previous.as_ref() {
        let retired = retire_command(
            &mut store,
            previous.input_command_id.as_deref(),
            now_ms,
            "microphone",
        )
        .and_then(|()| {
            if previous.output_command_id != previous.input_command_id {
                retire_command(
                    &mut store,
                    previous.output_command_id.as_deref(),
                    now_ms,
                    "speaker",
                )
            } else {
                Ok(())
            }
        });
        if let Err(error) = retired {
            // The previous owner has already been asked to stop and will stop;
            // it just did not say so in time. Leaving the row as it was would
            // claim a microphone this route no longer owns, so the old
            // generation is retired here — anything the unreleased command
            // still emits is stale rather than current — and the selection is
            // kept so the operator can simply start Talk again.
            return Err(match store.replace_voice_route(
                session_id,
                &previous.engine,
                &previous.input_endpoint,
                &previous.output_endpoint,
                None,
                None,
                now_ms,
            ) {
                Ok(_) => format!(
                    "{error}. The previous generation was retired, so this conversation now owns \
                     no microphone; start Talk again to re-acquire one."
                ),
                Err(retire_error) => format!(
                    "{error}. Retiring the previous generation also failed ({retire_error}), so \
                     the route row may still name a microphone it no longer owns."
                ),
            });
        }
    }
    store.replace_voice_route(
        session_id,
        engine,
        &input.token(),
        &output.token(),
        None,
        None,
        now_ms,
    )
}

pub fn activate_route(paths: &DaemonPaths, session_id: &str, now_ms: u64) -> Result<VoiceRouteRecord, String> {
    let mut store = RemoteStore::open(&paths.root)?;
    let route = store
        .voice_route(session_id)?
        .ok_or_else(|| "No voice route exists for this conversation".to_string())?;
    if route.state != "active" {
        return Err("Voice route is stopped".to_string());
    }
    if route.input_command_id.is_some() || route.output_command_id.is_some() {
        return Ok(route);
    }
    let input = AudioEndpoint::parse(&route.input_endpoint)?;
    let output = AudioEndpoint::parse(&route.output_endpoint)?;
    validate_endpoint(&store, &input, now_ms)?;
    validate_endpoint(&store, &output, now_ms)?;

    let paired_input = match &input { AudioEndpoint::PairedInput(id) => Some(id.as_str()), _ => None };
    let paired_output = match &output { AudioEndpoint::PairedOutput(id) => Some(id.as_str()), _ => None };
    let mut input_command_id = None;
    let mut output_command_id = None;

    if let Some(device_id) = paired_input {
        let role = if paired_output == Some(device_id) { "duplex" } else { "input" };
        input_command_id = Some(enqueue_role(
            &mut store,
            device_id,
            DeviceCapability::VoiceStream,
            session_id,
            &route.route_id,
            route.generation,
            role,
            &route.engine,
            now_ms,
        )?);
    }
    if let Some(device_id) = paired_output {
        if paired_input != Some(device_id) {
            match enqueue_role(
                &mut store,
                device_id,
                DeviceCapability::AudioPlayback,
                session_id,
                &route.route_id,
                route.generation,
                "output",
                &route.engine,
                now_ms,
            ) {
                Ok(command_id) => output_command_id = Some(command_id),
                Err(error) => {
                    cancel_command(&mut store, input_command_id.as_deref(), now_ms);
                    return Err(error);
                }
            }
        } else {
            output_command_id = input_command_id.clone();
        }
    }
    match store.set_voice_route_commands(
        session_id,
        route.generation,
        input_command_id.as_deref(),
        output_command_id.as_deref(),
        now_ms,
    ) {
        Ok(route) => Ok(route),
        Err(error) => {
            cancel_command(&mut store, input_command_id.as_deref(), now_ms);
            if output_command_id != input_command_id {
                cancel_command(&mut store, output_command_id.as_deref(), now_ms);
            }
            Err(error)
        }
    }
}

/// Changes a route that may already be live. Selection itself stays privacy
/// preserving — an inactive route never opens a microphone — but when an old
/// generation already owns a paired role this performs the full handoff before
/// returning. A failed destination activation rolls forward to a fresh
/// generation containing the previous endpoints and re-acquires those roles.
/// Reusing the old generation would make delayed frames from the failed move
/// indistinguishable from current media, so rollback is deliberately another
/// generation rather than a database rewind.
pub fn move_route(
    paths: &DaemonPaths,
    session_id: &str,
    input: Option<&str>,
    output: Option<&str>,
    now_ms: u64,
) -> Result<VoiceRouteRecord, String> {
    let previous = stored_route(paths, session_id)?
        .ok_or_else(|| format!("No active voice route for '{session_id}'"))?;
    let was_live = previous.input_command_id.is_some() || previous.output_command_id.is_some();
    let next_input = AudioEndpoint::parse(input.unwrap_or(&previous.input_endpoint))?;
    let next_output = AudioEndpoint::parse(output.unwrap_or(&previous.output_endpoint))?;
    // Real prepare phase: while the working route is still untouched, require
    // each changed paired destination to execute a bounded, non-media command
    // and report its current surface readiness. Only after those acknowledgements
    // do we retire the old capture owner and commit the new generation.
    if was_live {
        let mut store = RemoteStore::open(&paths.root)?;
        let next_generation = previous.generation.checked_add(1)
            .ok_or_else(|| "Voice route generation is exhausted".to_string())?;
        if next_input.token() != previous.input_endpoint
            && !already_carrying_route(&store, &previous, &next_input)?
        {
            prepare_paired_endpoint(&mut store, &next_input, session_id, &previous.route_id, next_generation, &previous.engine, now_ms)?;
        }
        if next_output.token() != previous.output_endpoint
            && !already_carrying_route(&store, &previous, &next_output)?
        {
            prepare_paired_endpoint(&mut store, &next_output, session_id, &previous.route_id, next_generation, &previous.engine, now_ms)?;
        }
    }
    let selected = set_route(
        paths,
        session_id,
        &previous.engine,
        input.unwrap_or(&previous.input_endpoint),
        output.unwrap_or(&previous.output_endpoint),
        now_ms,
    )?;
    if !was_live {
        return Ok(selected);
    }
    match activate_route(paths, session_id, now_ms) {
        Ok(active) => Ok(active),
        Err(activation_error) => {
            // Best effort retirement of anything the failed activation managed
            // to publish. `activate_route` also cancels its local command ids
            // on failure, but reading the authoritative row here closes the
            // race where command ids were committed immediately before another
            // endpoint rejected.
            if let Ok(mut store) = RemoteStore::open(&paths.root) {
                if let Ok(Some(failed)) = store.voice_route(session_id) {
                    cancel_command(&mut store, failed.input_command_id.as_deref(), now_ms);
                    if failed.output_command_id != failed.input_command_id {
                        cancel_command(&mut store, failed.output_command_id.as_deref(), now_ms);
                    }
                }
                let _ = store.replace_voice_route(
                    session_id,
                    &previous.engine,
                    &previous.input_endpoint,
                    &previous.output_endpoint,
                    None,
                    None,
                    now_ms,
                );
            }
            match activate_route(paths, session_id, now_ms) {
                Ok(_) => Err(format!(
                    "VoiceRoute handoff failed ({activation_error}); the previous endpoints were restored under a fresh generation"
                )),
                Err(rollback_error) => Err(format!(
                    "VoiceRoute handoff failed ({activation_error}); restoring the previous endpoints also failed ({rollback_error})"
                )),
            }
        }
    }
}

pub fn deactivate_route(paths: &DaemonPaths, session_id: &str, now_ms: u64) -> Result<Option<VoiceRouteRecord>, String> {
    let mut store = RemoteStore::open(&paths.root)?;
    let Some(route) = store.voice_route(session_id)? else { return Ok(None); };
    cancel_command(&mut store, route.input_command_id.as_deref(), now_ms);
    if route.output_command_id != route.input_command_id {
        cancel_command(&mut store, route.output_command_id.as_deref(), now_ms);
    }
    if route.state != "active" {
        return Ok(Some(route));
    }
    // Stopping Talk deliberately retires the generation as well as releasing
    // the roles. A socket already admitted under this generation is otherwise
    // still authoritative — `talk_input_route_live` only compares the route's
    // current generation — so a device that had not yet noticed the
    // cancellation could keep streaming into a conversation the operator
    // stopped. The endpoint selection survives; only the generation dies.
    store
        .replace_voice_route(
            session_id,
            &route.engine,
            &route.input_endpoint,
            &route.output_endpoint,
            None,
            None,
            now_ms,
        )
        .map(Some)
}

pub fn stop_route(paths: &DaemonPaths, session_id: &str, now_ms: u64) -> Result<Option<VoiceRouteRecord>, String> {
    let mut store = RemoteStore::open(&paths.root)?;
    let previous = store.voice_route(session_id)?;
    if let Some(previous) = previous.as_ref() {
        cancel_command(&mut store, previous.input_command_id.as_deref(), now_ms);
        if previous.output_command_id != previous.input_command_id {
            cancel_command(&mut store, previous.output_command_id.as_deref(), now_ms);
        }
    }
    store.stop_voice_route(session_id, now_ms)
}

fn stored_route(paths: &DaemonPaths, session_id: &str) -> Result<Option<VoiceRouteRecord>, String> {
    RemoteStore::open(&paths.root)?.voice_route(session_id)
}

/// Reading the route is also where a role a paired device handed back is
/// noticed, because nothing else watches: `deactivate_route` and `stop_route`
/// are both operator-driven, so a device that walked away used to leave the
/// conversation naming an endpoint nobody was holding.
///
/// The reconciliation is deliberately best effort. A read that failed because
/// the surviving paired endpoint happened to be offline would take the route
/// card away from the operator at exactly the moment they need it, so the
/// stored row is returned instead and the next read tries again.
pub fn route(paths: &DaemonPaths, session_id: &str) -> Result<Option<VoiceRouteRecord>, String> {
    let Some(current) = stored_route(paths, session_id)? else { return Ok(None); };
    let now_ms = super::now_ms_public()?;
    Ok(Some(match reclaim_released_endpoints(paths, &current, now_ms) {
        Ok(reconciled) => reconciled,
        Err(_) => current,
    }))
}

/// The marker a paired controller puts in a routed role's report when the
/// person holding the device chose to hand that role back.
///
/// It is the whole distinction this fallback turns on, and it cannot be
/// inferred. A device that walked out of the room, locked its screen or lost
/// its network ends the same command in the same states, and handing the
/// microphone to the laptop in those cases would silently change which room is
/// being listened to. So only an explicit statement counts, and the device
/// controller has to send it: the "Return control to the computer" button must
/// report the routed command with `"release": "returned_to_host"` in its
/// result, and no other path to `stopTalk` may set it.
const RELEASED_TO_HOST: &str = "returned_to_host";

/// Whether this role ended because the device said it was handing it back.
///
/// `Succeeded` alone is not enough: a backgrounded page and a dropped Talk
/// socket both end the role that way too, and a cancelled command is the
/// operator's own doing rather than the device's. Anything still running, or
/// terminal for any other reason, leaves the route exactly as it is.
fn released_by_device(store: &RemoteStore, command_id: Option<&str>) -> Result<bool, String> {
    let Some(command_id) = command_id else { return Ok(false); };
    let Some(command) = store.device_command(command_id)? else { return Ok(false); };
    Ok(command.state == super::protocol::DeviceCommandState::Succeeded
        && !command.cancel_requested
        && command
            .result
            .as_ref()
            .and_then(|result| result.get("release"))
            .and_then(serde_json::Value::as_str)
            == Some(RELEASED_TO_HOST))
}

/// Returns each direction whose paired device released it to this computer,
/// under a fresh generation of the same conversation and the same route id.
///
/// The other direction is left selected where it was. It still has to be
/// re-acquired, because a generation bump is exactly what makes the retired
/// generation's sockets stale — that is the point of the generation — so the
/// surviving paired role travels through the ordinary handoff rather than
/// being carried across untouched.
fn reclaim_released_endpoints(
    paths: &DaemonPaths,
    current: &VoiceRouteRecord,
    now_ms: u64,
) -> Result<VoiceRouteRecord, String> {
    if current.state != "active" {
        return Ok(current.clone());
    }
    let store = RemoteStore::open(&paths.root)?;
    let input_released = AudioEndpoint::parse(&current.input_endpoint)?.paired_device().is_some()
        && released_by_device(&store, current.input_command_id.as_deref())?;
    // A single command serving a duplex role releases both directions at once,
    // which is the device saying it is done with the conversation entirely.
    let output_released = AudioEndpoint::parse(&current.output_endpoint)?.paired_device().is_some()
        && released_by_device(&store, current.output_command_id.as_deref())?;
    if !input_released && !output_released {
        return Ok(current.clone());
    }
    drop(store);
    move_route(
        paths,
        &current.session_id,
        input_released.then_some(LOCAL_INPUT_DEFAULT),
        output_released.then_some(LOCAL_OUTPUT_DEFAULT),
        now_ms,
    )
}

pub fn append_event(
    paths: &DaemonPaths,
    session_id: &str,
    generation: u64,
    kind: &str,
    payload: &serde_json::Value,
    now_ms: u64,
) -> Result<VoiceRouteEventRecord, String> {
    RemoteStore::open(&paths.root)?.append_voice_route_event(session_id, generation, kind, payload, now_ms)
}

pub fn events(
    paths: &DaemonPaths,
    session_id: &str,
    after: u64,
    limit: u32,
) -> Result<Vec<VoiceRouteEventRecord>, String> {
    RemoteStore::open(&paths.root)?.voice_route_events(session_id, after, limit)
}

pub fn route_json(route: &VoiceRouteRecord) -> serde_json::Value {
    serde_json::json!({
        "session_id": route.session_id,
        "route_id": route.route_id,
        "generation": route.generation,
        "engine": route.engine,
        "input_endpoint": route.input_endpoint,
        "output_endpoint": route.output_endpoint,
        "state": route.state,
        "input_command_id": route.input_command_id,
        "output_command_id": route.output_command_id,
        "created_at_ms": route.created_at_ms,
        "updated_at_ms": route.updated_at_ms,
    })
}

pub fn event_json(event: &VoiceRouteEventRecord) -> serde_json::Value {
    serde_json::json!({
        "event_id": event.event_id,
        "session_id": event.session_id,
        "generation": event.generation,
        "kind": event.kind,
        "payload": event.payload,
        "created_at_ms": event.created_at_ms,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet, HashMap};
    use std::sync::Mutex;

    use super::super::protocol::{
        DeviceCommandState, DeviceSurface, RemoteAction, RemoteScopes, REMOTE_PROTOCOL_VERSION,
    };
    use super::super::store::RemoteSecretStore;
    use super::*;

    struct MemorySecrets(Mutex<HashMap<String, Vec<u8>>>);

    impl RemoteSecretStore for MemorySecrets {
        fn get(&self, slot: &str) -> Result<Vec<u8>, String> {
            self.0.lock().unwrap().get(slot).cloned().ok_or_else(|| "missing".to_string())
        }
        fn set(&self, slot: &str, secret: &[u8]) -> Result<(), String> {
            self.0.lock().unwrap().insert(slot.to_string(), secret.to_vec());
            Ok(())
        }
        fn delete(&self, slot: &str) -> Result<(), String> {
            self.0.lock().unwrap().remove(slot);
            Ok(())
        }
    }

    /// A real store under a real temporary daemon root, because every function
    /// under test reaches the database through `DaemonPaths`.
    fn fixture() -> (std::path::PathBuf, DaemonPaths, MemorySecrets) {
        let app_data = std::env::temp_dir()
            .join(format!("little-monkey-voice-route-{}", uuid::Uuid::new_v4()));
        let paths = DaemonPaths::under(&app_data);
        (app_data, paths, MemorySecrets(Mutex::new(HashMap::new())))
    }

    fn pair(store: &mut RemoteStore, secrets: &MemorySecrets, name: &str) -> String {
        let scopes = RemoteScopes {
            actions: BTreeSet::from([RemoteAction::ViewRuns]),
            run_ids: BTreeSet::from(["run-one".to_string()]),
            workspace_ids: BTreeSet::new(),
            max_artifact_bytes: 1_024,
        };
        let capabilities = BTreeSet::from([
            DeviceCapability::ViewRuns,
            DeviceCapability::MicrophoneCapture,
            DeviceCapability::VoiceStream,
            DeviceCapability::AudioPlayback,
        ]);
        let invitation = store
            .create_invitation_with_capabilities(&scopes, &capabilities, 1_000, 2_000)
            .unwrap();
        store
            .accept_invitation(&invitation.pairing_id, &invitation.token, name, "runner-one", 1_100, secrets)
            .unwrap()
            .device_id
    }

    fn surface(
        platform: &str,
        advertised: &[DeviceCapability],
        permission: OsPermission,
        readiness: DeviceReadiness,
        reported_at_ms: u64,
    ) -> DeviceSurface {
        DeviceSurface {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            platform: platform.to_string(),
            platform_version: "18.0".to_string(),
            app_version: "1.3.0".to_string(),
            device_model: "Test".to_string(),
            capabilities: advertised.iter().copied().collect(),
            permissions: advertised.iter().map(|capability| (*capability, permission)).collect::<BTreeMap<_, _>>(),
            readiness: advertised.iter().map(|capability| (*capability, readiness)).collect::<BTreeMap<_, _>>(),
            constraints: Default::default(),
            reported_at_ms,
        }
    }

    /// A ready microphone-and-speaker phone, described exactly as the device
    /// itself would describe it.
    fn ready_phone(store: &mut RemoteStore, secrets: &MemorySecrets, name: &str, now_ms: u64) -> String {
        let device_id = pair(store, secrets, name);
        store
            .save_device_surface(
                &device_id,
                &surface(
                    "ios",
                    &[DeviceCapability::VoiceStream, DeviceCapability::AudioPlayback],
                    OsPermission::Granted,
                    DeviceReadiness::Ready,
                    now_ms,
                ),
                now_ms,
            )
            .unwrap();
        device_id
    }

    fn endpoint(paths: &DaemonPaths, id: &str) -> EndpointDescriptor {
        endpoints(paths)
            .unwrap()
            .into_iter()
            .find(|endpoint| endpoint.id == id)
            .unwrap_or_else(|| panic!("endpoint '{id}' was not offered at all"))
    }

    #[test]
    fn endpoint_ids_are_directional_and_round_trip() {
        for value in [
            LOCAL_INPUT_DEFAULT,
            LOCAL_OUTPUT_DEFAULT,
            "paired:device-phone:input",
            "paired:device-phone:output",
            "local:input:BuiltInMic",
            "local:output:BuiltInSpeaker",
        ] {
            assert_eq!(AudioEndpoint::parse(value).unwrap().token(), value);
        }
        assert!(AudioEndpoint::parse("paired:device-phone").is_err());
    }

    /// The route is per conversation and its generation only ever goes up. A
    /// generation that restarted, or a second conversation sharing one row,
    /// would make a stale socket indistinguishable from the current one.
    #[test]
    fn set_route_is_conversation_scoped_and_mints_monotonic_generations() {
        let (app_data, paths, _secrets) = fixture();
        let first = set_route(&paths, "chat-one", "pipeline", LOCAL_INPUT_DEFAULT, LOCAL_OUTPUT_DEFAULT, 1_000).unwrap();
        assert_eq!(first.generation, 1);
        assert_eq!(first.state, "active");
        assert_eq!(first.input_endpoint, LOCAL_INPUT_DEFAULT);

        let second = set_route(&paths, "chat-one", "realtime", "local:input:usb-mic", LOCAL_OUTPUT_DEFAULT, 1_100).unwrap();
        assert_eq!(second.generation, 2, "a second selection must retire the first generation");
        assert_eq!(second.route_id, first.route_id, "the route identity survives a re-selection");
        assert_eq!(second.engine, "realtime");
        assert_eq!(second.input_endpoint, "local:input:usb-mic");

        let other = set_route(&paths, "chat-two", "pipeline", LOCAL_INPUT_DEFAULT, LOCAL_OUTPUT_DEFAULT, 1_200).unwrap();
        assert_eq!(other.generation, 1, "another conversation starts at its own generation 1");
        assert_ne!(other.route_id, first.route_id);
        assert_eq!(route(&paths, "chat-one").unwrap().unwrap().generation, 2);

        assert!(set_route(&paths, "chat-one", "pipeline", LOCAL_OUTPUT_DEFAULT, LOCAL_OUTPUT_DEFAULT, 1_300)
            .unwrap_err()
            .contains("input endpoint is not an input"));
        assert!(set_route(&paths, "chat-one", "whisper", LOCAL_INPUT_DEFAULT, LOCAL_OUTPUT_DEFAULT, 1_300).is_err());
        let _ = std::fs::remove_dir_all(app_data);
    }

    /// Every axis of the grant model has to be able to block a paired endpoint
    /// on its own, and a blocked endpoint is still *offered* — an operator who
    /// cannot see the phone cannot fix the permission it is waiting on.
    #[test]
    fn a_paired_endpoint_is_offered_but_blocked_when_any_single_axis_says_no() {
        let (app_data, paths, secrets) = fixture();
        let now = super::super::now_ms_public().unwrap();
        let mut store = RemoteStore::open(&paths.root).unwrap();
        let device = pair(&mut store, &secrets, "Studio Phone");
        let input_id = format!("paired:{device}:input");

        let described = endpoint(&paths, &input_id);
        assert!(!described.ready);
        // A device that has never described itself has never made contact
        // either, so the outermost axis — reachability — is the honest reason.
        assert_eq!(described.blocked_code.as_deref(), Some("offline"));
        assert!(!described.input_supported, "a device that has said nothing supports nothing");

        for (advertised, permission, readiness, reported_at_ms, expected) in [
            (vec![DeviceCapability::VoiceStream, DeviceCapability::AudioPlayback], OsPermission::Granted, DeviceReadiness::Ready, now, None),
            (vec![DeviceCapability::AudioPlayback], OsPermission::Granted, DeviceReadiness::Ready, now, Some("unsupported")),
            (vec![DeviceCapability::VoiceStream], OsPermission::Denied, DeviceReadiness::Ready, now, Some("permission_denied")),
            (vec![DeviceCapability::VoiceStream], OsPermission::Granted, DeviceReadiness::ForegroundRequired, now, Some("foreground_required")),
            (vec![DeviceCapability::VoiceStream], OsPermission::Granted, DeviceReadiness::Ready, now - ENDPOINT_ONLINE_WINDOW_MS - 1_000, Some("offline")),
        ] {
            store
                .save_device_surface(&device, &surface("ios", &advertised, permission, readiness, reported_at_ms), reported_at_ms)
                .unwrap();
            let described = endpoint(&paths, &input_id);
            assert_eq!(
                described.blocked_code.as_deref(),
                expected,
                "advertised {advertised:?} permission {permission:?} readiness {readiness:?} should block with {expected:?}"
            );
            assert_eq!(described.ready, expected.is_none());
            assert!(described.blocked_by.is_some() == expected.is_some(), "a blocked endpoint always says why");
        }

        // Withdrawing the operator's grant blocks it even though the device
        // still advertises a ready, permitted microphone.
        store
            .save_device_surface(
                &device,
                &surface("ios", &[DeviceCapability::VoiceStream], OsPermission::Granted, DeviceReadiness::Ready, now),
                now,
            )
            .unwrap();
        store
            .set_device_capabilities(
                &device,
                &BTreeSet::from([DeviceCapability::ViewRuns, DeviceCapability::MicrophoneCapture]),
                now,
            )
            .unwrap();
        assert_eq!(endpoint(&paths, &input_id).blocked_code.as_deref(), Some("not_granted"));

        let local = endpoint(&paths, LOCAL_INPUT_DEFAULT);
        assert!(local.ready && local.online && local.blocked_code.is_none());
        let _ = std::fs::remove_dir_all(app_data);
    }

    /// The one inference that must never happen: a phone is a phone, so it
    /// "obviously" has a microphone. Capability comes from the advertised
    /// surface alone, never from the platform string or the name the operator
    /// typed at pairing time.
    #[test]
    fn endpoint_capability_is_never_inferred_from_a_device_name_or_platform() {
        let (app_data, paths, secrets) = fixture();
        let now = super::super::now_ms_public().unwrap();
        let mut store = RemoteStore::open(&paths.root).unwrap();
        let device = pair(&mut store, &secrets, "iPhone with a great microphone");
        store
            .save_device_surface(
                &device,
                &surface("ios", &[DeviceCapability::AudioPlayback], OsPermission::Granted, DeviceReadiness::Ready, now),
                now,
            )
            .unwrap();

        let input = endpoint(&paths, &format!("paired:{device}:input"));
        assert!(!input.input_supported && !input.voice_stream_supported && !input.ready);
        assert_eq!(input.blocked_code.as_deref(), Some("unsupported"));
        let output = endpoint(&paths, &format!("paired:{device}:output"));
        assert!(output.output_supported && output.ready);
        assert!(!output.input_supported, "the speaker endpoint must not claim a microphone either");
        let _ = std::fs::remove_dir_all(app_data);
    }

    /// `latency_ms` is a measurement or it is nothing. It used to be hardcoded
    /// `None`, which made a structurally empty field look like a real one.
    #[test]
    fn endpoint_latency_is_the_observed_command_round_trip_and_nothing_before_one() {
        let (app_data, paths, secrets) = fixture();
        let now = super::super::now_ms_public().unwrap();
        let mut store = RemoteStore::open(&paths.root).unwrap();
        let device = ready_phone(&mut store, &secrets, "Phone", now);
        assert_eq!(endpoint(&paths, &format!("paired:{device}:output")).latency_ms, None);

        let command = store
            .enqueue_device_command(
                &DeviceCommandRequest {
                    device_id: device.clone(),
                    capability: DeviceCapability::AudioPlayback,
                    arguments: serde_json::json!({"mode": "talk_route_prepare"}),
                    source_run_id: None,
                    source_session_id: None,
                    source_tool_call_id: None,
                    invocation_id: None,
                    expires_at_ms: now + 600_000,
                },
                now,
            )
            .unwrap();
        store
            .complete_device_command(
                &device,
                &command.command_id,
                DeviceCommandState::Succeeded,
                Some(&serde_json::json!({"ready": true})),
                None,
                None,
                None,
                now + 250,
            )
            .unwrap();
        assert_eq!(
            endpoint(&paths, &format!("paired:{device}:output")).latency_ms,
            Some(250),
            "the reported latency is the queue-to-report round trip this device was seen to take"
        );
        assert_eq!(endpoint(&paths, LOCAL_OUTPUT_DEFAULT).latency_ms, None);
        let _ = std::fs::remove_dir_all(app_data);
    }

    fn role_of(store: &RemoteStore, command_id: &str) -> String {
        store.device_command(command_id).unwrap().unwrap().arguments["role"].as_str().unwrap().to_string()
    }

    /// One command per role, and exactly one when a single device holds both —
    /// a second microphone command for a duplex phone would be a second
    /// microphone owner.
    #[test]
    fn activate_route_enqueues_one_command_per_role_for_each_topology() {
        let (app_data, paths, secrets) = fixture();
        let now = super::super::now_ms_public().unwrap();
        let mut store = RemoteStore::open(&paths.root).unwrap();
        let phone = ready_phone(&mut store, &secrets, "Phone", now);
        let speaker = ready_phone(&mut store, &secrets, "Kitchen", now);

        set_route(&paths, "chat-topology", "pipeline", LOCAL_INPUT_DEFAULT, LOCAL_OUTPUT_DEFAULT, now).unwrap();
        let local = activate_route(&paths, "chat-topology", now).unwrap();
        assert_eq!((local.input_command_id.clone(), local.output_command_id.clone()), (None, None),
            "a fully local route queues nothing on any device");

        set_route(&paths, "chat-topology", "pipeline", &format!("paired:{phone}:input"), LOCAL_OUTPUT_DEFAULT, now).unwrap();
        let input_only = activate_route(&paths, "chat-topology", now).unwrap();
        let input_command = input_only.input_command_id.clone().unwrap();
        assert_eq!(input_only.output_command_id, None);
        assert_eq!(role_of(&store, &input_command), "input");
        let queued = store.device_command(&input_command).unwrap().unwrap();
        assert_eq!(queued.capability, DeviceCapability::VoiceStream);
        assert_eq!(queued.arguments["route_generation"], input_only.generation);
        assert_eq!(queued.arguments["mode"], "talk_route");

        set_route(&paths, "chat-topology", "pipeline", LOCAL_INPUT_DEFAULT, &format!("paired:{speaker}:output"), now).unwrap();
        let output_only = activate_route(&paths, "chat-topology", now).unwrap();
        assert_eq!(output_only.input_command_id, None);
        assert_eq!(role_of(&store, output_only.output_command_id.as_deref().unwrap()), "output");

        set_route(&paths, "chat-topology", "pipeline", &format!("paired:{phone}:input"), &format!("paired:{phone}:output"), now).unwrap();
        let duplex = activate_route(&paths, "chat-topology", now).unwrap();
        assert_eq!(duplex.input_command_id, duplex.output_command_id, "one device carrying both roles runs one command");
        assert_eq!(role_of(&store, duplex.input_command_id.as_deref().unwrap()), "duplex");

        set_route(&paths, "chat-topology", "pipeline", &format!("paired:{phone}:input"), &format!("paired:{speaker}:output"), now).unwrap();
        let split = activate_route(&paths, "chat-topology", now).unwrap();
        assert_ne!(split.input_command_id, split.output_command_id);
        assert_eq!(role_of(&store, split.input_command_id.as_deref().unwrap()), "input");
        assert_eq!(role_of(&store, split.output_command_id.as_deref().unwrap()), "output");
        assert_eq!(
            store.device_command(split.output_command_id.as_deref().unwrap()).unwrap().unwrap().capability,
            DeviceCapability::AudioPlayback
        );
        let _ = std::fs::remove_dir_all(app_data);
    }

    /// A half-activated route owns a microphone nobody can hear: the speaker
    /// never started and the phone is still capturing. The input command must
    /// be rolled back when the output command cannot be queued.
    #[test]
    fn activate_route_rolls_back_the_microphone_when_the_speaker_cannot_be_queued() {
        let (app_data, paths, secrets) = fixture();
        let now = super::super::now_ms_public().unwrap();
        let mut store = RemoteStore::open(&paths.root).unwrap();
        let phone = ready_phone(&mut store, &secrets, "Phone", now);
        let speaker = ready_phone(&mut store, &secrets, "Kitchen", now);
        let selected = set_route(
            &paths,
            "chat-rollback",
            "pipeline",
            &format!("paired:{phone}:input"),
            &format!("paired:{speaker}:output"),
            now,
        )
        .unwrap();
        // Occupy the exact invocation identity the speaker role will claim, with
        // different arguments — the queue refuses to replace it, which is the
        // realistic way the second enqueue fails after the first succeeded.
        store
            .enqueue_device_command(
                &DeviceCommandRequest {
                    device_id: speaker.clone(),
                    capability: DeviceCapability::AudioPlayback,
                    arguments: serde_json::json!({"mode": "talk_route", "role": "output", "stale": true}),
                    source_run_id: None,
                    source_session_id: None,
                    source_tool_call_id: None,
                    invocation_id: Some(format!("voice-route:chat-rollback:{}:output:{speaker}", selected.generation)),
                    expires_at_ms: now + 600_000,
                },
                now,
            )
            .unwrap();

        let error = activate_route(&paths, "chat-rollback", now).unwrap_err();
        assert!(error.contains("already queued a different device command"), "unexpected error: {error}");

        let after = route(&paths, "chat-rollback").unwrap().unwrap();
        assert_eq!((after.input_command_id, after.output_command_id), (None, None),
            "a route that could not start both roles must claim neither");
        let attempted = store
            .device_command_by_invocation(&format!("voice-route:chat-rollback:{}:input:{phone}", selected.generation))
            .unwrap()
            .unwrap();
        assert_eq!(attempted.state, DeviceCommandState::Cancelled,
            "the microphone command queued before the failure must be cancelled, not left running");
        let _ = std::fs::remove_dir_all(app_data);
    }

    /// The handoff contract: the previous microphone is cancelled *and reaches a
    /// terminal state* before the new generation exists. Asserted by watching
    /// the row while the old command is still running, because the end state of
    /// a commit-then-cancel implementation looks identical.
    #[test]
    fn move_route_commits_the_new_generation_only_after_the_old_microphone_released() {
        let (app_data, paths, secrets) = fixture();
        let now = super::super::now_ms_public().unwrap();
        let mut store = RemoteStore::open(&paths.root).unwrap();
        let phone = ready_phone(&mut store, &secrets, "Phone", now);
        set_route(&paths, "chat-handoff", "pipeline", &format!("paired:{phone}:input"), LOCAL_OUTPUT_DEFAULT, now).unwrap();
        let live = activate_route(&paths, "chat-handoff", now).unwrap();
        let holding = live.input_command_id.clone().unwrap();
        store.lease_device_command(&phone, 30_000, now).unwrap().unwrap();
        assert!(store.start_device_command(&phone, &holding, Some("execution-one"), now).unwrap().started);

        let root = paths.root.clone();
        let watcher_command = holding.clone();
        let watcher_device = phone.clone();
        let watcher = std::thread::spawn(move || {
            // Let the handoff get well past its prepare phase, then look at the
            // row while the old microphone is demonstrably still running.
            std::thread::sleep(std::time::Duration::from_millis(400));
            let mut store = RemoteStore::open(&root).unwrap();
            let observed_route = store.voice_route("chat-handoff").unwrap().unwrap();
            let observed_command = store.device_command(&watcher_command).unwrap().unwrap();
            store
                .complete_device_command(
                    &watcher_device,
                    &watcher_command,
                    DeviceCommandState::Cancelled,
                    None,
                    None,
                    Some("released"),
                    Some("execution-one"),
                    now + 400,
                )
                .unwrap();
            (observed_route, observed_command)
        });

        let moved = move_route(&paths, "chat-handoff", Some(LOCAL_INPUT_DEFAULT), None, now).unwrap();
        let (observed_route, observed_command) = watcher.join().unwrap();

        assert_eq!(observed_command.state, DeviceCommandState::Running);
        assert!(observed_command.cancel_requested, "the previous microphone is asked to stop first");
        assert_eq!(
            observed_route.generation, live.generation,
            "the new generation was committed while the previous microphone was still running"
        );
        assert_eq!(observed_route.input_endpoint, format!("paired:{phone}:input"));
        assert!(moved.generation > live.generation);
        assert_eq!(moved.route_id, live.route_id, "a move keeps the route identity");
        assert_eq!(moved.session_id, live.session_id);
        assert_eq!(moved.input_endpoint, LOCAL_INPUT_DEFAULT);
        assert!(store.device_command(&holding).unwrap().unwrap().state.terminal());
        let _ = std::fs::remove_dir_all(app_data);
    }

    /// When the destination cannot be activated, the conversation must come
    /// back to the endpoints it had — under a *new* generation, so nothing the
    /// failed move left in flight can pass as current.
    #[test]
    fn a_failed_handoff_restores_the_previous_endpoints_under_a_fresh_generation() {
        let (app_data, paths, secrets) = fixture();
        let now = super::super::now_ms_public().unwrap();
        let mut store = RemoteStore::open(&paths.root).unwrap();
        let phone = ready_phone(&mut store, &secrets, "Phone", now);
        let speaker = ready_phone(&mut store, &secrets, "Kitchen", now);
        set_route(&paths, "chat-failed", "pipeline", &format!("paired:{phone}:input"), LOCAL_OUTPUT_DEFAULT, now).unwrap();
        let live = activate_route(&paths, "chat-failed", now).unwrap();
        let doomed_generation = live.generation + 1;
        // The destination acknowledges its prepare, and then its role command
        // cannot be queued — the window this rollback exists for.
        store
            .enqueue_device_command(
                &DeviceCommandRequest {
                    device_id: speaker.clone(),
                    capability: DeviceCapability::AudioPlayback,
                    arguments: serde_json::json!({"mode": "talk_route", "role": "output", "stale": true}),
                    source_run_id: None,
                    source_session_id: None,
                    source_tool_call_id: None,
                    invocation_id: Some(format!("voice-route:chat-failed:{doomed_generation}:output:{speaker}")),
                    expires_at_ms: now + 600_000,
                },
                now,
            )
            .unwrap();

        let root = paths.root.clone();
        let prepare_invocation = format!("voice-route-prepare:chat-failed:{doomed_generation}:output:{speaker}");
        let answering_speaker = speaker.clone();
        let device = std::thread::spawn(move || {
            let mut store = RemoteStore::open(&root).unwrap();
            for _ in 0..100 {
                if let Some(command) = store.device_command_by_invocation(&prepare_invocation).unwrap() {
                    store
                        .complete_device_command(
                            &answering_speaker,
                            &command.command_id,
                            DeviceCommandState::Succeeded,
                            Some(&serde_json::json!({"ready": true})),
                            None,
                            None,
                            None,
                            now + 10,
                        )
                        .unwrap();
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            false
        });

        let error = move_route(&paths, "chat-failed", None, Some(&format!("paired:{speaker}:output")), now).unwrap_err();
        assert!(device.join().unwrap(), "the destination never saw its prepare command");
        assert!(error.contains("restored under a fresh generation"), "unexpected error: {error}");

        let restored = route(&paths, "chat-failed").unwrap().unwrap();
        assert_eq!(restored.input_endpoint, format!("paired:{phone}:input"));
        assert_eq!(restored.output_endpoint, LOCAL_OUTPUT_DEFAULT, "the speaker move must not stick");
        assert!(restored.generation > doomed_generation, "the rollback rolls forward, it does not rewind");
        assert!(restored.input_command_id.is_some(), "the restored microphone is re-acquired");
        assert!(!store.device_command(restored.input_command_id.as_deref().unwrap()).unwrap().unwrap().state.terminal());
        for stale in [live.generation, doomed_generation] {
            assert_eq!(
                append_event(&paths, "chat-failed", stale, "turn_finished", &serde_json::json!({"turn_id": "t"}), now)
                    .unwrap_err(),
                "Voice route generation is stale",
                "generation {stale} must never become current again"
            );
        }
        let _ = std::fs::remove_dir_all(app_data);
    }

    /// Stopping Talk on purpose has to end the generation as well as the roles:
    /// a socket already admitted under it stays authoritative otherwise.
    #[test]
    fn deactivate_and_stop_release_every_role_and_retire_the_generation() {
        let (app_data, paths, secrets) = fixture();
        let now = super::super::now_ms_public().unwrap();
        let mut store = RemoteStore::open(&paths.root).unwrap();
        let phone = ready_phone(&mut store, &secrets, "Phone", now);
        set_route(&paths, "chat-stop", "pipeline", &format!("paired:{phone}:input"), &format!("paired:{phone}:output"), now).unwrap();
        let live = activate_route(&paths, "chat-stop", now).unwrap();
        let carrying = live.input_command_id.clone().unwrap();

        let idle = deactivate_route(&paths, "chat-stop", now).unwrap().unwrap();
        assert_eq!(idle.state, "active", "the selection survives a stop; only the roles and the generation do not");
        assert_eq!((idle.input_command_id.clone(), idle.output_command_id.clone()), (None, None));
        assert!(idle.generation > live.generation, "a deliberate stop retires the generation");
        assert_eq!(store.device_command(&carrying).unwrap().unwrap().state, DeviceCommandState::Cancelled);
        assert_eq!(
            append_event(&paths, "chat-stop", live.generation, "input_ready", &serde_json::json!({}), now).unwrap_err(),
            "Voice route generation is stale"
        );
        append_event(&paths, "chat-stop", idle.generation, "input_ready", &serde_json::json!({}), now).unwrap();

        let stopped = stop_route(&paths, "chat-stop", now).unwrap().unwrap();
        assert_eq!(stopped.state, "stopped");
        assert!(stopped.generation > idle.generation);
        assert!(append_event(&paths, "chat-stop", stopped.generation, "input_ready", &serde_json::json!({}), now).is_err());
        assert!(
            store.device_commands(&phone, 50).unwrap().iter().all(|command| command.state.terminal()),
            "no capture may be left owned after Talk was stopped"
        );
        let _ = std::fs::remove_dir_all(app_data);
    }

    /// The coordination ledger is metadata only, and it belongs to exactly one
    /// generation. Either property failing would put raw audio, or audio from a
    /// retired route, into a conversation.
    #[test]
    fn the_event_ledger_refuses_media_and_stale_generations() {
        let (app_data, paths, _secrets) = fixture();
        let live = set_route(&paths, "chat-ledger", "pipeline", LOCAL_INPUT_DEFAULT, LOCAL_OUTPUT_DEFAULT, 1_000).unwrap();
        append_event(&paths, "chat-ledger", live.generation, "input_transcript", &serde_json::json!({"text": "hello"}), 1_001).unwrap();
        assert!(append_event(&paths, "chat-ledger", live.generation, "assistant_delta", &serde_json::json!({"audio_base64": "AAEC"}), 1_002)
            .unwrap_err()
            .contains("may not contain raw or encoded media"));
        let next = set_route(&paths, "chat-ledger", "pipeline", LOCAL_INPUT_DEFAULT, LOCAL_OUTPUT_DEFAULT, 1_003).unwrap();
        assert!(append_event(&paths, "chat-ledger", live.generation, "input_transcript", &serde_json::json!({"text": "late"}), 1_004).is_err());
        append_event(&paths, "chat-ledger", next.generation, "input_transcript", &serde_json::json!({"text": "current"}), 1_005).unwrap();

        let recorded = events(&paths, "chat-ledger", 0, 64).unwrap();
        assert_eq!(recorded.len(), 2, "only the two accepted events were stored");
        assert_eq!(recorded[0].payload["text"], "hello");
        assert_eq!(recorded[1].generation, next.generation);
        let _ = std::fs::remove_dir_all(app_data);
    }

    /// Puts the route's microphone command in the state a really running
    /// capture is in: leased, started, and only stopping when the device says
    /// so.
    fn capture_running(store: &mut RemoteStore, device_id: &str, command_id: &str, now_ms: u64) {
        store.lease_device_command(device_id, 30_000, now_ms).unwrap().unwrap();
        assert!(store.start_device_command(device_id, command_id, Some("execution-one"), now_ms).unwrap().started);
    }

    /// A microphone that never confirms its release used to leave the row
    /// pointing at it: the route claimed a capture it no longer owned, and
    /// nothing would ever re-acquire one.
    #[test]
    fn a_microphone_that_never_releases_leaves_the_route_owning_nothing_and_says_so() {
        let (app_data, paths, secrets) = fixture();
        let now = super::super::now_ms_public().unwrap();
        let mut store = RemoteStore::open(&paths.root).unwrap();
        let phone = ready_phone(&mut store, &secrets, "Phone", now);
        set_route(&paths, "chat-stuck", "pipeline", &format!("paired:{phone}:input"), LOCAL_OUTPUT_DEFAULT, now).unwrap();
        let live = activate_route(&paths, "chat-stuck", now).unwrap();
        let holding = live.input_command_id.clone().unwrap();
        capture_running(&mut store, &phone, &holding, now);

        let error = move_route(&paths, "chat-stuck", Some(LOCAL_INPUT_DEFAULT), None, now).unwrap_err();
        assert!(error.contains("did not release within 5 seconds"), "unexpected error: {error}");
        assert!(error.contains("owns no microphone"), "the error has to say what the route is left holding: {error}");

        let after = route(&paths, "chat-stuck").unwrap().unwrap();
        assert_eq!(after.input_endpoint, format!("paired:{phone}:input"), "the selection is kept");
        assert_eq!((after.input_command_id, after.output_command_id), (None, None),
            "the route must not claim a microphone the device never released");
        assert!(after.generation > live.generation, "the unreleased generation is retired");
        assert_eq!(
            append_event(&paths, "chat-stuck", live.generation, "input_ready", &serde_json::json!({}), now).unwrap_err(),
            "Voice route generation is stale",
            "anything the unreleased command still emits must read as stale"
        );
        let _ = std::fs::remove_dir_all(app_data);
    }

    /// Ends a routed role the way the device controller's "Return control to
    /// the computer" button has to: the command completes successfully, was
    /// never cancelled by the host, and says in its own result that the role
    /// was handed back.
    fn device_returns_control(store: &mut RemoteStore, device_id: &str, command_id: &str, now_ms: u64) {
        store
            .complete_device_command(
                device_id,
                command_id,
                DeviceCommandState::Succeeded,
                Some(&serde_json::json!({ "release": "returned_to_host", "role": "input" })),
                None,
                None,
                Some("execution-one"),
                now_ms,
            )
            .unwrap();
    }

    /// Nothing host-side re-selected local endpoints when a paired device ended
    /// its routed role, so pressing "Return control to the computer" left the
    /// conversation naming a microphone on a phone that had walked away, and no
    /// further audio could reach the turn.
    #[test]
    fn a_role_the_device_hands_back_returns_that_direction_to_this_computer() {
        let (app_data, paths, secrets) = fixture();
        let now = super::super::now_ms_public().unwrap();
        let mut store = RemoteStore::open(&paths.root).unwrap();
        let phone = ready_phone(&mut store, &secrets, "Phone", now);
        set_route(&paths, "chat-return", "pipeline", &format!("paired:{phone}:input"), LOCAL_OUTPUT_DEFAULT, now).unwrap();
        let live = activate_route(&paths, "chat-return", now).unwrap();
        let holding = live.input_command_id.clone().unwrap();
        capture_running(&mut store, &phone, &holding, now);

        device_returns_control(&mut store, &phone, &holding, now + 10);

        let after = route(&paths, "chat-return").unwrap().unwrap();
        assert_eq!(after.input_endpoint, LOCAL_INPUT_DEFAULT, "the microphone comes back to this computer");
        assert_eq!(after.output_endpoint, LOCAL_OUTPUT_DEFAULT, "the untouched direction keeps its selection");
        assert_eq!(after.route_id, live.route_id, "the same conversation keeps the same route");
        assert_eq!(after.session_id, live.session_id);
        assert_eq!(after.state, "active", "handing a role back ends the role, not the conversation");
        assert!(after.generation > live.generation, "the device's generation is retired with it");
        assert_eq!(after.input_command_id, None, "a local microphone is carried by no device command");
        assert_eq!(
            append_event(&paths, "chat-return", live.generation, "input_ready", &serde_json::json!({}), now).unwrap_err(),
            "Voice route generation is stale",
            "anything the released device still emits has to read as stale",
        );

        // Reading again is not a second handoff: the released command is no
        // longer named by the row, so the route settles rather than minting a
        // generation on every poll.
        let settled = route(&paths, "chat-return").unwrap().unwrap();
        assert_eq!(settled.generation, after.generation);
        let _ = std::fs::remove_dir_all(app_data);
    }

    /// A phone that lost its network, locked its screen or was backgrounded is
    /// not a phone that gave the microphone back. Falling back on those would
    /// silently move the conversation's ears into whatever room the laptop is
    /// in, so only the device's explicit statement counts.
    #[test]
    fn a_device_that_merely_drops_off_never_hands_the_microphone_to_this_computer() {
        let (app_data, paths, secrets) = fixture();
        let now = super::super::now_ms_public().unwrap();
        let mut store = RemoteStore::open(&paths.root).unwrap();
        let phone = ready_phone(&mut store, &secrets, "Phone", now);
        let microphone = format!("paired:{phone}:input");
        set_route(&paths, "chat-dropped", "pipeline", &microphone, LOCAL_OUTPUT_DEFAULT, now).unwrap();
        let live = activate_route(&paths, "chat-dropped", now).unwrap();
        let holding = live.input_command_id.clone().unwrap();
        capture_running(&mut store, &phone, &holding, now);

        // Off the network: the role is still running and nothing was reported.
        let silent = route(&paths, "chat-dropped").unwrap().unwrap();
        assert_eq!(silent.input_endpoint, microphone);
        assert_eq!(silent.generation, live.generation);
        assert_eq!(silent.input_command_id.as_deref(), Some(holding.as_str()));

        // Back at the host, but reporting the ordinary end of a role — a
        // backgrounded page reports exactly this — without claiming to have
        // handed anything back.
        store
            .complete_device_command(
                &phone,
                &holding,
                DeviceCommandState::Succeeded,
                Some(&serde_json::json!({ "role": "input", "duration_ms": 10 })),
                None,
                None,
                Some("execution-one"),
                now + 10,
            )
            .unwrap();
        let backgrounded = route(&paths, "chat-dropped").unwrap().unwrap();
        assert_eq!(backgrounded.input_endpoint, microphone, "the route still names the phone the operator chose");
        assert_eq!(backgrounded.generation, live.generation, "no generation was minted for a role nobody handed back");
        let _ = std::fs::remove_dir_all(app_data);
    }

    /// Moving the speaker onto the phone that already holds the microphone was
    /// unreachable: the prepare command queued behind the device's own running
    /// microphone command, which a paired controller executes strictly one at a
    /// time, so it could never be acknowledged inside the eight second bound.
    #[test]
    fn the_speaker_can_move_onto_the_phone_already_holding_the_microphone() {
        let (app_data, paths, secrets) = fixture();
        let now = super::super::now_ms_public().unwrap();
        let mut store = RemoteStore::open(&paths.root).unwrap();
        let phone = ready_phone(&mut store, &secrets, "Phone", now);
        set_route(&paths, "chat-duplex", "pipeline", &format!("paired:{phone}:input"), LOCAL_OUTPUT_DEFAULT, now).unwrap();
        let live = activate_route(&paths, "chat-duplex", now).unwrap();
        let holding = live.input_command_id.clone().unwrap();
        capture_running(&mut store, &phone, &holding, now);

        let root = paths.root.clone();
        let releasing = holding.clone();
        let releasing_device = phone.clone();
        let device = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            RemoteStore::open(&root)
                .unwrap()
                .complete_device_command(
                    &releasing_device,
                    &releasing,
                    DeviceCommandState::Cancelled,
                    None,
                    None,
                    Some("released"),
                    Some("execution-one"),
                    now + 200,
                )
                .unwrap();
        });

        let moved = move_route(&paths, "chat-duplex", None, Some(&format!("paired:{phone}:output")), now).unwrap();
        device.join().unwrap();
        assert!(moved.generation > live.generation);
        assert_eq!(moved.input_command_id, moved.output_command_id, "one device carrying both roles runs one command");
        assert_eq!(role_of(&store, moved.input_command_id.as_deref().unwrap()), "duplex");
        assert!(
            store
                .device_commands(&phone, 50)
                .unwrap()
                .iter()
                .all(|command| command.arguments["mode"] != "talk_route_prepare"),
            "a device already carrying this route must not be asked to prepare behind its own running command"
        );
        let _ = std::fs::remove_dir_all(app_data);
    }
}
