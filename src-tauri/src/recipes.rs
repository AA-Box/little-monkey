//! YAML/JSON "recipe" files — a saved agent task (prompt template + model
//! target + permission policy + declared parameters) runnable headlessly via
//! `monkey-cli task run` (CI-suitable, machine-readable output, deterministic
//! exit codes) or from the desktop app's Settings > Tasks recipe library.
//! Design doc: `docs/roadmap/p3-scheduled-automation.md`.
//!
//! `pub` (not `mod`, like `checkpoints`/`rules`/`memory` above) so
//! `monkey-cli`'s `task.rs` can call every function here directly — parsing,
//! validation, param substitution, and discovery are all `AppHandle`-free by
//! construction, following the same `*_impl` convention `checkpoints.rs`
//! establishes: only the thin `#[tauri::command]` wrappers at the bottom of
//! this file ever touch an `AppHandle`.
//!
//! Recipe discovery deliberately checks TWO locations, local shadowing
//! global by `name` (not filename): workspace-local
//! `.littlemonkey/recipes/*.{yml,yaml,json}` (checked into the repo,
//! shareable with a team) and the global `<app_data>/recipes/` directory
//! (the desktop app's Settings > Tasks panel writes here). `permission_mode`
//! is a required field with NO default — a lesson from Goose Recipes and
//! Cline's headless mode (see the design doc's "Competitor reference"):
//! nothing should run unattended without an explicit policy choice.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use regex::Regex;
use tauri::{Emitter, Manager};

/// Current (and, so far, only) recipe schema version.
pub const RECIPE_SCHEMA_VERSION: u32 = 1;

/// One saved recipe's model target — mirrors `monkey-cli`'s own `Target`
/// resolution (`chat.rs`), but kept independent of it: this is a shared-lib
/// type parsed from user-authored YAML, while `chat::Target` is a
/// `monkey-cli`-only binary type resolved against live provider/keychain
/// state. `monkey-cli`'s `task.rs` is what bridges the two at run time.
/// Exactly one of `provider` (+ `model`), `ollama`, or `local_url` must be
/// set — see [`RecipeTarget::validate`].
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct RecipeTarget {
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub ollama: Option<String>,
    #[serde(default)]
    pub local_url: Option<String>,
}

impl RecipeTarget {
    /// Enforces the design doc's XOR constraint: `provider: openrouter #
    /// XOR ollama: "qwen2.5:14b" XOR local_url: "http://127.0.0.1:8090"`.
    pub fn validate(&self) -> Result<(), String> {
        let set_count = [self.provider.is_some(), self.ollama.is_some(), self.local_url.is_some()]
            .iter()
            .filter(|set| **set)
            .count();
        if set_count == 0 {
            return Err("recipe target must set exactly one of provider, ollama, or local_url".to_string());
        }
        if set_count > 1 {
            return Err("recipe target must set exactly one of provider, ollama, or local_url — not more than one".to_string());
        }
        if self.provider.is_some() && self.model.is_none() {
            return Err("recipe target with 'provider' must also set 'model'".to_string());
        }
        Ok(())
    }
}

/// CLI-only output shaping — see `monkey-cli`'s `task run --json` (design
/// doc slice 1). Desktop-app runs (slice 2's "Run now") ignore this entirely
/// since they always render as an ordinary chat turn.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct RecipeOutput {
    #[serde(default)]
    pub json: bool,
}

/// A saved recipe, parsed from YAML or JSON (extension-sniffed — see
/// [`parse_recipe`]). `permission_mode` deliberately has NO `#[serde(default)]`:
/// omitting it from the recipe file is a hard parse error, not a silent
/// fallback to some default mode — see the module doc for why.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct Recipe {
    pub version: u32,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub target: RecipeTarget,
    #[serde(default)]
    pub workspace: Option<String>,
    pub permission_mode: String,
    #[serde(default)]
    pub system: Option<String>,
    pub prompt: String,
    /// Declared params: name -> optional default. A param with `None` has
    /// no default and MUST be supplied via `--param name=value` at run time
    /// (see [`resolve_param_values`]).
    #[serde(default)]
    pub params: HashMap<String, Option<String>>,
    #[serde(default)]
    pub max_iterations: Option<usize>,
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
    #[serde(default)]
    pub output: RecipeOutput,
}

