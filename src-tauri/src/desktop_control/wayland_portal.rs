//! Wayland Computer Use transport through xdg-desktop-portal.
//!
//! Wayland deliberately does not expose unrestricted global input or screen
//! capture. This backend therefore uses the compositor-mediated
//! `org.freedesktop.portal.RemoteDesktop`, `ScreenCast`, and `Screenshot`
//! interfaces. The portal owns the user-consent dialog; Little Monkey never
//! bypasses compositor security.
//!
//! Semantic UI remains AT-SPI in `desktop_control.rs`. This module only owns
//! compositor-controlled raw pointer/keyboard input and active-window capture.
//! RemoteDesktop also offers EIS/libei. We intentionally use the standard
//! D-Bus Notify* transport: it has the same portal grant/security boundary,
//! works without another native input stack, and we never call ConnectToEIS,
//! so the mutually-exclusive transports cannot be mixed accidentally.

use super::{DesktopInputBackend, MouseButtonKind};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::sync::mpsc;
use std::time::Duration;
use url::Url;
use uuid::Uuid;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, Proxy};

const PORTAL_SERVICE: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const REMOTE_DESKTOP_IFACE: &str = "org.freedesktop.portal.RemoteDesktop";
const SCREENCAST_IFACE: &str = "org.freedesktop.portal.ScreenCast";
const SCREENSHOT_IFACE: &str = "org.freedesktop.portal.Screenshot";
const REQUEST_IFACE: &str = "org.freedesktop.portal.Request";
const SESSION_IFACE: &str = "org.freedesktop.portal.Session";

const DEVICE_KEYBOARD: u32 = 1;
const DEVICE_POINTER: u32 = 2;
const REQUIRED_DEVICES: u32 = DEVICE_KEYBOARD | DEVICE_POINTER;
const SOURCE_MONITOR: u32 = 1;
const CURSOR_HIDDEN: u32 = 1;
const SCREENSHOT_ACTIVE_WINDOW: u32 = 8;
const PERSIST_WHILE_RUNNING: u32 = 1;
const PORTAL_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const BACKEND_COMMAND_TIMEOUT: Duration = Duration::from_secs(130);
const SCREENSHOT_MAX_BYTES: usize = 8 * 1024 * 1024;

const BTN_LEFT: i32 = 272;
const BTN_RIGHT: i32 = 273;
const BTN_MIDDLE: i32 = 274;
const KEY_RETURN: i32 = 0xff0d;
const KEY_TAB: i32 = 0xff09;
const KEY_ESCAPE: i32 = 0xff1b;
const KEY_BACKSPACE: i32 = 0xff08;
const KEY_CONTROL_L: i32 = 0xffe3;
const KEY_ALT_L: i32 = 0xffe9;
const KEY_SHIFT_L: i32 = 0xffe1;
const KEY_SUPER_L: i32 = 0xffeb;
const KEY_DELETE: i32 = 0xffff;
const KEY_UP: i32 = 0xff52;
const KEY_DOWN: i32 = 0xff54;
const KEY_LEFT: i32 = 0xff51;
const KEY_RIGHT: i32 = 0xff53;

type Vardict = HashMap<String, OwnedValue>;

#[derive(Clone, Debug, PartialEq)]
struct PortalStream {
    node_id: u32,
    position: (i32, i32),
    logical_size: (i32, i32),
}

impl PortalStream {
    fn contains(&self, x: i32, y: i32) -> bool {
        let (px, py) = self.position;
        let (width, height) = self.logical_size;
        width > 0
            && height > 0
            && x >= px
            && y >= py
            && i64::from(x) < i64::from(px) + i64::from(width)
            && i64::from(y) < i64::from(py) + i64::from(height)
    }

    fn local_coordinates(&self, x: i32, y: i32) -> Option<(f64, f64)> {
        self.contains(x, y).then_some((
            f64::from(x - self.position.0),
            f64::from(y - self.position.1),
        ))
    }
}

