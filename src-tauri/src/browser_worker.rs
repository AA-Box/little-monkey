//! Isolated, owned Chromium verification worker.
//!
//! The first slice intentionally exposes browser-page actions only. It uses a
//! disposable profile, Chrome DevTools Protocol request interception, exact
//! per-run origin grants, DNS re-resolution, and the durable artifact store.
//! No file URLs, uploads, downloads, clipboard, extension, or host-desktop
//! capability is exposed.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use url::Url;

use crate::artifact_store::{ArtifactBlob, ArtifactStore};

const PROFILE_MARKER: &str = ".little-monkey-browser-profile";
const MAX_CDP_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
const MAX_DEVTOOLS_HTTP_BYTES: usize = 2 * 1024 * 1024;
const MAX_SELECTOR_BYTES: usize = 8 * 1024;
const MAX_TYPE_BYTES: usize = 256 * 1024;
const MAX_ALLOWED_ORIGINS: usize = 32;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowserLimits {
    pub timeout_ms: u64,
    #[serde(default = "default_max_session_ms")]
    pub max_session_ms: u64,
    pub max_actions: u64,
    pub max_dom_bytes: u64,
    pub max_screenshot_bytes: u64,
    pub max_log_entries: usize,
    #[serde(default = "default_max_disk_bytes")]
    pub max_disk_bytes: u64,
}

fn default_max_session_ms() -> u64 {
    10 * 60_000
}

fn default_max_disk_bytes() -> u64 {
    256 * 1024 * 1024
}