fn is_valid_recipe_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Full validation beyond what `serde` already enforces (required fields
/// present, right types): schema version supported, name is a valid slug,
/// target's XOR constraint, and `permission_mode` is a real mode per
/// `permissions::VALID_MODES` — reusing that list directly so this can never
/// drift from what the permission gate itself accepts — MINUS `bypass`,
/// rejected separately below. A recipe can run fully unattended (croner-
/// scheduled by `scheduler.ts`, or via `monkey-cli task run` in CI), and
/// `bypass` short-circuits every tool prompt, `run_shell` included (see
/// `permissions::mode_short_circuit`'s `bypass` arm) — allowing it here would
/// let a scheduled/imported recipe execute arbitrary shell commands with no
/// human ever present to catch it, silently contradicting the "run_shell is
/// never auto-approved regardless of mode" invariant the rest of the app
/// holds to. Every other real mode still degrades safely unattended: it
/// prompts, gets no answer, and the run times out/fails instead of acting.
pub fn validate_recipe(recipe: &Recipe) -> Result<(), String> {
    if recipe.version != RECIPE_SCHEMA_VERSION {
        return Err(format!(
            "unsupported recipe version {} (expected {})",
            recipe.version, RECIPE_SCHEMA_VERSION
        ));
    }
    if !is_valid_recipe_name(&recipe.name) {
        return Err(format!("recipe name '{}' must match [a-z0-9-]+", recipe.name));
    }
    recipe.target.validate()?;
    if recipe.permission_mode == "bypass" {
        return Err(
            "recipe permission_mode 'bypass' is not allowed — recipes can run unattended, \
             and bypass auto-approves every tool (including run_shell) with nobody present \
             to catch it; pick a mode that still prompts or only auto-approves edits"
                .to_string(),
        );
    }
    if !crate::permissions::VALID_MODES.contains(&recipe.permission_mode.as_str()) {
        return Err(format!(
            "recipe permission_mode '{}' is invalid (expected one of {:?})",
            recipe.permission_mode,
            crate::permissions::VALID_MODES
        ));
    }
    if recipe.prompt.trim().is_empty() {
        return Err("recipe prompt must not be empty".to_string());
    }
    Ok(())
}

/// Parses `content` as YAML (via serde-saphyr) or JSON (via serde_json,
/// when `extension` is `"json"`, case-insensitive), then validates it —
/// callers never see an unvalidated `Recipe`.
pub fn parse_recipe(content: &str, extension: &str) -> Result<Recipe, String> {
    let recipe: Recipe = if extension.eq_ignore_ascii_case("json") {
        serde_json::from_str(content).map_err(|e| format!("Failed to parse recipe JSON: {e}"))?
    } else {
        serde_saphyr::from_str(content).map_err(|e| format!("Failed to parse recipe YAML: {e}"))?
    };
    validate_recipe(&recipe)?;
    Ok(recipe)
}

fn placeholder_regex() -> Regex {
    Regex::new(r"\{\{(\w+)\}\}").expect("static regex must compile")
}

/// Substitutes every `{{name}}` placeholder in `template` from `values`.
/// Every placeholder must resolve — an unsubstituted `{{name}}` left in the
/// output (no matching key in `values`) is a hard error, never sent to the
/// model as literal `{{...}}` text.
pub fn substitute_params(template: &str, values: &HashMap<String, String>) -> Result<String, String> {
    let re = placeholder_regex();
    let mut missing: Vec<String> = Vec::new();
    let substituted = re.replace_all(template, |caps: &regex::Captures| {
        let name = &caps[1];
        match values.get(name) {
            Some(v) => v.clone(),
            None => {
                missing.push(name.to_string());
                caps[0].to_string()
            }
        }
    });
    if !missing.is_empty() {
        missing.sort();
        missing.dedup();
        return Err(format!("unsubstituted parameter placeholder(s): {}", missing.join(", ")));
    }
    Ok(substituted.into_owned())
}

/// Resolves the final `name -> value` map for a recipe run: every key in
/// `overrides` (a `--param name=value` flag) must already be declared in
/// `recipe.params` — an unknown key is a hard error (typo protection, not a
/// silent no-op) — and every declared param either has an override, has its
/// own default, or is reported as missing (also a hard error, since a param
/// with no default and no override can't be substituted).
pub fn resolve_param_values(recipe: &Recipe, overrides: &HashMap<String, String>) -> Result<HashMap<String, String>, String> {
    let mut unknown: Vec<&str> = overrides
        .keys()
        .filter(|k| !recipe.params.contains_key(k.as_str()))
        .map(|k| k.as_str())
        .collect();
    if !unknown.is_empty() {
        unknown.sort();
        return Err(format!("unknown --param key(s) not declared in this recipe: {}", unknown.join(", ")));
    }

    let mut values = HashMap::new();
    let mut missing: Vec<&str> = Vec::new();
    for (name, default) in &recipe.params {
        if let Some(v) = overrides.get(name) {
            values.insert(name.clone(), v.clone());
        } else if let Some(d) = default {
            values.insert(name.clone(), d.clone());
        } else {
            missing.push(name.as_str());
        }
    }
    if !missing.is_empty() {
        missing.sort();
        return Err(format!("missing required --param value(s) (no default): {}", missing.join(", ")));
    }
    Ok(values)
}

/// A recipe's prompt/system, fully rendered (every `{{name}}` substituted) —
/// what `monkey-cli task run` and the GUI's `recipeRunner.ts` equivalent
/// actually feed into a turn.
#[derive(serde::Serialize)]
pub struct RenderedRecipe {
    pub prompt: String,
    pub system: Option<String>,
}

/// Resolves param values then substitutes them into `prompt`/`system` — the
/// one function both `task run` and the GUI's "Run now" call.
pub fn render_recipe(recipe: &Recipe, overrides: &HashMap<String, String>) -> Result<RenderedRecipe, String> {
    let values = resolve_param_values(recipe, overrides)?;
    let prompt = substitute_params(&recipe.prompt, &values)?;
    let system = recipe.system.as_deref().map(|s| substitute_params(s, &values)).transpose()?;
    Ok(RenderedRecipe { prompt, system })
}

const RECIPE_EXTENSIONS: &[&str] = &["yml", "yaml", "json"];

/// Where a discovered recipe file came from — `Workspace` shadows `Global`
/// when both declare the same `name` (see [`discover_recipes`]).
#[derive(serde::Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecipeSource {
    Workspace,
    Global,
}

