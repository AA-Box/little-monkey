/**
 * Studio's section nav, living in the left sidebar rather than as tabs above the
 * panel.
 *
 * The sidebar is otherwise wasted in Studio: it holds the chat session list,
 * which switches nothing here. Putting the sections there reuses that column and
 * gives the panel its full width back.
 *
 * Deliberately in its own module rather than in `StudioPanel`: the panel is lazy
 * (see `lazyComponents`), and `App` renders this nav eagerly, so importing the
 * type or the list from the panel would pull the whole panel into the entry
 * chunk and spend the bundle budget on a screen the user may never open.
 */
import { AudioLines, Boxes, Image, Video } from "lucide-react";

import { useT } from "../../lib/i18n";

/** Ordered as the sidebar lists them: the three things to make, then the
 *  library they all draw from. */
export const STUDIO_MODES = [
  { id: "image", labelKey: "Studio.tab.image", icon: Image },
  { id: "video", labelKey: "Studio.tab.video", icon: Video },
  { id: "audio", labelKey: "Studio.tab.audio", icon: AudioLines },
  { id: "models", labelKey: "Studio.tab.models", icon: Boxes },
] as const;

/** Derived from the list rather than declared beside it, so a mode cannot exist
 *  without a way to reach it — adding one here is the only way to add one at
 *  all, and `MODE_TASKS` in `StudioPanel` then fails to compile until the new
 *  mode is given its tasks. */
export type StudioMode = (typeof STUDIO_MODES)[number]["id"];

interface Props {
  active: StudioMode;
  onChange: (mode: StudioMode) => void;
}

export function StudioNav({ active, onChange }: Props) {
  const { t } = useT();
  return (
    // `nav` + `aria-current` rather than a tablist: these are sibling
    // destinations in a sidebar now, not tabs over one panel, and the sidebar
    // holds no tabpanel for a tablist to point at.
    // Labelled with the section's own name rather than a new string: "Studio"
    // is what a screen reader should say here, and it is already translated.
    <nav aria-label={t("App.section.studio")} className="flex flex-col gap-0.5 px-2">
      {STUDIO_MODES.map(({ id, labelKey, icon: Icon }) => {
        const isActive = id === active;
        return (
          <button
            key={id}
            type="button"
            aria-current={isActive ? "page" : undefined}
            onClick={() => onChange(id)}
            className={`flex items-center gap-2.5 rounded-lg px-2.5 py-1.5 text-left text-sm transition-colors ${
              isActive
                ? "bg-surface-2 font-medium text-foreground"
                : "text-muted hover:bg-surface-2 hover:text-foreground"
            }`}
          >
            <Icon size={16} className="shrink-0" />
            {t(labelKey)}
          </button>
        );
      })}
    </nav>
  );
}
