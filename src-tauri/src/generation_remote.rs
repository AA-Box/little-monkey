//! Studio's two remote generation backends: a user-run ComfyUI server and a
//! hosted OpenAI-compatible image API.
//!
//! Both are reached over plain HTTP. Nothing here launches, links against, or
//! ships any part of either — ComfyUI is GPL-3.0 and a Python process the user
//! installs themselves, and the hosted API is somebody else's server. That
//! arm's-length shape is deliberate and is what keeps this MIT app's licensing
//! unaffected; a bundled ComfyUI or a linked SDK would not.
//!
//! The managed `sd-server` path in [`crate::generation_commands`] stays the
//! default and is the only backend that runs the user's own weight files. These
//! two exist for what it cannot reach: architectures stable-diffusion.cpp has
//! no support for, and machines with no GPU at all.

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde_json::{json, Value};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use crate::generation::{
    replace_workflow_placeholders, GeneratedMedia, GenerationRequest, RemoteBackend,
    RemoteBackendKind,
};

/// Bounds one returned image. The engine-side ceiling is the artifact store's;
/// this one stops a hostile or misconfigured endpoint before that.
const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;
/// A cold ComfyUI loading a 20 GB checkpoint is slow, but not this slow.
const REMOTE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Runs one generation on a remote backend and returns the decoded media.
pub async fn run(
    backend: &RemoteBackend,
    model: &str,
    request: &GenerationRequest,
    job_id: &str,
    cancellation: &CancellationToken,
) -> Result<GeneratedMedia, String> {
    let (bytes, media_type) = match backend.kind {
        RemoteBackendKind::ComfyUi => run_comfy(backend, model, request, job_id, cancellation).await,
        RemoteBackendKind::OpenAiCompatible => {
            run_openai_compatible(backend, model, request, cancellation).await
        }
    }?;
    if bytes.is_empty() {
        return Err(format!("{} returned an empty image", backend.label));
    }
    Ok(GeneratedMedia {
        bytes,
        media_type,
        frame_count: 1,
        fps: 1,
    })
}

/// `POST /images/generations`, or `/images/edits` when a source image is given.
///
/// The key is read from the OS keychain by provider id and never leaves this
/// function; the app stores no credential of its own for a backend.
async fn run_openai_compatible(
    backend: &RemoteBackend,
    model: &str,
    request: &GenerationRequest,
    cancellation: &CancellationToken,
) -> Result<(Vec<u8>, String), String> {
    let provider = backend
        .provider_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or("This backend has no provider to read a key from")?;
    let key = crate::providers::read_key_with_env(provider)?;
    let base_url = if backend.base_url.trim().is_empty() {
        crate::providers::resolve_base_url(
            provider,
            &crate::providers::configured_custom_providers(),
        )?
    } else {
        backend.base_url.trim().to_string()
    };
    let base = base_url.trim_end_matches('/');

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(REMOTE_TIMEOUT)
        .build()
        .map_err(|error| error.to_string())?;

    let call = match &request.init_image_base64 {
        Some(encoded) => {
            let source = STANDARD
                .decode(encoded.as_bytes())
                .map_err(|_| "The source image is not valid base64".to_string())?;
            let form = reqwest::multipart::Form::new()
                .text("model", model.to_string())
                .text("prompt", request.prompt.clone())
                .text("size", format!("{}x{}", request.width, request.height))
                .text("response_format", "b64_json")
                .part(
                    "image",
                    reqwest::multipart::Part::bytes(source).file_name("source.png"),
                );
            crate::egress::send(
                client
                    .post(format!("{base}/images/edits"))
                    .bearer_auth(&key)
                    .multipart(form),
            )
        }
        None => crate::egress::send(
            client
                .post(format!("{base}/images/generations"))
                .bearer_auth(&key)
                .json(&json!({
                    "model": model,
                    "prompt": request.prompt,
                    "size": format!("{}x{}", request.width, request.height),
                    "response_format": "b64_json",
                    "n": 1,
                    "seed": request.seed,
                })),
        ),
    };
    let response = tokio::select! {
        _ = cancellation.cancelled() => return Err("Generation cancelled".to_string()),
        response = call => response.map_err(|error| error.to_string())?,
    };

    let status = response.status();
    let bytes = response.bytes().await.map_err(|error| error.to_string())?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(format!("{} returned more than 32 MiB", backend.label));
    }
    if !status.is_success() {
        return Err(format!(
            "{} returned {status}: {}",
            backend.label,
            String::from_utf8_lossy(&bytes)
        ));
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    // A URL in `data[0].url` is the other half of this API and is rejected
    // rather than followed: fetching it would be a second, unreviewed egress to
    // whatever host the response names.
    let encoded = value
        .pointer("/data/0/b64_json")
        .and_then(Value::as_str)
        .ok_or("This endpoint must return data[0].b64_json; output URLs are rejected")?;
    let image = STANDARD
        .decode(encoded)
        .map_err(|_| "This endpoint returned invalid base64")?;
    Ok((image, "image/png".to_string()))
}