impl Default for BrowserLimits {
    fn default() -> Self {
        Self {
            timeout_ms: 60_000,
            max_session_ms: default_max_session_ms(),
            max_actions: 100,
            max_dom_bytes: 4 * 1024 * 1024,
            max_screenshot_bytes: 12 * 1024 * 1024,
            max_log_entries: 2_000,
            max_disk_bytes: default_max_disk_bytes(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowserGrant {
    pub allowed_origins: Vec<String>,
    #[serde(default)]
    pub allow_loopback: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowserStartRequest {
    pub run_id: String,
    pub url: String,
    pub grant: BrowserGrant,
    #[serde(default)]
    pub limits: BrowserLimits,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowserViewport {
    pub width: u32,
    pub height: u32,
    pub device_scale_factor: f64,
    #[serde(default)]
    pub mobile: bool,
}

impl Default for BrowserViewport {
    fn default() -> Self {
        Self {
            width: 1440,
            height: 900,
            device_scale_factor: 1.0,
            mobile: false,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserSessionView {
    pub session_id: String,
    pub run_id: String,
    pub current_url: String,
    pub started_at_ms: u64,
    pub action_count: u64,
    pub cancelled: bool,
    pub viewport: BrowserViewport,
}

/// Minimal, secret-free grant view consumed by Security Doctor. It exposes
/// exact origins and the explicit loopback bit, never CDP connection details,
/// page content, cookies, or captured evidence.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserSecurityGrant {
    pub session_id: String,
    pub run_id: String,
    pub allowed_origins: Vec<String>,
    pub allow_loopback: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserEvidence {
    pub screenshot: Option<ArtifactBlob>,
    pub dom: Option<ArtifactBlob>,
    pub accessibility: Option<ArtifactBlob>,
    pub console: Option<ArtifactBlob>,
    pub network: Option<ArtifactBlob>,
    pub performance: Option<ArtifactBlob>,
    pub action_count: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserAnnotationRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserAnnotation {
    pub url: String,
    pub selector: String,
    pub tag: String,
    pub role: String,
    pub aria_label: String,
    pub text: String,
    pub rect: BrowserAnnotationRect,
    pub evidence: BrowserEvidence,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserInspection {
    pub url: String,
    pub title: String,
    pub dom: ArtifactBlob,
    pub accessibility: ArtifactBlob,
    pub accessibility_issues: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserActionResult {
    pub ok: bool,
    pub url: String,
    pub evidence: BrowserEvidence,
}

pub struct BrowserCommandState {
    profile_root: PathBuf,
    sessions: Mutex<HashMap<String, Arc<OwnedBrowser>>>,
}

impl BrowserCommandState {
    pub fn production(app_data_dir: &Path) -> Result<Self, String> {
        let profile_root = app_data_dir.join("browser-v1").join("profiles");
        ensure_private_directory(&profile_root)?;
        Ok(Self {
            profile_root,
            sessions: Mutex::new(HashMap::new()),
        })
    }

    fn insert(&self, browser: Arc<OwnedBrowser>) -> Result<(), String> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| "Browser session lock is poisoned".to_string())?;
        if sessions.contains_key(&browser.session_id) {
            return Err("Browser session id collision".to_string());
        }
        sessions.insert(browser.session_id.clone(), browser);
        Ok(())
    }

    fn get(&self, session_id: &str) -> Result<Arc<OwnedBrowser>, String> {
        validate_identifier("sessionId", session_id)?;
        self.sessions
            .lock()
            .map_err(|_| "Browser session lock is poisoned".to_string())?
            .get(session_id)
            .cloned()
            .ok_or_else(|| "Unknown browser session".to_string())
    }

    fn remove(&self, session_id: &str) -> Result<Option<Arc<OwnedBrowser>>, String> {
        validate_identifier("sessionId", session_id)?;
        Ok(self
            .sessions
            .lock()
            .map_err(|_| "Browser session lock is poisoned".to_string())?
            .remove(session_id))
    }

    /// Stops and removes every owned browser session for one workflow/run.
    /// The session map is drained before any potentially blocking process
    /// wait so a concurrent list/action cannot retain an already-cancelled
    /// browser. Calling this more than once is harmless.
    pub fn shutdown_run(&self, run_id: &str) -> Result<usize, String> {
        validate_identifier("runId", run_id)?;
        let browsers = {
            let mut sessions = self
                .sessions
                .lock()
                .map_err(|_| "Browser session lock is poisoned".to_string())?;
            let ids = sessions
                .iter()
                .filter(|(_, browser)| browser.run_id == run_id)
                .map(|(session_id, _)| session_id.clone())
                .collect::<Vec<_>>();
            ids.into_iter()
                .filter_map(|session_id| sessions.remove(&session_id))
                .collect::<Vec<_>>()
        };
        stop_browsers(browsers)
    }

    /// Synchronously terminates every Chromium process owned by this state.
    /// Tauri's final exit path skips Rust destructors, so the application exit
    /// handler must call this method explicitly rather than relying on Drop.
    pub fn shutdown_all(&self) -> Result<usize, String> {
        let browsers = {
            let mut sessions = self
                .sessions
                .lock()
                .map_err(|_| "Browser session lock is poisoned".to_string())?;
            sessions.drain().map(|(_, browser)| browser).collect()
        };
        stop_browsers(browsers)
    }

    /// Returns the active, exact origin grants for local posture inspection.
    /// The session map is copied before any CDP lock is taken so an audit
    /// never holds the global browser lock while waiting for an action.
    pub fn security_grants(&self) -> Result<Vec<BrowserSecurityGrant>, String> {
        let browsers = self
            .sessions
            .lock()
            .map_err(|_| "Browser session lock is poisoned".to_string())?
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut grants = Vec::with_capacity(browsers.len());
        for browser in browsers {
            let cdp = browser.cdp.try_lock().map_err(|error| match error {
                std::sync::TryLockError::Poisoned(_) => "Browser CDP lock is poisoned".to_string(),
                std::sync::TryLockError::WouldBlock => {
                    "A browser action is in progress; retry the audit when it finishes".to_string()
                }
            })?;
            grants.push(BrowserSecurityGrant {
                session_id: browser.session_id.clone(),
                run_id: browser.run_id.clone(),
                allowed_origins: cdp.grant.allowed_origins.clone(),
                allow_loopback: cdp.grant.allow_loopback,
            });
        }
        grants.sort_by(|left, right| left.session_id.cmp(&right.session_id));
        Ok(grants)
    }
}

fn stop_browsers(browsers: Vec<Arc<OwnedBrowser>>) -> Result<usize, String> {
    let count = browsers.len();
    let mut first_error = None;
    for browser in browsers {
        if let Err(error) = browser.stop() {
            first_error.get_or_insert(error);
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(count),
    }
}

/// Tauri-free browser adapter used by the workflow runtime and headless CLI.
/// It deliberately reuses the exact owned Chromium/session implementation
/// exposed by the desktop commands so both surfaces enforce identical origin,
/// DNS-rebinding, quota, disposable-profile, and artifact rules.
pub struct BrowserWorkflowAdapter {
    state: BrowserCommandState,
    artifacts: ArtifactStore,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkflowBrowserStartArguments {
    url: String,
    grant: BrowserGrant,
    #[serde(default)]
    limits: BrowserLimits,
}

impl BrowserWorkflowAdapter {
    pub fn production(app_data_dir: &Path) -> Result<Self, String> {
        Ok(Self {
            state: BrowserCommandState::production(app_data_dir)?,
            artifacts: ArtifactStore::new(app_data_dir.join("content-v1"))
                .map_err(|error| error.to_string())?,
        })
    }

    /// Executes one closed browser action. `run_id` is supplied by the
    /// workflow engine and cannot be forged through node arguments.
    pub fn execute(&self, run_id: &str, action: &str, arguments: Value) -> Result<Value, String> {
        let object = arguments
            .as_object()
            .ok_or_else(|| "browser arguments must be a JSON object".to_string())?;
        let string = |name: &str| -> Result<String, String> {
            object
                .get(name)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .ok_or_else(|| format!("browser action requires string {name}"))
        };
        let session = || -> Result<Arc<OwnedBrowser>, String> {
            let session_id = string("sessionId")?;
            self.state.get(&session_id)
        };
        match action {
            "start" => {
                let args: WorkflowBrowserStartArguments = serde_json::from_value(arguments.clone())
                    .map_err(|error| format!("invalid browser start arguments: {error}"))?;
                let browser = OwnedBrowser::launch(
                    self.state.profile_root.clone(),
                    self.artifacts.clone(),
                    BrowserStartRequest {
                        run_id: run_id.to_string(),
                        url: args.url,
                        grant: args.grant,
                        limits: args.limits,
                    },
                )?;
                let view = browser.view();
                self.state.insert(browser)?;
                serde_json::to_value(view).map_err(|error| error.to_string())
            }
            "list" => {
                let views = self
                    .state
                    .sessions
                    .lock()
                    .map_err(|_| "Browser session lock is poisoned".to_string())?
                    .values()
                    .map(|browser| browser.view())
                    .collect::<Vec<_>>();
                serde_json::to_value(views).map_err(|error| error.to_string())
            }
            "navigate" => serde_json::to_value(session()?.navigate(&string("url")?)?)
                .map_err(|error| error.to_string()),
            "reload" => {
                serde_json::to_value(session()?.reload()?).map_err(|error| error.to_string())
            }
            "set_viewport" => {
                let viewport: BrowserViewport = serde_json::from_value(
                    object
                        .get("viewport")
                        .cloned()
                        .ok_or_else(|| "browser set_viewport requires viewport".to_string())?,
                )
                .map_err(|error| format!("invalid browser viewport: {error}"))?;
                serde_json::to_value(session()?.set_viewport(viewport)?)
                    .map_err(|error| error.to_string())
            }
            "inspect" => {
                serde_json::to_value(session()?.inspect()?).map_err(|error| error.to_string())
            }
            "annotate" => serde_json::to_value(session()?.annotate(&string("selector")?)?)
                .map_err(|error| error.to_string()),
            "click" => serde_json::to_value(session()?.click(&string("selector")?)?)
                .map_err(|error| error.to_string()),
            "type_text" => {
                serde_json::to_value(session()?.type_text(&string("selector")?, &string("text")?)?)
                    .map_err(|error| error.to_string())
            }
            "scroll" => {
                let x = object
                    .get("x")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| "browser scroll requires integer x".to_string())?;
                let y = object
                    .get("y")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| "browser scroll requires integer y".to_string())?;
                serde_json::to_value(session()?.scroll(x, y)?).map_err(|error| error.to_string())
            }
            "screenshot" => {
                serde_json::to_value(session()?.screenshot()?).map_err(|error| error.to_string())
            }
            "capture_evidence" => serde_json::to_value(session()?.capture_evidence()?)
                .map_err(|error| error.to_string()),
            "stop" => {
                let session_id = string("sessionId")?;
                let browser = self
                    .state
                    .remove(&session_id)?
                    .ok_or_else(|| "Unknown browser session".to_string())?;
                browser.stop()?;
                Ok(json!({ "stopped": true, "sessionId": session_id }))
            }
            _ => Err(format!("unsupported browser workflow action: {action}")),
        }
    }

    pub fn shutdown_run(&self, run_id: &str) -> Result<usize, String> {
        self.state.shutdown_run(run_id)
    }

    pub fn shutdown_all(&self) -> Result<usize, String> {
        self.state.shutdown_all()
    }
}

struct OwnedBrowser {
    session_id: String,
    run_id: String,
    profile_root: PathBuf,
    profile: PathBuf,
    child: Mutex<Option<Child>>,
    cdp: Mutex<CdpConnection>,
    artifacts: ArtifactStore,
    limits: BrowserLimits,
    cancelled: AtomicBool,
    action_count: AtomicU64,
    artifact_bytes: AtomicU64,
    started: Instant,
    started_at_ms: u64,
    current_url: Mutex<String>,
    console: Mutex<Vec<Value>>,
    network: Mutex<Vec<Value>>,
    viewport: Mutex<BrowserViewport>,
}

impl OwnedBrowser {
    fn launch(
        profile_root: PathBuf,
        artifacts: ArtifactStore,
        request: BrowserStartRequest,
    ) -> Result<Arc<Self>, String> {
        validate_identifier("runId", &request.run_id)?;
        validate_limits(&request.limits)?;
        let grant = ValidatedGrant::new(request.grant)?;
        grant.validate_navigation(&request.url)?;
        // Chromium resolves a request only after Fetch.requestPaused is
        // continued. Pin each granted hostname to an address that was already
        // classified here so a second DNS answer cannot pivot the browser to
        // a private/link-local address between our check and the socket open.
        let resolver_rules = grant.chromium_resolver_rules()?;
        let session_id = format!("browser-{}", uuid::Uuid::new_v4());
        let profile = profile_root.join(&session_id);
        ensure_new_owned_profile(&profile_root, &profile, &session_id)?;
        let chrome = find_chromium()?;
        let mut command = Command::new(chrome);
        command
            .arg("--headless=new")
            .arg("--remote-debugging-port=0")
            .arg("--remote-allow-origins=http://127.0.0.1")
            .arg(format!("--user-data-dir={}", profile.display()))
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--disable-background-networking")
            .arg("--disable-component-update")
            .arg("--disable-default-apps")
            .arg("--disable-extensions")
            .arg("--disable-sync")
            .arg("--disable-translate")
            .arg("--disk-cache-size=1")
            .arg("--media-cache-size=1")
            .arg("--metrics-recording-only")
            .arg("--safebrowsing-disable-download-protection")
            .arg("--disable-features=DownloadBubble,DownloadLater,OptimizationHints");
        if !resolver_rules.is_empty() {
            command.arg(format!("--host-resolver-rules={resolver_rules}"));
        }
        command
            .arg("about:blank")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = command
            .spawn()
            .map_err(|error| format!("Failed to launch owned Chromium: {error}"))?;

        let launch_result = (|| {
            let port = wait_for_devtools_port(&profile, &mut child, Duration::from_secs(10))?;
            let websocket = discover_page_websocket(port)?;
            let mut cdp = CdpConnection::connect(&websocket, grant)?;
            cdp.command("Page.enable", json!({}))?;
            cdp.command("Runtime.enable", json!({}))?;
            cdp.command(
                "Network.enable",
                json!({
                    "maxTotalBufferSize": 4 * 1024 * 1024,
                    "maxResourceBufferSize": 1024 * 1024
                }),
            )?;
            cdp.command("Log.enable", json!({}))?;
            cdp.command("Accessibility.enable", json!({}))?;
            cdp.command("Performance.enable", json!({}))?;
            let viewport = BrowserViewport::default();
            cdp.command(
                "Emulation.setDeviceMetricsOverride",
                json!({
                    "width": viewport.width,
                    "height": viewport.height,
                    "deviceScaleFactor": viewport.device_scale_factor,
                    "mobile": viewport.mobile
                }),
            )?;
            cdp.command(
                "Fetch.enable",
                json!({
                    "patterns": [{"urlPattern":"*", "requestStage":"Request"}]
                }),
            )?;
            let _ = cdp.command(
                "Browser.setDownloadBehavior",
                json!({"behavior":"deny", "eventsEnabled":true}),
            );
            Ok(cdp)
        })();

        let cdp = match launch_result {
            Ok(cdp) => cdp,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = safe_remove_profile(&profile_root, &profile, &session_id);
                return Err(error);
            }
        };

        let browser = Arc::new(Self {
            session_id,
            run_id: request.run_id,
            profile_root,
            profile,
            child: Mutex::new(Some(child)),
            cdp: Mutex::new(cdp),
            artifacts,
            limits: request.limits,
            cancelled: AtomicBool::new(false),
            action_count: AtomicU64::new(0),
            artifact_bytes: AtomicU64::new(0),
            started: Instant::now(),
            started_at_ms: now_ms(),
            current_url: Mutex::new("about:blank".to_string()),
            console: Mutex::new(Vec::new()),
            network: Mutex::new(Vec::new()),
            viewport: Mutex::new(BrowserViewport::default()),
        });
        browser.navigate(&request.url)?;
        browser.capture_evidence()?;
        Ok(browser)
    }

    fn view(&self) -> BrowserSessionView {
        BrowserSessionView {
            session_id: self.session_id.clone(),
            run_id: self.run_id.clone(),
            current_url: self
                .current_url
                .lock()
                .map(|value| value.clone())
                .unwrap_or_default(),
            started_at_ms: self.started_at_ms,
            action_count: self.action_count.load(Ordering::SeqCst),
            cancelled: self.cancelled.load(Ordering::SeqCst),
            viewport: self
                .viewport
                .lock()
                .map(|value| value.clone())
                .unwrap_or_default(),
        }
    }

    fn begin_action(&self) -> Result<(), String> {
        if self.cancelled.load(Ordering::SeqCst) {
            return Err("Browser session is cancelled".to_string());
        }
        if self.started.elapsed() > Duration::from_millis(self.limits.max_session_ms) {
            self.cancelled.store(true, Ordering::SeqCst);
            let _ = self.stop();
            return Err("Browser session time quota exceeded".to_string());
        }
        let profile_bytes = owned_directory_size(&self.profile, self.limits.max_disk_bytes)?;
        if profile_bytes.saturating_add(self.artifact_bytes.load(Ordering::SeqCst))
            > self.limits.max_disk_bytes
        {
            self.cancelled.store(true, Ordering::SeqCst);
            let _ = self.stop();
            return Err("Browser session disk quota exceeded".to_string());
        }
        let next = self.action_count.fetch_add(1, Ordering::SeqCst) + 1;
        if next > self.limits.max_actions {
            self.cancelled.store(true, Ordering::SeqCst);
            // The same teardown the two quotas above perform, and its absence
            // here was not a shortcut but a leak: the `cancelled` gate at the top
            // of this function makes every later call return early, so nothing
            // could ever reach `stop()` again. Chromium stayed alive, idle, and
            // unreachable by anything except `browser_stop` — held open only by
            // the `Arc` in the session map.
            let _ = self.stop();
            return Err("Browser action quota exceeded".to_string());
        }
        Ok(())
    }

    fn put_artifact(&self, bytes: &[u8]) -> Result<ArtifactBlob, String> {
        let profile_bytes = owned_directory_size(&self.profile, self.limits.max_disk_bytes)?;
        let artifact_limit = self.limits.max_disk_bytes.saturating_sub(profile_bytes);
        if let Err(error) = reserve_quota(&self.artifact_bytes, artifact_limit, bytes.len() as u64)
        {
            self.cancelled.store(true, Ordering::SeqCst);
            let _ = self.stop();
            return Err(error);
        }
        match self.artifacts.put(bytes) {
            Ok(blob) => Ok(blob),
            Err(error) => {
                self.artifact_bytes
                    .fetch_sub(bytes.len() as u64, Ordering::SeqCst);
                Err(error.to_string())
            }
        }
    }

    fn with_cdp<T>(
        &self,
        operation: impl FnOnce(&mut CdpConnection) -> Result<T, String>,
    ) -> Result<T, String> {
        self.begin_action()?;
        let mut cdp = self
            .cdp
            .lock()
            .map_err(|_| "Browser CDP lock is poisoned".to_string())?;
        let result = operation(&mut cdp);
        self.collect_events(&mut cdp);
        if let Some(error) = cdp.security_error.take() {
            return Err(error);
        }
        result
    }

    fn collect_events(&self, cdp: &mut CdpConnection) {
        if let Ok(mut console) = self.console.lock() {
            if let Ok(mut network) = self.network.lock() {
                while let Some(event) = cdp.events.pop_front() {
                    let method = event
                        .get("method")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if matches!(
                        method,
                        "Runtime.consoleAPICalled" | "Log.entryAdded" | "Runtime.exceptionThrown"
                    ) {
                        push_bounded(&mut console, event, self.limits.max_log_entries);
                    } else if method.starts_with("Network.") || method.starts_with("Fetch.") {
                        push_bounded(&mut network, event, self.limits.max_log_entries);
                    }
                }
            }
        }
    }

    fn navigate(&self, url: &str) -> Result<BrowserActionResult, String> {
        let url = url.to_string();
        self.with_cdp(|cdp| {
            cdp.grant.validate_navigation(&url)?;
            cdp.command("Page.navigate", json!({"url":url}))?;
            cdp.wait_for_load(Duration::from_millis(self.limits.timeout_ms))
        })?;
        *self
            .current_url
            .lock()
            .map_err(|_| "Browser URL lock is poisoned".to_string())? = url;
        Ok(BrowserActionResult {
            ok: true,
            url: self.view().current_url,
            evidence: self.capture_evidence()?,
        })
    }

    fn reload(&self) -> Result<BrowserActionResult, String> {
        self.with_cdp(|cdp| {
            cdp.command("Page.reload", json!({"ignoreCache":true}))?;
            cdp.wait_for_load(Duration::from_millis(self.limits.timeout_ms))
        })?;
        Ok(BrowserActionResult {
            ok: true,
            url: self.view().current_url,
            evidence: self.capture_evidence()?,
        })
    }

    fn set_viewport(&self, viewport: BrowserViewport) -> Result<BrowserActionResult, String> {
        validate_viewport(&viewport)?;
        self.with_cdp(|cdp| {
            cdp.command(
                "Emulation.setDeviceMetricsOverride",
                json!({
                    "width": viewport.width,
                    "height": viewport.height,
                    "deviceScaleFactor": viewport.device_scale_factor,
                    "mobile": viewport.mobile
                }),
            )?;
            cdp.wait_quiet(Duration::from_millis(150))
        })?;
        *self
            .viewport
            .lock()
            .map_err(|_| "Browser viewport lock is poisoned".to_string())? = viewport;
        Ok(BrowserActionResult {
            ok: true,
            url: self.view().current_url,
            evidence: self.capture_evidence()?,
        })
    }

    fn inspect(&self) -> Result<BrowserInspection, String> {
        let payload = self.with_cdp(|cdp| {
            let dom = cdp.command(
                "Runtime.evaluate",
                json!({
                    "expression":"JSON.stringify({title:document.title,url:location.href,html:document.documentElement.outerHTML,accessibilityIssues:[...Array.from(document.querySelectorAll('img:not([alt])')).slice(0,25).map((_,i)=>`Image ${i+1} has no alt attribute`),...Array.from(document.querySelectorAll('button,a[href],input,select,textarea')).filter(e=>!((e.getAttribute('aria-label')||e.getAttribute('aria-labelledby')||e.textContent||e.getAttribute('title')||'').trim())&&!((e instanceof HTMLInputElement)&&['hidden','submit','button','image'].includes(e.type))).slice(0,50).map(e=>`${e.tagName.toLowerCase()} control has no accessible name`),...(!document.documentElement.lang?[`Document has no language attribute`]:[]),...(!document.querySelector('h1')?[`Document has no h1 heading`]:[])]})",
                    "returnByValue":true
                }),
            )?;
            let ax = cdp.command("Accessibility.getFullAXTree", json!({"depth":12}))?;
            Ok((dom, ax))
        })?;
        let text = payload
            .0
            .pointer("/result/value")
            .and_then(Value::as_str)
            .ok_or_else(|| "Chromium did not return a DOM snapshot".to_string())?;
        if text.len() as u64 > self.limits.max_dom_bytes {
            return Err("DOM snapshot exceeds its quota".to_string());
        }
        let dom_value: Value = serde_json::from_str(text).map_err(|error| error.to_string())?;
        let html = dom_value
            .get("html")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let dom = self.put_artifact(html.as_bytes())?;
        let ax_bytes = serde_json::to_vec(&payload.1).map_err(|error| error.to_string())?;
        if ax_bytes.len() as u64 > self.limits.max_dom_bytes {
            return Err("Accessibility snapshot exceeds its quota".to_string());
        }
        let accessibility = self.put_artifact(&ax_bytes)?;
        let url = dom_value
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if !url.is_empty() {
            *self
                .current_url
                .lock()
                .map_err(|_| "Browser URL lock is poisoned".to_string())? = url.clone();
        }
        Ok(BrowserInspection {
            url,
            title: dom_value
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            dom,
            accessibility,
            accessibility_issues: dom_value
                .get("accessibilityIssues")
                .and_then(Value::as_array)
                .map(|issues| {
                    issues
                        .iter()
                        .filter_map(Value::as_str)
                        .take(100)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
        })
    }

    fn performance_snapshot(&self) -> Result<ArtifactBlob, String> {
        let payload = self.with_cdp(|cdp| {
            let metrics = cdp.command("Performance.getMetrics", json!({}))?;
            let timing = cdp.command(
                "Runtime.evaluate",
                json!({
                    "expression":"JSON.stringify({navigation:performance.getEntriesByType('navigation').slice(0,1),paints:performance.getEntriesByType('paint').slice(0,16),resources:performance.getEntriesByType('resource').slice(0,256).map(({name,initiatorType,duration,transferSize,encodedBodySize,decodedBodySize})=>({name,initiatorType,duration,transferSize,encodedBodySize,decodedBodySize}))})",
                    "returnByValue":true
                }),
            )?;
            Ok(json!({ "metrics": metrics, "timing": timing }))
        })?;
        let bytes = serde_json::to_vec(&payload).map_err(|error| error.to_string())?;
        if bytes.len() as u64 > self.limits.max_dom_bytes {
            return Err("Performance snapshot exceeds its quota".to_string());
        }
        self.put_artifact(&bytes)
    }

    fn annotate(&self, selector: &str) -> Result<BrowserAnnotation, String> {
        validate_text("selector", selector, MAX_SELECTOR_BYTES)?;
        let encoded_selector =
            serde_json::to_string(selector).map_err(|error| error.to_string())?;
        let payload = self.with_cdp(|cdp| {
            cdp.command(
                "Runtime.evaluate",
                json!({
                    "expression":format!("(()=>{{const e=document.querySelector({encoded_selector});if(!e)throw new Error('selector not found');e.scrollIntoView({{block:'center',inline:'center'}});const r=e.getBoundingClientRect();const previousOutline=e.style.outline;const previousOutlineOffset=e.style.outlineOffset;e.style.outline='3px solid #f97316';e.style.outlineOffset='3px';return JSON.stringify({{tag:e.tagName.toLowerCase(),role:e.getAttribute('role')||'',ariaLabel:e.getAttribute('aria-label')||'',text:(e.textContent||'').trim().slice(0,2000),rect:{{x:r.x,y:r.y,width:r.width,height:r.height}},previousOutline,previousOutlineOffset}});}})()"),
                    "returnByValue":true
                }),
            )
        })?;
        if payload.get("exceptionDetails").is_some() {
            return Err("Browser annotation selector was not found".to_string());
        }
        let serialized = payload
            .pointer("/result/value")
            .and_then(Value::as_str)
            .ok_or_else(|| "Chromium did not return annotation context".to_string())?;
        let value: Value = serde_json::from_str(serialized).map_err(|error| error.to_string())?;
        let previous_outline = serde_json::to_string(
            value
                .get("previousOutline")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        )
        .map_err(|error| error.to_string())?;
        let previous_offset = serde_json::to_string(
            value
                .get("previousOutlineOffset")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        )
        .map_err(|error| error.to_string())?;
        let evidence_result = self.capture_evidence();
        let restore_result = self.with_cdp(|cdp| {
            cdp.command(
                "Runtime.evaluate",
                json!({
                    "expression":format!("(()=>{{const e=document.querySelector({encoded_selector});if(!e)return false;e.style.outline={previous_outline};e.style.outlineOffset={previous_offset};return true;}})()"),
                    "returnByValue":true
                }),
            )?;
            Ok(())
        });
        let evidence = evidence_result?;
        restore_result?;
        let rect = value.get("rect").cloned().unwrap_or(Value::Null);
        Ok(BrowserAnnotation {
            url: self.view().current_url,
            selector: selector.to_string(),
            tag: value
                .get("tag")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            role: value
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            aria_label: value
                .get("ariaLabel")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            text: value
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            rect: BrowserAnnotationRect {
                x: rect.get("x").and_then(Value::as_f64).unwrap_or_default(),
                y: rect.get("y").and_then(Value::as_f64).unwrap_or_default(),
                width: rect
                    .get("width")
                    .and_then(Value::as_f64)
                    .unwrap_or_default(),
                height: rect
                    .get("height")
                    .and_then(Value::as_f64)
                    .unwrap_or_default(),
            },
            evidence,
        })
    }

    fn evaluate_action(&self, expression: String) -> Result<BrowserActionResult, String> {
        self.with_cdp(|cdp| {
            let value = cdp.command(
                "Runtime.evaluate",
                json!({"expression":expression,"awaitPromise":true,"returnByValue":true}),
            )?;
            if value.get("exceptionDetails").is_some() {
                return Err("Browser action failed in the page".to_string());
            }
            cdp.wait_quiet(Duration::from_millis(250))?;
            Ok(())
        })?;
        Ok(BrowserActionResult {
            ok: true,
            url: self.view().current_url,
            evidence: self.capture_evidence()?,
        })
    }

    fn click(&self, selector: &str) -> Result<BrowserActionResult, String> {
        validate_text("selector", selector, MAX_SELECTOR_BYTES)?;
        let selector = serde_json::to_string(selector).map_err(|error| error.to_string())?;
        self.evaluate_action(format!(
            "(()=>{{const e=document.querySelector({selector});if(!e)throw new Error('selector not found');e.scrollIntoView({{block:'center'}});e.click();return true;}})()"
        ))
    }

    fn type_text(&self, selector: &str, text: &str) -> Result<BrowserActionResult, String> {
        validate_text("selector", selector, MAX_SELECTOR_BYTES)?;
        validate_text("text", text, MAX_TYPE_BYTES)?;
        let selector = serde_json::to_string(selector).map_err(|error| error.to_string())?;
        let text = serde_json::to_string(text).map_err(|error| error.to_string())?;
        self.evaluate_action(format!(
            "(()=>{{const e=document.querySelector({selector});if(!e)throw new Error('selector not found');e.focus();if('value'in e)e.value={text};else e.textContent={text};e.dispatchEvent(new InputEvent('input',{{bubbles:true,inputType:'insertText',data:{text}}}));e.dispatchEvent(new Event('change',{{bubbles:true}}));return true;}})()"
        ))
    }

    fn scroll(&self, x: i64, y: i64) -> Result<BrowserActionResult, String> {
        if x.unsigned_abs() > 1_000_000 || y.unsigned_abs() > 1_000_000 {
            return Err("Scroll delta exceeds its limit".to_string());
        }
        self.evaluate_action(format!("window.scrollBy({x},{y});true"))
    }

    fn screenshot(&self) -> Result<ArtifactBlob, String> {
        let value = self.with_cdp(|cdp| {
            cdp.command(
                "Page.captureScreenshot",
                json!({"format":"png","fromSurface":true,"captureBeyondViewport":false}),
            )
        })?;
        let encoded = value
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| "Chromium did not return screenshot data".to_string())?;
        if encoded.len() as u64 > self.limits.max_screenshot_bytes.saturating_mul(2) {
            return Err("Encoded screenshot exceeds its quota".to_string());
        }
        let bytes = STANDARD
            .decode(encoded)
            .map_err(|error| error.to_string())?;
        if bytes.len() as u64 > self.limits.max_screenshot_bytes {
            return Err("Screenshot exceeds its quota".to_string());
        }
        self.put_artifact(&bytes)
    }

    fn capture_evidence(&self) -> Result<BrowserEvidence, String> {
        if self.cancelled.load(Ordering::SeqCst) {
            return Err("Browser session is cancelled".to_string());
        }
        let screenshot = self.screenshot()?;
        let inspection = self.inspect()?;
        let performance = self.performance_snapshot()?;
        let console = serde_json::to_vec(
            &*self
                .console
                .lock()
                .map_err(|_| "Browser console lock is poisoned".to_string())?,
        )
        .map_err(|error| error.to_string())?;
        let network = serde_json::to_vec(
            &*self
                .network
                .lock()
                .map_err(|_| "Browser network lock is poisoned".to_string())?,
        )
        .map_err(|error| error.to_string())?;
        Ok(BrowserEvidence {
            screenshot: Some(screenshot),
            dom: Some(inspection.dom),
            accessibility: Some(inspection.accessibility),
            console: Some(self.put_artifact(&console)?),
            network: Some(self.put_artifact(&network)?),
            performance: Some(performance),
            action_count: self.action_count.load(Ordering::SeqCst),
        })
    }

    fn stop(&self) -> Result<(), String> {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Ok(mut child) = self.child.lock() {
            if let Some(mut child) = child.take() {
                let _ = child.kill();
                let deadline = Instant::now() + Duration::from_secs(2);
                loop {
                    match child.try_wait() {
                        Ok(Some(_)) => break,
                        Ok(None) if Instant::now() < deadline => {
                            std::thread::sleep(Duration::from_millis(20))
                        }
                        Ok(None) => {
                            let _ = child.kill();
                            let _ = child.wait();
                            break;
                        }
                        Err(_) => break,
                    }
                }
            }
        }
        safe_remove_profile(&self.profile_root, &self.profile, &self.session_id)
    }
}

impl Drop for OwnedBrowser {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[derive(Clone)]
struct ValidatedGrant {
    allowed_origins: Vec<String>,
    allow_loopback: bool,
}

impl ValidatedGrant {
    fn new(grant: BrowserGrant) -> Result<Self, String> {
        if grant.allowed_origins.is_empty() || grant.allowed_origins.len() > MAX_ALLOWED_ORIGINS {
            return Err("Browser grant requires 1..=32 exact origins".to_string());
        }
        let mut allowed_origins = Vec::new();
        for value in grant.allowed_origins {
            let url =
                Url::parse(&value).map_err(|error| format!("Invalid allowed origin: {error}"))?;
            if url.path() != "/"
                || url.query().is_some()
                || url.fragment().is_some()
                || !url.username().is_empty()
                || url.password().is_some()
            {
                return Err(
                    "Allowed browser entries must be origins, not URLs with paths or credentials"
                        .to_string(),
                );
            }
            let origin = normalized_origin(&url)?;
            if !allowed_origins.contains(&origin) {
                allowed_origins.push(origin);
            }
        }
        Ok(Self {
            allowed_origins,
            allow_loopback: grant.allow_loopback,
        })
    }

    fn validate_navigation(&self, value: &str) -> Result<(), String> {
        let url = validate_http_url(value)?;
        let origin = normalized_origin(&url)?;
        if !self.allowed_origins.contains(&origin) {
            return Err(format!(
                "Navigation origin '{origin}' is outside this run's grant"
            ));
        }
        self.validate_resolved(&url, true)
    }

    fn validate_request(&self, value: &str, document: bool) -> Result<(), String> {
        let url = validate_http_url(value)?;
        if !self.allowed_origins.contains(&normalized_origin(&url)?) {
            return Err(if document {
                "Redirect/document navigation left the granted origin set".to_string()
            } else {
                "Page subresource left the granted origin set".to_string()
            });
        }
        self.validate_resolved(&url, document)
    }

    fn validate_resolved(&self, url: &Url, _document: bool) -> Result<(), String> {
        self.resolved_addresses(url).map(|_| ())
    }

    fn resolved_addresses(&self, url: &Url) -> Result<Vec<IpAddr>, String> {
        let host = url
            .host_str()
            .ok_or_else(|| "Browser URL has no host".to_string())?;
        let port = url
            .port_or_known_default()
            .ok_or_else(|| "Browser URL has no port".to_string())?;
        let addresses: Vec<IpAddr> = (host, port)
            .to_socket_addrs()
            .map_err(|error| format!("Browser DNS resolution failed: {error}"))?
            .map(|address| address.ip())
            .collect();
        if addresses.is_empty() {
            return Err("Browser DNS resolution returned no addresses".to_string());
        }
        for address in &addresses {
            match classify_ip(*address) {
                IpClass::Public => {}
                IpClass::Loopback if self.allow_loopback => {}
                IpClass::Loopback => return Err("Loopback browser access requires an explicit per-run grant".to_string()),
                IpClass::Private => return Err("Private, link-local, multicast, and unspecified browser destinations are blocked".to_string()),
            }
        }
        Ok(addresses)
    }

    fn chromium_resolver_rules(&self) -> Result<String, String> {
        let mut pinned = BTreeMap::<String, IpAddr>::new();
        for origin in &self.allowed_origins {
            let url = Url::parse(origin).map_err(|error| error.to_string())?;
            let host = url
                .host_str()
                .ok_or_else(|| "Browser origin has no host".to_string())?;
            // Literal IP hosts cannot be DNS-rebound and need no resolver rule.
            if host.parse::<IpAddr>().is_ok() {
                continue;
            }
            let address = self
                .resolved_addresses(&url)?
                .into_iter()
                .next()
                .ok_or_else(|| "Browser DNS resolution returned no addresses".to_string())?;
            pinned.insert(host.to_string(), address);
        }
        Ok(pinned
            .into_iter()
            .map(|(host, address)| {
                let target = match address {
                    IpAddr::V4(value) => value.to_string(),
                    IpAddr::V6(value) => format!("[{value}]"),
                };
                format!("MAP {host} {target}")
            })
            .collect::<Vec<_>>()
            .join(", "))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IpClass {
    Public,
    Loopback,
    Private,
}

fn classify_ip(ip: IpAddr) -> IpClass {
    if ip.is_loopback() {
        return IpClass::Loopback;
    }
    match ip {
        IpAddr::V4(ip) if is_private_v4(ip) => IpClass::Private,
        IpAddr::V6(ip) if is_private_v6(ip) => IpClass::Private,
        _ => IpClass::Public,
    }
}

fn is_private_v4(ip: Ipv4Addr) -> bool {
    ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || ip.is_multicast()
        || matches!(
            ip.octets(),
            [100, 64..=127, _, _] | [198, 18..=19, _, _] | [192, 0, 0, _]
        )
}

fn is_private_v6(ip: Ipv6Addr) -> bool {
    ip.is_unspecified()
        || ip.is_multicast()
        || (ip.segments()[0] & 0xfe00) == 0xfc00
        || (ip.segments()[0] & 0xffc0) == 0xfe80
        || ip.to_ipv4_mapped().is_some_and(is_private_v4)
}

struct CdpConnection {
    stream: TcpStream,
    buffered: VecDeque<u8>,
    next_id: u64,
    events: VecDeque<Value>,
    grant: ValidatedGrant,
    security_error: Option<String>,
}

impl CdpConnection {
    fn connect(websocket: &str, grant: ValidatedGrant) -> Result<Self, String> {
        let url = Url::parse(websocket)
            .map_err(|error| format!("Invalid DevTools websocket: {error}"))?;
        if url.scheme() != "ws" || !matches!(url.host_str(), Some("127.0.0.1" | "localhost")) {
            return Err("DevTools websocket must be loopback ws:".to_string());
        }
        let port = url
            .port()
            .ok_or_else(|| "DevTools websocket has no port".to_string())?;
        let mut stream = TcpStream::connect(("127.0.0.1", port))
            .map_err(|error| format!("Cannot connect to DevTools: {error}"))?;
        stream
            .set_nodelay(true)
            .map_err(|error| error.to_string())?;
        let key_bytes: [u8; 16] = rand::random();
        let key = STANDARD.encode(key_bytes);
        let path = match url.query() {
            Some(query) => format!("{}?{query}", url.path()),
            None => url.path().to_string(),
        };
        write!(stream,
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\nOrigin: http://127.0.0.1\r\n\r\n"
        ).map_err(|error| error.to_string())?;
        stream.flush().map_err(|error| error.to_string())?;
        let (header, leftover) = read_http_header(&mut stream)?;
        if !header.starts_with("HTTP/1.1 101") && !header.starts_with("HTTP/1.0 101") {
            return Err("Chromium rejected the DevTools websocket handshake".to_string());
        }
        let expected = websocket_accept(&key);
        let accepted = header.lines().find_map(|line| {
            line.split_once(':').and_then(|(name, value)| {
                name.eq_ignore_ascii_case("sec-websocket-accept")
                    .then(|| value.trim())
            })
        });
        if accepted != Some(expected.as_str()) {
            return Err("DevTools websocket handshake integrity check failed".to_string());
        }
        Ok(Self {
            stream,
            buffered: leftover.into(),
            next_id: 1,
            events: VecDeque::new(),
            grant,
            security_error: None,
        })
    }

    fn command(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.send_json(&json!({"id":id,"method":method,"params":params}))?;
        loop {
            let message = self
                .read_json(None)?
                .ok_or_else(|| "DevTools connection closed".to_string())?;
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(error) = message.get("error") {
                    return Err(format!("CDP {method} failed: {error}"));
                }
                if let Some(error) = self.security_error.take() {
                    return Err(error);
                }
                return Ok(message.get("result").cloned().unwrap_or_else(|| json!({})));
            }
            self.handle_event(message)?;
        }
    }

    fn send_json(&mut self, value: &Value) -> Result<(), String> {
        let payload = serde_json::to_vec(value).map_err(|error| error.to_string())?;
        self.send_frame(0x1, &payload)
    }

    fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<(), String> {
        if payload.len() > MAX_CDP_MESSAGE_BYTES {
            return Err("CDP message exceeds 16 MiB".to_string());
        }
        let mask: [u8; 4] = rand::random();
        let mut frame = Vec::with_capacity(payload.len() + 14);
        frame.push(0x80 | opcode);
        match payload.len() {
            len if len < 126 => frame.push(0x80 | len as u8),
            len if len <= u16::MAX as usize => {
                frame.push(0x80 | 126);
                frame.extend_from_slice(&(len as u16).to_be_bytes());
            }
            len => {
                frame.push(0x80 | 127);
                frame.extend_from_slice(&(len as u64).to_be_bytes());
            }
        }
        frame.extend_from_slice(&mask);
        frame.extend(
            payload
                .iter()
                .enumerate()
                .map(|(index, byte)| byte ^ mask[index % 4]),
        );
        self.stream
            .write_all(&frame)
            .map_err(|error| format!("CDP write failed: {error}"))?;
        self.stream
            .flush()
            .map_err(|error| format!("CDP flush failed: {error}"))
    }

    fn read_json(&mut self, timeout: Option<Duration>) -> Result<Option<Value>, String> {
        self.stream
            .set_read_timeout(timeout)
            .map_err(|error| error.to_string())?;
        let mut message = Vec::new();
        let mut started = false;
        loop {
            let Some((fin, opcode, payload)) = self.read_frame()? else {
                return Ok(None);
            };
            match opcode {
                0x8 => return Ok(None),
                0x9 => {
                    self.send_frame(0xA, &payload)?;
                    continue;
                }
                0xA => continue,
                0x1 => {
                    message = payload;
                    started = true;
                }
                0x0 if started => message.extend_from_slice(&payload),
                _ => continue,
            }
            if message.len() > MAX_CDP_MESSAGE_BYTES {
                return Err("CDP response exceeds 16 MiB".to_string());
            }
            if fin {
                return serde_json::from_slice(&message)
                    .map(Some)
                    .map_err(|error| format!("Invalid CDP JSON: {error}"));
            }
        }
    }

    fn read_frame(&mut self) -> Result<Option<(bool, u8, Vec<u8>)>, String> {
        let mut header = [0_u8; 2];
        if !self.read_exact_optional(&mut header)? {
            return Ok(None);
        }
        let fin = header[0] & 0x80 != 0;
        let opcode = header[0] & 0x0f;
        let masked = header[1] & 0x80 != 0;
        let mut length = u64::from(header[1] & 0x7f);
        if length == 126 {
            let mut bytes = [0; 2];
            self.read_exact_required(&mut bytes)?;
            length = u64::from(u16::from_be_bytes(bytes));
        } else if length == 127 {
            let mut bytes = [0; 8];
            self.read_exact_required(&mut bytes)?;
            length = u64::from_be_bytes(bytes);
        }
        if length > MAX_CDP_MESSAGE_BYTES as u64 {
            return Err("CDP frame exceeds 16 MiB".to_string());
        }
        let mut mask = [0_u8; 4];
        if masked {
            self.read_exact_required(&mut mask)?;
        }
        let mut payload = vec![0_u8; length as usize];
        self.read_exact_required(&mut payload)?;
        if masked {
            for (index, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask[index % 4];
            }
        }
        Ok(Some((fin, opcode, payload)))
    }

    fn read_exact_optional(&mut self, output: &mut [u8]) -> Result<bool, String> {
        let mut offset = 0;
        while offset < output.len() {
            while offset < output.len() {
                let Some(value) = self.buffered.pop_front() else {
                    break;
                };
                output[offset] = value;
                offset += 1;
            }
            if offset == output.len() {
                return Ok(true);
            }
            match self.stream.read(&mut output[offset..]) {
                Ok(0) if offset == 0 => return Ok(false),
                Ok(0) => return Err("Truncated CDP frame".to_string()),
                Ok(count) => offset += count,
                Err(error)
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
                        && offset == 0 =>
                {
                    return Ok(false)
                }
                Err(error) => return Err(format!("CDP read failed: {error}")),
            }
        }
        Ok(true)
    }

    fn read_exact_required(&mut self, output: &mut [u8]) -> Result<(), String> {
        self.read_exact_optional(output)?
            .then_some(())
            .ok_or_else(|| "Truncated CDP frame".to_string())
    }

    fn handle_event(&mut self, event: Value) -> Result<(), String> {
        if event.get("method").and_then(Value::as_str) == Some("Fetch.requestPaused") {
            let request_id = event
                .pointer("/params/requestId")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let url = event
                .pointer("/params/request/url")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let document = event
                .pointer("/params/resourceType")
                .and_then(Value::as_str)
                == Some("Document");
            let decision = self.grant.validate_request(url, document);
            let id = self.next_id;
            self.next_id = self.next_id.saturating_add(1);
            match decision {
                Ok(()) => self.send_json(&json!({"id":id,"method":"Fetch.continueRequest","params":{"requestId":request_id}}))?,
                Err(error) => {
                    self.security_error = Some(error);
                    self.send_json(&json!({"id":id,"method":"Fetch.failRequest","params":{"requestId":request_id,"errorReason":"BlockedByClient"}}))?;
                }
            }
        }
        self.events.push_back(event);
        Ok(())
    }

    fn wait_for_load(&mut self, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(error) = self.security_error.take() {
                return Err(error);
            }
            match self.read_json(Some(Duration::from_millis(100)))? {
                Some(event)
                    if event.get("method").and_then(Value::as_str)
                        == Some("Page.loadEventFired") =>
                {
                    self.events.push_back(event);
                    return Ok(());
                }
                Some(event) => self.handle_event(event)?,
                None => {}
            }
        }
        Err("Browser navigation timed out".to_string())
    }

    fn wait_quiet(&mut self, duration: Duration) -> Result<(), String> {
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline {
            match self.read_json(Some(Duration::from_millis(30)))? {
                Some(event) => self.handle_event(event)?,
                None => break,
            }
        }
        if let Some(error) = self.security_error.take() {
            Err(error)
        } else {
            Ok(())
        }
    }
}

fn validate_http_url(value: &str) -> Result<Url, String> {
    if value.len() > 16 * 1024 {
        return Err("Browser URL exceeds 16 KiB".to_string());
    }
    let url = Url::parse(value).map_err(|error| format!("Invalid browser URL: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("Only http: and https: browser URLs are allowed".to_string());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("Browser URLs cannot contain credentials".to_string());
    }
    if url.host_str().is_none() {
        return Err("Browser URL has no host".to_string());
    }
    Ok(url)
}

fn normalized_origin(url: &Url) -> Result<String, String> {
    let host = url
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?;
    let default = match url.scheme() {
        "http" => 80,
        "https" => 443,
        _ => return Err("Only HTTP origins are supported".to_string()),
    };
    Ok(match url.port() {
        Some(port) if port != default => format!("{}://{}:{port}", url.scheme(), host),
        _ => format!("{}://{}", url.scheme(), host),
    })
}

fn validate_limits(limits: &BrowserLimits) -> Result<(), String> {
    if !(1_000..=15 * 60_000).contains(&limits.timeout_ms)
        || !(1_000..=24 * 60 * 60_000).contains(&limits.max_session_ms)
        || !(1..=10_000).contains(&limits.max_actions)
        || !(1_024..=64 * 1024 * 1024).contains(&limits.max_dom_bytes)
        || !(1_024..=64 * 1024 * 1024).contains(&limits.max_screenshot_bytes)
        || !(1..=20_000).contains(&limits.max_log_entries)
        || !(1024 * 1024..=16 * 1024 * 1024 * 1024).contains(&limits.max_disk_bytes)
    {
        return Err("Browser limits are outside supported bounds".to_string());
    }
    Ok(())
}

fn validate_viewport(viewport: &BrowserViewport) -> Result<(), String> {
    if !(320..=3840).contains(&viewport.width)
        || !(320..=2160).contains(&viewport.height)
        || !viewport.device_scale_factor.is_finite()
        || !(1.0..=3.0).contains(&viewport.device_scale_factor)
    {
        return Err("Browser viewport is outside supported bounds".to_string());
    }
    Ok(())
}

fn reserve_quota(counter: &AtomicU64, limit: u64, bytes: u64) -> Result<(), String> {
    counter
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
            current.checked_add(bytes).filter(|next| *next <= limit)
        })
        .map(|_| ())
        .map_err(|_| "Browser session disk quota exceeded".to_string())
}

fn owned_directory_size(root: &Path, limit: u64) -> Result<u64, String> {
    if !root.exists() {
        return Ok(0);
    }
    let mut total = 0_u64;
    let mut entries = 0_u64;
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)
            .map_err(|error| format!("Cannot inspect browser profile quota: {error}"))?
        {
            let entry = entry.map_err(|error| error.to_string())?;
            entries = entries.saturating_add(1);
            if entries > 250_000 {
                return Err("Browser profile contains too many entries".to_string());
            }
            let metadata = entry
                .path()
                .symlink_metadata()
                .map_err(|error| error.to_string())?;
            if metadata.is_dir() {
                pending.push(entry.path());
            } else if metadata.is_file() {
                total = total.saturating_add(metadata.len());
                if total > limit {
                    return Ok(total);
                }
            }
        }
    }
    Ok(total)
}

fn validate_identifier(label: &str, value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 256
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        Err(format!("Invalid browser {label}"))
    } else {
        Ok(())
    }
}

fn validate_text(label: &str, value: &str, max: usize) -> Result<(), String> {
    if value.is_empty() || value.len() > max || value.contains('\0') {
        Err(format!("Browser {label} is empty or exceeds its limit"))
    } else {
        Ok(())
    }
}

fn ensure_private_directory(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path)
        .map_err(|error| format!("Cannot create {}: {error}", path.display()))?;
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("Browser directory is not a real directory".to_string());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn ensure_new_owned_profile(root: &Path, profile: &Path, session_id: &str) -> Result<(), String> {
    if profile.exists() {
        return Err("Browser profile already exists".to_string());
    }
    ensure_private_directory(profile)?;
    let canonical_root = fs::canonicalize(root).map_err(|error| error.to_string())?;
    let canonical_profile = fs::canonicalize(profile).map_err(|error| error.to_string())?;
    if !canonical_profile.starts_with(canonical_root) {
        return Err("Browser profile escaped owned storage".to_string());
    }
    let marker = profile.join(PROFILE_MARKER);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(marker).map_err(|error| error.to_string())?;
    file.write_all(session_id.as_bytes())
        .and_then(|_| file.sync_all())
        .map_err(|error| error.to_string())
}

fn safe_remove_profile(root: &Path, profile: &Path, session_id: &str) -> Result<(), String> {
    if !profile.exists() {
        return Ok(());
    }
    let canonical_root = fs::canonicalize(root).map_err(|error| error.to_string())?;
    let canonical_profile = fs::canonicalize(profile).map_err(|error| error.to_string())?;
    if !canonical_profile.starts_with(&canonical_root) || canonical_profile == canonical_root {
        return Err("Refusing to remove non-owned browser profile".to_string());
    }
    let marker = fs::read_to_string(canonical_profile.join(PROFILE_MARKER))
        .map_err(|_| "Refusing browser cleanup without ownership marker".to_string())?;
    if marker != session_id {
        return Err("Refusing browser cleanup with mismatched ownership marker".to_string());
    }
    fs::remove_dir_all(canonical_profile)
        .map_err(|error| format!("Failed to remove browser profile: {error}"))
}

fn find_chromium() -> Result<PathBuf, String> {
    if let Ok(value) = std::env::var("LITTLE_MONKEY_CHROMIUM_PATH") {
        return validate_chromium_path(PathBuf::from(value));
    }
    let mut candidates = Vec::new();
    #[cfg(target_os = "macos")]
    candidates.extend([
        PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
        PathBuf::from("/Applications/Chromium.app/Contents/MacOS/Chromium"),
    ]);
    #[cfg(target_os = "windows")]
    for root in [
        std::env::var_os("PROGRAMFILES"),
        std::env::var_os("PROGRAMFILES(X86)"),
        std::env::var_os("LOCALAPPDATA"),
    ]
    .into_iter()
    .flatten()
    {
        candidates.push(PathBuf::from(root).join("Google/Chrome/Application/chrome.exe"));
    }
    #[cfg(target_os = "linux")]
    candidates.extend([
        PathBuf::from("/usr/bin/google-chrome"),
        PathBuf::from("/usr/bin/chromium"),
        PathBuf::from("/usr/bin/chromium-browser"),
    ]);
    candidates.into_iter().find_map(|path| path.exists().then(|| validate_chromium_path(path))).transpose()?.ok_or_else(|| "No supported Chromium worker was found; install Chrome/Chromium or set LITTLE_MONKEY_CHROMIUM_PATH".to_string())
}

fn validate_chromium_path(path: PathBuf) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("Chromium worker path must be absolute".to_string());
    }
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| format!("Cannot inspect Chromium worker: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("Chromium worker must be a real regular file".to_string());
    }
    Ok(path)
}

fn wait_for_devtools_port(
    profile: &Path,
    child: &mut Child,
    timeout: Duration,
) -> Result<u16, String> {
    let path = profile.join("DevToolsActivePort");
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            return Err(format!("Owned Chromium exited during startup: {status}"));
        }
        if let Ok(contents) = fs::read_to_string(&path) {
            let port = contents
                .lines()
                .next()
                .and_then(|line| line.parse::<u16>().ok())
                .filter(|port| *port > 0)
                .ok_or_else(|| "Invalid DevToolsActivePort".to_string())?;
            return Ok(port);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Err("Owned Chromium did not expose DevTools within 10 seconds".to_string())
}

fn discover_page_websocket(port: u16) -> Result<String, String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).map_err(|error| error.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|error| error.to_string())?;
    write!(
        stream,
        "GET /json/list HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )
    .map_err(|error| error.to_string())?;
    stream.flush().map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 16 * 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => {
                bytes.extend_from_slice(&chunk[..count]);
                if bytes.len() > MAX_DEVTOOLS_HTTP_BYTES {
                    return Err("DevTools target response exceeds 2 MiB".to_string());
                }
                if let Some(split) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                    let body_start = split + 4;
                    let header = String::from_utf8_lossy(&bytes[..body_start]);
                    if let Some(length) = header.lines().find_map(|line| {
                        line.split_once(':').and_then(|(name, value)| {
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())
                                .flatten()
                        })
                    }) {
                        if bytes.len().saturating_sub(body_start) >= length {
                            break;
                        }
                    }
                }
            }
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                break;
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    let split = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| "Invalid DevTools HTTP response".to_string())?
        + 4;
    let header = String::from_utf8_lossy(&bytes[..split]);
    if !header.starts_with("HTTP/1.1 200") {
        return Err("DevTools target discovery failed".to_string());
    }
    let targets: Vec<Value> = serde_json::from_slice(&bytes[split..])
        .map_err(|error| format!("Invalid DevTools target list: {error}"))?;
    targets
        .into_iter()
        .find(|target| target.get("type").and_then(Value::as_str) == Some("page"))
        .and_then(|target| {
            target
                .get("webSocketDebuggerUrl")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .ok_or_else(|| "Chromium exposed no page target".to_string())
}

fn read_http_header(stream: &mut TcpStream) -> Result<(String, Vec<u8>), String> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let count = stream.read(&mut chunk).map_err(|error| error.to_string())?;
        if count == 0 {
            return Err("Truncated websocket handshake".to_string());
        }
        bytes.extend_from_slice(&chunk[..count]);
        if bytes.len() > 64 * 1024 {
            return Err("Websocket handshake is too large".to_string());
        }
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let end = index + 4;
            let header = String::from_utf8(bytes[..end].to_vec())
                .map_err(|_| "Websocket handshake is not UTF-8".to_string())?;
            return Ok((header, bytes[end..].to_vec()));
        }
    }
}