#[derive(Debug)]
struct PortalSession {
    path: OwnedObjectPath,
    streams: Vec<PortalStream>,
    devices: u32,
}

#[derive(Debug)]
enum InputCommand {
    Move(i32, i32),
    Click(MouseButtonKind),
    Drag(i32, i32, i32, i32),
    Scroll(i32, i32),
    Key(String),
    Hotkey(Vec<String>),
    Shutdown,
}

struct WorkerMessage {
    command: InputCommand,
    reply: mpsc::SyncSender<Result<(), String>>,
}

/// Production raw-input backend for a Wayland session.
///
/// The D-Bus connection and portal session stay on one worker thread. This
/// keeps the synchronous `DesktopInputBackend` contract safe for Tauri and the
/// resident daemon without nesting async runtimes.
pub(super) struct WaylandPortalBackend {
    sender: mpsc::Sender<WorkerMessage>,
}

impl WaylandPortalBackend {
    pub(super) fn new() -> Result<Self, String> {
        let (sender, receiver) = mpsc::channel::<WorkerMessage>();
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), String>>(1);
        std::thread::Builder::new()
            .name("computer-use-wayland-portal".to_string())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = ready_tx.send(Err(capability_error(format!(
                            "could not create the portal runtime: {error}"
                        ))));
                        return;
                    }
                };
                runtime.block_on(async move {
                    let connection = match Connection::session().await {
                        Ok(connection) => connection,
                        Err(error) => {
                            let _ = ready_tx.send(Err(capability_error(format!(
                                "could not connect to the user D-Bus session: {error}"
                            ))));
                            return;
                        }
                    };
                    if let Err(error) = probe_remote_desktop(&connection).await {
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                    if ready_tx.send(Ok(())).is_err() {
                        return;
                    }
                    let mut core = PortalCore {
                        connection,
                        session: None,
                    };
                    while let Ok(message) = receiver.recv() {
                        if matches!(message.command, InputCommand::Shutdown) {
                            let result = core.close_session().await;
                            let _ = message.reply.send(result);
                            break;
                        }
                        let result = core.execute(message.command).await;
                        let _ = message.reply.send(result);
                    }
                    let _ = core.close_session().await;
                });
            })
            .map_err(|error| capability_error(format!("could not start portal worker: {error}")))?;

        match ready_rx.recv_timeout(Duration::from_secs(15)) {
            Ok(Ok(())) => Ok(Self { sender }),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(capability_error(
                "the Wayland portal capability probe timed out".to_string(),
            )),
        }
    }

    fn request(&self, command: InputCommand) -> Result<(), String> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.sender
            .send(WorkerMessage {
                command,
                reply: reply_tx,
            })
            .map_err(|_| capability_error("the Wayland portal worker stopped".to_string()))?;
        reply_rx.recv_timeout(BACKEND_COMMAND_TIMEOUT).map_err(|_| {
            capability_error("the Wayland portal input request timed out".to_string())
        })?
    }
}

impl Drop for WaylandPortalBackend {
    fn drop(&mut self) {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        if self
            .sender
            .send(WorkerMessage {
                command: InputCommand::Shutdown,
                reply: reply_tx,
            })
            .is_ok()
        {
            let _ = reply_rx.recv_timeout(Duration::from_secs(2));
        }
    }
}

impl DesktopInputBackend for WaylandPortalBackend {
    fn move_mouse(&self, x: i32, y: i32) -> Result<(), String> {
        self.request(InputCommand::Move(x, y))
    }

    fn click(&self, button: MouseButtonKind) -> Result<(), String> {
        self.request(InputCommand::Click(button))
    }

    fn key_press(&self, key: &str) -> Result<(), String> {
        self.request(InputCommand::Key(key.to_string()))
    }