/// One recipe file found on disk — `recipe`/`error` are mutually exclusive
/// (a parse/validation failure still surfaces the file with `error` set
/// rather than silently dropping it, so `recipes_list`/`task list` can show
/// "this recipe is broken" instead of just omitting it).
#[derive(serde::Serialize, Clone, Debug)]
pub struct DiscoveredRecipe {
    pub path: PathBuf,
    pub source: RecipeSource,
    pub recipe: Option<Recipe>,
    pub error: Option<String>,
}

fn scan_recipe_dir(dir: &Path, source: RecipeSource) -> Vec<DiscoveredRecipe> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        if !RECIPE_EXTENSIONS.iter().any(|allowed| allowed.eq_ignore_ascii_case(ext)) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        match parse_recipe(&content, ext) {
            Ok(recipe) => out.push(DiscoveredRecipe { path, source: source.clone(), recipe: Some(recipe), error: None }),
            Err(e) => out.push(DiscoveredRecipe { path, source: source.clone(), recipe: None, error: Some(e) }),
        }
    }
    out
}

/// Discovers every recipe visible right now: workspace-local
/// `.littlemonkey/recipes/` (skipped entirely when `workspace_root` is
/// `None` — no workspace open) plus the global `<app_data>/recipes/`
/// directory, with a workspace recipe shadowing a global one of the same
/// `name` (never both — the workspace copy wins, matching "local shadows
/// global" from the design doc).
pub fn discover_recipes(workspace_root: Option<&Path>, app_data_dir: &Path) -> Vec<DiscoveredRecipe> {
    let mut local = workspace_root
        .map(|root| scan_recipe_dir(&root.join(".littlemonkey").join("recipes"), RecipeSource::Workspace))
        .unwrap_or_default();
    let global = scan_recipe_dir(&app_data_dir.join("recipes"), RecipeSource::Global);

    let local_names: std::collections::HashSet<String> =
        local.iter().filter_map(|d| d.recipe.as_ref().map(|r| r.name.clone())).collect();

    local.extend(
        global
            .into_iter()
            .filter(|d| d.recipe.as_ref().map(|r| !local_names.contains(&r.name)).unwrap_or(true)),
    );
    local
}

/// Resolves `name_or_path`: a direct filesystem path to a recipe file if one
/// exists at that exact path, otherwise a bare recipe `name` looked up via
/// [`discover_recipes`]. Used by `recipes_read`; see [`resolve_recipe_with_path`]
/// for the variant `monkey-cli task run` needs (which also needs the file's
/// own directory, to resolve a recipe's `workspace: .` field against it).
pub fn resolve_recipe(name_or_path: &str, workspace_root: Option<&Path>, app_data_dir: &Path) -> Result<Recipe, String> {
    resolve_recipe_with_path(name_or_path, workspace_root, app_data_dir).map(|(recipe, _path)| recipe)
}

