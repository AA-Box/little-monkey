//! Runtime PR Watcher and Capability Feed (ROADMAP.md Phase 8, item 18).
//!
//! Fetches recently-merged, closed pull requests from `ollama/ollama`'s
//! public REST API, classifies which ones plausibly touch Little Monkey's
//! own runtime surface area (GGUF/quantization, chat templates/tool calling,
//! API routes, hardware/GPU backends, KV cache/context, model manifest/
//! registry) with a small keyword heuristic, and persists a running report
//! of "newly relevant since last check" upstream changes with a suggested
//! Little Monkey action for each.
//!
//! Self-contained: engine + thin command layer live in one file, the same
//! convention `diagnostics.rs`/`automations.rs`/`privacy_firewall.rs` use.
//!
//! ## Scope decisions (see the shipping PR for the full rationale)
//!
//! - **On-demand, not scheduled.** This app's only existing scheduling
//!   primitive (`automations.rs`'s croner-backed cron validation plus
//!   `scheduler.ts`'s tick) exists to run *user-authored recipes*, not to
//!   invoke an arbitrary Rust function on a timer. Building a general
//!   "run this backend check every N days unattended" subsystem is out of
//!   scope for this item alone, so this ships as an explicit
//!   `runtime_pr_watcher_check_now` action instead — the persisted
//!   [`RuntimePrWatcherState::last_seen_pr_number`] means running it monthly
//!   (or whenever the user remembers to) still only ever surfaces genuinely
//!   new upstream activity, which is what the "monthly report" acceptance
//!   criterion actually requires.
//! - **A bespoke transport trait, not `runtime_adapter::HttpTransport`.**
//!   That trait's `HttpRequest` has no way to attach arbitrary headers
//!   (only `content_type`), and GitHub's REST API hard-rejects any request
//!   with no `User-Agent` header. Rather than widen a heavily-reused shared
//!   trait for one caller, this module follows the same precedent as
//!   `m3_runtime_hub::{M3DownloadTransport, M3CatalogSource}`: a minimal,
//!   purpose-built trait with its own `reqwest`-backed implementation (same
//!   `reqwest` dependency already in `Cargo.toml`, just not the same shared
//!   trait boundary).
//! - **Merged-only signal.** `state=closed` on GitHub's pulls endpoint
//!   includes PRs that were closed *without* merging; those never shipped
//!   upstream, so they are fetched (the roadmap item's own text names this
//!   exact query) but filtered out before classification — only
//!   `merged_at.is_some()` PRs can appear in the report, matching the
//!   roadmap's own methodology note ("3,430 merged PRs used as the main
//!   signal").
//! - **Non-matching PRs are omitted, not shown with a "no action" label.**
//!   A real sweep of an active upstream repo is mostly PRs this app has no
//!   stake in (docs, unrelated model additions, CI, typo fixes). Excluding
//!   them from the report has the same practical effect as labeling them
//!   "informational, no action needed" without inflating the report with
//!   noise on every check.
//! - **`ollama/ollama` only.** The roadmap item also names `llama.cpp` and
//!   MLX as "if time allows"; `repo` is a plain parameter throughout this
//!   module specifically so adding a second watched repo later is additive,
//!   but only `ollama/ollama` is wired into the command layer/UI today.

use serde::{Deserialize, Serialize};
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

use crate::AppState;

/// The only repository this feature watches today — see the module doc's
/// "ollama/ollama only" scope note.
pub const WATCHED_REPO: &str = "ollama/ollama";

const GITHUB_API_BASE: &str = "https://api.github.com";
/// Bounds each `check_now` invocation to at most `MAX_PAGES` GitHub requests
/// (well under the ~60/hour unauthenticated rate limit even if a user
/// mashes the button), and each page to `PER_PAGE` PRs.
const PER_PAGE: u32 = 30;
const MAX_PAGES: usize = 3;
/// Defensive cap on a single page response's size — GitHub's real responses
/// are a few KB to low hundreds of KB for this query; this only guards
/// against a misbehaving/compromised endpoint returning something absurd.
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
/// How much of a PR's body is considered for keyword matching. Bodies can be
/// very long (screenshots, checklists); the topic is almost always evident
/// from the title or the first few sentences.
const BODY_SNIPPET_CHARS: usize = 500;
/// Caps how many accumulated relevant PRs are kept in the persisted report
/// so the state file (and the UI list) can't grow unbounded across years of
/// monthly checks.
const MAX_ACCUMULATED_RELEVANT: usize = 200;

const STATE_SUBDIR: &str = "runtime_pr_watcher";
const STATE_FILE_NAME: &str = "state.json";
const SCHEMA_VERSION: u32 = 1;