    fn drag(&self, from_x: i32, from_y: i32, to_x: i32, to_y: i32) -> Result<(), String> {
        self.request(InputCommand::Drag(from_x, from_y, to_x, to_y))
    }

    fn scroll(&self, delta_x: i32, delta_y: i32) -> Result<(), String> {
        self.request(InputCommand::Scroll(delta_x, delta_y))
    }

    fn hotkey(&self, keys: &[String]) -> Result<(), String> {
        self.request(InputCommand::Hotkey(keys.to_vec()))
    }
}

struct PortalCore {
    connection: Connection,
    session: Option<PortalSession>,
}

impl PortalCore {
    async fn execute(&mut self, command: InputCommand) -> Result<(), String> {
        self.ensure_session().await?;
        match command {
            InputCommand::Move(x, y) => self.move_pointer(x, y).await,
            InputCommand::Click(button) => self.click(button).await,
            InputCommand::Drag(from_x, from_y, to_x, to_y) => {
                self.move_pointer(from_x, from_y).await?;
                self.pointer_button(MouseButtonKind::Left, 1).await?;
                if let Err(error) = self.move_pointer(to_x, to_y).await {
                    let _ = self.pointer_button(MouseButtonKind::Left, 0).await;
                    return Err(error);
                }
                self.pointer_button(MouseButtonKind::Left, 0).await
            }
            InputCommand::Scroll(delta_x, delta_y) => self.scroll(delta_x, delta_y).await,
            InputCommand::Key(key) => {
                let keysym = keysym_for_key(&key)?;
                self.keysym(keysym, 1).await?;
                self.keysym(keysym, 0).await
            }
            InputCommand::Hotkey(keys) => {
                let parsed: Result<Vec<i32>, String> =
                    keys.iter().map(|key| keysym_for_key(key)).collect();
                let parsed = parsed?;
                for keysym in &parsed {
                    self.keysym(*keysym, 1).await?;
                }
                for keysym in parsed.iter().rev() {
                    self.keysym(*keysym, 0).await?;
                }
                Ok(())
            }
            InputCommand::Shutdown => Ok(()),
        }
    }

    async fn ensure_session(&mut self) -> Result<(), String> {
        if self.session.is_none() {
            self.session = Some(create_remote_desktop_session(&self.connection).await?);
        }
        Ok(())
    }

    fn session(&self) -> Result<&PortalSession, String> {
        self.session
            .as_ref()
            .ok_or_else(|| capability_error("the portal session is not active".to_string()))
    }

