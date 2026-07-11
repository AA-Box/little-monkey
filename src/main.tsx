import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import "./index.css";
import { applyTheme, getStoredTheme } from "./lib/theme";
import { hydrateSessions, useSessionStore } from "./store/sessionStore";
import { hydratePrompts } from "./store/promptStore";

applyTheme(getStoredTheme());

// Sessions (and the prompt/persona library, same file-based pattern — see
// src-tauri/src/prompts.rs) are persisted in files in the app data dir and
// loaded asynchronously — render only after both hydrate so nothing a user
// does can race them and get overwritten. Design doc note: the doc places
// this "hydratePrompts() at boot in App.tsx" — `hydrateSessions()` itself
// actually lives here in main.tsx (pre-render, not an App.tsx effect), so
// `hydratePrompts()` follows suit for the same "no user action can race
// hydration" reason.
void Promise.all([hydrateSessions(), hydratePrompts()]).finally(() => {
  // Secondary windows opened by the session menu's "Open in > Split view/New
  // window" (see src-tauri/src/system.rs `open_session_window`) load this
  // same entry point with `?session=<id>` — switch to it before the first
  // render. No-ops if the id doesn't exist.
  const preselectedSessionId = new URLSearchParams(window.location.search).get("session");
  if (preselectedSessionId) {
    useSessionStore.getState().switchSession(preselectedSessionId);
  }

  ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
    <React.StrictMode>
      <App />
    </React.StrictMode>,
  );
});