// --- Relevance classification -------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrTopic {
    GgufQuantization,
    ChatTemplateToolCalling,
    ApiRoutes,
    HardwareGpuBackends,
    KvCacheContext,
    ModelManifestRegistry,
}

impl PrTopic {
    /// A short, concrete suggestion tied to the actual Little Monkey surface
    /// each bucket maps to — never a generic "review this" placeholder.
    pub fn suggested_action(self) -> &'static str {
        match self {
            PrTopic::GgufQuantization => {
                "Review whether the Quantization Workbench's GGUF/safetensors header parsing and quant-type table still match this change."
            }
            PrTopic::ChatTemplateToolCalling => {
                "Review whether the Chat Template and Renderer Compatibility Lab's fixtures still cover this change."
            }
            PrTopic::ApiRoutes => {
                "Review whether Little Monkey's OpenAI/Ollama API compatibility harness still matches this route's request/response shape."
            }
            PrTopic::HardwareGpuBackends => {
                "Review whether the Hardware Compatibility Matrix and Adaptive Runtime Scheduler need a new backend, driver, or fallback case."
            }
            PrTopic::KvCacheContext => {
                "Review whether the Context and KV Cache Control Center's classification still matches this behavior."
            }
            PrTopic::ModelManifestRegistry => {
                "Review whether the Model Manifest/Blob/Digest Store or catalog registry needs an update for this change."
            }
        }
    }
}

type TopicKeywords = (PrTopic, &'static [&'static str]);

/// Keyword buckets checked in this fixed order — title is checked against
/// every bucket before body is ever consulted, and within each pass the
/// first bucket (in this array's order) whose keywords hit wins. A PR whose
/// title plausibly reads as both, e.g., API-shaped and tool-calling-shaped
/// is intentionally bucketed under whichever list comes first here; ties
/// are rare enough in practice that a documented, deterministic tiebreak
/// beats a fancier scoring scheme for a heuristic this size.
const TOPIC_KEYWORDS: &[TopicKeywords] = &[
    (
        PrTopic::GgufQuantization,
        &[
            "gguf",
            "quantiz",
            "imatrix",
            "q4_k",
            "q5_k",
            "q6_k",
            "q8_0",
            "ggml-quant",
            "safetensors",
        ],
    ),
    (
        PrTopic::ChatTemplateToolCalling,
        &[
            "chat template",
            "tool call",
            "tool-call",
            "function call",
            "function-call",
            "jinja",
            "system prompt",
            "structured output",
            "json schema",
            "thinking mode",
            "reasoning content",
        ],
    ),
    (
        PrTopic::ApiRoutes,
        &[
            "/api/",
            "/v1/",
            "openai-compat",
            "openai compat",
            "api route",
            "rest api",
            "api server",
            "api endpoint",
        ],
    ),
    (
        PrTopic::HardwareGpuBackends,
        &[
            "rocm",
            "vulkan",
            "cuda",
            "metal",
            "flash attention",
            "gpu offload",
            "amd gpu",
            "nvidia",
            "jetson",
            "cpu offload",
            "multi-gpu",
            "multi gpu",
        ],
    ),
    (
        PrTopic::KvCacheContext,
        &[
            "kv cache",
            "kv-cache",
            "context window",
            "context length",
            "num_ctx",
            "context shift",
            "cache reuse",
            "context cache",
        ],
    ),
    (
        PrTopic::ModelManifestRegistry,
        &[
            "manifest",
            "registry",
            "digest",
            "model blob",
            "modelfile",
            "pull model",
            "push model",
            "model card",
        ],
    ),
];

/// Classifies a PR title (plus an optional body snippet) into the single
/// best-matching topic bucket, or `None` when nothing plausibly relevant to
/// Little Monkey's own runtime surface was found — see the module doc's
/// "non-matching PRs are omitted" scope note for what callers do with that.
pub fn classify_pr(title: &str, body_snippet: Option<&str>) -> Option<PrTopic> {
    let title_lower = title.to_lowercase();
    if let Some(topic) = match_topic(&title_lower) {
        return Some(topic);
    }
    let body_lower = body_snippet.unwrap_or_default().to_lowercase();
    if body_lower.is_empty() {
        return None;
    }
    match_topic(&body_lower)
}

fn match_topic(haystack: &str) -> Option<PrTopic> {
    TOPIC_KEYWORDS.iter().find_map(|(topic, keywords)| {
        if keywords.iter().any(|keyword| haystack.contains(keyword)) {
            Some(*topic)
        } else {
            None
        }
    })
}

// --- GitHub REST API transport -------------------------------------------