fn websocket_accept(key: &str) -> String {
    let mut bytes = Vec::from(key.as_bytes());
    bytes.extend_from_slice(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    STANDARD.encode(sha1(&bytes))
}

fn sha1(input: &[u8]) -> [u8; 20] {
    let mut data = input.to_vec();
    let bit_len = (data.len() as u64) * 8;
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&bit_len.to_be_bytes());
    let mut h = [
        0x67452301_u32,
        0xEFCDAB89,
        0x98BADCFE,
        0x10325476,
        0xC3D2E1F0,
    ];
    for chunk in data.chunks_exact(64) {
        let mut w = [0_u32; 80];
        for (index, word) in w.iter_mut().take(16).enumerate() {
            *word = u32::from_be_bytes(chunk[index * 4..index * 4 + 4].try_into().unwrap());
        }
        for index in 16..80 {
            w[index] = (w[index - 3] ^ w[index - 8] ^ w[index - 14] ^ w[index - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (index, word) in w.iter().enumerate() {
            let (f, k) = match index {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    let mut output = [0_u8; 20];
    for (index, value) in h.into_iter().enumerate() {
        output[index * 4..index * 4 + 4].copy_from_slice(&value.to_be_bytes());
    }
    output
}

fn push_bounded(values: &mut Vec<Value>, value: Value, max: usize) {
    if values.len() == max {
        values.remove(0);
    }
    values.push(value);
}
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[tauri::command]
pub async fn browser_start(
    app: tauri::AppHandle,
    state: tauri::State<'_, BrowserCommandState>,
    app_state: tauri::State<'_, crate::AppState>,
    request: BrowserStartRequest,
) -> Result<BrowserSessionView, String> {
    let profile_root = state.profile_root.clone();
    let artifacts = crate::artifact_commands::store_for(&app, app_state.inner())?;
    let browser =
        tokio::task::spawn_blocking(move || OwnedBrowser::launch(profile_root, artifacts, request))
            .await
            .map_err(|error| error.to_string())??;
    let view = browser.view();
    state.insert(browser)?;
    Ok(view)
}

#[tauri::command]
pub fn browser_list(
    state: tauri::State<'_, BrowserCommandState>,
) -> Result<Vec<BrowserSessionView>, String> {
    Ok(state
        .sessions
        .lock()
        .map_err(|_| "Browser session lock is poisoned".to_string())?
        .values()
        .map(|session| session.view())
        .collect())
}

#[tauri::command]
pub async fn browser_navigate(
    state: tauri::State<'_, BrowserCommandState>,
    session_id: String,
    url: String,
) -> Result<BrowserActionResult, String> {
    let session = state.get(&session_id)?;
    tokio::task::spawn_blocking(move || session.navigate(&url))
        .await
        .map_err(|error| error.to_string())?
}

#[tauri::command]
pub async fn browser_reload(
    state: tauri::State<'_, BrowserCommandState>,
    session_id: String,
) -> Result<BrowserActionResult, String> {
    let session = state.get(&session_id)?;
    tokio::task::spawn_blocking(move || session.reload())
        .await
        .map_err(|error| error.to_string())?
}

#[tauri::command]
pub async fn browser_set_viewport(
    state: tauri::State<'_, BrowserCommandState>,
    session_id: String,
    viewport: BrowserViewport,
) -> Result<BrowserActionResult, String> {
    let session = state.get(&session_id)?;
    tokio::task::spawn_blocking(move || session.set_viewport(viewport))
        .await
        .map_err(|error| error.to_string())?
}

#[tauri::command]
pub async fn browser_inspect(
    state: tauri::State<'_, BrowserCommandState>,
    session_id: String,
) -> Result<BrowserInspection, String> {
    let session = state.get(&session_id)?;
    tokio::task::spawn_blocking(move || session.inspect())
        .await
        .map_err(|error| error.to_string())?
}

#[tauri::command]
pub async fn browser_annotate(
    state: tauri::State<'_, BrowserCommandState>,
    session_id: String,
    selector: String,
) -> Result<BrowserAnnotation, String> {
    let session = state.get(&session_id)?;
    tokio::task::spawn_blocking(move || session.annotate(&selector))
        .await
        .map_err(|error| error.to_string())?
}

#[tauri::command]
pub async fn browser_click(
    state: tauri::State<'_, BrowserCommandState>,
    session_id: String,
    selector: String,
) -> Result<BrowserActionResult, String> {
    let session = state.get(&session_id)?;
    tokio::task::spawn_blocking(move || session.click(&selector))
        .await
        .map_err(|error| error.to_string())?
}

#[tauri::command]
pub async fn browser_type_text(
    state: tauri::State<'_, BrowserCommandState>,
    session_id: String,
    selector: String,
    text: String,
) -> Result<BrowserActionResult, String> {
    let session = state.get(&session_id)?;
    tokio::task::spawn_blocking(move || session.type_text(&selector, &text))
        .await
        .map_err(|error| error.to_string())?
}

#[tauri::command]
pub async fn browser_scroll(
    state: tauri::State<'_, BrowserCommandState>,
    session_id: String,
    x: i64,
    y: i64,
) -> Result<BrowserActionResult, String> {
    let session = state.get(&session_id)?;
    tokio::task::spawn_blocking(move || session.scroll(x, y))
        .await
        .map_err(|error| error.to_string())?
}

#[tauri::command]
pub async fn browser_capture_evidence(
    state: tauri::State<'_, BrowserCommandState>,
    session_id: String,
) -> Result<BrowserEvidence, String> {
    let session = state.get(&session_id)?;
    tokio::task::spawn_blocking(move || session.capture_evidence())
        .await
        .map_err(|error| error.to_string())?
}

#[tauri::command]
pub async fn browser_stop(
    state: tauri::State<'_, BrowserCommandState>,
    session_id: String,
) -> Result<(), String> {
    let session = state
        .remove(&session_id)?
        .ok_or_else(|| "Unknown browser session".to_string())?;
    tokio::task::spawn_blocking(move || session.stop())
        .await
        .map_err(|error| error.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::atomic::AtomicBool;

    /// A child for the quota tests to observe.
    ///
    /// There is no portable long-lived child, and two attempts at one both failed
    /// only on Windows CI: `sleep 30` does not exist there at all, and `ping -n 31`
    /// exited immediately on the runner. `timeout` is not the answer either — it
    /// refuses to run with redirected stdin, which these tests always use.
    ///
    /// So the tests no longer depend on one. The load-bearing assertion is the
    /// portable invariant that `stop()` *takes* the child out of the session, which
    /// holds whether or not the child is still running — and since the defect was
    /// precisely that `stop()` was never called, that is the right thing to assert.
    /// Unix additionally gets a real `sleep`, so there the process death is checked
    /// at the OS level as well.
    fn observable_child() -> Child {
        #[cfg(windows)]
        let mut command = {
            // Only ever inspected as a handle, never waited on, so its lifetime
            // does not matter here.
            let mut command = std::process::Command::new("cmd");
            command.args(["/C", "exit"]);
            command
        };
        #[cfg(unix)]
        let mut command = {
            let mut command = std::process::Command::new("sleep");
            command.arg("30");
            command
        };
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("the stand-in child spawns")
    }

    /// Builds a session around a real but trivial child, so the quota branches in
    /// `begin_action` can be exercised without Chromium.
    ///
    /// The child is a plain `sleep`: `stop()` only kills and reaps whatever is in
    /// `self.child`, and never asks what program it is.
    fn quota_session(max_actions: u64) -> (Arc<OwnedBrowser>, u32) {
        let root = std::env::temp_dir().join(format!("lm-browser-quota-{}", uuid::Uuid::new_v4()));
        let profiles = root.join("profiles");
        let profile = profiles.join("session");
        ensure_private_directory(&profile).unwrap();
        std::fs::write(profile.join(PROFILE_MARKER), b"quota-session").ok();
        let artifacts = ArtifactStore::new(root.join("artifacts")).unwrap();

        // A CdpConnection needs a real socket; nothing in this test speaks to it.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let accepted = std::thread::spawn(move || listener.accept().unwrap().0);
        let stream = std::net::TcpStream::connect(address).unwrap();
        let _server_side = accepted.join().unwrap();

        let child = observable_child();
        let pid = child.id();

        let grant = ValidatedGrant::new(BrowserGrant {
            allowed_origins: vec!["http://127.0.0.1:1".to_string()],
            allow_loopback: true,
        })
        .unwrap();

        (
            Arc::new(OwnedBrowser {
                session_id: "quota-session".to_string(),
                run_id: "quota-run".to_string(),
                profile_root: profiles,
                profile,
                child: Mutex::new(Some(child)),
                cdp: Mutex::new(CdpConnection {
                    stream,
                    buffered: VecDeque::new(),
                    next_id: 1,
                    events: VecDeque::new(),
                    grant,
                    security_error: None,
                }),
                artifacts,
                limits: BrowserLimits {
                    max_actions,
                    ..BrowserLimits::default()
                },
                cancelled: AtomicBool::new(false),
                action_count: AtomicU64::new(0),
                artifact_bytes: AtomicU64::new(0),
                started: Instant::now(),
                started_at_ms: 1_800_000_000_000,
                current_url: Mutex::new(String::new()),
                console: Mutex::new(Vec::new()),
                network: Mutex::new(Vec::new()),
                viewport: Mutex::new(BrowserViewport::default()),
            }),
            pid,
        )
    }

    /// The action quota latched `cancelled` without killing Chromium, and the
    /// `cancelled` check at the top of `begin_action` then made every later call
    /// return early — so nothing could ever reach `stop()` again. The child was
    /// left idle *and* unreachable.
    ///
    /// Asserted on the child rather than on a flag, because the flag was exactly
    /// what was already being set correctly.
    #[test]
    fn tripping_the_action_quota_kills_the_child_it_cancels() {
        let (browser, pid) = quota_session(1);

        browser
            .begin_action()
            .expect("the first action is inside the quota");
        assert!(
            browser.child.lock().unwrap().is_some(),
            "the session must still hold its child while it is under quota"
        );

        let error = browser
            .begin_action()
            .expect_err("the second action exceeds a quota of one");
        assert!(error.contains("action quota"), "got {error:?}");

        assert!(
            browser.child.lock().unwrap().is_none(),
            "the quota cancelled the session without tearing its child down, and no \
             later call can reach stop() once `cancelled` is latched"
        );
        // Where a genuinely long-lived stand-in is available, check the stronger
        // claim too: the process is gone, not merely disowned.
        #[cfg(unix)]
        assert!(
            !crate::os_signal::process_is_alive(pid),
            "the child outlived the quota that cancelled its session"
        );
        let _ = pid;
    }

    /// The counterpart: a session inside its quota must not be torn down. Without
    /// this, tearing the child down unconditionally would pass the test above.
    #[test]
    fn a_session_inside_its_action_quota_keeps_its_child() {
        let (browser, pid) = quota_session(8);

        for _ in 0..4 {
            browser.begin_action().expect("still inside the quota");
        }

        assert!(
            browser.child.lock().unwrap().is_some(),
            "a session under its quota must keep its child"
        );
        #[cfg(unix)]
        assert!(
            crate::os_signal::process_is_alive(pid),
            "a session under its quota must keep running"
        );
        let _ = pid;
        browser.stop().ok();
    }

    #[test]
    fn websocket_accept_matches_rfc_fixture() {
        assert_eq!(
            websocket_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn schemes_credentials_and_private_networks_are_blocked() {
        assert!(validate_http_url("file:///etc/passwd").is_err());
        assert!(validate_http_url("http://user:pass@example.com/").is_err());
        assert_eq!(classify_ip("127.0.0.1".parse().unwrap()), IpClass::Loopback);
        assert_eq!(
            classify_ip("169.254.1.1".parse().unwrap()),
            IpClass::Private
        );
        assert_eq!(classify_ip("10.1.2.3".parse().unwrap()), IpClass::Private);
        assert_eq!(classify_ip("8.8.8.8".parse().unwrap()), IpClass::Public);
    }

    #[test]
    fn viewport_limits_cover_supported_desktop_and_mobile_presets() {
        assert!(validate_viewport(&BrowserViewport::default()).is_ok());
        assert!(validate_viewport(&BrowserViewport {
            width: 390,
            height: 844,
            device_scale_factor: 3.0,
            mobile: true,
        })
        .is_ok());
        assert!(validate_viewport(&BrowserViewport {
            width: 200,
            height: 844,
            device_scale_factor: 2.0,
            mobile: true,
        })
        .is_err());
        assert!(validate_viewport(&BrowserViewport {
            width: 1920,
            height: 1080,
            device_scale_factor: 4.0,
            mobile: false,
        })
        .is_err());
    }

    #[test]
    fn exact_origin_and_loopback_grant_are_required() {
        let denied = ValidatedGrant::new(BrowserGrant {
            allowed_origins: vec!["http://127.0.0.1:3000".into()],
            allow_loopback: false,
        })
        .unwrap();
        assert!(denied
            .validate_navigation("http://127.0.0.1:3000/")
            .is_err());
        let allowed = ValidatedGrant::new(BrowserGrant {
            allowed_origins: vec!["http://127.0.0.1:3000".into()],
            allow_loopback: true,
        })
        .unwrap();
        assert!(allowed
            .validate_navigation("http://127.0.0.1:3000/a")
            .is_ok());
        assert!(allowed
            .validate_navigation("http://127.0.0.1:3001/a")
            .is_err());
    }

    #[test]
    fn page_requests_cannot_expand_the_run_grant() {
        let grant = ValidatedGrant::new(BrowserGrant {
            allowed_origins: vec!["https://example.com".into()],
            allow_loopback: false,
        })
        .unwrap();
        assert!(grant.validate_request("file:///etc/passwd", false).is_err());
        assert!(grant
            .validate_request("http://10.10.10.10/secret", false)
            .is_err());
        assert!(grant
            .validate_request("http://169.254.169.254/latest", false)
            .is_err());
        assert!(grant
            .validate_request("https://other.example/path", true)
            .is_err());
        assert!(grant
            .validate_request("https://cdn.example/path.js", false)
            .unwrap_err()
            .contains("subresource"));
    }

    #[test]
    fn hostname_grants_are_pinned_before_chromium_can_resolve_again() {
        let grant = ValidatedGrant::new(BrowserGrant {
            allowed_origins: vec!["http://localhost:43210".into()],
            allow_loopback: true,
        })
        .unwrap();
        let rules = grant.chromium_resolver_rules().unwrap();
        assert!(rules.starts_with("MAP localhost "));
        assert!(rules.contains("127.0.0.1") || rules.contains("[::1]"));
    }

    #[test]
    fn profile_cleanup_requires_matching_marker() {
        let root = std::env::temp_dir().join(format!("lm-browser-test-{}", uuid::Uuid::new_v4()));
        ensure_private_directory(&root).unwrap();
        let profile = root.join("browser-test");
        ensure_new_owned_profile(&root, &profile, "browser-test").unwrap();
        assert!(safe_remove_profile(&root, &profile, "wrong").is_err());
        assert!(safe_remove_profile(&root, &profile, "browser-test").is_ok());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn workflow_adapter_reuses_the_owned_session_registry() {
        let root =
            std::env::temp_dir().join(format!("lm-browser-workflow-test-{}", uuid::Uuid::new_v4()));
        let adapter = BrowserWorkflowAdapter::production(&root).unwrap();
        assert_eq!(
            adapter.execute("run-1", "list", json!({})).unwrap(),
            json!([])
        );
        assert!(adapter
            .execute("run-1", "inspect", json!({"sessionId":"missing"}))
            .unwrap_err()
            .contains("Unknown browser session"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn browser_time_and_disk_quotas_are_bounded_and_atomic() {
        let mut limits = BrowserLimits::default();
        assert!(validate_limits(&limits).is_ok());
        limits.max_session_ms = 999;
        assert!(validate_limits(&limits).is_err());
        limits = BrowserLimits::default();
        limits.max_disk_bytes = 1024;
        assert!(validate_limits(&limits).is_err());

        let used = AtomicU64::new(0);
        reserve_quota(&used, 10, 6).unwrap();
        assert!(reserve_quota(&used, 10, 5).is_err());
        assert_eq!(used.load(Ordering::SeqCst), 6);
        reserve_quota(&used, 10, 4).unwrap();
        assert_eq!(used.load(Ordering::SeqCst), 10);
    }

    #[test]
    #[ignore = "requires an installed Chromium executable and loopback sockets"]
    fn owned_chromium_runs_maintained_flows_and_records_evidence() {
        if find_chromium().is_err() {
            return;
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_server = stop.clone();
        let server = std::thread::spawn(move || {
            let body = br#"<!doctype html><html><body><input id='name'><button id='go' onclick='document.body.dataset.clicked="yes";console.log("clicked")'>Go</button><div style='height:2000px'></div></body></html>"#;
            while !stop_server.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut request = [0_u8; 2048];
                        let _ = stream.read(&mut request);
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = stream.write_all(response.as_bytes());
                        let _ = stream.write_all(body);
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        let root = std::env::temp_dir().join(format!("lm-browser-live-{}", uuid::Uuid::new_v4()));
        let profiles = root.join("profiles");
        ensure_private_directory(&profiles).unwrap();
        let artifacts = ArtifactStore::new(root.join("artifacts")).unwrap();
        let url = format!("http://127.0.0.1:{port}/");
        let browser = OwnedBrowser::launch(
            profiles,
            artifacts,
            BrowserStartRequest {
                run_id: "live-flow".to_string(),
                url: url.clone(),
                grant: BrowserGrant {
                    allowed_origins: vec![format!("http://127.0.0.1:{port}")],
                    allow_loopback: true,
                },
                limits: BrowserLimits {
                    max_actions: 1_000,
                    ..BrowserLimits::default()
                },
            },
        )
        .unwrap();
        let viewport = BrowserViewport {
            width: 390,
            height: 844,
            device_scale_factor: 3.0,
            mobile: true,
        };
        browser.set_viewport(viewport.clone()).unwrap();
        assert_eq!(browser.view().viewport.width, viewport.width);
        assert_eq!(browser.view().viewport.height, viewport.height);
        browser.reload().unwrap();
        let annotation = browser.annotate("#go").unwrap();
        assert_eq!(annotation.tag, "button");
        assert!(annotation.evidence.screenshot.is_some());
        let corpus: Value = serde_json::from_str(include_str!(
            "../fixtures/browser-v1/deterministic-flows.json"
        ))
        .unwrap();
        let flows = corpus["flows"].as_array().unwrap();
        assert_eq!(flows.len(), 10);
        let mut completed = 0_usize;
        let mut failures = Vec::new();
        for flow in flows {
            let id = flow["id"].as_str().unwrap();
            let result = (|| -> Result<(), String> {
                let flow_url = format!("{url}?flow={id}");
                browser.navigate(&flow_url)?;
                browser.type_text("#name", flow["text"].as_str().unwrap())?;
                browser.click("#go")?;
                browser.scroll(0, flow["scrollY"].as_i64().unwrap())?;
                let inspection = browser.inspect()?;
                if inspection.url != flow_url {
                    return Err(format!("unexpected URL {}", inspection.url));
                }
                let evidence = browser.capture_evidence()?;
                if evidence.screenshot.is_none()
                    || evidence.dom.is_none()
                    || evidence.accessibility.is_none()
                    || evidence.console.is_none()
                    || evidence.network.is_none()
                    || evidence.performance.is_none()
                {
                    return Err("flow did not persist its full evidence set".to_string());
                }
                let dom = browser
                    .artifacts
                    .read(&evidence.dom.unwrap().id)
                    .map_err(|error| error.to_string())?;
                if !String::from_utf8_lossy(&dom).contains("data-clicked=\"yes\"") {
                    return Err("clicked DOM state was not captured".to_string());
                }
                Ok(())
            })();
            match result {
                Ok(()) => completed += 1,
                Err(error) => failures.push(format!("{id}: {error}")),
            }
        }
        assert!(
            completed * 100 >= flows.len() * 90,
            "{completed}/{} browser flows completed: {}",
            flows.len(),
            failures.join("; ")
        );
        let stopping = Instant::now();
        browser.stop().unwrap();
        assert!(stopping.elapsed() < Duration::from_secs(2));
        stop.store(true, Ordering::SeqCst);
        server.join().unwrap();
        let _ = fs::remove_dir_all(root);
    }
}
