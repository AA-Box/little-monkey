import { invoke } from "@tauri-apps/api/core";
import { runComputerUseFullProductE2e } from "./lib/computerUseFullProductE2e";

const reportBootstrapFailure = (error: unknown) => invoke("computer_use_full_product_report", {
  report: {
    status: "failed",
    error: error instanceof Error ? error.message : String(error),
    real_frontend_dispatcher: false,
    real_tauri_ipc: false,
  },
}).catch(() => undefined);

const isCompanionOverlay = new URLSearchParams(window.location.search).get("overlay") === "1";

if (!isCompanionOverlay) {
  window.setTimeout(() => {
    void runComputerUseFullProductE2e().catch(reportBootstrapFailure);
  }, 1_500);
}