/// Same resolution as [`resolve_recipe`], but also returns the file path the
/// recipe was loaded from.
pub fn resolve_recipe_with_path(
    name_or_path: &str,
    workspace_root: Option<&Path>,
    app_data_dir: &Path,
) -> Result<(Recipe, PathBuf), String> {
    let direct_path = Path::new(name_or_path);
    if direct_path.is_file() {
        let ext = direct_path.extension().and_then(|e| e.to_str()).unwrap_or("yml");
        let content = std::fs::read_to_string(direct_path).map_err(|e| format!("Failed to read '{name_or_path}': {e}"))?;
        return Ok((parse_recipe(&content, ext)?, direct_path.to_path_buf()));
    }
    discover_recipes(workspace_root, app_data_dir)
        .into_iter()
        .filter_map(|d| d.recipe.map(|r| (r, d.path)))
        .find(|(r, _path)| r.name == name_or_path)
        .ok_or_else(|| {
            format!(
                "No recipe named '{name_or_path}' found (checked workspace .littlemonkey/recipes/ and the global recipes directory)"
            )
        })
}

fn validate_recipe_id(name: &str) -> Result<(), String> {
    if !is_valid_recipe_name(name) {
        return Err(format!("recipe name '{name}' must match [a-z0-9-]+"));
    }
    Ok(())
}

/// Saves `yaml_content` as `<app_data>/recipes/<name>.yml`, atomically
/// (temp file + rename, same pattern as `sessions.rs::save_to`) — always
/// into the GLOBAL directory: the desktop app's recipe library (design doc
/// slice 2) has no concept of "which workspace" a saved recipe belongs to,
/// unlike a hand-authored `.littlemonkey/recipes/` file committed to a repo.
pub fn save_recipe_impl(app_data_dir: &Path, name: &str, yaml_content: &str) -> Result<Recipe, String> {
    validate_recipe_id(name)?;
    let recipe = parse_recipe(yaml_content, "yml")?;
    if recipe.name != name {
        return Err(format!("recipe content's name '{}' does not match the target '{name}'", recipe.name));
    }
    let dir = app_data_dir.join("recipes");
    std::fs::create_dir_all(&dir).map_err(|e| format!("Failed to create recipes directory: {e}"))?;
    let path = dir.join(format!("{name}.yml"));
    let tmp = path.with_extension("yml.tmp");
    std::fs::write(&tmp, yaml_content).map_err(|e| format!("Failed to write recipe: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("Failed to finalize recipe: {e}"))?;
    Ok(recipe)
}

/// Deletes `<app_data>/recipes/<name>.yml` — a no-op success (not an error)
/// when it's already gone, same "delete is idempotent" convention as every
/// other per-item store in this codebase.
pub fn delete_recipe_impl(app_data_dir: &Path, name: &str) -> Result<(), String> {
    validate_recipe_id(name)?;
    let path = app_data_dir.join("recipes").join(format!("{name}.yml"));
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("Failed to delete recipe: {e}")),
    }
}

// ---------------------------------------------------------------------------
// Tauri commands — thin wrappers resolving `AppHandle`/`AppState` down to the
// plain paths every `*_impl`/free function above actually needs.
// ---------------------------------------------------------------------------

fn app_data_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    app.path().app_data_dir().map_err(|e| format!("Failed to resolve app data dir: {e}"))
}

#[tauri::command]
pub fn recipes_list(
    app: tauri::AppHandle,
    state: tauri::State<'_, crate::AppState>,
) -> Result<Vec<DiscoveredRecipe>, String> {
    let workspace_root = crate::workspace::primary_root_canon(state.inner()).ok();
    Ok(discover_recipes(workspace_root.as_deref(), &app_data_dir(&app)?))
}

#[tauri::command]
pub fn recipes_read(
    app: tauri::AppHandle,
    state: tauri::State<'_, crate::AppState>,
    name_or_path: String,
) -> Result<Recipe, String> {
    let workspace_root = crate::workspace::primary_root_canon(state.inner()).ok();
    resolve_recipe(&name_or_path, workspace_root.as_deref(), &app_data_dir(&app)?)
}

/// Reads a recipe's raw (unparsed) file content — the Settings > Tasks
/// editor's "Edit" action needs the original YAML text to edit, not the
/// parsed `Recipe` `recipes_read` returns. Resolution is otherwise
/// identical to `recipes_read`; `tool_read_file` can't be reused here since
/// it's sandboxed to workspace roots and a global recipe lives in the
/// app-data directory, outside all of them.
#[tauri::command]
pub fn recipes_read_raw(
    app: tauri::AppHandle,
    state: tauri::State<'_, crate::AppState>,
    name_or_path: String,
) -> Result<String, String> {
    let workspace_root = crate::workspace::primary_root_canon(state.inner()).ok();
    let (_recipe, path) = resolve_recipe_with_path(&name_or_path, workspace_root.as_deref(), &app_data_dir(&app)?)?;
    std::fs::read_to_string(&path).map_err(|e| format!("Failed to read '{}': {e}", path.display()))
}