    async fn remote_proxy(&self) -> Result<Proxy<'_>, String> {
        Proxy::new(
            &self.connection,
            PORTAL_SERVICE,
            PORTAL_PATH,
            REMOTE_DESKTOP_IFACE,
        )
        .await
        .map_err(|error| portal_error("create RemoteDesktop proxy", error))
    }

    async fn move_pointer(&self, x: i32, y: i32) -> Result<(), String> {
        let session = self.session()?;
        require_devices(session.devices, DEVICE_POINTER, "pointer")?;
        let stream = session
            .streams
            .iter()
            .find(|stream| stream.contains(x, y))
            .ok_or_else(|| {
                capability_error(format!(
                    "the approved monitor set does not contain coordinate ({x}, {y})"
                ))
            })?;
        let (local_x, local_y) = stream.local_coordinates(x, y).ok_or_else(|| {
            capability_error("could not map pointer coordinate into approved stream".to_string())
        })?;
        let proxy = self.remote_proxy().await?;
        let options: HashMap<&str, Value<'_>> = HashMap::new();
        proxy
            .call::<_, _, ()>(
                "NotifyPointerMotionAbsolute",
                &(&session.path, options, stream.node_id, local_x, local_y),
            )
            .await
            .map_err(|error| portal_error("send absolute pointer motion", error))
    }

    async fn pointer_button(&self, button: MouseButtonKind, state: u32) -> Result<(), String> {
        let session = self.session()?;
        require_devices(session.devices, DEVICE_POINTER, "pointer")?;
        let code = match button {
            MouseButtonKind::Left => BTN_LEFT,
            MouseButtonKind::Right => BTN_RIGHT,
            MouseButtonKind::Middle => BTN_MIDDLE,
        };
        let proxy = self.remote_proxy().await?;
        let options: HashMap<&str, Value<'_>> = HashMap::new();
        proxy
            .call::<_, _, ()>("NotifyPointerButton", &(&session.path, options, code, state))
            .await
            .map_err(|error| portal_error("send pointer button", error))
    }

    async fn click(&self, button: MouseButtonKind) -> Result<(), String> {
        self.pointer_button(button, 1).await?;
        self.pointer_button(button, 0).await
    }

    async fn scroll(&self, delta_x: i32, delta_y: i32) -> Result<(), String> {
        let session = self.session()?;
        require_devices(session.devices, DEVICE_POINTER, "pointer")?;
        let proxy = self.remote_proxy().await?;
        let mut options: HashMap<&str, Value<'_>> = HashMap::new();
        options.insert("finish", Value::from(true));
        proxy
            .call::<_, _, ()>(
                "NotifyPointerAxis",
                &(
                    &session.path,
                    options,
                    f64::from(delta_x),
                    f64::from(delta_y),
                ),
            )
            .await
            .map_err(|error| portal_error("send pointer scroll", error))
    }

    async fn keysym(&self, keysym: i32, state: u32) -> Result<(), String> {
        let session = self.session()?;
        require_devices(session.devices, DEVICE_KEYBOARD, "keyboard")?;
        let proxy = self.remote_proxy().await?;
        let options: HashMap<&str, Value<'_>> = HashMap::new();
        proxy
            .call::<_, _, ()>("NotifyKeyboardKeysym", &(&session.path, options, keysym, state))
            .await
            .map_err(|error| portal_error("send keyboard input", error))
    }

    async fn close_session(&mut self) -> Result<(), String> {
        let Some(session) = self.session.take() else {
            return Ok(());
        };
        close_session_path(&self.connection, &session.path).await
    }
}

async fn probe_remote_desktop(connection: &Connection) -> Result<(), String> {
    let remote = Proxy::new(
        connection,
        PORTAL_SERVICE,
        PORTAL_PATH,
        REMOTE_DESKTOP_IFACE,
    )
    .await
    .map_err(|error| portal_error("create RemoteDesktop proxy", error))?;
    let available: u32 = remote
        .get_property("AvailableDeviceTypes")
        .await
        .map_err(|error| portal_error("read RemoteDesktop.AvailableDeviceTypes", error))?;
    if available & REQUIRED_DEVICES != REQUIRED_DEVICES {
        return Err(capability_error(format!(
            "the compositor portal does not provide both keyboard and pointer RemoteDesktop devices (available bitmask {available})"
        )));
    }

    let screencast = Proxy::new(
        connection,
        PORTAL_SERVICE,
        PORTAL_PATH,
        SCREENCAST_IFACE,
    )
    .await
    .map_err(|error| portal_error("create ScreenCast proxy", error))?;
    let sources: u32 = screencast
        .get_property("AvailableSourceTypes")
        .await
        .map_err(|error| portal_error("read ScreenCast.AvailableSourceTypes", error))?;
    if sources & SOURCE_MONITOR == 0 {
        return Err(capability_error(
            "the compositor portal does not expose monitor streams required for bounded absolute pointer mapping"
                .to_string(),
        ));
    }
    Ok(())
}

