import { lazy, Suspense } from "react";

const StandardsStudioPanelImpl = lazy(() =>
  import("./StandardsStudioPanelImpl").then((module) => ({ default: module.StandardsStudioPanelImpl })),
);

export function StandardsStudioPanel() {
  return (
    <Suspense
      fallback={(
        <div className="flex min-h-32 w-full items-center justify-center" aria-busy="true">
          <div className="h-5 w-5 animate-spin rounded-full border-2 border-border border-t-accent" />
        </div>
      )}
    >
      <StandardsStudioPanelImpl />
    </Suspense>
  );
}
