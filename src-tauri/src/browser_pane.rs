//! Native in-app browser pane (Claude-Desktop-style tabbed browser).
//!
//! Each tab is a real child webview (`tauri` `unstable` multiwebview API)
//! added on top of the main app webview and clipped to the bounds of the
//! pane's content area, which the frontend reports via
//! `browser_pane_set_bounds`. The frontend owns tab order/selection UI;
//! this module owns webview lifecycle, geometry, and page metadata events.
//!
//! Security posture: tab webviews are plain `WebviewBuilder`s with no
//! capabilities attached, so remote pages get no Tauri IPC surface. Only
//! `http:`/`https:`/`about:` URLs are accepted, `window.open` requests are
//! denied and surfaced to the frontend as a "new tab" event instead.

use std::collections::HashMap;
use std::sync::Mutex;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tauri::webview::{NewWindowResponse, PageLoadEvent, WebviewBuilder};
use tauri::{
    AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, Runtime, State, Url, Webview,
    WebviewUrl, Window,
};

/// Label prefix for every webview this module creates. Doubles as the guard
/// that keeps the commands from ever touching the main app webview.
const LABEL_PREFIX: &str = "browser-pane-";

/// Event carrying per-tab page metadata updates to the frontend.
const TAB_EVENT: &str = "browser-pane://tab";
/// Event asking the frontend to open a `window.open`/`target=_blank` URL as
/// a new tab (the popup itself is denied).
const NEW_WINDOW_EVENT: &str = "browser-pane://new-window";

/// Favicon fetches are capped so a hostile server can't feed us gigabytes.
const MAX_FAVICON_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PaneBounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Default)]
struct PaneInner {
    next_id: u64,
    /// Labels of live tab webviews, in creation order.
    tabs: Vec<String>,
    /// Label of the tab currently shown (frontend keeps the same notion).
    active: Option<String>,
    /// Last content-area rect reported by the frontend, in logical px.
    bounds: Option<PaneBounds>,
    /// Whether the pane region is currently visible (pane open, no overlay).
    visible: bool,
    /// host -> data URL (or None for a confirmed miss), so tab switches and
    /// re-navigations don't refetch.
    favicons: HashMap<String, Option<String>>,
}

