//! Lifecycle management for the local `llama-server` (llama.cpp) process.
//!
//! Little Monkey does not bundle or auto-download the `llama-server` binary — it must
//! already be installed on the host (e.g. via `brew install llama.cpp`). This
//! module locates that binary, spawns it against a chosen GGUF model, polls
//! its `/health` endpoint until it is ready to serve requests, and exposes
//! Tauri commands so the frontend can start/stop it and read its status.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;
use tauri::{AppHandle, Emitter, State};

use crate::AppState;

/// In-memory state for the managed `llama-server` child process.
pub struct LlamaState {
    pub process: Option<std::process::Child>,
    pub port: u16,
    pub model_path: Option<String>,
    pub status: String,
}

impl Default for LlamaState {
    fn default() -> Self {
        LlamaState {
            process: None,
            port: 8090,
            model_path: None,
            status: "stopped".to_string(),
        }
    }
}

/// Locate the `llama-server` binary: first on PATH (via `which`), then in the
/// common Homebrew install locations.
fn find_llama_server_binary() -> Result<String, String> {
    if let Ok(output) = Command::new("which").arg("llama-server").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() && Path::new(&path).exists() {
                return Ok(path);
            }
        }
    }

    for candidate in ["/opt/homebrew/bin/llama-server", "/usr/local/bin/llama-server"] {
        if Path::new(candidate).exists() {
            return Ok(candidate.to_string());
        }
    }

    Err(
        "Could not find the `llama-server` binary on your PATH or in common install locations.\n\n\
         Install llama.cpp to get it:\n\n  brew install llama.cpp\n\n\
         (On Linux, install via your package manager or build llama.cpp from source: \
         https://github.com/ggerganov/llama.cpp). Once installed, make sure `llama-server` \
         is on your PATH and try again."
            .to_string(),
    )
}

/// Emit a `llama://status` event to all windows with the current status snapshot.
fn emit_status(app: &AppHandle, status: &str, port: u16, model_path: &Option<String>) {
    let _ = app.emit(
        "llama://status",
        json!({
            "status": status,
            "port": port,
            "model_path": model_path,
        }),
    );
}

/// Start the `llama-server` process for the given model, waiting for it to
/// report healthy (or fail/time out).
#[tauri::command]
pub async fn llama_start(
    app: AppHandle,
    state: State<'_, AppState>,
    model_path: String,
    ctx_size: u32,
    gpu_layers: i32,
) -> Result<(), String> {
    // If a previous process is still running, stop it first so we don't leak
    // orphaned llama-server instances or leave a stale port bound.
    {
        let mut llama = state.llama.lock().map_err(|e| e.to_string())?;
        if let Some(mut child) = llama.process.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    let binary = find_llama_server_binary()?;

    let port = {
        let mut llama = state.llama.lock().map_err(|e| e.to_string())?;
        llama.status = "starting".to_string();
        llama.model_path = Some(model_path.clone());
        llama.port
    };

    emit_status(&app, "starting", port, &Some(model_path.clone()));

    let spawn_result = Command::new(&binary)
        .arg("-m")
        .arg(&model_path)
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .arg("-c")
        .arg(ctx_size.to_string())
        .arg("-ngl")
        .arg(gpu_layers.to_string())
        .arg("--jinja")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();

    let child = match spawn_result {
        Ok(child) => child,
        Err(e) => {
            let mut llama = state.llama.lock().map_err(|e| e.to_string())?;
            llama.status = "error".to_string();
            drop(llama);
            emit_status(&app, "error", port, &Some(model_path.clone()));
            return Err(format!("Failed to spawn llama-server: {e}"));
        }
    };

    {
        let mut llama = state.llama.lock().map_err(|e| e.to_string())?;
        llama.process = Some(child);
    }

    // Poll the health endpoint until it responds successfully, the process
    // exits early, or we hit the 60s timeout.
    let client = reqwest::Client::new();
    let health_url = format!("http://127.0.0.1:{port}/health");
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut ready = false;
    let mut failure: Option<String> = None;

    while Instant::now() < deadline {
        {
            let mut llama = state.llama.lock().map_err(|e| e.to_string())?;
            if let Some(child) = llama.process.as_mut() {
                match child.try_wait() {
                    Ok(Some(exit_status)) => {
                        failure = Some(format!(
                            "llama-server exited unexpectedly before becoming ready (status: {exit_status})"
                        ));
                    }
                    Ok(None) => {}
                    Err(e) => {
                        failure = Some(format!("Failed to check llama-server process status: {e}"));
                    }
                }
            }
        }

        if failure.is_some() {
            break;
        }

        if let Ok(resp) = client
            .get(&health_url)
            .timeout(Duration::from_secs(2))
            .send()
            .await
        {
            if resp.status().is_success() {
                ready = true;
                break;
            }
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    if ready {
        let mut llama = state.llama.lock().map_err(|e| e.to_string())?;
        llama.status = "ready".to_string();
        drop(llama);
        emit_status(&app, "ready", port, &Some(model_path.clone()));
        Ok(())
    } else {
        let error_message =
            failure.unwrap_or_else(|| "Timed out waiting for llama-server to become healthy after 60s".to_string());

        {
            let mut llama = state.llama.lock().map_err(|e| e.to_string())?;
            if let Some(mut child) = llama.process.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            llama.status = "error".to_string();
        }

        emit_status(&app, "error", port, &Some(model_path.clone()));
        Err(error_message)
    }
}

/// Kill the managed `llama-server` process, if any, and mark it stopped.
#[tauri::command]
pub async fn llama_stop(state: State<'_, AppState>) -> Result<(), String> {
    let mut llama = state.llama.lock().map_err(|e| e.to_string())?;
    if let Some(mut child) = llama.process.take() {
        child
            .kill()
            .map_err(|e| format!("Failed to kill llama-server process: {e}"))?;
        let _ = child.wait();
    }
    llama.status = "stopped".to_string();
    Ok(())
}

/// Return the current status snapshot: `{status, port, model_path}`.
#[tauri::command]
pub fn llama_status(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let llama = state.llama.lock().map_err(|e| e.to_string())?;
    Ok(json!({
        "status": llama.status,
        "port": llama.port,
        "model_path": llama.model_path,
    }))
}