/// Resolves `name_or_path` and renders its prompt/system with `overrides` —
/// the one place `{{param}}` substitution happens for the desktop app's "Run
/// now" (`recipeRunner.ts`), so there is exactly one implementation shared
/// with `monkey-cli task run`, not two independently maintained ones.
#[tauri::command]
pub fn recipes_render(
    app: tauri::AppHandle,
    state: tauri::State<'_, crate::AppState>,
    name_or_path: String,
    overrides: HashMap<String, String>,
) -> Result<RenderedRecipe, String> {
    let workspace_root = crate::workspace::primary_root_canon(state.inner()).ok();
    let recipe = resolve_recipe(&name_or_path, workspace_root.as_deref(), &app_data_dir(&app)?)?;
    render_recipe(&recipe, &overrides)
}

/// Emitted after a successful `recipes_save`/`recipes_delete`, with the
/// acting window's label as payload — same cross-window sync mechanism as
/// `sessions.rs`/`prompts.rs`: another open window re-lists on this instead
/// of polling, and ignores its own echo by comparing the payload to its own
/// label.
pub const RECIPES_CHANGED_EVENT: &str = "recipes://changed";

#[tauri::command]
pub fn recipes_save(app: tauri::AppHandle, window: tauri::Window, name: String, content: String) -> Result<Recipe, String> {
    let recipe = save_recipe_impl(&app_data_dir(&app)?, &name, &content)?;
    let _ = app.emit(RECIPES_CHANGED_EVENT, window.label());
    Ok(recipe)
}

#[tauri::command]
pub fn recipes_delete(app: tauri::AppHandle, window: tauri::Window, name: String) -> Result<(), String> {
    delete_recipe_impl(&app_data_dir(&app)?, &name)?;
    let _ = app.emit(RECIPES_CHANGED_EVENT, window.label());
    Ok(())
}

