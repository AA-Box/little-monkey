import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { ErrorBoundary } from "./components/ErrorBoundary";
import "./index.css";
import { applyAppearance, subscribeToSystemTheme } from "./lib/theme";
import { useSettingsStore } from "./store/settingsStore";
import { hydrateSessions, useSessionStore } from "./store/sessionStore";
import { hydratePrompts } from "./store/promptStore";
import { CompanionOverlay } from "./components/Companion";

applyAppearance(useSettingsStore.getState());
subscribeToSystemTheme(() => {
  const settings = useSettingsStore.getState();
  if (settings.themePreference === "system") applyAppearance(settings);
});

// Sessions (and the prompt/persona library, same file-based pattern — see
// src-tauri/src/prompts.rs) are persisted in files in the app data dir and
// loaded asynchronously — render only after both hydrate so nothing a user
// does can race them and get overwritten. Design doc note: the doc places
// this "hydratePrompts() at boot in App.tsx" — `hydrateSessions()` itself
// actually lives here in main.tsx (pre-render, not an App.tsx effect), so
// `hydratePrompts()` follows suit for the same "no user action can race
// hydration" reason.
const isCompanionOverlay = new URLSearchParams(window.location.search).get("overlay") === "1";

if (isCompanionOverlay) {
  ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
    <React.StrictMode>
      <ErrorBoundary>
        <CompanionOverlay />
      </ErrorBoundary>
    </React.StrictMode>,
  );
} else void Promise.all([hydrateSessions(), hydratePrompts()]).finally(() => {
  // Secondary windows opened by the session menu's "Open in > Split view/New
  // window" (see src-tauri/src/system.rs `open_session_window`) load this
  // same entry point with `?session=<id>` — switch to it before the first
  // render. No-ops if the id doesn't exist.
  const preselectedSessionId = new URLSearchParams(window.location.search).get("session");
  if (preselectedSessionId) {
    useSessionStore.getState().switchSession(preselectedSessionId);
  }

  // Top-level boundary: without it a render error anywhere unmounts the
  // entire tree and the window goes blank with no way back but a restart.
  ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
    <React.StrictMode>
      <ErrorBoundary>
        <App />
      </ErrorBoundary>
    </React.StrictMode>,
  );
});
