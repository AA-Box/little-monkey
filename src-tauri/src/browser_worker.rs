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
use crate::egress::{EgressDenial, EgressRule};

const PROFILE_MARKER: &str = ".little-monkey-browser-profile";
const MAX_CDP_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
const MAX_DEVTOOLS_HTTP_BYTES: usize = 2 * 1024 * 1024;
const MAX_SELECTOR_BYTES: usize = 8 * 1024;
const MAX_TYPE_BYTES: usize = 256 * 1024;
const MAX_ALLOWED_ORIGINS: usize = 32;

/// How often the watchdog re-examines live sessions.
///
/// A cadence, not a budget — the distinction is why this has a value while
/// `max_session_ms` keeps its own user-facing default. *How long* a session may
/// live is policy; *how promptly* an expired one is noticed is an implementation
/// detail, and leaving it unset would mean the sweep never runs at all, which is
/// not a bound.
///
/// Thirty seconds against a ten-minute default budget bounds reclaim latency at
/// five percent of the budget, and the sweep is cheap by construction: it reads
/// two integers and calls `try_wait` per session, and deliberately never walks a
/// profile directory (see [`sweep_verdict`]).
pub const BROWSER_WATCHDOG_INTERVAL: Duration = Duration::from_secs(30);

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

/// What ended a session, so a reclaim is distinguishable from a caller pressing
/// stop. `cancelled` alone could not answer that: every teardown path set the
/// same bool, so a Chromium the watchdog took back and one the user closed were
/// indistinguishable afterwards.
///
/// Named after the bound that fired rather than after the code that noticed it,
/// which is how [`crate::process_table::ProcessExit::reason`] already reports
/// this ("for `LimitExceeded` this must name the limit that fired"). That is why
/// [`Self::SessionClock`] is shared by the action path and the sweep: it is one
/// bound with two enforcement points, and splitting it would imply the session
/// died of different causes depending on who happened to look. The watchdog's own
/// record of what it took is its return value,
/// [`BrowserCommandState::sweep_expired_sessions`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum BrowserCancelReason {
    /// A caller asked: `browser_stop`, the workflow `stop` action, run shutdown,
    /// or application exit.
    Stopped,
    /// The per-session action budget ran out.
    ActionQuota,
    /// The session wall clock ran out.
    SessionClock,
    /// The profile-plus-artifact disk budget ran out.
    DiskQuota,
    /// Chromium ended on its own and nothing here asked it to. Only the sweep can
    /// ever report this — the action path never asked the question.
    ChildExited,
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
    /// Set whenever `cancelled` is, and never overwritten afterwards, so the
    /// first cause to fire is the one reported rather than the last teardown to
    /// run over it.
    pub cancel_reason: Option<BrowserCancelReason>,
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

    /// One watchdog pass over the registry: re-examines every live session and
    /// takes back the ones that are past their session clock or whose Chromium is
    /// already gone.
    ///
    /// This exists because every bound in this module was enforced from
    /// [`OwnedBrowser::begin_action`], which is reachable only through `with_cdp`
    /// — that is, only when something drives the session. An idle session was
    /// never re-examined, so its clock could not fire while nothing touched it,
    /// and child liveness was never asked at all: `try_wait` appeared only inside
    /// `stop()` and at launch, so a Chromium that died on its own left a session
    /// still reporting itself alive and still holding its profile.
    ///
    /// **The clock is applied to every session, including one a human has open.**
    /// Nothing here can tell a Workbench tab from a workflow step: both the
    /// human-driven surfaces (Workbench, Visual Edit) and the automated ones
    /// (synthetic monitors, workflow replay) go through the same `browser_start`
    /// into this same registry, and `run_id` is caller-supplied free text, so
    /// there is no field to filter on. The conservative reading is nonetheless to
    /// apply it, because `begin_action` *already* enforces this exact bound with
    /// this exact limit: an idle tab past `max_session_ms` was going to die on the
    /// user's next click either way. The sweep changes only *when* Chromium is
    /// released, never *whether* a session survives its next action — the one case
    /// it genuinely changes is the leak it is here to fix, a session nobody ever
    /// touches again.
    ///
    /// The cadence is not decided here; see [`run_browser_watchdog`].
    ///
    /// Reclaiming goes through the ordinary `stop()` teardown rather than a second
    /// kill path, so profile removal and the marker check stay in one place. The
    /// registry is drained before any `stop()` runs, for the reason
    /// [`Self::shutdown_run`] gives: `stop()` can block for up to two seconds
    /// waiting on the child, and a concurrent list or action must not be able to
    /// retain an already-reclaimed browser during it.
    pub fn sweep_expired_sessions(&self) -> Result<Vec<BrowserSweepOutcome>, String> {
        self.sweep_sessions(&|browser| browser.child_exited())
    }

    /// [`Self::sweep_expired_sessions`] with the liveness probe injected, for the
    /// reason [`crate::process_table::ProcessTable::reap_dead_hosts`] injects
    /// `host_is_alive`: the rule is then testable without a process whose liveness
    /// the test has to arrange. That matters more here than there, because this
    /// module has no portable long-lived stand-in child — see `observable_child` in
    /// the tests, where two attempts at one failed on Windows.
    fn sweep_sessions(
        &self,
        child_exited: &dyn Fn(&OwnedBrowser) -> bool,
    ) -> Result<Vec<BrowserSweepOutcome>, String> {
        // Copied out before any per-session lock is taken, exactly as
        // `security_grants` does: the liveness probe reaches into the child lock,
        // and holding the global registry lock across that would let one session's
        // teardown block every list and every action.
        let live = self
            .sessions
            .lock()
            .map_err(|_| "Browser session lock is poisoned".to_string())?
            .values()
            .cloned()
            .collect::<Vec<_>>();

        let mut condemned = Vec::new();
        for browser in live {
            match browser.sweep_verdict_now(child_exited(&browser)) {
                SweepVerdict::Keep => {}
                SweepVerdict::Reclaim(reason) => condemned.push((browser, Some(reason))),
                SweepVerdict::Evict => condemned.push((browser, None)),
            }
        }

        let mut outcomes = Vec::with_capacity(condemned.len());
        let mut reclaimed = Vec::with_capacity(condemned.len());
        {
            let mut sessions = self
                .sessions
                .lock()
                .map_err(|_| "Browser session lock is poisoned".to_string())?;
            for (browser, reason) in condemned {
                // Absent means a concurrent `browser_stop` won the race and owns
                // the teardown; session ids are fresh uuids, so this can never be
                // a different session that took the same key.
                if sessions.remove(&browser.session_id).is_none() {
                    continue;
                }
                if let Some(reason) = reason {
                    browser.record_cancel_reason(reason);
                }
                outcomes.push(BrowserSweepOutcome {
                    session_id: browser.session_id.clone(),
                    run_id: browser.run_id.clone(),
                    reason: browser.cancel_reason(),
                });
                reclaimed.push(browser);
            }
        }
        stop_browsers(reclaimed)?;
        Ok(outcomes)
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

/// What one watchdog pass decided about one session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SweepVerdict {
    /// Live and inside its bounds. Leave it in the registry, untouched.
    Keep,
    /// Take it out of the registry, run the ordinary `stop()` teardown, and record
    /// `reason` as what ended it.
    Reclaim(BrowserCancelReason),
    /// Already cancelled, and still in the registry because nothing removes a
    /// session there except `browser_stop`: the quota paths latch `cancelled` and
    /// tear the child down in place. Evict it without attributing a new cause —
    /// the reason it already recorded is the true one.
    Evict,
}