#[derive(Default)]
pub struct BrowserPaneState {
    inner: Mutex<PaneInner>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct TabEventPayload {
    label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    loading: Option<bool>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct NewWindowPayload {
    url: String,
}

fn parse_pane_url(raw: &str) -> Result<Url, String> {
    let url = Url::parse(raw.trim()).map_err(|err| format!("invalid URL: {err}"))?;
    match url.scheme() {
        "http" | "https" | "about" => Ok(url),
        other => Err(format!("unsupported URL scheme: {other}:")),
    }
}

fn require_pane_label(label: &str) -> Result<(), String> {
    if label.starts_with(LABEL_PREFIX) {
        Ok(())
    } else {
        Err("not a browser pane webview".into())
    }
}

fn main_window<R: Runtime>(app: &AppHandle<R>) -> Result<Window<R>, String> {
    app.get_window("main")
        .ok_or_else(|| "main window not found".to_string())
}

fn pane_webviews<R: Runtime>(window: &Window<R>) -> Vec<Webview<R>> {
    window
        .webviews()
        .into_iter()
        .filter(|webview| webview.label().starts_with(LABEL_PREFIX))
        .collect()
}

fn find_pane_webview<R: Runtime>(app: &AppHandle<R>, label: &str) -> Result<Webview<R>, String> {
    require_pane_label(label)?;
    let window = main_window(app)?;
    pane_webviews(&window)
        .into_iter()
        .find(|webview| webview.label() == label)
        .ok_or_else(|| format!("browser tab {label} not found"))
}

fn apply_bounds<R: Runtime>(webview: &Webview<R>, bounds: PaneBounds) {
    let _ = webview.set_position(LogicalPosition::new(bounds.x, bounds.y));
    let _ = webview.set_size(LogicalSize::new(bounds.width.max(1.0), bounds.height.max(1.0)));
}

fn emit_tab_event<R: Runtime>(app: &AppHandle<R>, payload: TabEventPayload) {
    let _ = app.emit(TAB_EVENT, payload);
}

/// Create a new tab webview at `url`, make it the active (visible) tab and
/// hide the previously active one. Returns the new tab's label.
#[tauri::command]
pub async fn browser_pane_open_tab(
    app: AppHandle,
    state: State<'_, BrowserPaneState>,
    url: String,
) -> Result<String, String> {
    let parsed = parse_pane_url(&url)?;
    let window = main_window(&app)?;

    let (label, bounds, previous) = {
        let mut inner = state.inner.lock().map_err(|_| "browser pane state poisoned")?;
        inner.next_id += 1;
        let label = format!("{LABEL_PREFIX}{}", inner.next_id);
        (label, inner.bounds.unwrap_or_default(), inner.active.clone())
    };

    let nav_app = app.clone();
    let nav_label = label.clone();
    let load_app = app.clone();
    let load_label = label.clone();
    let title_app = app.clone();
    let title_label = label.clone();
    let popup_app = app.clone();

    let builder = WebviewBuilder::new(&label, WebviewUrl::External(parsed))
        .on_navigation(move |url| {
            let allowed = matches!(url.scheme(), "http" | "https" | "about");
            if allowed {
                emit_tab_event(
                    &nav_app,
                    TabEventPayload {
                        label: nav_label.clone(),
                        url: Some(url.to_string()),
                        title: None,
                        loading: None,
                    },
                );
            }
            allowed
        })
        .on_page_load(move |_, payload| {
            emit_tab_event(
                &load_app,
                TabEventPayload {
                    label: load_label.clone(),
                    url: Some(payload.url().to_string()),
                    title: None,
                    loading: Some(matches!(payload.event(), PageLoadEvent::Started)),
                },
            );
        })
        .on_document_title_changed(move |_, title| {
            emit_tab_event(
                &title_app,
                TabEventPayload {
                    label: title_label.clone(),
                    url: None,
                    title: Some(title),
                    loading: None,
                },
            );
        })
        .on_new_window(move |url, _| {
            if matches!(url.scheme(), "http" | "https") {
                let _ = popup_app.emit(NEW_WINDOW_EVENT, NewWindowPayload { url: url.to_string() });
            }
            NewWindowResponse::Deny
        });

    let webview = window
        .add_child(
            builder,
            LogicalPosition::new(bounds.x, bounds.y),
            LogicalSize::new(bounds.width.max(1.0), bounds.height.max(1.0)),
        )
        .map_err(|err| format!("failed to create browser tab: {err}"))?;
    let _ = webview.set_focus();

    if let Some(previous) = previous {
        if let Ok(prev_webview) = find_pane_webview(&app, &previous) {
            let _ = prev_webview.hide();
        }
    }

    {
        let mut inner = state.inner.lock().map_err(|_| "browser pane state poisoned")?;
        inner.tabs.push(label.clone());
        inner.active = Some(label.clone());
        inner.visible = true;
    }

    Ok(label)
}

#[tauri::command]
pub async fn browser_pane_close_tab(
    app: AppHandle,
    state: State<'_, BrowserPaneState>,
    label: String,
) -> Result<(), String> {
    let webview = find_pane_webview(&app, &label)?;
    webview
        .close()
        .map_err(|err| format!("failed to close browser tab: {err}"))?;
    let mut inner = state.inner.lock().map_err(|_| "browser pane state poisoned")?;
    inner.tabs.retain(|tab| tab != &label);
    if inner.active.as_deref() == Some(label.as_str()) {
        inner.active = None;
    }
    Ok(())
}

/// Show `label`, hide every other tab webview. Also re-applies the last
/// known bounds so a tab that was created/hidden while the pane resized
/// comes back at the right rect.
#[tauri::command]
pub async fn browser_pane_select_tab(
    app: AppHandle,
    state: State<'_, BrowserPaneState>,
    label: String,
) -> Result<(), String> {
    require_pane_label(&label)?;
    let window = main_window(&app)?;
    let (bounds, visible) = {
        let mut inner = state.inner.lock().map_err(|_| "browser pane state poisoned")?;
        inner.active = Some(label.clone());
        (inner.bounds.unwrap_or_default(), inner.visible)
    };
    let mut found = false;
    for webview in pane_webviews(&window) {
        if webview.label() == label {
            found = true;
            apply_bounds(&webview, bounds);
            if visible {
                let _ = webview.show();
                let _ = webview.set_focus();
            }
        } else {
            let _ = webview.hide();
        }
    }
    if found {
        Ok(())
    } else {
        Err(format!("browser tab {label} not found"))
    }
}

/// Frontend reports the pane content-area rect (logical px, window-relative).
#[tauri::command]
pub async fn browser_pane_set_bounds(
    app: AppHandle,
    state: State<'_, BrowserPaneState>,
    bounds: PaneBounds,
) -> Result<(), String> {
    {
        let mut inner = state.inner.lock().map_err(|_| "browser pane state poisoned")?;
        inner.bounds = Some(bounds);
    }
    let window = main_window(&app)?;
    for webview in pane_webviews(&window) {
        apply_bounds(&webview, bounds);
    }
    Ok(())
}

/// Hide all tab webviews (pane closed / a modal is over it) or re-show the
/// active one. Native webviews always paint above the app's DOM, so the
/// frontend must call this whenever anything should render over the pane.
#[tauri::command]
pub async fn browser_pane_set_visible(
    app: AppHandle,
    state: State<'_, BrowserPaneState>,
    visible: bool,
) -> Result<(), String> {
    let active = {
        let mut inner = state.inner.lock().map_err(|_| "browser pane state poisoned")?;
        inner.visible = visible;
        inner.active.clone()
    };
    let window = main_window(&app)?;
    for webview in pane_webviews(&window) {
        if visible && Some(webview.label()) == active.as_deref() {
            let _ = webview.show();
        } else {
            let _ = webview.hide();
        }
    }
    Ok(())
}

#[tauri::command]
pub async fn browser_pane_navigate(app: AppHandle, label: String, url: String) -> Result<(), String> {
    let parsed = parse_pane_url(&url)?;
    let webview = find_pane_webview(&app, &label)?;
    webview
        .navigate(parsed)
        .map_err(|err| format!("navigation failed: {err}"))
}

#[tauri::command]
pub async fn browser_pane_go_back(app: AppHandle, label: String) -> Result<(), String> {
    let webview = find_pane_webview(&app, &label)?;
    webview
        .eval("history.back()")
        .map_err(|err| format!("history.back failed: {err}"))
}

#[tauri::command]
pub async fn browser_pane_go_forward(app: AppHandle, label: String) -> Result<(), String> {
    let webview = find_pane_webview(&app, &label)?;
    webview
        .eval("history.forward()")
        .map_err(|err| format!("history.forward failed: {err}"))
}

#[tauri::command]
pub async fn browser_pane_reload(app: AppHandle, label: String) -> Result<(), String> {
    let webview = find_pane_webview(&app, &label)?;
    webview
        .reload()
        .map_err(|err| format!("reload failed: {err}"))
}

/// Fetch a small favicon for the page's host and return it as a `data:` URL
/// (the app webview's CSP allows `data:` images but no remote hosts).
/// Cached per host; `Ok(None)` is a cached "this host has no favicon".
#[tauri::command]
pub async fn browser_pane_favicon(
    state: State<'_, BrowserPaneState>,
    page_url: String,
) -> Result<Option<String>, String> {
    let parsed = parse_pane_url(&page_url)?;
    let Some(host) = parsed.host_str().map(str::to_string) else {
        return Ok(None);
    };
    {
        let inner = state.inner.lock().map_err(|_| "browser pane state poisoned")?;
        if let Some(cached) = inner.favicons.get(&host) {
            return Ok(cached.clone());
        }
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .map_err(|err| err.to_string())?;
    let candidates = [
        format!("https://{host}/favicon.ico"),
        // Public favicon service fallback for hosts that keep the icon
        // somewhere the /favicon.ico convention doesn't cover.
        format!("https://icons.duckduckgo.com/ip3/{host}.ico"),
    ];

    let mut favicon: Option<String> = None;
    for candidate in candidates {
        let Ok(response) = client.get(&candidate).send().await else {
            continue;
        };
        if !response.status().is_success() {
            continue;
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("image/x-icon")
            .split(';')
            .next()
            .unwrap_or("image/x-icon")
            .trim()
            .to_string();
        if content_type.starts_with("text/") {
            // Typically an HTML 404/consent page served with status 200.
            continue;
        }
        let Ok(bytes) = response.bytes().await else {
            continue;
        };
        if bytes.is_empty() || bytes.len() > MAX_FAVICON_BYTES {
            continue;
        }
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        favicon = Some(format!("data:{content_type};base64,{encoded}"));
        break;
    }

    let mut inner = state.inner.lock().map_err(|_| "browser pane state poisoned")?;
    inner.favicons.insert(host, favicon.clone());
    Ok(favicon)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pane_url_accepts_http_https_about() {
        assert!(parse_pane_url("https://example.com").is_ok());
        assert!(parse_pane_url("http://127.0.0.1:1420/").is_ok());
        assert!(parse_pane_url("about:blank").is_ok());
    }

    #[test]
    fn parse_pane_url_rejects_other_schemes() {
        assert!(parse_pane_url("file:///etc/passwd").is_err());
        assert!(parse_pane_url("javascript:alert(1)").is_err());
        assert!(parse_pane_url("data:text/html,<b>x</b>").is_err());
        assert!(parse_pane_url("not a url").is_err());
    }

    #[test]
    fn require_pane_label_guards_prefix() {
        assert!(require_pane_label("browser-pane-3").is_ok());
        assert!(require_pane_label("main").is_err());
    }
}