async fn create_remote_desktop_session(connection: &Connection) -> Result<PortalSession, String> {
    let remote = Proxy::new(
        connection,
        PORTAL_SERVICE,
        PORTAL_PATH,
        REMOTE_DESKTOP_IFACE,
    )
    .await
    .map_err(|error| portal_error("create RemoteDesktop proxy", error))?;

    let create_token = token("create");
    let session_token = token("session");
    let mut create_options = HashMap::<&str, Value<'_>>::new();
    create_options.insert("handle_token", Value::from(create_token.as_str()));
    create_options.insert("session_handle_token", Value::from(session_token.as_str()));
    let create_results = portal_request(
        connection,
        &remote,
        "CreateSession",
        &create_token,
        &create_options,
    )
    .await?;
    let session_string = value_string(&create_results, "session_handle")?;
    let session_path = OwnedObjectPath::try_from(session_string.as_str()).map_err(|error| {
        capability_error(format!(
            "RemoteDesktop returned an invalid session object path: {error}"
        ))
    })?;

    let expected_session = portal_session_path(connection, &session_token)?;
    if session_path.as_str() != expected_session {
        let _ = close_session_path(connection, &session_path).await;
        return Err(capability_error(format!(
            "RemoteDesktop returned session path {}; expected {expected_session}",
            session_path.as_str()
        )));
    }

    let result = async {
        let select_token = token("devices");
        let mut select_options = HashMap::<&str, Value<'_>>::new();
        select_options.insert("handle_token", Value::from(select_token.as_str()));
        select_options.insert("types", Value::from(REQUIRED_DEVICES));
        select_options.insert("persist_mode", Value::from(PERSIST_WHILE_RUNNING));
        portal_request(
            connection,
            &remote,
            "SelectDevices",
            &select_token,
            &(&session_path, select_options),
        )
        .await?;

        let screencast = Proxy::new(
            connection,
            PORTAL_SERVICE,
            PORTAL_PATH,
            SCREENCAST_IFACE,
        )
        .await
        .map_err(|error| portal_error("create ScreenCast proxy", error))?;
        let sources_token = token("sources");
        let mut source_options = HashMap::<&str, Value<'_>>::new();
        source_options.insert("handle_token", Value::from(sources_token.as_str()));
        source_options.insert("types", Value::from(SOURCE_MONITOR));
        source_options.insert("multiple", Value::from(true));
        source_options.insert("cursor_mode", Value::from(CURSOR_HIDDEN));
        portal_request(
            connection,
            &screencast,
            "SelectSources",
            &sources_token,
            &(&session_path, source_options),
        )
        .await?;

        let start_token = token("start");
        let mut start_options = HashMap::<&str, Value<'_>>::new();
        start_options.insert("handle_token", Value::from(start_token.as_str()));
        let results = portal_request(
            connection,
            &remote,
            "Start",
            &start_token,
            &(&session_path, "", start_options),
        )
        .await?;
        let devices = value_u32(&results, "devices")?;
        require_devices(devices, REQUIRED_DEVICES, "keyboard and pointer")?;
        let streams = value_streams(&results)?;
        if streams.is_empty() {
            return Err(capability_error(
                "the approved RemoteDesktop session returned no monitor stream for absolute pointer mapping"
                    .to_string(),
            ));
        }
        Ok(PortalSession {
            path: session_path.clone(),
            streams,
            devices,
        })
    }
    .await;

    if result.is_err() {
        let _ = close_session_path(connection, &session_path).await;
    }
    result
}

