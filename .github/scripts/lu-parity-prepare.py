from pathlib import Path
import subprocess

UNRELATED_RUST = [
    "src-tauri/src/bin/monkey-cli/daemon/remote/api.rs",
    "src-tauri/src/bin/monkey-cli/daemon/remote/mod.rs",
    "src-tauri/src/bin/monkey-cli/daemon/remote/protocol.rs",
    "src-tauri/src/bin/monkey-cli/daemon/remote/realtime_bridge.rs",
    "src-tauri/src/bin/monkey-cli/daemon/remote/store.rs",
    "src-tauri/src/bin/monkey-cli/daemon/remote/talk.rs",
    "src-tauri/src/bin/monkey-cli/daemon/remote/talk_socket.rs",
    "src-tauri/src/bin/monkey-cli/daemon/remote/voice_route.rs",
    "src-tauri/src/bin/monkey-cli/main.rs",
    "src-tauri/src/bin/monkey-cli/sse.rs",
    "src-tauri/src/checkpoints.rs",
    "src-tauri/src/cli_install.rs",
    "src-tauri/src/daemon_commands.rs",
    "src-tauri/src/dictation/macos.rs",
    "src-tauri/src/local_wake_word.rs",
    "src-tauri/src/local_whisper.rs",
    "src-tauri/src/m3_production.rs",
    "src-tauri/src/mlx_runtime.rs",
    "src-tauri/src/models.rs",
    "src-tauri/src/realtime_voice.rs",
    "src-tauri/src/security_doctor.rs",
    "src-tauri/src/system.rs",
]
subprocess.run(["git", "checkout", "origin/develop", "--", *UNRELATED_RUST], check=True)

for stale in [".ci-rust-check.log", ".ci-rust-errors.log"]:
    Path(stale).unlink(missing_ok=True)

path = Path("src-tauri/src/studio_parity.rs")
text = path.read_text()


def replace(old: str, new: str) -> None:
    global text
    if new in text:
        return
    if old not in text:
        raise SystemExit(f"patch anchor not found: {old[:100]!r}")
    text = text.replace(old, new, 1)


replace(
    "use std::time::{Duration, SystemTime};",
    "use std::time::{Duration, Instant, SystemTime};",
)
replace(
    '''fn public_client() -> Result<reqwest::Client, String> {
    crate::web::executable_extension_http_client(Duration::from_secs(10 * 60))
        .map_err(|error| error.to_string())
}

fn validate_public_https(url: &Url) -> Result<(), String> {
    if url.scheme() != "https" {
        return Err("Studio discovery only permits HTTPS downloads".to_string());
    }
    if url.host_str().is_none() {
        return Err("The download URL has no host".to_string());
    }
    Ok(())
}''',
    '''fn public_client() -> Result<reqwest::Client, String> {
    crate::egress::public_download_client(
        crate::egress::PublicDestinations::Only,
        "studio-community-assets",
    )
    .timeout(Duration::from_secs(10 * 60))
    .build()
    .map_err(|error| error.to_string())
}

fn validate_public_https(url: &Url) -> Result<(), String> {
    crate::egress::classify_public_download_url(
        url,
        crate::egress::PublicDestinations::Only,
    )
    .map_err(|error| error.to_string())
}''',
)
replace(
    '''    url.query_pairs_mut()
        .append_pair("search", query)
        .append_pair("limit", "24")
        .append_pair("full", "true");''',
    '''    url.query_pairs_mut()
        .append_pair("search", query)
        .append_pair("limit", "24")
        .append_pair("full", "true")
        .append_pair("blobs", "true");''',
)
replace(
    '''pub async fn studio_workflow_run(
    app: AppHandle,
    request: StudioWorkflowRequest,
) -> Result<Vec<GenerationEntry>, String> {
    if serde_json::to_vec(&request.workflow)''',
    '''pub async fn studio_workflow_run(
    app: AppHandle,
    request: StudioWorkflowRequest,
) -> Result<Vec<GenerationEntry>, String> {
    if !matches!(
        request.kind.as_str(),
        "music" | "talking_character" | "motion_control" | "extend_video"
    ) {
        return Err("Unknown Studio workflow preset".into());
    }
    if serde_json::to_vec(&request.workflow)''',
)
replace(
    '''    let submitted: ComfyPromptResponse = response.json().await.map_err(|e| e.to_string())?;
    let history = loop {
        tokio::time::sleep(Duration::from_millis(750)).await;''',
    '''    let submitted: ComfyPromptResponse = response.json().await.map_err(|e| e.to_string())?;
    let deadline = Instant::now() + WORKFLOW_TIMEOUT;
    let history = loop {
        if Instant::now() >= deadline {
            return Err("ComfyUI workflow timed out".into());
        }
        tokio::time::sleep(Duration::from_millis(750)).await;''',
)
path.write_text(text)