/// The whole watchdog rule, with no clock, no registry and no Chromium in it.
///
/// Ordering is the substance of this function:
///
/// - `cancelled` wins outright. A cancelled session has already been torn down by
///   whichever bound fired, so asking anything else about it can only overwrite a
///   specific cause ("action quota") with a vaguer one that merely happens to also
///   be true by now ("session clock").
/// - `child_exited` beats the clock. If Chromium is gone the session is unusable
///   whatever its clock says, and "the child exited" is the more specific fact.
/// - The clock uses the same strict `>` as [`OwnedBrowser::begin_action`], so the
///   two enforcement points agree exactly at the boundary instead of the sweep
///   reclaiming a session the next action would have allowed.
///
/// **The disk quota is deliberately absent.** `owned_directory_size` walks the
/// whole profile tree and stats every entry, up to a 250,000-entry ceiling. That
/// cost is acceptable once per action, when a caller is already waiting on
/// Chromium; paying it for every live session on a timer is not, and nothing about
/// an idle session makes its profile grow. Disk stays where it is: checked when an
/// action or an artifact write is about to add to it.
fn sweep_verdict(
    elapsed_ms: u64,
    limits: &BrowserLimits,
    child_exited: bool,
    cancelled: bool,
) -> SweepVerdict {
    if cancelled {
        return SweepVerdict::Evict;
    }
    if child_exited {
        return SweepVerdict::Reclaim(BrowserCancelReason::ChildExited);
    }
    if elapsed_ms > limits.max_session_ms {
        return SweepVerdict::Reclaim(BrowserCancelReason::SessionClock);
    }
    SweepVerdict::Keep
}

/// One session the watchdog took back, and why.
///
/// This is the record that distinguishes a reclaim from a caller-initiated stop:
/// a `browser_stop` produces no outcome here at all.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserSweepOutcome {
    pub session_id: String,
    pub run_id: String,
    /// For an eviction this is the cause the session had already recorded, not one
    /// the sweep invented. `None` only if it latched `cancelled` with no reason,
    /// which no path in this module does.
    pub reason: Option<BrowserCancelReason>,
}