async fn portal_request<B>(
    connection: &Connection,
    proxy: &Proxy<'_>,
    method: &str,
    token: &str,
    body: &B,
) -> Result<Vardict, String>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    let expected_path = portal_request_path(connection, token)?;
    let request = Proxy::new(
        connection,
        PORTAL_SERVICE,
        expected_path.as_str(),
        REQUEST_IFACE,
    )
    .await
    .map_err(|error| portal_error("create portal Request proxy", error))?;
    // Subscribe before invoking the method. A fast backend may emit Response
    // immediately, and subscribing afterwards creates a real lost-signal race.
    let mut responses = request
        .receive_signal("Response")
        .await
        .map_err(|error| portal_error("subscribe to portal Response", error))?;
    let returned: OwnedObjectPath = proxy
        .call(method, body)
        .await
        .map_err(|error| portal_error(&format!("call {method}"), error))?;
    if returned.as_str() != expected_path {
        return Err(capability_error(format!(
            "portal method {method} returned request path {}; expected {expected_path}",
            returned.as_str()
        )));
    }

    let response = tokio::time::timeout(PORTAL_REQUEST_TIMEOUT, responses.next())
        .await
        .map_err(|_| capability_error(format!("portal request {method} timed out")))?
        .ok_or_else(|| capability_error(format!("portal request {method} closed without a response")))?;
    let (code, results): (u32, Vardict) = response
        .body()
        .deserialize()
        .map_err(|error| portal_error(&format!("decode {method} response"), error))?;
    match code {
        0 => Ok(results),
        1 => Err(format!(
            "WAYLAND_OPERATOR_CANCELLED: the user cancelled portal request {method}"
        )),
        other => Err(capability_error(format!(
            "portal request {method} failed with response code {other}"
        ))),
    }
}

async fn close_session_path(connection: &Connection, path: &OwnedObjectPath) -> Result<(), String> {
    let proxy = Proxy::new(connection, PORTAL_SERVICE, path.as_str(), SESSION_IFACE)
        .await
        .map_err(|error| portal_error("create portal Session proxy", error))?;
    proxy
        .call::<_, _, ()>("Close", &())
        .await
        .map_err(|error| portal_error("close portal Session", error))
}

fn portal_sender(connection: &Connection) -> Result<String, String> {
    let unique = connection.unique_name().ok_or_else(|| {
        capability_error("the D-Bus connection has no unique sender name".to_string())
    })?;
    Ok(unique.as_str().trim_start_matches(':').replace('.', "_"))
}

fn portal_request_path(connection: &Connection, token: &str) -> Result<String, String> {
    Ok(format!(
        "/org/freedesktop/portal/desktop/request/{}/{token}",
        portal_sender(connection)?
    ))
}

fn portal_session_path(connection: &Connection, token: &str) -> Result<String, String> {
    Ok(format!(
        "/org/freedesktop/portal/desktop/session/{}/{token}",
        portal_sender(connection)?
    ))
}

fn token(prefix: &str) -> String {
    format!("lm_{prefix}_{}", Uuid::new_v4().simple())
}

fn value_string(values: &Vardict, key: &str) -> Result<String, String> {
    let value = values
        .get(key)
        .ok_or_else(|| capability_error(format!("portal response omitted {key}")))?
        .try_clone()
        .map_err(|error| portal_error(&format!("clone portal field {key}"), error))?;
    String::try_from(value)
        .map_err(|error| portal_error(&format!("decode portal field {key} as string"), error))
}

fn value_u32(values: &Vardict, key: &str) -> Result<u32, String> {
    let value = values
        .get(key)
        .ok_or_else(|| capability_error(format!("portal response omitted {key}")))?
        .try_clone()
        .map_err(|error| portal_error(&format!("clone portal field {key}"), error))?;
    u32::try_from(value)
        .map_err(|error| portal_error(&format!("decode portal field {key} as u32"), error))
}

fn value_streams(values: &Vardict) -> Result<Vec<PortalStream>, String> {
    let value = values
        .get("streams")
        .ok_or_else(|| capability_error("portal response omitted streams".to_string()))?
        .try_clone()
        .map_err(|error| portal_error("clone portal streams", error))?;
    let raw: Vec<(u32, Vardict)> = Vec::try_from(value)
        .map_err(|error| portal_error("decode portal streams", error))?;
    raw.into_iter()
        .map(|(node_id, props)| {
            let position = optional_pair_i32(&props, "position").unwrap_or((0, 0));
            let logical_size = optional_pair_i32(&props, "logical_size")
                .or_else(|| optional_pair_i32(&props, "size"))
                .ok_or_else(|| {
                    capability_error(format!(
                        "portal stream {node_id} has no logical_size/size metadata"
                    ))
                })?;
            if logical_size.0 <= 0 || logical_size.1 <= 0 {
                return Err(capability_error(format!(
                    "portal stream {node_id} has invalid logical size {logical_size:?}"
                )));
            }
            Ok(PortalStream {
                node_id,
                position,
                logical_size,
            })
        })
        .collect()
}