/// Submit the user's workflow, poll `/history`, download the result.
async fn run_comfy(
    backend: &RemoteBackend,
    model: &str,
    request: &GenerationRequest,
    job_id: &str,
    cancellation: &CancellationToken,
) -> Result<(Vec<u8>, String), String> {
    let mut workflow = backend
        .workflow_template
        .clone()
        .ok_or("This ComfyUI backend has no workflow")?;
    replace_workflow_placeholders(&mut workflow, request, model);

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        // A silence budget, not a deadline for the whole request: the `/view`
        // download below reads a finished render in one `bytes()` call, and a
        // ComfyUI on the LAN rather than on loopback needs longer than any
        // fixed total would allow. The small JSON polls are bounded just as
        // well by silence.
        .read_timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| error.to_string())?;
    let base = backend.base_url.trim().trim_end_matches('/');

    let response = crate::egress::send(
        client
            .post(format!("{base}/prompt"))
            .json(&json!({ "prompt": workflow, "client_id": job_id })),
    )
    .await
    .map_err(|error| format!("Submit ComfyUI workflow: {error}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let detail = response.text().await.unwrap_or_default();
        // ComfyUI answers a graph it cannot run with a node-by-node reason.
        // That body is the only actionable part of the failure — a missing
        // checkpoint and a mistyped placeholder both come back as a 400.
        let (detail, _) = crate::output_cap::cap_tail(detail.trim().to_string(), 2_000);
        return Err(if detail.is_empty() {
            format!("ComfyUI rejected the workflow ({status})")
        } else {
            format!("ComfyUI rejected the workflow ({status}):\n{detail}")
        });
    }
    let prompt_id = response
        .json::<Value>()
        .await
        .map_err(|error| error.to_string())?
        .get("prompt_id")
        .and_then(Value::as_str)
        .ok_or("ComfyUI omitted a prompt id")?
        .to_string();

    let deadline = tokio::time::Instant::now() + REMOTE_TIMEOUT;
    let descriptor = loop {
        if cancellation.is_cancelled() {
            let _ = crate::egress::send(client.post(format!("{base}/interrupt"))).await;
            return Err("Generation cancelled".to_string());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("ComfyUI generation exceeded 30 minutes".to_string());
        }
        let response = crate::egress::send(client.get(format!("{base}/history/{prompt_id}")))
            .await
            .map_err(|error| error.to_string())?;
        if response.status().is_success() {
            let value: Value = response.json().await.map_err(|error| error.to_string())?;
            if let Some(descriptor) = value.get(&prompt_id).and_then(comfy_output_image) {
                break descriptor;
            }
        }
        tokio::time::sleep(Duration::from_millis(750)).await;
    };

    let response = crate::egress::send(client.get(format!("{base}/view")).query(&[
        ("filename", descriptor.filename.as_str()),
        ("subfolder", descriptor.subfolder.as_str()),
        ("type", descriptor.kind.as_str()),
    ]))
    .await
    .map_err(|error| error.to_string())?;
    if !response.status().is_success() {
        return Err(format!(
            "ComfyUI image download returned {}",
            response.status()
        ));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|error| error.to_string())?
        .to_vec();
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err("ComfyUI returned more than 32 MiB".to_string());
    }
    Ok((bytes, "image/png".to_string()))
}