pub type PrWatcherFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, PrWatcherError>> + Send + 'a>>;

pub struct GithubHttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone)]
pub enum PrWatcherError {
    /// GitHub's unauthenticated rate limit was hit (HTTP 403/429). Never
    /// retried automatically — see [`fetch_new_closed_pulls`].
    RateLimited,
    Http(u16),
    Network(String),
    Transport(String),
    Parse(String),
}

impl PrWatcherError {
    pub fn user_message(&self) -> String {
        match self {
            PrWatcherError::RateLimited => {
                "GitHub's public API rate limit was hit while checking for upstream changes. Wait a while and try again.".to_string()
            }
            PrWatcherError::Http(status) => {
                format!("GitHub API returned an unexpected error (HTTP {status}). Try again later.")
            }
            PrWatcherError::Network(message) => {
                format!("Could not reach GitHub to check for upstream changes: {message}")
            }
            PrWatcherError::Transport(message) => {
                format!("GitHub's response could not be used safely: {message}")
            }
            PrWatcherError::Parse(message) => {
                format!("Could not read GitHub's response: {message}")
            }
        }
    }
}

/// Minimal transport seam for the GitHub REST API — mockable in tests so the
/// unit test suite never makes a real network call. Kept to exactly one
/// "GET this URL" method because that's all this feature ever needs; see
/// the module doc for why this isn't `runtime_adapter::HttpTransport`.
pub trait GithubPullsTransport: Send + Sync {
    fn get<'a>(&'a self, url: &'a str) -> PrWatcherFuture<'a, GithubHttpResponse>;
}

pub struct ReqwestGithubPullsTransport {
    client: reqwest::Client,
}

impl ReqwestGithubPullsTransport {
    pub fn new() -> Result<Self, PrWatcherError> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|error| PrWatcherError::Network(error.to_string()))?;
        Ok(Self { client })
    }
}