/// Validates recipe content without saving it — the editor's live-validate
/// affordance (design doc slice 2). Extension-sniffs the same way
/// [`parse_recipe`] does, defaulting to YAML since the editor is a plain
/// YAML textarea.
#[tauri::command]
pub fn recipes_validate(content: String, extension: Option<String>) -> Result<Recipe, String> {
    parse_recipe(&content, extension.as_deref().unwrap_or("yml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_target() -> RecipeTarget {
        RecipeTarget { provider: Some("openrouter".to_string()), model: Some("anthropic/claude-sonnet".to_string()), ollama: None, local_url: None }
    }

    fn valid_recipe() -> Recipe {
        Recipe {
            version: 1,
            name: "nightly-deps-audit".to_string(),
            description: Some("Audit dependencies".to_string()),
            target: valid_target(),
            workspace: None,
            permission_mode: "acceptEdits".to_string(),
            system: None,
            prompt: "Check {{manifest}} for outdated deps.".to_string(),
            params: HashMap::from([("manifest".to_string(), Some("package.json".to_string()))]),
            max_iterations: None,
            timeout_seconds: None,
            output: RecipeOutput::default(),
        }
    }

    // --- RecipeTarget::validate ---

    #[test]
    fn target_rejects_nothing_set() {
        let t = RecipeTarget::default();
        assert!(t.validate().is_err());
    }

    #[test]
    fn target_rejects_more_than_one_set() {
        let t = RecipeTarget { provider: Some("openrouter".to_string()), model: Some("x".to_string()), ollama: Some("qwen2.5:14b".to_string()), local_url: None };
        assert!(t.validate().is_err());
    }

    #[test]
    fn target_rejects_provider_without_model() {
        let t = RecipeTarget { provider: Some("openrouter".to_string()), model: None, ollama: None, local_url: None };
        let err = t.validate().unwrap_err();
        assert!(err.contains("model"));
    }

    #[test]
    fn target_accepts_ollama_alone() {
        let t = RecipeTarget { provider: None, model: None, ollama: Some("qwen2.5:14b".to_string()), local_url: None };
        assert!(t.validate().is_ok());
    }

    #[test]
    fn target_accepts_local_url_alone() {
        let t = RecipeTarget { provider: None, model: None, ollama: None, local_url: Some("http://127.0.0.1:8090".to_string()) };
        assert!(t.validate().is_ok());
    }

    /// Shared with `recipeStore.test.ts`'s canonical-fixture test — a single
    /// fixture read by both a Rust unit test and a vitest test, not two
    /// independently hand-typed literals, is what actually pins the
    /// TS<->Rust schema against drift (ROADMAP.md §3 item 6). Recipes are
    /// the schema most likely to be hand-edited by users (YAML files
    /// authored outside either language), which makes this the fixture
    /// pair with the most to protect. Exercises both `Option<T>` branches
    /// (`workspace`/`system`/`max_iterations` absent, `description`/
    /// `timeout_seconds` present) alongside `#[serde(default)]` leniency.
    const CANONICAL_RECIPE_JSON: &str = include_str!("../fixtures/recipe.canonical.json");

    #[test]
    fn recipe_deserializes_canonical_fixture() {
        let recipe: Recipe = serde_json::from_str(CANONICAL_RECIPE_JSON).unwrap();
        assert_eq!(recipe.version, 1);
        assert_eq!(recipe.name, "nightly-deps-audit");
        assert_eq!(recipe.description.as_deref(), Some("Audit dependencies for known vulnerabilities and file a report"));
        assert_eq!(recipe.target.provider.as_deref(), Some("openrouter"));
        assert_eq!(recipe.target.model.as_deref(), Some("anthropic/claude-sonnet"));
        assert_eq!(recipe.target.ollama, None);
        assert_eq!(recipe.target.local_url, None);
        assert_eq!(recipe.workspace, None);
        assert_eq!(recipe.permission_mode, "acceptEdits");
        assert_eq!(recipe.system, None);
        assert_eq!(recipe.prompt, "Check {{manifest}} for outdated or vulnerable dependencies and summarize findings.");
        assert_eq!(recipe.params.get("manifest"), Some(&Some("package.json".to_string())));
        assert_eq!(recipe.max_iterations, None);
        assert_eq!(recipe.timeout_seconds, Some(900));
        assert!(!recipe.output.json);
        // The fixture is a well-formed recipe, not just a well-formed shape.
        assert!(validate_recipe(&recipe).is_ok());
    }

    // --- validate_recipe ---

    #[test]
    fn validate_recipe_accepts_a_well_formed_recipe() {
        assert!(validate_recipe(&valid_recipe()).is_ok());
    }

    #[test]
    fn validate_recipe_rejects_unsupported_version() {
        let mut r = valid_recipe();
        r.version = 2;
        assert!(validate_recipe(&r).unwrap_err().contains("version"));
    }

    #[test]
    fn validate_recipe_rejects_a_bad_name() {
        for bad in ["Has Spaces", "UPPER", "trailing_underscore_", "has/slash"] {
            let mut r = valid_recipe();
            r.name = bad.to_string();
            assert!(validate_recipe(&r).is_err(), "expected '{bad}' to be rejected");
        }
    }

    #[test]
    fn validate_recipe_rejects_an_invalid_permission_mode() {
        let mut r = valid_recipe();
        r.permission_mode = "yolo".to_string();
        assert!(validate_recipe(&r).unwrap_err().contains("permission_mode"));
    }

    #[test]
    fn validate_recipe_accepts_every_real_permission_mode_except_bypass() {
        for mode in crate::permissions::VALID_MODES {
            let mut r = valid_recipe();
            r.permission_mode = mode.to_string();
            if *mode == "bypass" {
                assert!(validate_recipe(&r).is_err(), "expected 'bypass' to be rejected");
            } else {
                assert!(validate_recipe(&r).is_ok(), "expected '{mode}' to be accepted");
            }
        }
    }

    #[test]
    fn validate_recipe_rejects_bypass_permission_mode() {
        let mut r = valid_recipe();
        r.permission_mode = "bypass".to_string();
        let err = validate_recipe(&r).unwrap_err();
        assert!(err.contains("bypass"), "error should mention bypass: {err}");
        assert!(err.contains("unattended"), "error should explain why: {err}");
    }

    #[test]
    fn validate_recipe_rejects_an_empty_prompt() {
        let mut r = valid_recipe();
        r.prompt = "   ".to_string();
        assert!(validate_recipe(&r).is_err());
    }

    #[test]
    fn validate_recipe_rejects_a_bad_target() {
        let mut r = valid_recipe();
        r.target = RecipeTarget::default();
        assert!(validate_recipe(&r).is_err());
    }

    // --- parse_recipe: YAML + JSON, matching the design doc's exact shape ---

    const YAML_RECIPE: &str = r#"
version: 1
name: nightly-deps-audit
description: Audit dependencies and write a report
target:
  provider: openrouter
  model: anthropic/claude-sonnet
permission_mode: acceptEdits
prompt: |
  Check {{manifest}} for outdated deps and summarize risks.
params:
  manifest: package.json
"#;

    #[test]
    fn parse_recipe_reads_the_design_docs_yaml_shape() {
        let recipe = parse_recipe(YAML_RECIPE, "yml").expect("should parse");
        assert_eq!(recipe.name, "nightly-deps-audit");
        assert_eq!(recipe.target.provider.as_deref(), Some("openrouter"));
        assert_eq!(recipe.target.model.as_deref(), Some("anthropic/claude-sonnet"));
        assert_eq!(recipe.permission_mode, "acceptEdits");
        assert!(recipe.prompt.contains("{{manifest}}"));
        assert_eq!(recipe.params.get("manifest"), Some(&Some("package.json".to_string())));
    }

    #[test]
    fn parse_recipe_rejects_malformed_yaml() {
        assert!(parse_recipe("not: [valid: yaml", "yml").is_err());
    }

    #[test]
    fn parse_recipe_rejects_a_recipe_missing_permission_mode() {
        let no_mode = YAML_RECIPE.replace("permission_mode: acceptEdits\n", "");
        let err = parse_recipe(&no_mode, "yml").unwrap_err();
        // serde's own "missing field" error — the whole point of NOT giving
        // `permission_mode` a `#[serde(default)]`.
        assert!(err.to_lowercase().contains("permission_mode") || err.to_lowercase().contains("missing"));
    }

    #[test]
    fn parse_recipe_reads_json_when_extension_is_json() {
        let json = r#"{
            "version": 1,
            "name": "json-recipe",
            "target": {"ollama": "qwen2.5:14b"},
            "permission_mode": "manual",
            "prompt": "Do the thing."
        }"#;
        let recipe = parse_recipe(json, "json").expect("should parse");
        assert_eq!(recipe.name, "json-recipe");
        assert_eq!(recipe.target.ollama.as_deref(), Some("qwen2.5:14b"));
    }

    // --- substitute_params / resolve_param_values / render_recipe ---

    #[test]
    fn substitute_params_replaces_every_placeholder() {
        let values = HashMap::from([("name".to_string(), "world".to_string())]);
        assert_eq!(substitute_params("Hello {{name}}!", &values).unwrap(), "Hello world!");
    }

    #[test]
    fn substitute_params_errors_on_unresolved_placeholders_listing_each_one() {
        let err = substitute_params("{{a}} and {{b}}", &HashMap::new()).unwrap_err();
        assert!(err.contains('a'));
        assert!(err.contains('b'));
    }

    #[test]
    fn resolve_param_values_rejects_an_unknown_override_key() {
        let recipe = valid_recipe();
        let overrides = HashMap::from([("typo_key".to_string(), "x".to_string())]);
        let err = resolve_param_values(&recipe, &overrides).unwrap_err();
        assert!(err.contains("typo_key"));
    }

    #[test]
    fn resolve_param_values_uses_the_override_over_the_default() {
        let recipe = valid_recipe();
        let overrides = HashMap::from([("manifest".to_string(), "pyproject.toml".to_string())]);
        let values = resolve_param_values(&recipe, &overrides).unwrap();
        assert_eq!(values.get("manifest"), Some(&"pyproject.toml".to_string()));
    }

    #[test]
    fn resolve_param_values_uses_the_default_when_no_override_given() {
        let recipe = valid_recipe();
        let values = resolve_param_values(&recipe, &HashMap::new()).unwrap();
        assert_eq!(values.get("manifest"), Some(&"package.json".to_string()));
    }

    #[test]
    fn resolve_param_values_errors_when_a_no_default_param_has_no_override() {
        let mut recipe = valid_recipe();
        recipe.params.insert("required_param".to_string(), None);
        let err = resolve_param_values(&recipe, &HashMap::new()).unwrap_err();
        assert!(err.contains("required_param"));
    }

    #[test]
    fn render_recipe_substitutes_both_prompt_and_system() {
        let mut recipe = valid_recipe();
        recipe.system = Some("You are auditing {{manifest}}.".to_string());
        let rendered = render_recipe(&recipe, &HashMap::new()).unwrap();
        assert!(rendered.prompt.contains("package.json"));
        assert_eq!(rendered.system.as_deref(), Some("You are auditing package.json."));
    }

    // --- discovery / resolution / save / delete ---

    fn temp_dir(label: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("little_monkey_recipes_test_{label}_{}_{n}_{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_recipe_file(dir: &Path, filename: &str, name: &str) {
        std::fs::create_dir_all(dir).unwrap();
        let content = format!(
            "version: 1\nname: {name}\ntarget:\n  ollama: qwen2.5:14b\npermission_mode: manual\nprompt: do the thing\n"
        );
        std::fs::write(dir.join(filename), content).unwrap();
    }

    #[test]
    fn discover_recipes_finds_both_workspace_and_global_recipes() {
        let workspace = temp_dir("ws");
        let app_data = temp_dir("app");
        write_recipe_file(&workspace.join(".littlemonkey").join("recipes"), "local.yml", "local-recipe");
        write_recipe_file(&app_data.join("recipes"), "global.yml", "global-recipe");

        let found = discover_recipes(Some(&workspace), &app_data);
        let names: Vec<&str> = found.iter().filter_map(|d| d.recipe.as_ref().map(|r| r.name.as_str())).collect();
        assert!(names.contains(&"local-recipe"));
        assert!(names.contains(&"global-recipe"));
    }

    #[test]
    fn discover_recipes_lets_a_workspace_recipe_shadow_a_global_one_with_the_same_name() {
        let workspace = temp_dir("ws-shadow");
        let app_data = temp_dir("app-shadow");
        write_recipe_file(&workspace.join(".littlemonkey").join("recipes"), "r.yml", "shared-name");
        write_recipe_file(&app_data.join("recipes"), "r.yml", "shared-name");

        let found = discover_recipes(Some(&workspace), &app_data);
        let matches: Vec<&DiscoveredRecipe> = found.iter().filter(|d| d.recipe.as_ref().map(|r| r.name == "shared-name").unwrap_or(false)).collect();
        assert_eq!(matches.len(), 1, "the global copy must be shadowed, not listed twice");
        assert_eq!(matches[0].source, RecipeSource::Workspace);
    }

    #[test]
    fn discover_recipes_tolerates_no_workspace_open() {
        let app_data = temp_dir("app-no-ws");
        write_recipe_file(&app_data.join("recipes"), "g.yml", "global-only");
        let found = discover_recipes(None, &app_data);
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn discover_recipes_surfaces_a_malformed_file_with_an_error_instead_of_dropping_it() {
        let app_data = temp_dir("app-malformed");
        let dir = app_data.join("recipes");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("broken.yml"), "not: [valid").unwrap();

        let found = discover_recipes(None, &app_data);
        assert_eq!(found.len(), 1);
        assert!(found[0].recipe.is_none());
        assert!(found[0].error.is_some());
    }

    #[test]
    fn resolve_recipe_finds_a_recipe_by_bare_name() {
        let app_data = temp_dir("app-resolve-name");
        write_recipe_file(&app_data.join("recipes"), "g.yml", "findable");
        let recipe = resolve_recipe("findable", None, &app_data).unwrap();
        assert_eq!(recipe.name, "findable");
    }

    #[test]
    fn resolve_recipe_finds_a_recipe_by_direct_path() {
        let app_data = temp_dir("app-resolve-path");
        let dir = app_data.join("somewhere-else");
        write_recipe_file(&dir, "custom.yml", "path-recipe");
        let recipe = resolve_recipe(dir.join("custom.yml").to_str().unwrap(), None, &app_data).unwrap();
        assert_eq!(recipe.name, "path-recipe");
    }

    #[test]
    fn resolve_recipe_errors_with_a_clear_message_when_nothing_matches() {
        let app_data = temp_dir("app-resolve-missing");
        let err = resolve_recipe("does-not-exist", None, &app_data).unwrap_err();
        assert!(err.contains("does-not-exist"));
    }

    #[test]
    fn save_then_read_back_roundtrips_and_writes_atomically() {
        let app_data = temp_dir("app-save");
        let yaml = "version: 1\nname: saved-recipe\ntarget:\n  ollama: qwen2.5:14b\npermission_mode: manual\nprompt: do it\n";
        let saved = save_recipe_impl(&app_data, "saved-recipe", yaml).unwrap();
        assert_eq!(saved.name, "saved-recipe");
        assert!(!app_data.join("recipes").join("saved-recipe.yml.tmp").exists());

        let reread = resolve_recipe("saved-recipe", None, &app_data).unwrap();
        assert_eq!(reread.name, "saved-recipe");
    }

    #[test]
    fn save_recipe_rejects_a_name_content_mismatch() {
        let app_data = temp_dir("app-save-mismatch");
        let yaml = "version: 1\nname: actual-name\ntarget:\n  ollama: q\npermission_mode: manual\nprompt: x\n";
        let err = save_recipe_impl(&app_data, "different-name", yaml).unwrap_err();
        assert!(err.contains("does not match"));
    }

    #[test]
    fn delete_recipe_removes_the_file_and_is_idempotent() {
        let app_data = temp_dir("app-delete");
        let yaml = "version: 1\nname: to-delete\ntarget:\n  ollama: q\npermission_mode: manual\nprompt: x\n";
        save_recipe_impl(&app_data, "to-delete", yaml).unwrap();
        assert!(app_data.join("recipes").join("to-delete.yml").exists());

        delete_recipe_impl(&app_data, "to-delete").unwrap();
        assert!(!app_data.join("recipes").join("to-delete.yml").exists());

        // Deleting again must not error.
        delete_recipe_impl(&app_data, "to-delete").unwrap();
    }

    #[test]
    fn recipe_id_validation_rejects_path_traversal_style_names() {
        let app_data = temp_dir("app-traversal");
        assert!(save_recipe_impl(&app_data, "../evil", "version: 1\nname: x\ntarget:\n  ollama: q\npermission_mode: manual\nprompt: x\n").is_err());
        assert!(delete_recipe_impl(&app_data, "../evil").is_err());
    }
}