/// Drives [`BrowserCommandState::sweep_expired_sessions`] on `interval` until
/// `stop` is set, handing every pass to `observe`. Blocking: the caller owns the
/// thread, so the caller also owns whether a shutdown joins it.
///
/// **`interval` has no default anywhere in this module and no caller is wired up
/// here.** How promptly an idle Chromium should be released is a trade between
/// reclaim latency and timer cost — a judgement about what this app is for, which
/// this file cannot derive. The sweep is cheap by construction (no directory walk;
/// see [`sweep_verdict`]), so there is no technical value that falls out either.
/// Shipping the mechanism with the cadence unset is the point, not an oversight.
///
/// A sweep error does not end the loop. A poisoned registry lock is the realistic
/// cause, and ending the watchdog on it would turn one panic elsewhere into a
/// permanent leak of every session opened afterwards. `observe` sees the error;
/// logging belongs to the caller, not to this module.
///
/// `stop` is re-read after each pass so a shutdown does not wait out a whole
/// interval it is already known to be pointless — but a long `interval` still
/// delays the check, which is the other reason the thread is the caller's.
pub fn run_browser_watchdog(
    state: &BrowserCommandState,
    interval: Duration,
    stop: &AtomicBool,
    mut observe: impl FnMut(Result<Vec<BrowserSweepOutcome>, String>),
) {
    while !stop.load(Ordering::SeqCst) {
        observe(state.sweep_expired_sessions());
        if stop.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(interval);
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
    /// Why `cancelled` was set. Not an `AtomicU8`-flavoured enum because the
    /// first-writer-wins rule needs a compare-and-set anyway, and a `Mutex` states
    /// it without hand-rolling one.
    cancel_reason: Mutex<Option<BrowserCancelReason>>,
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
        grant
            .validate_navigation(&request.url)
            .map_err(denial_text)?;
        // Chromium resolves a request only after Fetch.requestPaused is
        // continued. Pin each granted hostname to an address that was already
        // classified here so a second DNS answer cannot pivot the browser to
        // a private/link-local address between our check and the socket open.
        let resolver_rules = grant.chromium_resolver_rules().map_err(denial_text)?;
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
            cancel_reason: Mutex::new(None),
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
            cancel_reason: self.cancel_reason(),
            viewport: self
                .viewport
                .lock()
                .map(|value| value.clone())
                .unwrap_or_default(),
        }
    }

    fn cancel_reason(&self) -> Option<BrowserCancelReason> {
        self.cancel_reason.lock().ok().and_then(|value| *value)
    }

    /// First writer wins. Every teardown path ends at `stop()`, which records
    /// [`BrowserCancelReason::Stopped`], so last-writer-wins would relabel every
    /// quota trip and every reclaim as an ordinary caller-initiated stop — losing
    /// exactly the distinction the reason exists to make.
    fn record_cancel_reason(&self, reason: BrowserCancelReason) {
        if let Ok(mut current) = self.cancel_reason.lock() {
            current.get_or_insert(reason);
        }
    }

    /// Whether Chromium ended without anything here asking it to.
    ///
    /// `try_wait` is the only non-blocking answer available, and it also reaps: once
    /// it reports an exit the status is cached, so the `stop()` that follows still
    /// completes promptly instead of blocking on a child that is already gone.
    ///
    /// An empty child slot is *not* reported as exited. `stop()` is the only thing
    /// that empties it, so such a session is already torn down and `cancelled`
    /// describes it — answering "exited" here would attribute a spontaneous crash to
    /// a teardown this module performed.
    fn child_exited(&self) -> bool {
        match self.child.lock() {
            Ok(mut slot) => match slot.as_mut() {
                Some(child) => matches!(child.try_wait(), Ok(Some(_))),
                None => false,
            },
            // A poisoned child lock cannot be inspected. Reading that as "exited"
            // would reclaim a live session on the strength of an unrelated panic,
            // so it reads as live and the session keeps its next action.
            Err(_) => false,
        }
    }

    /// The impure half of the watchdog rule: reads the clock and the cancelled
    /// latch, and hands them plus `child_exited` to [`sweep_verdict`], which
    /// decides. Nothing is enforced here — this only gathers.
    fn sweep_verdict_now(&self, child_exited: bool) -> SweepVerdict {
        sweep_verdict(
            self.started
                .elapsed()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
            &self.limits,
            child_exited,
            self.cancelled.load(Ordering::SeqCst),
        )
    }

    fn begin_action(&self) -> Result<(), String> {
        if self.cancelled.load(Ordering::SeqCst) {
            return Err("Browser session is cancelled".to_string());
        }
        if self.started.elapsed() > Duration::from_millis(self.limits.max_session_ms) {
            self.cancelled.store(true, Ordering::SeqCst);
            self.record_cancel_reason(BrowserCancelReason::SessionClock);
            let _ = self.stop();
            return Err("Browser session time quota exceeded".to_string());
        }
        let profile_bytes = owned_directory_size(&self.profile, self.limits.max_disk_bytes)?;
        if profile_bytes.saturating_add(self.artifact_bytes.load(Ordering::SeqCst))
            > self.limits.max_disk_bytes
        {
            self.cancelled.store(true, Ordering::SeqCst);
            self.record_cancel_reason(BrowserCancelReason::DiskQuota);
            let _ = self.stop();
            return Err("Browser session disk quota exceeded".to_string());
        }
        let next = self.action_count.fetch_add(1, Ordering::SeqCst) + 1;
        if next > self.limits.max_actions {
            self.cancelled.store(true, Ordering::SeqCst);
            self.record_cancel_reason(BrowserCancelReason::ActionQuota);
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
            self.record_cancel_reason(BrowserCancelReason::DiskQuota);
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
        if let Some(denial) = cdp.security_error.take() {
            return Err(denial_text(denial));
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
            cdp.grant.validate_navigation(&url).map_err(denial_text)?;
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

    /// The single teardown path. Reclaims reuse it rather than adding a second kill
    /// path, so the marker-checked profile removal stays in one place; they only
    /// record their reason first, and [`Self::record_cancel_reason`] keeps that
    /// first cause from being relabelled `Stopped` here.
    fn stop(&self) -> Result<(), String> {
        self.cancelled.store(true, Ordering::SeqCst);
        self.record_cancel_reason(BrowserCancelReason::Stopped);
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
    /// Builds the per-run grant from what the caller asked for.
    ///
    /// # Why these three refusals stay prose while the request guards are typed
    ///
    /// Everything below is a *configuration* defect: an empty or oversized origin
    /// list, an entry that is not a URL at all, an entry that carries a path or
    /// credentials. None of them is a decision about where the browser may go —
    /// they all fire before any destination exists, and they are answered by
    /// fixing the grant, not by granting more. An [`EgressRule`] is scoped to a
    /// request: its code is what a denial sink stores per refused destination, and
    /// its detail names the address or origin that tripped it. Recording
    /// `egress.url-malformed` for a typo in a *grant entry* would put a
    /// configuration mistake into the same counter as a page trying to reach
    /// `169.254.169.254`, and an operator reading that counter could no longer
    /// tell an attack from a bad config file.
    ///
    /// [`normalized_origin`] is the exception, because it is shared with the
    /// request path: it returns a typed denial, and this is the one place where
    /// that denial is rendered straight to prose rather than propagated.
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
            let origin = normalized_origin(&url).map_err(denial_text)?;
            if !allowed_origins.contains(&origin) {
                allowed_origins.push(origin);
            }
        }
        Ok(Self {
            allowed_origins,
            allow_loopback: grant.allow_loopback,
        })
    }

    fn validate_navigation(&self, value: &str) -> Result<(), EgressDenial> {
        let verdict = self.classify_navigation(value);
        if let Err(denial) = &verdict {
            crate::denial_sink::record(BROWSER_GUARD, denial, None);
        }
        verdict
    }

    fn classify_navigation(&self, value: &str) -> Result<(), EgressDenial> {
        let url = validate_http_url(value)?;
        let origin = normalized_origin(&url)?;
        if !self.allowed_origins.contains(&origin) {
            return Err(EgressDenial::about(
                EgressRule::OriginNotAllowlisted,
                format!("navigation origin '{origin}' is outside this run's grant"),
            ));
        }
        self.validate_resolved(&url, true)
    }

    fn validate_request(&self, value: &str, document: bool) -> Result<(), EgressDenial> {
        let verdict = self.classify_request(value, document);
        if let Err(denial) = &verdict {
            crate::denial_sink::record(BROWSER_GUARD, denial, None);
        }
        verdict
    }

    fn classify_request(&self, value: &str, document: bool) -> Result<(), EgressDenial> {
        let url = validate_http_url(value)?;
        let origin = normalized_origin(&url)?;
        if !self.allowed_origins.contains(&origin) {
            // Two rules rather than one rule with a flag, because the two are not
            // the same event: a document hop is the page navigating itself
            // somewhere the run was never granted, while a subresource is the page
            // pulling in a third party. They were already two distinct sentences
            // here on purpose, and the test that pins the distinction is the only
            // egress refusal in this file whose text anybody asserted on.
            let rule = if document {
                EgressRule::RedirectLeftGrant
            } else {
                EgressRule::SubresourceLeftGrant
            };
            return Err(EgressDenial::about(
                rule,
                format!("'{origin}' is outside this run's grant"),
            ));
        }
        self.validate_resolved(&url, document)
    }

    fn validate_resolved(&self, url: &Url, _document: bool) -> Result<(), EgressDenial> {
        self.resolved_addresses(url).map(|_| ())
    }

    fn resolved_addresses(&self, url: &Url) -> Result<Vec<IpAddr>, EgressDenial> {
        let host = url
            .host_str()
            .ok_or_else(|| EgressDenial::new(EgressRule::HostMissing))?;
        let port = url
            .port_or_known_default()
            .ok_or_else(|| EgressDenial::new(EgressRule::PortMissing))?;
        let addresses: Vec<IpAddr> = (host, port)
            .to_socket_addrs()
            .map_err(|error| {
                EgressDenial::about(
                    EgressRule::DnsResolutionFailed,
                    // The host, never the URL: a path or query on a browser target
                    // is page-supplied and may carry a session token, and the host
                    // is the whole of what failed to resolve.
                    format!("browser DNS resolution failed for {host}: {error}"),
                )
            })?
            .map(|address| address.ip())
            .collect();
        if addresses.is_empty() {
            return Err(EgressDenial::about(EgressRule::DnsNoAddresses, host));
        }
        for address in &addresses {
            // Every refusal names the address that tripped it. A name can resolve
            // to several answers and any one of them is enough to refuse the whole
            // set, so a message that named no address at all — which is what this
            // used to be — left no way to tell which answer was the problem.
            match classify_ip(*address) {
                None => {}
                Some(EgressRule::Loopback) if self.allow_loopback => {}
                Some(EgressRule::Loopback) => {
                    return Err(EgressDenial::about(
                        EgressRule::Loopback,
                        format!("{address} requires an explicit per-run loopback grant"),
                    ))
                }
                Some(rule) => return Err(EgressDenial::about(rule, address.to_string())),
            }
        }
        Ok(addresses)
    }

    fn chromium_resolver_rules(&self) -> Result<String, EgressDenial> {
        let mut pinned = BTreeMap::<String, IpAddr>::new();
        for origin in &self.allowed_origins {
            // Re-parsing this file's own normalized output, so a failure here is a
            // broken invariant rather than caller input. Typed all the same: the
            // alternative is one `String` in a function whose every other refusal
            // is a rule, and `UrlMalformed` exists precisely so "did not parse"
            // stays distinguishable from "a rule refused it".
            let url = Url::parse(origin).map_err(|error| {
                EgressDenial::about(EgressRule::UrlMalformed, error.to_string())
            })?;
            let host = url
                .host_str()
                .ok_or_else(|| EgressDenial::new(EgressRule::HostMissing))?;
            // Literal IP hosts cannot be DNS-rebound and need no resolver rule.
            if host.parse::<IpAddr>().is_ok() {
                continue;
            }
            let address = self
                .resolved_addresses(&url)?
                .into_iter()
                .next()
                .ok_or_else(|| EgressDenial::about(EgressRule::DnsNoAddresses, host))?;
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

/// The rule that names `ip`'s address class, or `None` when no rule in this file
/// accounts for it and the browser may be pointed at it.
///
/// # Why this replaced a three-variant `IpClass`
///
/// `IpClass::Private` was the single verdict of eleven distinct predicates, and
/// the one sentence built from it named four of them ("Private, link-local,
/// multicast, and unspecified") — so a refused navigation could not say whether it
/// had hit RFC 1918, CGNAT, the benchmarking range or the broadcast address, and
/// neither could a test. Decomposing the verdict rather than the message is what
/// makes the difference visible to both.
///
/// # `Some(Loopback)` is not by itself a refusal
///
/// This file treats loopback specially and no other class that way: it is reachable
/// with an explicit per-run grant, everything else never is. So the rule is
/// *reported* here and the allow decision stays with the caller, which is the only
/// place the grant is known. Every other `Some` is refused unconditionally.
///
/// # Deliberately not `web.rs`'s classifier
///
/// `web.rs::blocked_reason_ip` has the same shape and shares this vocabulary, but
/// it is a different function on purpose: it refuses four classes where this
/// refuses eleven, and unifying them would newly refuse fetches that work today
/// (CGNAT is Tailscale's default range). See `egress`'s module doc.
fn classify_ip(ip: IpAddr) -> Option<EgressRule> {
    if ip.is_loopback() {
        return Some(EgressRule::Loopback);
    }
    match ip {
        IpAddr::V4(ip) => classify_v4(ip),
        IpAddr::V6(ip) => classify_v6(ip),
    }
}

/// One rule per class, in the order the predicate disjunction that preceded this
/// listed them. The classes are unchanged: this names the verdict, it does not
/// widen or narrow the set of refused addresses.
///
/// # Two things a reader will look for and not find
///
/// `240.0.0.0/4` and all of `0.0.0.0/8` except `0.0.0.0` itself are **not**
/// refused, and no branch below returns [`EgressRule::ReservedRange`] or
/// [`EgressRule::ThisNetwork`]. That is the behaviour this file already had;
/// adding either rule here would refuse addresses it accepts today, which is a
/// separate decision from naming the rules it already enforces.
///
/// # The loopback branch below is not redundant
///
/// [`classify_ip`] does answer `127.0.0.0/8` before either family helper is
/// reached, so for a v4 address this arm never fires. It exists for the address
/// that arrives here the other way: the IPv4-**mapped** form `::ffff:127.0.0.1` is
/// not `Ipv6Addr::is_loopback`, so `classify_ip` passes it through to
/// [`classify_v6`], which unwraps it to a bare `127.0.0.1` and delegates here.
/// Without this branch that address matched nothing and was classified as an
/// ordinary public navigation target — a loopback destination reachable without the
/// explicit per-run loopback grant that gates `127.0.0.1` itself.
fn classify_v4(ip: Ipv4Addr) -> Option<EgressRule> {
    if ip.is_loopback() {
        return Some(EgressRule::Loopback); // 127/8, reached via `::ffff:127.0.0.1`
    }
    if ip.is_private() {
        return Some(EgressRule::PrivateV4); // 10/8, 172.16/12, 192.168/16
    }
    if ip.is_link_local() {
        return Some(EgressRule::LinkLocal); // 169.254/16
    }
    if ip.is_broadcast() {
        return Some(EgressRule::Broadcast); // 255.255.255.255
    }
    if ip.is_documentation() {
        return Some(EgressRule::TestNet); // 192.0.2/24, 198.51.100/24, 203.0.113/24
    }
    if ip.is_unspecified() {
        return Some(EgressRule::Unspecified); // 0.0.0.0
    }
    if ip.is_multicast() {
        return Some(EgressRule::Multicast); // 224/4
    }
    match ip.octets() {
        [100, 64..=127, _, _] => Some(EgressRule::Cgnat),
        [198, 18..=19, _, _] => Some(EgressRule::Benchmarking),
        [192, 0, 0, _] => Some(EgressRule::ProtocolAssignments),
        // `0.0.0.0/8` beyond the `0.0.0.0` that `is_unspecified` above already
        // caught, and `240.0.0.0/4` minus the `255.255.255.255` that
        // `is_broadcast` caught. Both were navigable here while
        // `knowledge_pipeline.rs` — the broadest of the four guards — refused
        // them, so this guard called two non-routable ranges public. Ordering
        // matters and is why these are arms rather than earlier `if`s: the two
        // single addresses keep their own specific rules, and only the rest of
        // each range falls through to these.
        //
        // Deliberately spelled the same way as the broad guard's own tests
        // (`0.1.2.3` and `240.0.0.1`), so the two files agree by construction
        // rather than by coincidence.
        [0, _, _, _] => Some(EgressRule::ThisNetwork),
        [240..=255, _, _, _] => Some(EgressRule::ReservedRange),
        _ => None,
    }
}

/// The IPv6 half of [`classify_v4`], in the order the disjunction it replaced had.
fn classify_v6(ip: Ipv6Addr) -> Option<EgressRule> {
    // First, because `classify_ip` checks `is_loopback` before reaching here so
    // `::1` is already handled — but `::127.0.0.1` is not loopback by that
    // predicate and is not what `to_ipv4_mapped` matches, so it classified as
    // public. See `egress::is_ipv4_compatible`.
    if crate::egress::is_ipv4_compatible(&ip) {
        return Some(EgressRule::Ipv4Compatible); // ::/96, minus `::` and `::1`
    }
    if ip.is_unspecified() {
        return Some(EgressRule::Unspecified); // ::
    }
    if ip.is_multicast() {
        return Some(EgressRule::Multicast); // ff00::/8
    }
    if (ip.segments()[0] & 0xfe00) == 0xfc00 {
        return Some(EgressRule::UniqueLocalV6); // fc00::/7
    }
    if (ip.segments()[0] & 0xffc0) == 0xfe80 {
        return Some(EgressRule::LinkLocal); // fe80::/10
    }
    // Last, and reporting whichever v4 rule the wrapped address trips rather than a
    // rule of its own: `::ffff:10.0.0.1` is a private address, and calling it
    // anything else would hide that from whoever reads the denial.
    ip.to_ipv4_mapped().and_then(classify_v4)
}

struct CdpConnection {
    stream: TcpStream,
    buffered: VecDeque<u8>,
    next_id: u64,
    events: VecDeque<Value>,
    grant: ValidatedGrant,
    /// The refusal that failed an intercepted request, held until the action that
    /// provoked it can return it.
    ///
    /// Holds the denial rather than a rendered sentence. Chromium is told
    /// `BlockedByClient` immediately and the caller only learns why on the next
    /// `take()`, so this field is the whole of the refusal's memory on this path —
    /// and rendering it here would mean every re-raise site below had lost the rule
    /// before it was reached. Kept as a value, the rule survives the round trip and
    /// only becomes prose at the `String` boundary, exactly like the returned
    /// refusals. See [`denial_text`].
    security_error: Option<EgressDenial>,
}

impl CdpConnection {
    fn connect(websocket: &str, grant: ValidatedGrant) -> Result<Self, String> {
        let url = Url::parse(websocket)
            .map_err(|error| format!("Invalid DevTools websocket: {error}"))?;
        if url.scheme() != "ws" || !matches!(url.host_str(), Some("127.0.0.1" | "localhost")) {
            return Err("DevTools websocket must be loopback ws:".to_string());
        }
        // Prose, not a rule, and deliberately: this is the local DevTools control
        // channel, so a handshake failure here is not an egress denial. Recording it
        // under an `egress.*` code would put a local wiring fault in the same counter
        // as a page reaching for `169.254.169.254`, which is exactly the conflation
        // the rule codes exist to end. The loopback-`ws:` check above stays prose for
        // the same reason — this channel *must* be loopback, so `egress.loopback`
        // would name the rule that refuses what is required here.
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
                if let Some(denial) = self.security_error.take() {
                    return Err(denial_text(denial));
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
                Err(denial) => {
                    self.security_error = Some(denial);
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
            if let Some(denial) = self.security_error.take() {
                return Err(denial_text(denial));
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
        if let Some(denial) = self.security_error.take() {
            Err(denial_text(denial))
        } else {
            Ok(())
        }
    }
}

/// Renders a typed denial for one of the `String`-shaped boundaries.
///
/// The twelve `#[tauri::command]`s and [`BrowserWorkflowAdapter::execute`] are
/// `Result<_, String>` by their own contract — Tauri serializes the error to the
/// webview and the workflow engine puts it in a node result — so a denial has to
/// become prose somewhere. It becomes prose *here*, as late as it can, and through
/// [`EgressDenial`]'s own `Display`, which is the only rendering that carries the
/// rule code. Every hand-built refusal string this replaced dropped the code at
/// the refusal site instead, which is why nothing downstream could tell two
/// refusals apart. Greppable on purpose: each `map_err(denial_text)` is a place a
/// rule stops being a value.
fn denial_text(denial: EgressDenial) -> String {
    denial.to_string()
}

/// Names this guard in a denial record.
const BROWSER_GUARD: &str = "browser.navigation";

fn validate_http_url(value: &str) -> Result<Url, EgressDenial> {
    if value.len() > 16 * 1024 {
        // The length, not the URL: quoting 16 KiB of caller-supplied text into an
        // error that surfaces in the UI is its own problem.
        return Err(EgressDenial::about(
            EgressRule::UrlTooLong,
            format!(
                "browser URL is {} bytes, over the 16 KiB limit",
                value.len()
            ),
        ));
    }
    let url = Url::parse(value)
        .map_err(|error| EgressDenial::about(EgressRule::UrlMalformed, error.to_string()))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(EgressDenial::about(
            EgressRule::SchemeNotAllowed,
            format!(
                "only http: and https: browser URLs are allowed, not '{}:'",
                url.scheme()
            ),
        ));
    }
    // No detail at all, and the one refusal in this file for which that is the
    // correct choice: `EgressRule::redacts_target` is true for exactly this rule
    // because the URL is what carries the secret. Naming the host would already be
    // most of the way to naming the credential's owner, and the summary says
    // everything the caller needs to fix it.
    if !url.username().is_empty() || url.password().is_some() {
        return Err(EgressDenial::new(EgressRule::EmbeddedCredentials));
    }
    if url.host_str().is_none() {
        return Err(EgressDenial::new(EgressRule::HostMissing));
    }
    Ok(url)
}

fn normalized_origin(url: &Url) -> Result<String, EgressDenial> {
    let host = url
        .host_str()
        .ok_or_else(|| EgressDenial::new(EgressRule::HostMissing))?;
    let default = match url.scheme() {
        "http" => 80,
        "https" => 443,
        _ => {
            return Err(EgressDenial::about(
                EgressRule::SchemeNotAllowed,
                format!("only HTTP origins are supported, not '{}:'", url.scheme()),
            ))
        }
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
        quota_session_with(BrowserLimits {
            max_actions,
            ..BrowserLimits::default()
        })
    }

    /// The same fixture with the whole limit set open, for the bounds the action
    /// count is not the subject of. Session ids are unique per fixture so several
    /// of these can share one `BrowserCommandState`, which the sweep needs.
    fn quota_session_with(limits: BrowserLimits) -> (Arc<OwnedBrowser>, u32) {
        let root = std::env::temp_dir().join(format!("lm-browser-quota-{}", uuid::Uuid::new_v4()));
        let profiles = root.join("profiles");
        let session_id = format!("quota-session-{}", uuid::Uuid::new_v4());
        let profile = profiles.join(&session_id);
        ensure_private_directory(&profile).unwrap();
        std::fs::write(profile.join(PROFILE_MARKER), session_id.as_bytes()).ok();
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
                session_id,
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
                limits,
                cancelled: AtomicBool::new(false),
                cancel_reason: Mutex::new(None),
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
    /// `classify_ip` catches `::1` up front, but `::127.0.0.1` is not loopback by
    /// that predicate and is not what `to_ipv4_mapped()` matches — so an
    /// agent-driven navigation to it classified as allowed and needed no grant.
    ///
    /// Asserted as `Ipv4Compatible` rather than merely "not allowed": the rule is
    /// the claim. `::127.0.0.1` is refused because the deprecated wrapper is refused
    /// whole, *not* because anything unwrapped it and recognised loopback, and only
    /// naming the rule says which of those two happened.
    #[test]
    fn the_deprecated_ipv4_compatible_form_is_not_a_public_navigation_target() {
        use std::str::FromStr;
        for text in ["::127.0.0.1", "::10.0.0.1"] {
            let address = IpAddr::V6(Ipv6Addr::from_str(text).unwrap());
            assert_eq!(
                classify_ip(address),
                Some(EgressRule::Ipv4Compatible),
                "{text} must not classify as public"
            );
        }
        assert_eq!(
            classify_ip(IpAddr::V6(
                Ipv6Addr::from_str("2606:2800:220:1:248:1893:25c8:1946").unwrap()
            )),
            None
        );
    }

    /// Two non-routable IPv4 ranges classified as public navigation targets here
    /// while `knowledge_pipeline.rs` — the broadest of the four guards — refused
    /// them. Fail-open, not fail-closed, which is what separates this from the
    /// bracket bug in the same guard.
    ///
    /// The rule is asserted rather than "not `None`", because the two ranges each
    /// contain one address that already had its own rule and must keep it:
    /// `0.0.0.0` is `Unspecified` and `255.255.255.255` is `Broadcast`. A test that
    /// only checked "refused" would pass even if the new arms had swallowed those
    /// two, which is the mistake the ordering here exists to avoid.
    #[test]
    fn this_network_and_the_reserved_range_are_not_public_navigation_targets() {
        for (text, rule) in [
            // The ranges the guard was missing.
            ("0.1.2.3", EgressRule::ThisNetwork),
            ("0.255.255.255", EgressRule::ThisNetwork),
            ("240.0.0.1", EgressRule::ReservedRange),
            ("255.255.255.254", EgressRule::ReservedRange),
            // The two single addresses inside them that keep their own rules.
            ("0.0.0.0", EgressRule::Unspecified),
            ("255.255.255.255", EgressRule::Broadcast),
        ] {
            let address: IpAddr = text.parse().expect("test address parses");
            assert_eq!(
                classify_ip(address),
                Some(rule),
                "{text} must be refused as {}",
                rule.code()
            );
        }

        // Lower boundary of the new `240..=255` arm: 239 is the last multicast
        // octet, so this proves the arm starts where it claims and has not
        // swallowed the octet below it — and that multicast still answers with its
        // own rule rather than with the reserved one.
        assert_eq!(
            classify_ip("239.255.255.255".parse::<IpAddr>().unwrap()),
            Some(EgressRule::Multicast),
            "239/8 is multicast, not the reserved range"
        );

        // The counter-test: "refuse everything" would pass every assertion above.
        assert_eq!(
            classify_ip("1.1.1.1".parse::<IpAddr>().unwrap()),
            None,
            "an ordinary public address must still be navigable"
        );
    }

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

    /// The whole watchdog rule, with no registry, no Chromium and no timer.
    ///
    /// Every bound is asserted with its counterpart immediately beside it, because
    /// each of these branches passes trivially against a rule that fires on
    /// everything: a sweep that reclaimed unconditionally would satisfy the
    /// reclaim cases and destroy every live session.
    #[test]
    fn the_sweep_rule_bounds_the_clock_and_liveness_and_nothing_else() {
        let limits = BrowserLimits {
            max_session_ms: 10 * 60_000,
            ..BrowserLimits::default()
        };

        // Live, inside the clock: untouched.
        assert_eq!(sweep_verdict(0, &limits, false, false), SweepVerdict::Keep);
        assert_eq!(
            sweep_verdict(10 * 60_000 - 1, &limits, false, false),
            SweepVerdict::Keep
        );
        // Exactly at the budget is still inside it, matching `begin_action`'s
        // strict `>`. If the two disagreed here the sweep would reclaim sessions
        // the next action would have allowed.
        assert_eq!(
            sweep_verdict(10 * 60_000, &limits, false, false),
            SweepVerdict::Keep
        );

        // One millisecond past it: reclaimed, and named after the clock.
        assert_eq!(
            sweep_verdict(10 * 60_000 + 1, &limits, false, false),
            SweepVerdict::Reclaim(BrowserCancelReason::SessionClock)
        );

        // A child that ended on its own is reclaimed even with the clock nowhere
        // near its budget — the liveness question `begin_action` never asked.
        assert_eq!(
            sweep_verdict(0, &limits, true, false),
            SweepVerdict::Reclaim(BrowserCancelReason::ChildExited)
        );
        // ...and it outranks the clock when both are true, because a gone Chromium
        // is the more specific fact about why the session is finished.
        assert_eq!(
            sweep_verdict(10 * 60_000 + 1, &limits, true, false),
            SweepVerdict::Reclaim(BrowserCancelReason::ChildExited)
        );

        // Already cancelled: evicted, never re-attributed. Both other conditions
        // hold here, so a rule that checked them first would overwrite the real
        // cause (say, the action quota) with one that is merely also true by now.
        assert_eq!(
            sweep_verdict(10 * 60_000 + 1, &limits, true, true),
            SweepVerdict::Evict
        );
        assert_eq!(sweep_verdict(0, &limits, false, true), SweepVerdict::Evict);

        // The disk budget is not part of this rule and has no way to become part
        // of it: a session with a 1 MiB ceiling and a clock to spare is kept, so
        // no sweep can trigger the profile walk that `owned_directory_size` is.
        let tiny_disk = BrowserLimits {
            max_disk_bytes: 1024 * 1024,
            ..limits.clone()
        };
        assert_eq!(
            sweep_verdict(0, &tiny_disk, false, false),
            SweepVerdict::Keep
        );
    }

    /// The registry half: the sweep must take the condemned session *out* of the
    /// map and run the ordinary `stop()` teardown on it, and must leave everything
    /// else exactly where it was.
    ///
    /// Liveness is injected (`|_| false` — every child running) so this asserts the
    /// clock branch on every platform. The fixture has no long-lived Windows child,
    /// so a real probe would read every session there as crashed and this could
    /// never reach the clock at all.
    #[test]
    fn the_sweep_reclaims_an_idle_expired_session_and_leaves_a_live_one() {
        let root = std::env::temp_dir().join(format!("lm-browser-sweep-{}", uuid::Uuid::new_v4()));
        let state = BrowserCommandState::production(&root).unwrap();

        // Zero means "already past its budget on the first tick", which is the
        // only way to age a session without sleeping. It is a test input, not a
        // proposed default — see the counterpart below, which is what stops a
        // sweep that reclaims everything from passing this.
        let (expired, _) = quota_session_with(BrowserLimits {
            max_session_ms: 0,
            ..BrowserLimits::default()
        });
        let (live, _) = quota_session_with(BrowserLimits::default());
        state.insert(expired.clone()).unwrap();
        state.insert(live.clone()).unwrap();

        let outcomes = state.sweep_sessions(&|_| false).unwrap();

        assert_eq!(outcomes.len(), 1, "got {outcomes:?}");
        assert_eq!(outcomes[0].session_id, expired.session_id);
        assert_eq!(outcomes[0].run_id, expired.run_id);
        assert_eq!(
            outcomes[0].reason,
            Some(BrowserCancelReason::SessionClock),
            "a reclaim must name the bound that fired, not the caller-stop reason"
        );

        assert!(
            expired.child.lock().unwrap().is_none(),
            "the reclaim must go through stop(), which takes the child, rather than \
             only dropping the session out of the registry"
        );
        assert!(
            state.get(&expired.session_id).is_err(),
            "a reclaimed session must be gone from the registry, not left in it \
             reporting itself alive"
        );

        // The counterpart. Without it, a sweep that reclaimed unconditionally
        // would satisfy every assertion above while destroying working sessions.
        assert!(
            state.get(&live.session_id).is_ok(),
            "a session inside its clock must stay in the registry"
        );
        assert!(
            live.child.lock().unwrap().is_some(),
            "a session inside its clock must keep its child"
        );
        assert_eq!(live.view().cancel_reason, None);
        assert!(!live.view().cancelled);

        live.stop().ok();
        let _ = fs::remove_dir_all(root);
    }

    /// A Chromium that died on its own left a session that still reported itself
    /// alive: `try_wait` appeared only inside `stop()` and at launch, so nothing
    /// ever asked. Here the clock has 10 minutes to spare, so liveness is the only
    /// thing that can reclaim it.
    #[test]
    fn the_sweep_reclaims_a_session_whose_chromium_died_on_its_own() {
        let root = std::env::temp_dir().join(format!("lm-browser-dead-{}", uuid::Uuid::new_v4()));
        let state = BrowserCommandState::production(&root).unwrap();
        let (browser, _) = quota_session_with(BrowserLimits::default());
        state.insert(browser.clone()).unwrap();

        let outcomes = state.sweep_sessions(&|_| true).unwrap();

        assert_eq!(outcomes.len(), 1, "got {outcomes:?}");
        assert_eq!(
            outcomes[0].reason,
            Some(BrowserCancelReason::ChildExited),
            "a session reclaimed because its browser is gone must not be reported \
             as a caller-initiated stop"
        );
        assert!(state.get(&browser.session_id).is_err());
        assert!(browser.child.lock().unwrap().is_none());
        let _ = fs::remove_dir_all(root);
    }

    /// `child_exited` answers about the process, and the two answers it must not
    /// confuse are "ended on its own" and "we ended it".
    ///
    /// The reaped case is arranged by this test rather than waited for, so it does
    /// not depend on any external process's lifetime: `wait()` caches the status,
    /// which is what `try_wait` then reports.
    #[test]
    fn a_stopped_child_is_not_reported_as_having_exited_on_its_own() {
        let (browser, _) = quota_session_with(BrowserLimits::default());

        // A live child, where one is available. On Windows the stand-in exits
        // immediately (see `observable_child`), so this is the OS-level claim that
        // cannot be made portably — and the reason the sweep injects liveness.
        #[cfg(unix)]
        assert!(
            !browser.child_exited(),
            "a running child must not read as having exited"
        );

        {
            let mut slot = browser.child.lock().unwrap();
            let child = slot.as_mut().expect("the fixture holds a child");
            let _ = child.kill();
            child.wait().expect("the stand-in child is reaped");
        }
        assert!(
            browser.child_exited(),
            "a child that is gone while still in the session must read as exited"
        );

        // After `stop()` the slot is empty, which is *this module's* teardown and
        // not a spontaneous exit. Reporting it as one would make every stopped
        // session look like a crash.
        browser.stop().ok();
        assert!(
            !browser.child_exited(),
            "a session whose child stop() already took must not read as crashed"
        );
    }

    /// A session that a quota already cancelled is still in the registry, because
    /// nothing removes one there except `browser_stop`. The sweep evicts it — and
    /// must not relabel it, which is the whole reason the reason exists.
    #[test]
    fn the_sweep_evicts_an_already_cancelled_session_without_relabelling_it() {
        let root = std::env::temp_dir().join(format!("lm-browser-evict-{}", uuid::Uuid::new_v4()));
        let state = BrowserCommandState::production(&root).unwrap();
        let (browser, _) = quota_session_with(BrowserLimits {
            max_actions: 1,
            ..BrowserLimits::default()
        });
        state.insert(browser.clone()).unwrap();

        browser.begin_action().expect("the first action is allowed");
        browser
            .begin_action()
            .expect_err("the second exceeds a quota of one");
        assert_eq!(
            browser.view().cancel_reason,
            Some(BrowserCancelReason::ActionQuota)
        );

        // `stop()` has already emptied the child slot, so the probe is forced to
        // "exited" to give the sweep a competing condition: a rule that checked
        // liveness before the cancelled latch would relabel this a crash. The clock
        // ordering cannot be staged here — a session past its clock never reaches
        // the action quota — and is covered by the rule test instead.
        let outcomes = state.sweep_sessions(&|_| true).unwrap();

        assert_eq!(outcomes.len(), 1, "got {outcomes:?}");
        assert_eq!(
            outcomes[0].reason,
            Some(BrowserCancelReason::ActionQuota),
            "the sweep must report the cause the session already recorded, not \
             re-attribute it to whatever is also true by the time it looks"
        );
        assert_eq!(
            browser.view().cancel_reason,
            Some(BrowserCancelReason::ActionQuota),
            "the recorded cause must survive the reclaim's own stop()"
        );
        assert!(state.get(&browser.session_id).is_err());
        let _ = fs::remove_dir_all(root);
    }

    /// The sweep deliberately excludes the disk quota, because
    /// `owned_directory_size` walks the whole profile and stats every entry. This
    /// asserts the exclusion where it would actually be reintroduced: a session
    /// already over its disk budget is left alone by the sweep while the action
    /// path still refuses it, so the two cannot be quietly merged.
    #[test]
    fn the_sweep_does_not_walk_the_profile_for_the_disk_quota() {
        let (browser, _) = quota_session_with(BrowserLimits {
            max_disk_bytes: 1024,
            ..BrowserLimits::default()
        });
        std::fs::write(browser.profile.join("bulk"), vec![0_u8; 8 * 1024]).unwrap();

        assert_eq!(
            browser.sweep_verdict_now(false),
            SweepVerdict::Keep,
            "the sweep must not enforce the disk quota; it runs on a timer over \
             every live session and the profile walk is unbounded work"
        );
        assert!(
            browser
                .begin_action()
                .expect_err("the action path still owns the disk quota")
                .contains("disk quota"),
            "excluding disk from the sweep must not stop the action path enforcing it"
        );
        browser.stop().ok();
    }

    /// The driver contract: it keeps sweeping, and `stop` ends it.
    #[test]
    fn the_watchdog_loop_runs_until_it_is_stopped() {
        let root = std::env::temp_dir().join(format!("lm-browser-loop-{}", uuid::Uuid::new_v4()));
        let state = BrowserCommandState::production(&root).unwrap();
        let stop = AtomicBool::new(false);
        let mut passes = 0_usize;

        // A zero interval keeps the test off the clock entirely; the cadence is a
        // caller's parameter precisely so nothing here has to pick one.
        run_browser_watchdog(&state, Duration::ZERO, &stop, |outcomes| {
            outcomes.expect("an empty registry sweeps cleanly");
            passes += 1;
            if passes == 3 {
                stop.store(true, Ordering::SeqCst);
            }
        });

        assert_eq!(
            passes, 3,
            "the loop must keep sweeping until stopped, not sweep once and return"
        );
        let _ = fs::remove_dir_all(root);
    }

    /// The other half of the driver contract, and the reason `stop` is re-read
    /// after each pass rather than only at the top: a shutdown must not have to
    /// wait out a whole interval that is already known to be pointless. Timed
    /// rather than structural because the delay *is* the defect — the loop
    /// terminates either way.
    #[test]
    fn stopping_the_watchdog_does_not_wait_out_its_interval() {
        let root = std::env::temp_dir().join(format!("lm-browser-prompt-{}", uuid::Uuid::new_v4()));
        let state = BrowserCommandState::production(&root).unwrap();
        let stop = AtomicBool::new(false);
        let started = Instant::now();

        // Long enough that sleeping through it is unmistakable, and never actually
        // slept when the loop is correct.
        run_browser_watchdog(&state, Duration::from_secs(5), &stop, |outcomes| {
            outcomes.expect("an empty registry sweeps cleanly");
            stop.store(true, Ordering::SeqCst);
        });

        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the loop slept out its interval after stop was set; took {:?}",
            started.elapsed()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn websocket_accept_matches_rfc_fixture() {
        assert_eq!(
            websocket_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    /// The same four claims as before, each now asserted by rule.
    ///
    /// `is_err()` was all this could say while these returned strings, and an
    /// `Err` is the one thing every wrong implementation also produces: a guard
    /// that refused `file:` for having no host, or refused a private address as a
    /// malformed URL, passed it unchanged. The rule is what distinguishes "refused"
    /// from "refused for the reason claimed".
    #[test]
    fn schemes_credentials_and_private_networks_are_blocked() {
        assert_eq!(
            validate_http_url("file:///etc/passwd").unwrap_err().rule(),
            EgressRule::SchemeNotAllowed
        );

        let credentials = validate_http_url("http://user:pass@example.com/").unwrap_err();
        assert_eq!(credentials.rule(), EgressRule::EmbeddedCredentials);
        // The one rule whose diagnostic may not quote what it refused, so the
        // rendered sentence is asserted too and not just the rule.
        assert!(credentials.rule().redacts_target());
        assert_eq!(credentials.detail(), None);
        let rendered = credentials.to_string();
        assert!(!rendered.contains("user:pass"), "leaked: {rendered}");
        assert!(!rendered.contains("example.com"), "leaked: {rendered}");

        assert_eq!(
            classify_ip("127.0.0.1".parse().unwrap()),
            Some(EgressRule::Loopback)
        );
        assert_eq!(
            classify_ip("169.254.1.1".parse().unwrap()),
            Some(EgressRule::LinkLocal)
        );
        assert_eq!(
            classify_ip("10.1.2.3".parse().unwrap()),
            Some(EgressRule::PrivateV4)
        );
        assert_eq!(classify_ip("8.8.8.8".parse().unwrap()), None);
    }

    /// One representative address per class this file refuses, asserted by rule.
    ///
    /// The list is the readable inventory of what a browser session cannot be
    /// pointed at, which the chain of predicates is not. It also pins the
    /// *precedence* between overlapping-looking branches: `192.0.0.1` is a protocol
    /// assignment and not documentation, `255.255.255.255` is the broadcast address
    /// and not multicast, and a v4-mapped v6 address reports its inner v4 rule
    /// rather than a wrapper rule of its own.
    #[test]
    fn every_refused_address_class_reports_its_own_rule() {
        for (text, expected) in [
            // v4, in the order `classify_v4` tests them.
            ("10.1.2.3", EgressRule::PrivateV4),
            ("172.16.0.1", EgressRule::PrivateV4),
            ("192.168.1.1", EgressRule::PrivateV4),
            ("169.254.169.254", EgressRule::LinkLocal),
            ("255.255.255.255", EgressRule::Broadcast),
            ("192.0.2.1", EgressRule::TestNet),
            ("198.51.100.1", EgressRule::TestNet),
            ("203.0.113.1", EgressRule::TestNet),
            ("0.0.0.0", EgressRule::Unspecified),
            ("224.0.0.1", EgressRule::Multicast),
            ("239.255.255.250", EgressRule::Multicast),
            ("100.64.0.1", EgressRule::Cgnat),
            ("100.127.255.255", EgressRule::Cgnat),
            ("198.18.0.1", EgressRule::Benchmarking),
            ("198.19.255.255", EgressRule::Benchmarking),
            ("192.0.0.1", EgressRule::ProtocolAssignments),
            ("127.0.0.1", EgressRule::Loopback),
            ("127.1.2.3", EgressRule::Loopback),
            // v6, in the order `classify_v6` tests them.
            ("::127.0.0.1", EgressRule::Ipv4Compatible),
            ("::93.184.216.34", EgressRule::Ipv4Compatible),
            ("::", EgressRule::Unspecified),
            ("ff02::1", EgressRule::Multicast),
            ("fc00::1", EgressRule::UniqueLocalV6),
            ("fd12:3456::1", EgressRule::UniqueLocalV6),
            ("fe80::1", EgressRule::LinkLocal),
            ("::1", EgressRule::Loopback),
            // The mapped form keeps whichever rule its inner address trips.
            ("::ffff:10.0.0.1", EgressRule::PrivateV4),
            ("::ffff:169.254.169.254", EgressRule::LinkLocal),
        ] {
            let address: IpAddr = text.parse().expect("test address parses");
            assert_eq!(
                classify_ip(address),
                Some(expected),
                "{text} must be refused as {}",
                expected.code()
            );
        }
    }

    /// The counter-test, without which "refuse everything" would pass every
    /// assertion above: real public addresses in both families still classify as
    /// allowed.
    #[test]
    fn public_addresses_in_both_families_are_still_allowed() {
        for text in [
            "8.8.8.8",
            "1.1.1.1",
            "93.184.216.34",
            "2606:2800:220:1:248:1893:25c8:1946",
            "2001:4860:4860::8888",
        ] {
            let address: IpAddr = text.parse().expect("test address parses");
            assert_eq!(
                classify_ip(address),
                None,
                "{text} is a public address and must stay reachable"
            );
        }
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

    /// Two independent requirements, and asserting only `is_err()` could not tell
    /// them apart: a missing loopback grant and an origin outside the grant were the
    /// same `Err`, so a guard that refused every loopback URL regardless of the
    /// grant — or one that refused every URL regardless of its origin — passed this
    /// test. The rule says which requirement each case is about.
    #[test]
    fn exact_origin_and_loopback_grant_are_required() {
        let denied = ValidatedGrant::new(BrowserGrant {
            allowed_origins: vec!["http://127.0.0.1:3000".into()],
            allow_loopback: false,
        })
        .unwrap();
        let refusal = denied
            .validate_navigation("http://127.0.0.1:3000/")
            .unwrap_err();
        assert_eq!(
            refusal.rule(),
            EgressRule::Loopback,
            "the origin is granted; the loopback class is what is not"
        );
        // The address that tripped it, because a name can resolve to several and
        // only one of them may be the loopback answer.
        assert_eq!(
            refusal.detail(),
            Some("127.0.0.1 requires an explicit per-run loopback grant")
        );

        let allowed = ValidatedGrant::new(BrowserGrant {
            allowed_origins: vec!["http://127.0.0.1:3000".into()],
            allow_loopback: true,
        })
        .unwrap();
        assert!(allowed
            .validate_navigation("http://127.0.0.1:3000/a")
            .is_ok());
        assert_eq!(
            allowed
                .validate_navigation("http://127.0.0.1:3001/a")
                .unwrap_err()
                .rule(),
            EgressRule::OriginNotAllowlisted,
            "the loopback grant is held; the port is what leaves the grant"
        );
    }

    /// Four refusals that were four identical `Err`s, now four named rules.
    ///
    /// Writing them down exposed something the old assertions hid: with a grant of
    /// `https://example.com`, the two IP-literal cases never reach the address
    /// classifier at all — their *origin* is already outside the grant, so the
    /// origin rule answers first and the private-address rules are not what is being
    /// tested here. That layer is tested by
    /// [`an_allowlisted_origin_that_resolves_into_a_refused_class_is_still_blocked`],
    /// which grants the origin so the classifier is the only thing left to refuse it.
    #[test]
    fn page_requests_cannot_expand_the_run_grant() {
        let grant = ValidatedGrant::new(BrowserGrant {
            allowed_origins: vec!["https://example.com".into()],
            allow_loopback: false,
        })
        .unwrap();
        let rule_for = |result: Result<(), EgressDenial>| result.unwrap_err().rule();

        assert_eq!(
            rule_for(grant.validate_request("file:///etc/passwd", false)),
            EgressRule::SchemeNotAllowed
        );
        assert_eq!(
            rule_for(grant.validate_request("http://10.10.10.10/secret", false)),
            EgressRule::SubresourceLeftGrant,
            "the origin rule fires before the address is ever classified"
        );
        assert_eq!(
            rule_for(grant.validate_request("http://169.254.169.254/latest", false)),
            EgressRule::SubresourceLeftGrant,
            "the origin rule fires before the address is ever classified"
        );
        // The distinction this pair exists to hold: a document hop and a subresource
        // leaving the same grant are two rules, not one sentence with two spellings.
        assert_eq!(
            rule_for(grant.validate_request("https://other.example/path", true)),
            EgressRule::RedirectLeftGrant
        );
        let subresource = grant
            .validate_request("https://cdn.example/path.js", false)
            .unwrap_err();
        assert_eq!(subresource.rule(), EgressRule::SubresourceLeftGrant);
        assert_ne!(
            subresource.rule(),
            EgressRule::RedirectLeftGrant,
            "these two must stay distinguishable by type, not by prose"
        );
        assert_eq!(
            subresource.detail(),
            Some("'https://cdn.example' is outside this run's grant")
        );
    }

    /// The second layer, which the test above cannot reach: an origin the run *was*
    /// granted, whose host resolves into a class this file refuses.
    ///
    /// A grant naming a private IP literal is a plausible mistake for a caller to
    /// make, and the address classifier is the only thing standing behind it. Every
    /// address here is a literal, so nothing in this test resolves a name off-box.
    #[test]
    fn an_allowlisted_origin_that_resolves_into_a_refused_class_is_still_blocked() {
        for (origin, target, expected) in [
            (
                "http://10.10.10.10",
                "http://10.10.10.10/secret",
                EgressRule::PrivateV4,
            ),
            (
                "http://169.254.169.254",
                "http://169.254.169.254/latest/meta-data/",
                EgressRule::LinkLocal,
            ),
            (
                "http://192.0.0.1",
                "http://192.0.0.1/",
                EgressRule::ProtocolAssignments,
            ),
        ] {
            let grant = ValidatedGrant::new(BrowserGrant {
                allowed_origins: vec![origin.to_string()],
                allow_loopback: false,
            })
            .unwrap();
            let denial = grant.validate_request(target, false).unwrap_err();
            assert_eq!(
                denial.rule(),
                expected,
                "{origin} is granted, so {} is what must refuse it",
                expected.code()
            );
            // The offending address, so a multi-answer refusal says which answer.
            assert!(
                denial
                    .detail()
                    .is_some_and(|detail| detail.contains(origin.trim_start_matches("http://"))),
                "the refusal must name the address it refused: {denial}"
            );
            // A document hop reaches the same classifier by the same route.
            assert_eq!(
                grant.validate_request(target, true).unwrap_err().rule(),
                expected
            );
        }
    }

    /// The IPv4-**mapped** loopback form was classified as an ordinary public
    /// navigation target: it is not `Ipv6Addr::is_loopback`, so it slipped past
    /// [`classify_ip`]'s loopback check, and the v4 helper it then unwrapped into had
    /// no loopback branch — because until that unwrap existed, nothing had ever
    /// reached it with a loopback address.
    ///
    /// Sibling of the `::127.0.0.1` bug the shared `egress::is_ipv4_compatible`
    /// predicate closed, and it survived that fix because the compatible form and the
    /// mapped form are different ranges reached by different branches.
    ///
    /// # Whether it was reachable end to end depends on the platform
    ///
    /// `Url::host_str` serializes an IPv6 literal *with* its brackets, and
    /// `("[::ffff:127.0.0.1]", port).to_socket_addrs()` is where the platforms part
    /// company: macOS and Linux refuse to parse it, so the target is refused as a
    /// resolution failure before the classifier is consulted, while **Windows
    /// resolves it** — so on Windows this was a live bypass, and a granted
    /// `http://[::ffff:127.0.0.1]` origin reached this machine's own loopback
    /// services without the explicit per-run loopback grant that a plain
    /// `127.0.0.1` requires. Elsewhere the same fix is defensive: a classifier must
    /// not call a loopback address public whatever route reaches it, and a hostname
    /// whose resolver answers with a mapped address is such a route.
    ///
    /// The classifier assertions below are the load-bearing half on every platform.
    /// The `validate_navigation` half asserts the one invariant that holds on all of
    /// them, and deliberately does not pin either platform's resolver behaviour as
    /// the expected answer — an earlier version pinned the macOS verdict and failed
    /// on Windows for a reason that was not a defect.
    /// Two different guards refusing, and the `guard` column keeping them apart.
    ///
    /// This is the column's whole justification. The four guards disagree about
    /// which address classes they block — deliberately, since the broadest blocks
    /// CGNAT and unifying them would refuse fetches that work today — so a sink that
    /// recorded only the rule would average those disagreements into one number and
    /// an operator could not tell which guard was involved.
    ///
    /// Drives the real guards rather than synthesising records, so it also proves
    /// both call sites are actually wired.
    #[test]
    fn two_guards_refusing_are_distinguishable_in_the_record() {
        let _serialized = crate::denial_sink::test_lock();
        let directory =
            std::env::temp_dir().join(format!("lm-two-guards-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).expect("creates the directory");
        let path = directory.join(crate::denial_sink::SINK_FILE);
        crate::denial_sink::install(
            crate::denial_sink::DenialSink::open(&path).expect("the sink opens"),
        );

        // The web fetch guard, on a link-local literal.
        let refused = crate::web::validate_fetch_url(
            &reqwest::Url::parse("http://169.254.1.1/probe").expect("parses"),
            false,
        );
        assert!(refused.is_err());

        // The browser guard, on an origin outside the run's grant.
        let grant = ValidatedGrant::new(BrowserGrant {
            allowed_origins: vec!["https://granted.example".to_string()],
            allow_loopback: false,
        })
        .expect("the grant is valid");
        assert!(grant
            .validate_navigation("https://ungranted.example/page")
            .is_err());

        let reader = crate::denial_sink::DenialSink::open(&path).expect("reopens for reading");
        let rows = reader.recent(64).expect("reads");

        let web = rows
            .iter()
            .find(|row| row.detail.as_deref() == Some("169.254.1.1"))
            .expect("the web guard's refusal was recorded");
        assert_eq!(web.rule_code, EgressRule::LinkLocal.code());
        assert_eq!(web.guard, "web.fetch");

        let browser = rows
            .iter()
            .find(|row| {
                row.detail
                    .as_deref()
                    .is_some_and(|detail| detail.contains("ungranted.example"))
            })
            .expect("the browser guard's refusal was recorded");
        assert_eq!(browser.rule_code, EgressRule::OriginNotAllowlisted.code());
        assert_eq!(browser.guard, "browser.navigation");

        assert_ne!(
            web.guard, browser.guard,
            "the column exists precisely to keep these apart"
        );

        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn the_ipv4_mapped_loopback_form_is_classified_as_loopback() {
        for text in ["::ffff:127.0.0.1", "::ffff:127.1.2.3"] {
            let address: Ipv6Addr = text.parse().expect("parses");
            assert!(
                !address.is_loopback(),
                "{text} is deliberately NOT `is_loopback`, which is the whole trap"
            );
            assert_eq!(
                classify_ip(IpAddr::V6(address)),
                Some(EgressRule::Loopback),
                "{text} must be reported as loopback, not as a public target"
            );
        }

        // Counter-test: a public address in the same wrapper is still reachable, so
        // the new branch did not refuse the whole mapped range.
        assert_eq!(
            classify_ip(IpAddr::V6(
                "::ffff:93.184.216.34".parse::<Ipv6Addr>().unwrap()
            )),
            None
        );

        // End to end, through the gate that actually decides a navigation. The origin
        // is granted, so the only thing left to refuse it is the address — and on
        // Windows, where the bracketed literal does resolve, this is the assertion
        // that fails without the fix.
        let grant = ValidatedGrant::new(BrowserGrant {
            allowed_origins: vec!["http://[::ffff:127.0.0.1]:11434".into()],
            allow_loopback: false,
        })
        .unwrap();
        let denial = grant
            .validate_navigation("http://[::ffff:127.0.0.1]:11434/api/tags")
            .expect_err("loopback without a grant must never be allowed");
        match denial.rule() {
            // macOS and Linux: `to_socket_addrs` cannot parse the bracketed host, so
            // the target is refused before the classifier is reached. Accepted rather
            // than asserted, because it is this platform's resolver talking and not
            // this guard's policy — and because it is a bug of its own, recorded in
            // the roadmap: `web.rs` avoids it by matching on `Url::host()`, the parsed
            // enum, and its own comment names this "bracket-handling class of bug".
            EgressRule::DnsResolutionFailed => {}
            // Windows: the host resolves, so the classifier decides — and it must say
            // loopback. Any other rule, and above all `Ok`, is the bypass.
            rule => assert_eq!(
                rule,
                EgressRule::Loopback,
                "a resolvable mapped-loopback target must be refused as loopback"
            ),
        }
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
