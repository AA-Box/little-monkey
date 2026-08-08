/**
 * Studio's section switcher, living at the top of the left sidebar with that
 * section's settings beneath it.
 *
 * The sidebar is otherwise wasted in Studio: it held the chat session list,
 * which switches nothing here.
 *
 * Deliberately in its own module rather than in `StudioPanel`: the panel is lazy
 * (see `lazyComponents`), and `App` renders this eagerly, so importing the type
 * or the list from the panel would pull the whole panel into the entry chunk and
 * spend the bundle budget on a screen the user may never open.
 */
import { SegmentedControl } from "../ui";
import { useT } from "../../lib/i18n";

/** Ordered as the row reads them: the three things to make, then the two
 *  libraries they draw from — the models that generate, and the tools that
 *  operate on what came out. */
export const STUDIO_MODES = [
  { id: "image", labelKey: "Studio.tab.image" },
  { id: "video", labelKey: "Studio.tab.video" },
  { id: "audio", labelKey: "Studio.tab.audio" },
  { id: "models", labelKey: "Studio.tab.models" },
  { id: "tools", labelKey: "Studio.tab.tools" },
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
    // The same pill track the Chat/Studio switcher above it uses, because this
    // is the same kind of control: which whole section you are in, not a tab
    // over one panel. Two stacked pill rows read as one hierarchy.
    <SegmentedControl
      ariaLabel={t("App.section.studio")}
      active={active}
      onChange={onChange}
      items={STUDIO_MODES.map(({ id, labelKey }) => ({ id, label: t(labelKey) }))}
    />
  );
}
