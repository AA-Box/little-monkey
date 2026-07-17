import { useEffect, type KeyboardEvent } from "react";
import { Activity, BookOpen, Boxes, Cpu, Network, PackageCheck, RefreshCw, ServerCog, type LucideIcon } from "lucide-react";
import { useRuntimeHubStore, type RuntimeHubSection } from "../../store/runtimeHubStore";
import { BusyButton, ErrorNotice } from "./runtimeHub/RuntimeHubShared";
import { RuntimeHubOverview } from "./runtimeHub/RuntimeHubOverview";
import { RuntimeHubModels } from "./runtimeHub/RuntimeHubModels";
import { RuntimeHubComponents } from "./runtimeHub/RuntimeHubComponents";
import { RuntimeHubCatalogs } from "./runtimeHub/RuntimeHubCatalogs";
import { RuntimeHubRuntimes } from "./runtimeHub/RuntimeHubRuntimes";
import { RuntimeHubApi } from "./runtimeHub/RuntimeHubApi";
import { RuntimeHubLan } from "./runtimeHub/RuntimeHubLan";

const SECTIONS: Array<{ id: RuntimeHubSection; label: string; icon: LucideIcon }> = [
  { id: "overview", label: "Overview", icon: Activity },
  { id: "models", label: "Models", icon: Boxes },
  { id: "components", label: "Components", icon: PackageCheck },
  { id: "catalogs", label: "Catalogs", icon: BookOpen },
  { id: "runtimes", label: "Runtimes", icon: Cpu },
  { id: "api", label: "API", icon: ServerCog },
  { id: "lan", label: "LAN", icon: Network },
];

export function RuntimeHubPanel() {
  const section = useRuntimeHubStore((state) => state.section);
  const setSection = useRuntimeHubStore((state) => state.setSection);
  const refresh = useRuntimeHubStore((state) => state.refresh);
  const loaded = useRuntimeHubStore((state) => state.loaded);
  const refreshing = useRuntimeHubStore((state) => state.busy.overview || state.busy["lan-refresh"]);
  const overviewError = useRuntimeHubStore((state) => state.errors.overview);

  useEffect(() => {
    if (!loaded) void refresh().catch(() => {});
  }, [loaded, refresh]);

  function handleTabKey(event: KeyboardEvent<HTMLButtonElement>, current: number) {
    if (!(["ArrowLeft", "ArrowRight", "Home", "End"] as string[]).includes(event.key)) return;
    event.preventDefault();
    const next = event.key === "Home"
      ? 0
      : event.key === "End"
        ? SECTIONS.length - 1
        : (current + (event.key === "ArrowRight" ? 1 : -1) + SECTIONS.length) % SECTIONS.length;
    setSection(SECTIONS[next].id);
    document.getElementById(`runtime-hub-tab-${SECTIONS[next].id}`)?.focus();
  }

  return (
    <div className="flex min-w-0 flex-col gap-5 py-2">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0">
          <div className="flex items-center gap-2">
            <ServerCog size={18} className="text-accent" aria-hidden="true" />
            <h2 className="text-base font-semibold text-foreground">Runtime Hub</h2>
          </div>
          <p className="mt-1 max-w-2xl text-xs leading-5 text-muted">
            Discover verified models, manage Ollama, llama.cpp and MLX, test compatible APIs, and securely pair LAN clients.
          </p>
        </div>
        <BusyButton type="button" busy={refreshing} onClick={() => void refresh().catch(() => {})}>
          <RefreshCw size={15} aria-hidden="true" /> Refresh all
        </BusyButton>
      </div>

      <div
        role="tablist"
        aria-label="Runtime Hub sections"
        className="grid grid-cols-2 gap-1 rounded-lg border border-border bg-surface p-1 sm:grid-cols-3 xl:grid-cols-7"
      >
        {SECTIONS.map((entry, index) => {
          const Icon = entry.icon;
          const active = entry.id === section;
          return (
            <button
              key={entry.id}
              id={`runtime-hub-tab-${entry.id}`}
              type="button"
              role="tab"
              aria-selected={active}
              aria-controls={`runtime-hub-panel-${entry.id}`}
              tabIndex={active ? 0 : -1}
              onClick={() => setSection(entry.id)}
              onKeyDown={(event) => handleTabKey(event, index)}
              className={`flex min-h-11 cursor-pointer items-center justify-center gap-2 rounded-md px-3 py-2 text-sm font-medium transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent motion-reduce:transition-none ${
                active ? "bg-surface-2 text-foreground shadow-sm" : "text-muted hover:bg-surface-2 hover:text-foreground"
              }`}
            >
              <Icon size={15} aria-hidden="true" />
              {entry.label}
            </button>
          );
        })}
      </div>

      {!loaded && refreshing ? (
        <div role="status" className="flex min-h-48 items-center justify-center rounded-lg border border-dashed border-border text-sm text-muted">
          Loading hardware, storage, models, and runtimes…
        </div>
      ) : (
        <>
          {!loaded && <ErrorNotice message={overviewError} />}
          {section === "overview" && <RuntimeHubOverview />}
          {section === "models" && <RuntimeHubModels />}
          {section === "components" && <RuntimeHubComponents />}
          {section === "catalogs" && <RuntimeHubCatalogs />}
          {section === "runtimes" && <RuntimeHubRuntimes />}
          {section === "api" && <RuntimeHubApi />}
          {section === "lan" && <RuntimeHubLan />}
        </>
      )}
    </div>
  );
}

export default RuntimeHubPanel;
