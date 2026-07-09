import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import "./index.css";
import { applyTheme, getStoredTheme } from "./lib/theme";
import { hydrateSessions, useSessionStore } from "./store/sessionStore";

applyTheme(getStoredTheme());

// Sessions are persisted in a file in the app data dir (see
// src-tauri/src/sessions.rs) and loaded asynchronously — render only after
// hydration so nothing a user does can race it and get overwritten.
void hydrateSessions().finally(() => {
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