/// Where one finished ComfyUI image lives on its server.
struct ComfyImage {
    filename: String,
    subfolder: String,
    kind: String,
}

/// Finds the first saved image across a completed prompt's node outputs.
///
/// Which node saved it is workflow-specific, so every output is scanned rather
/// than one well-known node id being assumed.
fn comfy_output_image(value: &Value) -> Option<ComfyImage> {
    value
        .get("outputs")?
        .as_object()?
        .values()
        .find_map(|output| {
            let image = output.get("images")?.as_array()?.first()?;
            Some(ComfyImage {
                filename: image.get("filename")?.as_str()?.to_string(),
                subfolder: image
                    .get("subfolder")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                kind: image
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("output")
                    .to_string(),
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generation::{parse_remote_model_id, remote_model_id, GenerationTask};

    fn request() -> GenerationRequest {
        serde_json::from_value(json!({
            "modelId": "remote:comfy:sd_xl_base_1.0.safetensors",
            "task": "text_to_image",
            "prompt": "a cat",
            "negativePrompt": "blurry",
            "width": 1024,
            "height": 1024,
            "steps": 20,
            "cfgScale": 7.0,
            "seed": 42,
        }))
        .expect("request fixture must deserialize")
    }

    #[test]
    fn round_trips_a_model_id_whose_name_contains_colons() {
        let id = remote_model_id("fal", "black-forest-labs/flux:1.1");
        let (backend, model) = parse_remote_model_id(&id).expect("id must parse");
        assert_eq!(backend, "fal");
        assert_eq!(model, "black-forest-labs/flux:1.1");
        assert_eq!(parse_remote_model_id("sdxl-local"), None);
    }

    #[test]
    fn substitutes_workflow_placeholders_with_their_json_types() {
        let mut workflow = json!({
            "3": { "inputs": { "steps": "{{steps}}", "cfg": "{{cfg_scale}}", "seed": "{{seed}}" } },
            "6": { "inputs": { "text": "{{prompt}}" } },
            "7": { "inputs": { "text": "worst quality, {{negative_prompt}}" } },
            "4": { "inputs": { "ckpt_name": "{{model}}" } },
        });
        replace_workflow_placeholders(&mut workflow, &request(), "sd_xl_base_1.0.safetensors");

        // Numbers must arrive as numbers or ComfyUI rejects the whole graph.
        assert_eq!(workflow["3"]["inputs"]["steps"], json!(20));
        assert_eq!(workflow["3"]["inputs"]["cfg"], json!(7.0));
        assert_eq!(workflow["3"]["inputs"]["seed"], json!(42));
        assert_eq!(workflow["6"]["inputs"]["text"], json!("a cat"));
        // Embedded placeholders can only be text, and are spliced in place.
        assert_eq!(
            workflow["7"]["inputs"]["text"],
            json!("worst quality, blurry")
        );
        assert_eq!(
            workflow["4"]["inputs"]["ckpt_name"],
            json!("sd_xl_base_1.0.safetensors")
        );
    }

    #[test]
    fn reads_the_saved_image_out_of_any_node() {
        let history = json!({
            "outputs": {
                "12": { "text": ["ignored"] },
                "9": { "images": [{ "filename": "out.png", "subfolder": "day", "type": "output" }] },
            }
        });
        let image = comfy_output_image(&history).expect("an image must be found");
        assert_eq!(image.filename, "out.png");
        assert_eq!(image.subfolder, "day");
        assert_eq!(image.kind, "output");
        assert!(comfy_output_image(&json!({ "outputs": {} })).is_none());
    }

    #[test]
    fn a_comfy_backend_offers_only_text_to_image() {
        let backend: RemoteBackend = serde_json::from_value(json!({
            "id": "comfy",
            "label": "ComfyUI",
            "kind": "comfy_ui",
            "baseUrl": "http://127.0.0.1:8188",
            "workflowTemplate": {},
            "models": ["sd_xl_base_1.0.safetensors"],
        }))
        .expect("backend fixture must deserialize");
        assert_eq!(backend.tasks(), vec![GenerationTask::TextToImage]);
    }
}