impl GithubPullsTransport for ReqwestGithubPullsTransport {
    fn get<'a>(&'a self, url: &'a str) -> PrWatcherFuture<'a, GithubHttpResponse> {
        Box::pin(async move {
            let response = crate::egress::send(
                self.client
                    .get(url)
                    // GitHub's REST API hard-rejects a request with no
                    // `User-Agent` (HTTP 403); the exact string doesn't matter
                    // to GitHub beyond being present, but a real product
                    // identifier keeps this app's traffic identifiable in
                    // GitHub's own logs if it's ever worth following up on.
                    .header(
                        reqwest::header::USER_AGENT,
                        "little-monkey-runtime-pr-watcher",
                    )
                    .header(reqwest::header::ACCEPT, "application/vnd.github+json")
                    .header("X-GitHub-Api-Version", "2022-11-28"),
            )
            .await
            .map_err(|error| PrWatcherError::Network(error.to_string()))?;
            let status = response.status().as_u16();
            let bytes = response
                .bytes()
                .await
                .map_err(|error| PrWatcherError::Network(error.to_string()))?;
            if bytes.len() > MAX_RESPONSE_BYTES {
                return Err(PrWatcherError::Transport(
                    "GitHub response exceeded the safety size limit".to_string(),
                ));
            }
            Ok(GithubHttpResponse {
                status,
                body: bytes.to_vec(),
            })
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
struct GithubPullRequestRaw {
    number: u64,
    title: String,
    html_url: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    merged_at: Option<String>,
}

/// Fetches closed PRs for `repo`, newest-updated first, stopping once either
/// `MAX_PAGES` is reached, a page comes back empty, or (when
/// `last_seen_pr_number` is known) a whole page contains no PR numbered
/// higher than it. That last heuristic isn't perfectly sound — GitHub sorts
/// by `updated_at`, so a long-merged low-numbered PR that just received an
/// unrelated label edit could in principle resurface near the top — but it
/// keeps a routine monthly check to a single request in the common case
/// (nothing new landed) while never missing new PRs on the pages actually
/// scanned, and never retries a request that already came back rate-limited
/// or otherwise failed.
async fn fetch_new_closed_pulls(
    transport: &dyn GithubPullsTransport,
    repo: &str,
    last_seen_pr_number: Option<u64>,
) -> Result<Vec<GithubPullRequestRaw>, PrWatcherError> {
    let mut collected = Vec::new();
    for page in 1..=MAX_PAGES {
        let url = format!(
            "{GITHUB_API_BASE}/repos/{repo}/pulls?state=closed&sort=updated&direction=desc&per_page={PER_PAGE}&page={page}"
        );
        let response = transport.get(&url).await?;
        if response.status == 403 || response.status == 429 {
            return Err(PrWatcherError::RateLimited);
        }
        if response.status != 200 {
            return Err(PrWatcherError::Http(response.status));
        }
        let page_prs: Vec<GithubPullRequestRaw> = serde_json::from_slice(&response.body)
            .map_err(|error| PrWatcherError::Parse(error.to_string()))?;
        if page_prs.is_empty() {
            break;
        }
        let any_new = match last_seen_pr_number {
            None => true,
            Some(seen) => page_prs.iter().any(|pr| pr.number > seen),
        };
        collected.extend(page_prs);
        if !any_new {
            break;
        }
    }
    Ok(collected)
}

/// Filters fetched PRs down to the ones worth surfacing: merged (not just
/// closed — see the module doc), not already seen by a previous check, and
/// matching a topic bucket.
fn build_relevant_entries(
    prs: &[GithubPullRequestRaw],
    last_seen_pr_number: Option<u64>,
) -> Vec<RelevantPrEntry> {
    prs.iter()
        .filter(|pr| match last_seen_pr_number {
            None => true,
            Some(seen) => pr.number > seen,
        })
        .filter(|pr| pr.merged_at.is_some())
        .filter_map(|pr| {
            let body_snippet: String = pr
                .body
                .as_deref()
                .unwrap_or_default()
                .chars()
                .take(BODY_SNIPPET_CHARS)
                .collect();
            classify_pr(&pr.title, Some(&body_snippet)).map(|topic| RelevantPrEntry {
                number: pr.number,
                title: pr.title.clone(),
                url: pr.html_url.clone(),
                merged: true,
                topic,
                suggested_action: topic.suggested_action().to_string(),
            })
        })
        .collect()
}

// --- Persisted state and report shapes -----------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelevantPrEntry {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub merged: bool,
    pub topic: PrTopic,
    pub suggested_action: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimePrWatcherState {
    pub schema_version: u32,
    pub source_repo: String,
    pub last_checked_at_ms: Option<u64>,
    pub last_check_error: Option<String>,
    pub last_seen_pr_number: Option<u64>,
    /// Accumulated relevant PRs across every check so far, newest first,
    /// capped at [`MAX_ACCUMULATED_RELEVANT`] — this is "the report" shown
    /// in the UI, not just the delta from the most recent check.
    pub relevant_prs: Vec<RelevantPrEntry>,
}

impl Default for RuntimePrWatcherState {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            source_repo: WATCHED_REPO.to_string(),
            last_checked_at_ms: None,
            last_check_error: None,
            last_seen_pr_number: None,
            relevant_prs: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimePrWatcherCheckResult {
    pub state: RuntimePrWatcherState,
    /// Just the PRs this particular check found that weren't already known
    /// — a subset of `state.relevant_prs`, useful for a UI that wants to
    /// highlight "N new since last time" separately from the full report.
    pub newly_relevant: Vec<RelevantPrEntry>,
    pub scanned_count: usize,
}

fn state_path(app_data: &Path) -> PathBuf {
    app_data.join(STATE_SUBDIR).join(STATE_FILE_NAME)
}

pub fn load_state_impl(app_data: &Path) -> Result<RuntimePrWatcherState, String> {
    match fs::read(state_path(app_data)) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| format!("Invalid runtime PR watcher state: {error}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(RuntimePrWatcherState::default())
        }
        Err(error) => Err(format!("Could not read runtime PR watcher state: {error}")),
    }
}

/// Atomic temp-file-then-rename write — the same pattern
/// `automations.rs::save_to`/`security_doctor.rs::atomic_write_private_json`/
/// `privacy_firewall.rs::save_policy_impl` already use for every other
/// app-data JSON file in this crate.
pub fn save_state_impl(app_data: &Path, state: &RuntimePrWatcherState) -> Result<(), String> {
    let dir = app_data.join(STATE_SUBDIR);
    fs::create_dir_all(&dir)
        .map_err(|error| format!("Could not create the runtime PR watcher directory: {error}"))?;
    let path = state_path(app_data);
    let bytes = serde_json::to_vec_pretty(state)
        .map_err(|error| format!("Could not serialize runtime PR watcher state: {error}"))?;
    let temp = dir.join(format!("state-{}.tmp", Uuid::new_v4().simple()));
    let result = fs::write(&temp, &bytes)
        .map_err(|error| format!("Could not write runtime PR watcher state: {error}"))
        .and_then(|()| {
            fs::rename(&temp, &path)
                .map_err(|error| format!("Could not publish runtime PR watcher state: {error}"))
        });
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// The full "check now" orchestration: fetch, classify, diff against the
/// persisted baseline, and save. Tauri-free (takes an injected transport, a
/// plain `app_data` path, and an explicit clock reading) so it's directly
/// unit-testable and reusable outside the desktop command layer.
///
/// The network round trip happens with no lock held (a `std::sync::Mutex`
/// guard can't be held across an `.await` point), then `lock` is acquired
/// only around the final load-merge-save — the same shape
/// `triage.rs::triage_refresh_impl` uses for its own network-then-persist
/// command. A concurrent second call started mid-fetch is reconciled by
/// reloading state fresh right before merging, not by holding the lock the
/// whole time.
pub async fn check_now_core(
    transport: &dyn GithubPullsTransport,
    app_data: &Path,
    lock: &std::sync::Mutex<()>,
    repo: &str,
    now_ms: u64,
) -> Result<RuntimePrWatcherCheckResult, String> {
    let baseline = load_state_impl(app_data)?;
    let baseline_last_seen = if baseline.source_repo == repo {
        baseline.last_seen_pr_number
    } else {
        None
    };

    let fetch_result = fetch_new_closed_pulls(transport, repo, baseline_last_seen).await;

    let _guard = lock
        .lock()
        .map_err(|_| "Runtime PR watcher lock was poisoned".to_string())?;
    let mut state = load_state_impl(app_data)?;
    if state.source_repo != repo {
        state = RuntimePrWatcherState {
            source_repo: repo.to_string(),
            ..RuntimePrWatcherState::default()
        };
    }

    match fetch_result {
        Ok(prs) => {
            let scanned_count = prs.len();
            let newly_relevant = build_relevant_entries(&prs, state.last_seen_pr_number);
            let max_seen = prs.iter().map(|pr| pr.number).max();
            state.last_seen_pr_number = match (state.last_seen_pr_number, max_seen) {
                (Some(previous), Some(fetched)) => Some(previous.max(fetched)),
                (previous, None) => previous,
                (None, fetched) => fetched,
            };
            state.last_checked_at_ms = Some(now_ms);
            state.last_check_error = None;
            for entry in newly_relevant.iter().rev() {
                state.relevant_prs.retain(|existing| existing.number != entry.number);
                state.relevant_prs.insert(0, entry.clone());
            }
            if state.relevant_prs.len() > MAX_ACCUMULATED_RELEVANT {
                state.relevant_prs.truncate(MAX_ACCUMULATED_RELEVANT);
            }
            save_state_impl(app_data, &state)?;
            Ok(RuntimePrWatcherCheckResult {
                state,
                newly_relevant,
                scanned_count,
            })
        }
        Err(error) => {
            state.last_check_error = Some(error.user_message());
            save_state_impl(app_data, &state)?;
            Err(error.user_message())
        }
    }
}

// --- Tauri command layer ---------------------------------------------------

fn app_data_dir() -> Result<PathBuf, String> {
    crate::app_paths::data_dir()
        .ok_or_else(|| "Could not resolve the application data directory".to_string())
}

fn now_ms() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .map_err(|error| error.to_string())
}

/// Loads the persisted report without making any network call — what the
/// Runtime Hub's Upstream Watcher panel calls on mount so opening the tab
/// never itself burns rate-limit budget.
#[tauri::command]
pub fn runtime_pr_watcher_state() -> Result<RuntimePrWatcherState, String> {
    load_state_impl(&app_data_dir()?)
}

/// "Check now" — the on-demand action described in the module doc's scope
/// notes. Never panics on a network failure or a GitHub rate limit; both
/// degrade to an `Err(String)` the frontend renders through the same
/// `ErrorNotice`/`busy`/`errors` convention every other Runtime Hub action
/// uses.
#[tauri::command]
pub async fn runtime_pr_watcher_check_now(
    state: tauri::State<'_, AppState>,
) -> Result<RuntimePrWatcherCheckResult, String> {
    let transport = ReqwestGithubPullsTransport::new().map_err(|error| error.user_message())?;
    check_now_core(
        &transport,
        &app_data_dir()?,
        &state.runtime_pr_watcher_lock,
        WATCHED_REPO,
        now_ms()?,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    // --- classify_pr ------------------------------------------------------
    //
    // Titles below are labeled honestly per-test: `_real_pr_title` fixtures
    // are short paraphrases of well-known, historically real ollama/ollama
    // PR themes (recalled from general public knowledge, not verbatim
    // reproductions of any specific PR's exact title/body); `_synthetic_`
    // fixtures are constructed purely to exercise a keyword bucket.

    #[test]
    fn classify_pr_matches_gguf_quantization_on_a_real_pr_title() {
        assert_eq!(
            classify_pr("add imatrix-based quantization support", None),
            Some(PrTopic::GgufQuantization)
        );
    }

    #[test]
    fn classify_pr_matches_gguf_quantization_on_a_synthetic_title() {
        assert_eq!(
            classify_pr("Support Q6_K quantization for the new model family", None),
            Some(PrTopic::GgufQuantization)
        );
    }

    #[test]
    fn classify_pr_matches_chat_template_tool_calling_on_a_real_pr_title() {
        assert_eq!(
            classify_pr("add tool calling support for compatible models", None),
            Some(PrTopic::ChatTemplateToolCalling)
        );
    }

    #[test]
    fn classify_pr_matches_chat_template_tool_calling_on_a_synthetic_title() {
        assert_eq!(
            classify_pr("Fix Jinja chat template rendering for system prompts", None),
            Some(PrTopic::ChatTemplateToolCalling)
        );
    }

    #[test]
    fn classify_pr_matches_api_routes_on_a_real_pr_title() {
        assert_eq!(
            classify_pr("add /api/embed endpoint", None),
            Some(PrTopic::ApiRoutes)
        );
    }

    #[test]
    fn classify_pr_matches_api_routes_on_a_synthetic_title() {
        assert_eq!(
            classify_pr("Add a new REST API endpoint for batch generation", None),
            Some(PrTopic::ApiRoutes)
        );
    }

    #[test]
    fn classify_pr_matches_hardware_gpu_backends_on_a_real_pr_title() {
        assert_eq!(
            classify_pr("add ROCm support for AMD GPUs", None),
            Some(PrTopic::HardwareGpuBackends)
        );
    }

    #[test]
    fn classify_pr_matches_hardware_gpu_backends_on_a_synthetic_title() {
        assert_eq!(
            classify_pr("Add Vulkan backend for older GPUs without CUDA", None),
            Some(PrTopic::HardwareGpuBackends)
        );
    }

    #[test]
    fn classify_pr_matches_kv_cache_context_on_a_real_pr_title() {
        assert_eq!(
            classify_pr("fix KV cache allocation for long context", None),
            Some(PrTopic::KvCacheContext)
        );
    }

    #[test]
    fn classify_pr_matches_kv_cache_context_on_a_synthetic_title() {
        assert_eq!(
            classify_pr("Increase default context window and fix context shift bug", None),
            Some(PrTopic::KvCacheContext)
        );
    }

    #[test]
    fn classify_pr_matches_model_manifest_registry_on_a_real_pr_title() {
        assert_eq!(
            classify_pr("add support for pulling models from a private registry", None),
            Some(PrTopic::ModelManifestRegistry)
        );
    }

    #[test]
    fn classify_pr_matches_model_manifest_registry_on_a_synthetic_title() {
        assert_eq!(
            classify_pr("Fix manifest digest mismatch when pushing a model", None),
            Some(PrTopic::ModelManifestRegistry)
        );
    }

    #[test]
    fn classify_pr_falls_back_to_the_body_snippet_when_the_title_has_no_keywords() {
        assert_eq!(
            classify_pr(
                "Fix a typo in the docs",
                Some("This also fixes an unrelated flash attention regression on CUDA.")
            ),
            Some(PrTopic::HardwareGpuBackends)
        );
    }

    #[test]
    fn classify_pr_returns_none_for_an_unrelated_synthetic_title() {
        assert_eq!(
            classify_pr("Fix a typo in the contributing guide", Some("Just a docs fix.")),
            None
        );
    }

    // --- fetch_new_closed_pulls / MockTransport ---------------------------

    struct MockTransport {
        responses: Mutex<VecDeque<Result<GithubHttpResponse, PrWatcherError>>>,
        requested_urls: Mutex<Vec<String>>,
    }

    impl MockTransport {
        fn new(responses: Vec<Result<GithubHttpResponse, PrWatcherError>>) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().collect()),
                requested_urls: Mutex::new(Vec::new()),
            }
        }

        fn requested_count(&self) -> usize {
            self.requested_urls.lock().unwrap().len()
        }
    }

    impl GithubPullsTransport for MockTransport {
        fn get<'a>(&'a self, url: &'a str) -> PrWatcherFuture<'a, GithubHttpResponse> {
            self.requested_urls.lock().unwrap().push(url.to_string());
            let next = self.responses.lock().unwrap().pop_front();
            Box::pin(async move {
                next.unwrap_or_else(|| {
                    Err(PrWatcherError::Transport(
                        "no mock response queued".to_string(),
                    ))
                })
            })
        }
    }

    fn pr_json(number: u64, title: &str, merged: bool, body: Option<&str>) -> Value {
        json!({
            "number": number,
            "title": title,
            "html_url": format!("https://github.com/ollama/ollama/pull/{number}"),
            "body": body,
            "merged_at": if merged { Some("2026-05-01T00:00:00Z") } else { None::<&str> },
            "state": "closed",
        })
    }

    fn page_response(prs: Vec<Value>) -> Result<GithubHttpResponse, PrWatcherError> {
        Ok(GithubHttpResponse {
            status: 200,
            body: serde_json::to_vec(&Value::Array(prs)).unwrap(),
        })
    }

    #[tokio::test]
    async fn fetch_new_closed_pulls_stops_once_a_page_has_no_prs_newer_than_last_seen() {
        let transport = MockTransport::new(vec![
            page_response(vec![
                pr_json(105, "add ROCm support", true, None),
                pr_json(104, "add /api/embed endpoint", true, None),
            ]),
            page_response(vec![
                pr_json(100, "already seen", true, None),
                pr_json(99, "also already seen", true, None),
            ]),
        ]);

        let result = fetch_new_closed_pulls(&transport, "ollama/ollama", Some(100))
            .await
            .expect("fetch should succeed");

        assert_eq!(result.len(), 4);
        assert_eq!(transport.requested_count(), 2, "should stop after page 2, never requesting page 3");
    }

    #[tokio::test]
    async fn fetch_new_closed_pulls_stops_on_an_empty_page() {
        let transport = MockTransport::new(vec![
            page_response(vec![pr_json(200, "add tool calling", true, None)]),
            page_response(vec![]),
        ]);

        let result = fetch_new_closed_pulls(&transport, "ollama/ollama", None)
            .await
            .expect("fetch should succeed");

        assert_eq!(result.len(), 1);
        assert_eq!(transport.requested_count(), 2);
    }

    #[tokio::test]
    async fn fetch_new_closed_pulls_never_exceeds_max_pages_when_everything_looks_new() {
        let transport = MockTransport::new(vec![
            page_response(vec![pr_json(1, "a", true, None)]),
            page_response(vec![pr_json(2, "b", true, None)]),
            page_response(vec![pr_json(3, "c", true, None)]),
            page_response(vec![pr_json(4, "d", true, None)]),
        ]);

        let result = fetch_new_closed_pulls(&transport, "ollama/ollama", None)
            .await
            .expect("fetch should succeed");

        assert_eq!(result.len(), 3, "should stop at MAX_PAGES even with no last_seen baseline");
        assert_eq!(transport.requested_count(), 3);
    }

    #[tokio::test]
    async fn fetch_new_closed_pulls_returns_rate_limited_on_403_and_never_pages_further() {
        let transport = MockTransport::new(vec![
            Ok(GithubHttpResponse {
                status: 403,
                body: br#"{"message":"API rate limit exceeded"}"#.to_vec(),
            }),
            page_response(vec![pr_json(1, "should never be requested", true, None)]),
        ]);

        let error = fetch_new_closed_pulls(&transport, "ollama/ollama", None)
            .await
            .expect_err("a 403 should surface as an error, not a crash");

        assert!(matches!(error, PrWatcherError::RateLimited));
        assert_eq!(transport.requested_count(), 1, "must not retry-loop into the rate limit");
    }

    #[tokio::test]
    async fn fetch_new_closed_pulls_degrades_gracefully_on_a_network_failure() {
        let transport = MockTransport::new(vec![Err(PrWatcherError::Network(
            "connection refused".to_string(),
        ))]);

        let error = fetch_new_closed_pulls(&transport, "ollama/ollama", None)
            .await
            .expect_err("a network failure should surface as an error, not panic");

        assert!(matches!(error, PrWatcherError::Network(_)));
    }

    #[tokio::test]
    async fn fetch_new_closed_pulls_reports_a_parse_error_on_malformed_json_instead_of_panicking() {
        let transport = MockTransport::new(vec![Ok(GithubHttpResponse {
            status: 200,
            body: b"not json".to_vec(),
        })]);

        let error = fetch_new_closed_pulls(&transport, "ollama/ollama", None)
            .await
            .expect_err("malformed JSON should surface as an error, not panic");

        assert!(matches!(error, PrWatcherError::Parse(_)));
    }

    // --- build_relevant_entries --------------------------------------------

    #[test]
    fn build_relevant_entries_excludes_unmerged_already_seen_and_unrelated_prs() {
        let raw = serde_json::from_value::<Vec<GithubPullRequestRaw>>(Value::Array(vec![
            pr_json(10, "add ROCm support", true, None), // relevant, new
            pr_json(9, "add Vulkan support", false, None), // closed but not merged: excluded
            pr_json(8, "fix flash attention on CUDA", true, None), // relevant but already seen
            pr_json(11, "fix a typo in the readme", true, None), // merged, new, but no topic match
        ]))
        .unwrap();

        let entries = build_relevant_entries(&raw, Some(8));

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].number, 10);
        assert_eq!(entries[0].topic, PrTopic::HardwareGpuBackends);
        assert!(entries[0].merged);
    }