fn optional_pair_i32(values: &Vardict, key: &str) -> Option<(i32, i32)> {
    let value = values.get(key)?.try_clone().ok()?;
    <(i32, i32)>::try_from(value).ok()
}

fn require_devices(devices: u32, required: u32, label: &str) -> Result<(), String> {
    if devices & required == required {
        Ok(())
    } else {
        Err(capability_error(format!(
            "the user/compositor did not grant required {label} access (granted bitmask {devices})"
        )))
    }
}

fn keysym_for_char(character: char) -> Result<i32, String> {
    let code = u32::from(character);
    let keysym = if code <= 0xff { code } else { 0x0100_0000 | code };
    i32::try_from(keysym).map_err(|_| "Unsupported Unicode key symbol".to_string())
}

fn keysym_for_key(key: &str) -> Result<i32, String> {
    let mut chars = key.chars();
    if let (Some(single), None) = (chars.next(), chars.next()) {
        return keysym_for_char(single);
    }
    Ok(match key.to_ascii_lowercase().as_str() {
        "enter" | "return" => KEY_RETURN,
        "tab" => KEY_TAB,
        "space" => i32::from(b' '),
        "escape" | "esc" => KEY_ESCAPE,
        "backspace" => KEY_BACKSPACE,
        "ctrl" | "control" => KEY_CONTROL_L,
        "alt" | "option" => KEY_ALT_L,
        "shift" => KEY_SHIFT_L,
        "cmd" | "command" | "meta" | "super" | "windows" | "win" => KEY_SUPER_L,
        "delete" => KEY_DELETE,
        "up" => KEY_UP,
        "down" => KEY_DOWN,
        "left" => KEY_LEFT,
        "right" => KEY_RIGHT,
        _ => return Err(format!("Unsupported key name: {key}")),
    })
}

fn capability_error(message: String) -> String {
    format!("WAYLAND_CAPABILITY_UNAVAILABLE: {message}")
}

fn portal_error(context: &str, error: impl std::fmt::Display) -> String {
    capability_error(format!("{context}: {error}"))
}

/// Captures only the currently active window through Screenshot portal v3.
/// The caller must freshly verify that its allowlisted target is frontmost.
/// Requiring ActiveWindow avoids a whole-display fallback that would expose
/// unrelated applications.
pub(super) fn screenshot_active_window() -> Result<Vec<u8>, String> {
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("computer-use-wayland-screenshot".to_string())
        .spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| capability_error(format!("could not create screenshot runtime: {error}")))
                .and_then(|runtime| runtime.block_on(screenshot_active_window_async()));
            let _ = tx.send(result);
        })
        .map_err(|error| capability_error(format!("could not start screenshot worker: {error}")))?;
    rx.recv_timeout(BACKEND_COMMAND_TIMEOUT)
        .map_err(|_| capability_error("active-window screenshot timed out".to_string()))?
}

