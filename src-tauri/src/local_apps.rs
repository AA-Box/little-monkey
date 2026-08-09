//! Local App Builder (ROADMAP.md, Phase 3) — publishes a saved Recipe
//! (`recipes.rs`) as a small static local page served by the local API
//! server (`server.rs`), authenticated by a scoped token that can do exactly
//! one thing: trigger that one recipe's run.
//!
//! A published app is `LocalAppDefinition`, persisted at
//! `<app_data>/local_apps.json`. Publishing mints a token via
//! `server::create_local_app_token_with_state` carrying only
//! `Scope::LocalAppRun` and `bound_local_app_id: Some(id)` — the recipe/
//! workflow-run capability the design brief asks for is that scope, made
//! narrow by construction rather than by a runtime check alone: an empty
//! `backends` list means the token can never reach `chat`/`models`/
//! `embeddings` either. See `server.rs`'s `authenticate_local_app_token` for
//! where that pairing is actually enforced per request.
//!
//! The generated page (`<app_data>/local_apps/<id>/index.html`) embeds the
//! token's plaintext directly, the same way `chatWidgetEmbed.ts`'s copy-
//! pasted `<script>` snippet already does for the Developer API's Chat
//! scope — Little Monkey never persists a token's plaintext, so embedding it
//! into a file the user asked to be created is the one place it's ever
//! written to disk.
//!
//! `POST /v1/workflows/runs` submission was called out in `server.rs`'s
//! module doc as a deliberate non-goal requiring "an explicit per-run
//! approval, mirroring `permissions.rs`" before it could ever be added — the
//! `run` route this module's pages call goes through exactly that approval
//! (`permissions::request_permission`, in `server::handle_local_app_run`)
//! before it ever emits [`LOCAL_APP_RUN_REQUESTED_EVENT`] for the frontend
//! to act on.

use std::path::{Path, PathBuf};

use tauri::{AppHandle, Emitter};

use crate::recipes::Recipe;
use crate::AppState;
use crate::profiles::ProfileScopedPaths;

const CONFIG_FILE: &str = "local_apps.json";
const APPS_DIR: &str = "local_apps";

/// Emitted after a successful `local_apps_publish`/`local_apps_unpublish`,
/// with the acting window's label as payload — same cross-window sync
/// convention as `recipes.rs`'s `RECIPES_CHANGED_EVENT`.
pub const LOCAL_APPS_CHANGED_EVENT: &str = "local-apps://changed";

/// Emitted by `server::handle_local_app_run` once a run request has cleared
/// scope/binding checks, param validation, and human approval — the
/// frontend's `localAppsStore.ts` listens for this once, at app startup
/// (mirroring `App.tsx`'s existing `onRunCancellationRequested` listener),
/// and calls `recipeRunner.ts`'s `runRecipeNow` tagged with `app_id`.
pub const LOCAL_APP_RUN_REQUESTED_EVENT: &str = "local-apps://run-requested";

#[derive(serde::Serialize, Clone, Debug)]
pub struct LocalAppRunRequestedPayload {
    pub app_id: String,
    pub recipe_name: String,
    pub params: std::collections::HashMap<String, String>,
}

/// Which static page `publish_impl` generates. All five share the same
/// underlying mechanism today (a param form that POSTs to the `run` route —
/// see [`render_page_html`]) since that's the only execution primitive this
/// stage builds; the distinction is copy/labeling, not different wiring.
/// `Dashboard`/`ApprovalPage`/`ReportGenerator`/`ChatWidget` richer,
/// template-specific rendering is future work, not faked here.
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LocalAppTemplate {
    Form,
    Dashboard,
    ApprovalPage,
    ReportGenerator,
    ChatWidget,
}

impl LocalAppTemplate {
    fn heading_and_blurb(self) -> (&'static str, &'static str) {
        match self {
            LocalAppTemplate::Form => (
                "Form",
                "Fill in the parameters below and run the recipe.",
            ),
            LocalAppTemplate::Dashboard => (
                "Dashboard",
                "A quick-glance runner for this recipe's parameters.",
            ),
            LocalAppTemplate::ApprovalPage => (
                "Approval",
                "Review the parameters below, then approve to run the recipe.",
            ),
            LocalAppTemplate::ReportGenerator => (
                "Report generator",
                "Generate a report by running the recipe with these parameters.",
            ),
            LocalAppTemplate::ChatWidget => (
                "Chat",
                "A minimal one-shot prompt runner for this recipe.",
            ),
        }
    }
}