    // --- persistence round trip --------------------------------------------

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "little-monkey-runtime-pr-watcher-{label}-{}",
                Uuid::new_v4().simple()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn load_without_a_saved_file_returns_the_default_state() {
        let temp = TestDir::new("defaults");
        let state = load_state_impl(&temp.0).expect("load should succeed");
        assert_eq!(state, RuntimePrWatcherState::default());
    }

    #[test]
    fn state_round_trips_through_save_and_load() {
        let temp = TestDir::new("round-trip");
        let mut state = RuntimePrWatcherState::default();
        state.last_checked_at_ms = Some(1_752_000_000_000);
        state.last_seen_pr_number = Some(42);
        state.relevant_prs.push(RelevantPrEntry {
            number: 42,
            title: "add ROCm support".to_string(),
            url: "https://github.com/ollama/ollama/pull/42".to_string(),
            merged: true,
            topic: PrTopic::HardwareGpuBackends,
            suggested_action: PrTopic::HardwareGpuBackends.suggested_action().to_string(),
        });

        save_state_impl(&temp.0, &state).expect("save should succeed");
        let loaded = load_state_impl(&temp.0).expect("load should succeed");

        assert_eq!(loaded, state);
    }

    // --- check_now_core end-to-end ------------------------------------------

    #[tokio::test]
    async fn check_now_core_only_surfaces_genuinely_new_prs_on_a_second_call() {
        let temp = TestDir::new("second-call");
        let lock = Mutex::new(());

        let first_transport = MockTransport::new(vec![
            page_response(vec![
                pr_json(20, "add ROCm support", true, None),
                pr_json(19, "fix a typo", true, None),
            ]),
            page_response(vec![]),
        ]);
        let first = check_now_core(&first_transport, &temp.0, &lock, "ollama/ollama", 1_000)
            .await
            .expect("first check should succeed");
        assert_eq!(first.newly_relevant.len(), 1);
        assert_eq!(first.state.last_seen_pr_number, Some(20));
        assert_eq!(first.state.relevant_prs.len(), 1);

        let second_transport = MockTransport::new(vec![
            page_response(vec![
                pr_json(22, "add /api/embed endpoint", true, None),
                pr_json(20, "add ROCm support", true, None), // already seen
            ]),
            page_response(vec![]),
        ]);
        let second = check_now_core(&second_transport, &temp.0, &lock, "ollama/ollama", 2_000)
            .await
            .expect("second check should succeed");

        assert_eq!(second.newly_relevant.len(), 1);
        assert_eq!(second.newly_relevant[0].number, 22);
        assert_eq!(second.state.last_seen_pr_number, Some(22));
        assert_eq!(
            second.state.relevant_prs.len(),
            2,
            "the report should accumulate across checks, not just show the latest delta"
        );
    }