async fn screenshot_active_window_async() -> Result<Vec<u8>, String> {
    let connection = Connection::session()
        .await
        .map_err(|error| portal_error("connect to D-Bus for screenshot", error))?;
    let screenshot = Proxy::new(
        &connection,
        PORTAL_SERVICE,
        PORTAL_PATH,
        SCREENSHOT_IFACE,
    )
    .await
    .map_err(|error| portal_error("create Screenshot proxy", error))?;
    let version: u32 = screenshot
        .get_property("version")
        .await
        .map_err(|error| portal_error("read Screenshot.version", error))?;
    if version < 3 {
        return Err(capability_error(format!(
            "Screenshot portal version {version} cannot guarantee active-window-only capture; version 3+ is required"
        )));
    }
    let available: u32 = screenshot
        .get_property("AvailableTargets")
        .await
        .map_err(|error| portal_error("read Screenshot.AvailableTargets", error))?;
    if available & SCREENSHOT_ACTIVE_WINDOW == 0 {
        return Err(capability_error(
            "Screenshot portal does not support ActiveWindow capture; refusing whole-screen fallback"
                .to_string(),
        ));
    }
    let request_token = token("shot");
    let mut options = HashMap::<&str, Value<'_>>::new();
    options.insert("handle_token", Value::from(request_token.as_str()));
    options.insert("interactive", Value::from(false));
    options.insert("target", Value::from(SCREENSHOT_ACTIVE_WINDOW));
    let results = portal_request(
        &connection,
        &screenshot,
        "Screenshot",
        &request_token,
        &("", options),
    )
    .await?;
    let uri = value_string(&results, "uri")?;
    let url = Url::parse(&uri)
        .map_err(|error| capability_error(format!("Screenshot portal returned invalid URI: {error}")))?;
    if url.scheme() != "file" {
        return Err(capability_error(format!(
            "Screenshot portal returned unsupported URI scheme {}",
            url.scheme()
        )));
    }
    let path = url
        .to_file_path()
        .map_err(|_| capability_error("Screenshot portal returned a non-local file URI".to_string()))?;
    let metadata = std::fs::metadata(&path)
        .map_err(|error| capability_error(format!("could not stat portal screenshot: {error}")))?;
    if metadata.len() > SCREENSHOT_MAX_BYTES as u64 {
        let _ = std::fs::remove_file(&path);
        return Err(capability_error(format!(
            "portal screenshot exceeds the {SCREENSHOT_MAX_BYTES} byte artifact limit"
        )));
    }
    let bytes = std::fs::read(&path)
        .map_err(|error| capability_error(format!("could not read portal screenshot: {error}")))?;
    let _ = std::fs::remove_file(&path);
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err(capability_error(
            "Screenshot portal did not return a PNG image".to_string(),
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_global_coordinates_into_selected_monitor_stream() {
        let left = PortalStream {
            node_id: 11,
            position: (-1920, 0),
            logical_size: (1920, 1080),
        };
        let primary = PortalStream {
            node_id: 22,
            position: (0, 0),
            logical_size: (2560, 1440),
        };
        assert_eq!(left.local_coordinates(-100, 500), Some((1820.0, 500.0)));
        assert_eq!(primary.local_coordinates(100, 500), Some((100.0, 500.0)));
        assert_eq!(primary.local_coordinates(-1, 500), None);
        assert_eq!(primary.local_coordinates(2560, 500), None);
    }

    #[test]
    fn unicode_keysyms_follow_xkb_unicode_encoding() {
        assert_eq!(keysym_for_char('A').unwrap(), 0x41);
        assert_eq!(keysym_for_char('é').unwrap(), 0xe9);
        assert_eq!(keysym_for_char('λ').unwrap(), 0x0100_03bb);
    }

    #[test]
    fn named_keysyms_match_x11_values() {
        assert_eq!(keysym_for_key("enter").unwrap(), KEY_RETURN);
        assert_eq!(keysym_for_key("ctrl").unwrap(), KEY_CONTROL_L);
        assert_eq!(keysym_for_key("super").unwrap(), KEY_SUPER_L);
        assert!(keysym_for_key("definitely-not-a-key").is_err());
    }

    #[test]
    fn required_device_check_is_fail_closed() {
        assert!(require_devices(REQUIRED_DEVICES, REQUIRED_DEVICES, "all").is_ok());
        assert!(require_devices(DEVICE_POINTER, REQUIRED_DEVICES, "all").is_err());
    }
}