/// A published Local App. `param_bindings` maps a recipe-declared param name
/// to the label the generated page shows for it — every key must already be
/// declared in the resolved recipe's own `params` (validated in
/// [`publish_impl`]), the same "unknown key is a hard error" stance
/// `recipes::resolve_param_values` takes.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct LocalAppDefinition {
    pub id: String,
    pub name: String,
    pub recipe_name: String,
    pub template: LocalAppTemplate,
    #[serde(default)]
    pub param_bindings: std::collections::HashMap<String, String>,
    pub scoped_token_id: String,
    pub created_at: u64,
    pub enabled: bool,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default)]
pub struct LocalAppsConfig {
    #[serde(default)]
    pub apps: Vec<LocalAppDefinition>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Same slug shape `recipes.rs::is_valid_recipe_name` uses, but generated
/// (never user-typed): every id is a fresh `Uuid::new_v4()`. Validated again
/// on every lookup from an untrusted path segment (the static-file and `run`
/// HTTP routes) before it ever touches a filesystem join — an unvalidated
/// id here would let `app_data/local_apps/<id>` itself escape the intended
/// directory before path canonicalization even runs.
pub fn is_valid_app_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
}

pub fn config_file_path(app: &AppHandle) -> Result<PathBuf, String> {
    let base = app
        .profile_data_dir()
        .map_err(|e| format!("Failed to resolve app data directory: {e}"))?;
    if !base.exists() {
        std::fs::create_dir_all(&base)
            .map_err(|e| format!("Failed to create app data directory {}: {e}", base.display()))?;
    }
    Ok(base.join(CONFIG_FILE))
}

pub fn load_config_impl(path: &Path) -> Result<LocalAppsConfig, String> {
    match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw).map_err(|e| format!("Corrupt local_apps.json: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(LocalAppsConfig::default()),
        Err(e) => Err(format!("Failed to read local_apps.json: {e}")),
    }
}

