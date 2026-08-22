import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { ErrorBoundary } from "./components/ErrorBoundary";
import "./index.css";
import { applyAppearance, subscribeToSystemTheme } from "./lib/theme";
import { resolveAppearanceSettings } from "./lib/appearanceProfiles";
import { useSettingsStore } from "./store/settingsStore";
import { primaryRoot, useWorkspaceStore } from "./store/workspaceStore";
import { hydrateSessions, useSessionStore } from "./store/sessionStore";
import { hydratePrompts } from "./store/promptStore";
import { CompanionOverlay } from "./components/Companion";
import { loadLocaleTranslations } from "./lib/i18n";
import { useLocaleStore } from "./store/localeStore";
import { useSkillActivationPolicyStore } from "./store/skillActivationPolicyStore";

function committedAppearance() {
  const settings = useSettingsStore.getState();
  const workspaceKey = primaryRoot(useWorkspaceStore.getState().roots)?.path ?? null;
  return resolveAppearanceSettings(
    settings.deviceAppearance,
    settings.appearanceWorkspaceOverrides,
    workspaceKey,
  );
}

function applyCommittedAppearance(): void {
  applyAppearance(committedAppearance());
}

applyCommittedAppearance();
useSettingsStore.subscribe((settings, previous) => {
  if (
    settings.deviceAppearance !== previous.deviceAppearance
    || settings.appearanceWorkspaceOverrides !== previous.appearanceWorkspaceOverrides
  ) {
    applyCommittedAppearance();
  }
});
useWorkspaceStore.subscribe((workspace, previous) => {
  if (workspace.roots !== previous.roots) applyCommittedAppearance();
});
subscribeToSystemTheme(() => {
  const appearance = committedAppearance();
  if (appearance.themePreference === "system") applyAppearance(appearance);
});

// Sessions (and the prompt/persona library, same file-based pattern — see
// src-tauri/src/prompts.rs) are persisted in files in the app data dir and
// loaded asynchronously — render only after both hydrate so nothing a user
// does can race them and get overwritten. Design doc note: the doc places
// this "hydratePrompts() at boot in App.tsx" — `hydrateSessions()` itself
// actually lives here in main.tsx (pre-render, not an App.tsx effect), so
// `hydratePrompts()` follows suit for the same "no user action can race
// hydration" reason.
// The webview's native right-click menu ("Inspect Element", "AutoFill", ...)
// is a browser affordance, not an app one — suppress it everywhere in release
// builds. Kept in dev so the inspector stays one right-click away.
if (!import.meta.env.DEV) {
  document.addEventListener("contextmenu", (event) => event.preventDefault());
}

const isCompanionOverlay = new URLSearchParams(window.location.search).get("overlay") === "1";
const localeReady = loadLocaleTranslations(useLocaleStore.getState().locale);

if (isCompanionOverlay) {
  void localeReady.finally(() => {
    ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
      <React.StrictMode>
        <ErrorBoundary>
          <CompanionOverlay />
        </ErrorBoundary>
      </React.StrictMode>,
    );
  });
} else void Promise.all([hydrateSessions(), hydratePrompts(), useSkillActivationPolicyStore.getState().hydrate(), localeReady]).finally(() => {
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
  if (import.meta.env.VITE_COMPUTER_USE_FULL_PRODUCT_E2E === "1") {
    void import("./lib/computerUseFullProductE2e").then(({ runComputerUseFullProductE2e }) => {
      window.setTimeout(() => void runComputerUseFullProductE2e(), 1_500);
    });
  }
});