    #[tokio::test]
    async fn check_now_core_degrades_gracefully_on_rate_limit_without_losing_prior_state() {
        let temp = TestDir::new("rate-limited");
        let lock = Mutex::new(());

        let good_transport = MockTransport::new(vec![
            page_response(vec![pr_json(5, "add ROCm support", true, None)]),
            page_response(vec![]),
        ]);
        let good = check_now_core(&good_transport, &temp.0, &lock, "ollama/ollama", 1_000)
            .await
            .expect("first check should succeed");
        assert_eq!(good.state.last_checked_at_ms, Some(1_000));

        let rate_limited_transport = MockTransport::new(vec![Ok(GithubHttpResponse {
            status: 403,
            body: br#"{"message":"API rate limit exceeded"}"#.to_vec(),
        })]);
        let error = check_now_core(
            &rate_limited_transport,
            &temp.0,
            &lock,
            "ollama/ollama",
            2_000,
        )
        .await
        .expect_err("a rate-limited check should surface an error, not panic");

        assert!(error.to_lowercase().contains("rate limit"));

        let state_after = load_state_impl(&temp.0).expect("load should succeed");
        assert_eq!(
            state_after.last_checked_at_ms,
            Some(1_000),
            "a failed check must not overwrite the last successful timestamp"
        );
        assert_eq!(state_after.last_seen_pr_number, Some(5));
        assert_eq!(state_after.relevant_prs.len(), 1);
        assert!(state_after.last_check_error.is_some());
    }
}