pub fn save_config_impl(path: &Path, config: &LocalAppsConfig) -> Result<(), String> {
    let payload = serde_json::to_string_pretty(config)
        .map_err(|e| format!("Failed to serialize local_apps.json: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &payload).map_err(|e| format!("Failed to write local_apps.json: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("Failed to finalize local_apps.json: {e}"))?;
    Ok(())
}

fn app_dir(app_data_dir: &Path, id: &str) -> PathBuf {
    app_data_dir.join(APPS_DIR).join(id)
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn js_string(value: &str) -> String {
    serde_json::to_string(value).expect("JSON string encoding never fails")
}

/// Builds the static single-file page for `def`: a form with one input per
/// declared recipe param (labeled via `def.param_bindings`, falling back to
/// the raw param name), that `fetch()`-POSTs the filled-in values as JSON to
/// the local API server's `run` route with the minted token as a bearer
/// header — the same "embed the plaintext directly in the generated
/// artifact" approach `chatWidgetEmbed.ts` already uses for the Developer
/// API's chat widget snippet.
fn render_page_html(def: &LocalAppDefinition, recipe: &Recipe, token: &str, port: u16) -> String {
    let (heading, blurb) = def.template.heading_and_blurb();
    let mut param_names: Vec<&String> = recipe.params.keys().collect();
    param_names.sort();

    let mut fields = String::new();
    for name in &param_names {
        let label = def
            .param_bindings
            .get(*name)
            .cloned()
            .unwrap_or_else(|| (*name).clone());
        let default_value = recipe.params.get(*name).cloned().flatten().unwrap_or_default();
        fields.push_str(&format!(
            "<label class=\"field\"><span>{}</span><input type=\"text\" name=\"{}\" value=\"{}\" /></label>\n",
            html_escape(&label),
            html_escape(name),
            html_escape(&default_value),
        ));
    }
    if param_names.is_empty() {
        fields.push_str("<p class=\"muted\">This recipe declares no parameters.</p>\n");
    }

    let param_names_json = serde_json::to_string(&param_names).unwrap_or_else(|_| "[]".to_string());

    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8" />
<title>{title}</title>
<meta name="viewport" content="width=device-width, initial-scale=1" />
<style>
  :root {{ color-scheme: light dark; }}
  body {{ font-family: system-ui, sans-serif; max-width: 40rem; margin: 2rem auto; padding: 0 1rem; }}
  h1 {{ font-size: 1.25rem; }}
  p.muted {{ color: #6b7280; }}
  .field {{ display: flex; flex-direction: column; gap: 0.25rem; margin-bottom: 0.75rem; }}
  .field span {{ font-size: 0.85rem; font-weight: 600; }}
  input {{ padding: 0.5rem; font-size: 1rem; }}
  button {{ padding: 0.6rem 1.2rem; font-size: 1rem; cursor: pointer; }}
  #status {{ margin-top: 1rem; font-size: 0.9rem; }}
</style>
</head>
<body>
<h1>{heading_escaped}: {recipe_name_escaped}</h1>
<p class="muted">{blurb_escaped}</p>
<form id="run-form">
{fields}
<button type="submit">Run</button>
</form>
<p id="status"></p>
<script>
(function() {{
  var paramNames = {param_names_json};
  var form = document.getElementById("run-form");
  var status = document.getElementById("status");
  form.addEventListener("submit", function(event) {{
    event.preventDefault();
    var values = {{}};
    for (var i = 0; i < paramNames.length; i++) {{
      var name = paramNames[i];
      var input = form.elements.namedItem(name);
      values[name] = input ? input.value : "";
    }}
    status.textContent = "Sending…";
    fetch({run_url}, {{
      method: "POST",
      headers: {{
        "Content-Type": "application/json",
        "Authorization": "Bearer {token}",
      }},
      body: JSON.stringify(values),
    }}).then(function(response) {{
      if (response.status === 202) {{
        status.textContent = "Sent — approve it in the desktop app to run.";
      }} else {{
        response.json().then(function(body) {{
          status.textContent = "Error: " + (body.error && body.error.message ? body.error.message : response.status);
        }}).catch(function() {{
          status.textContent = "Error: " + response.status;
        }});
      }}
    }}).catch(function(error) {{
      status.textContent = "Error: " + error;
    }});
  }});
}})();
</script>
</body>
</html>
"#,
        title = html_escape(&def.name),
        heading_escaped = html_escape(heading),
        recipe_name_escaped = html_escape(&def.recipe_name),
        blurb_escaped = html_escape(blurb),
        fields = fields,
        param_names_json = param_names_json,
        run_url = js_string(&format!("http://127.0.0.1:{port}/v1/local-apps/{}/run", def.id)),
        token = token,
    )
}

/// Reads one file from `<app_data>/local_apps/<app_id>/`, rejecting
/// anything that resolves outside that exact directory — canonicalize, then
/// verify the prefix, the same convention `native_skills.rs` uses for its
/// own workspace-scoped file reads. `app_id` is validated before it's ever
/// joined onto a path: an attacker-controlled id containing `..` must be
/// rejected before path construction, not just after, since the id itself
/// (not only `rel_path`) forms part of the directory being canonicalized.
pub fn read_static_file(app_data_dir: &Path, app_id: &str, rel_path: &str) -> Result<Vec<u8>, String> {
    if !is_valid_app_id(app_id) {
        return Err("invalid Local App id".to_string());
    }
    let root = app_dir(app_data_dir, app_id);
    let canonical_root = std::fs::canonicalize(&root).map_err(|e| e.to_string())?;
    let relative = if rel_path.is_empty() || rel_path.ends_with('/') {
        format!("{rel_path}index.html")
    } else {
        rel_path.to_string()
    };
    let requested = root.join(&relative);
    let canonical_requested = std::fs::canonicalize(&requested).map_err(|e| e.to_string())?;
    if !canonical_requested.starts_with(&canonical_root) {
        return Err("path escapes the Local App's served directory".to_string());
    }
    std::fs::read(&canonical_requested).map_err(|e| e.to_string())
}

/// Resolves the recipe, mints a token scoped to exactly this one app (via
/// `server::create_local_app_token_with_state`), writes the generated page,
/// and persists the definition — in that order, so a failure partway
/// through (e.g. a duplicate name) never leaves an orphaned token or
/// half-written directory: every step before the final config save is
/// either read-only or writes into a location keyed by a freshly generated
/// id nothing else could already reference.
pub fn publish_impl(
    app: &AppHandle,
    state: &AppState,
    recipe_name: &str,
    template: LocalAppTemplate,
    param_bindings: std::collections::HashMap<String, String>,
) -> Result<LocalAppDefinition, String> {
    let app_data_dir = app
        .profile_data_dir()
        .map_err(|e| format!("Failed to resolve app data directory: {e}"))?;
    let workspace_root = crate::workspace::primary_root_canon(state).ok();
    let (recipe, _path) = crate::recipes::resolve_recipe_with_path(
        recipe_name,
        workspace_root.as_deref(),
        &app_data_dir,
    )?;

    let mut unknown: Vec<&String> = param_bindings
        .keys()
        .filter(|k| !recipe.params.contains_key(k.as_str()))
        .collect();
    if !unknown.is_empty() {
        unknown.sort();
        return Err(format!(
            "param_bindings references param(s) not declared in recipe '{}': {}",
            recipe.name,
            unknown
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    let id = uuid::Uuid::new_v4().to_string();
    let server_config = crate::server::load_config_impl(&crate::server::config_file_path(app)?)?;
    let (token, token_entry) = crate::server::create_local_app_token_with_state(
        state,
        &crate::server::config_file_path(app)?,
        &format!("Local App: {}", recipe.name),
        &id,
    )?;

    let definition = LocalAppDefinition {
        id: id.clone(),
        name: recipe.name.clone(),
        recipe_name: recipe.name.clone(),
        template,
        param_bindings,
        scoped_token_id: token_entry.id.clone(),
        created_at: now_ms(),
        enabled: true,
    };

    let html = render_page_html(&definition, &recipe, &token, server_config.port);
    let dir = app_dir(&app_data_dir, &id);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("Failed to create Local App directory: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("Failed to protect Local App directory: {e}"))?;
    }
    let index_path = dir.join("index.html");
    let tmp = index_path.with_extension("html.tmp");
    // The generated page embeds the run's plaintext bearer token (see this
    // module's doc comment) — the only place a token's plaintext is ever
    // written to disk, so it's protected the same way every other
    // secret-bearing file in this codebase is (0o600, owner-only).
    std::fs::write(&tmp, &html).map_err(|e| format!("Failed to write Local App page: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("Failed to protect Local App page: {e}"))?;
    }
    std::fs::rename(&tmp, &index_path)
        .map_err(|e| format!("Failed to finalize Local App page: {e}"))?;

    let config_path = config_file_path(app)?;
    {
        let _guard = state
            .local_apps_config_lock
            .lock()
            .map_err(|_| "Local Apps config lock poisoned".to_string())?;
        let mut config = load_config_impl(&config_path)?;
        config.apps.push(definition.clone());
        save_config_impl(&config_path, &config)?;
    }

    Ok(definition)
}

pub fn list_impl(app: &AppHandle) -> Result<Vec<LocalAppDefinition>, String> {
    Ok(load_config_impl(&config_file_path(app)?)?.apps)
}

/// Removes the definition, revokes its scoped token (immediate — see
/// `server::revoke_token_with_state_impl`'s doc comment), and best-effort
/// deletes its served directory. The directory removal is not allowed to
/// fail the whole operation: once the token is revoked and the definition
/// is gone from `local_apps.json`, the app is unpublished from every
/// caller's perspective even if a locked file leaves stray bytes on disk.
pub fn unpublish_impl(app: &AppHandle, state: &AppState, id: &str) -> Result<(), String> {
    let config_path = config_file_path(app)?;
    let removed = {
        let _guard = state
            .local_apps_config_lock
            .lock()
            .map_err(|_| "Local Apps config lock poisoned".to_string())?;
        let mut config = load_config_impl(&config_path)?;
        let index = config
            .apps
            .iter()
            .position(|a| a.id == id)
            .ok_or_else(|| format!("Unknown Local App '{id}'"))?;
        let removed = config.apps.remove(index);
        save_config_impl(&config_path, &config)?;
        removed
    };

    let _ = crate::server::revoke_token_with_state_impl(
        state,
        &crate::server::config_file_path(app)?,
        &removed.scoped_token_id,
    );

    if is_valid_app_id(id) {
        let app_data_dir = app
            .profile_data_dir()
            .map_err(|e| format!("Failed to resolve app data directory: {e}"))?;
        let _ = std::fs::remove_dir_all(app_dir(&app_data_dir, id));
    }

    Ok(())
}

/// Returns the local URL a published app is served at — the server must
/// actually be running (or subsequently started) on this port for the link
/// to resolve; this just formats the address, same "compute the address,
/// don't verify liveness" stance `ApiServerPanel`'s own base-URL display
/// takes.
pub fn open_impl(app: &AppHandle, id: &str) -> Result<String, String> {
    let config = load_config_impl(&config_file_path(app)?)?;
    let def = config
        .apps
        .iter()
        .find(|a| a.id == id)
        .ok_or_else(|| format!("Unknown Local App '{id}'"))?;
    let server_config = crate::server::load_config_impl(&crate::server::config_file_path(app)?)?;
    Ok(format!(
        "http://127.0.0.1:{}/local-apps/{}",
        server_config.port, def.id
    ))
}

// ---------------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn local_apps_publish(
    app: AppHandle,
    window: tauri::Window,
    state: tauri::State<'_, AppState>,
    recipe_name: String,
    template: LocalAppTemplate,
    param_bindings: std::collections::HashMap<String, String>,
) -> Result<LocalAppDefinition, String> {
    let definition = publish_impl(&app, state.inner(), &recipe_name, template, param_bindings)?;
    let _ = app.emit(LOCAL_APPS_CHANGED_EVENT, window.label());
    Ok(definition)
}

#[tauri::command]
pub fn local_apps_list(app: AppHandle) -> Result<Vec<LocalAppDefinition>, String> {
    list_impl(&app)
}

#[tauri::command]
pub fn local_apps_unpublish(
    app: AppHandle,
    window: tauri::Window,
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    unpublish_impl(&app, state.inner(), &id)?;
    let _ = app.emit(LOCAL_APPS_CHANGED_EVENT, window.label());
    Ok(())
}

#[tauri::command]
pub fn local_apps_open(app: AppHandle, id: String) -> Result<String, String> {
    open_impl(&app, &id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_recipe() -> Recipe {
        crate::recipes::parse_recipe(
            "version: 1\nname: nightly-audit\npermission_mode: acceptEdits\nprompt: \"Audit {{target}}\"\ntarget:\n  provider: openrouter\n  model: anthropic/claude-sonnet\nparams:\n  target: package.json\n",
            "yml",
        )
        .unwrap()
    }

    #[test]
    fn app_id_validation_rejects_traversal_and_empty_and_overlong_ids() {
        assert!(is_valid_app_id("a1b2c3d4-e5f6-47a8-9bcd-1234567890ab"));
        assert!(!is_valid_app_id(""));
        assert!(!is_valid_app_id("../../etc"));
        assert!(!is_valid_app_id("has/slash"));
        assert!(!is_valid_app_id(&"a".repeat(65)));
    }

    #[test]
    fn read_static_file_rejects_path_traversal_outside_the_app_directory() {
        let tmp = std::env::temp_dir().join(format!("lmk-local-apps-test-{}", uuid::Uuid::new_v4()));
        let app_id = "11111111-1111-4111-8111-111111111111";
        let app_dir_path = tmp.join(APPS_DIR).join(app_id);
        std::fs::create_dir_all(&app_dir_path).unwrap();
        std::fs::write(app_dir_path.join("index.html"), "<html>ok</html>").unwrap();
        // A sibling secret file outside the app's own directory.
        std::fs::write(tmp.join("secret.txt"), "top secret").unwrap();

        let ok = read_static_file(&tmp, app_id, "");
        assert_eq!(ok.unwrap(), b"<html>ok</html>".to_vec());

        let traversal = read_static_file(&tmp, app_id, "../secret.txt");
        assert!(traversal.is_err(), "traversal must be rejected, not read the sibling file");

        let bad_id = read_static_file(&tmp, "../../etc", "passwd");
        assert!(bad_id.is_err());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn render_page_html_embeds_every_declared_param_and_the_run_url_and_token() {
        let recipe = sample_recipe();
        let def = LocalAppDefinition {
            id: "app-1".to_string(),
            name: "nightly-audit".to_string(),
            recipe_name: "nightly-audit".to_string(),
            template: LocalAppTemplate::Form,
            param_bindings: std::collections::HashMap::from([(
                "target".to_string(),
                "Target file".to_string(),
            )]),
            scoped_token_id: "tok-1".to_string(),
            created_at: 1,
            enabled: true,
        };
        let html = render_page_html(&def, &recipe, "lmk-plaintext-token", 1234);
        assert!(html.contains("Target file"));
        assert!(html.contains("name=\"target\""));
        assert!(html.contains("http://127.0.0.1:1234/v1/local-apps/app-1/run"));
        assert!(html.contains("lmk-plaintext-token"));
    }

    #[test]
    fn render_page_html_html_escapes_param_names_and_default_values_instead_of_json_encoding_them() {
        let mut recipe = sample_recipe();
        recipe.params.clear();
        recipe.params.insert(
            "a\" onmouseover=\"alert(1)".to_string(),
            Some("Hello World".to_string()),
        );
        recipe.params.insert(
            "quoted".to_string(),
            Some("x\" onmouseover=\"alert(1)".to_string()),
        );
        let def = LocalAppDefinition {
            id: "app-1".to_string(),
            name: "nightly-audit".to_string(),
            recipe_name: "nightly-audit".to_string(),
            template: LocalAppTemplate::Form,
            param_bindings: std::collections::HashMap::new(),
            scoped_token_id: "tok-1".to_string(),
            created_at: 1,
            enabled: true,
        };
        let html = render_page_html(&def, &recipe, "lmk-plaintext-token", 1234);

        // A default value containing an ordinary space must survive intact,
        // fully quoted — not be truncated into an unquoted attribute.
        assert!(html.contains("value=\"Hello World\""));
        // A param name/default value containing a raw quote must be entity-
        // escaped, never allowed to close the attribute or inject a new one.
        assert!(!html.contains("onmouseover=\"alert(1)\""));
        assert!(html.contains("name=\"a&quot; onmouseover=&quot;alert(1)\""));
        assert!(html.contains("value=\"x&quot; onmouseover=&quot;alert(1)\""));
    }

    #[test]
    fn config_round_trips_through_save_and_load() {
        let tmp = std::env::temp_dir().join(format!("lmk-local-apps-config-{}", uuid::Uuid::new_v4()));
        let mut config = LocalAppsConfig::default();
        config.apps.push(LocalAppDefinition {
            id: "app-1".to_string(),
            name: "n".to_string(),
            recipe_name: "n".to_string(),
            template: LocalAppTemplate::Form,
            param_bindings: std::collections::HashMap::new(),
            scoped_token_id: "tok-1".to_string(),
            created_at: 1,
            enabled: true,
        });
        save_config_impl(&tmp, &config).unwrap();
        let loaded = load_config_impl(&tmp).unwrap();
        assert_eq!(loaded.apps.len(), 1);
        assert_eq!(loaded.apps[0].id, "app-1");
        assert!(!tmp.with_extension("json.tmp").exists());
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn load_config_defaults_when_file_is_missing() {
        let tmp = std::env::temp_dir().join(format!("lmk-local-apps-missing-{}", uuid::Uuid::new_v4()));
        let config = load_config_impl(&tmp).unwrap();
        assert!(config.apps.is_empty());
    }
}
