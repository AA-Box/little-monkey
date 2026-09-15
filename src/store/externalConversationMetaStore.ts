import { create } from "zustand";

/**
 * What the desktop remembers about an outside conversation that the daemon
 * does not: whether it is pinned, unread, archived, which group it is filed
 * under, and the name the user gave it. The daemon owns the conversation and
 * its transcript; these are this desktop's own notes about it, so they live
 * here, per device, like the sidebar's view preferences.
 *
 * Keyed by the same `"<environment> <id>"` string the sidebar uses as the
 * row id, so a row and its notes can never disagree about which is which.
 */

/** localStorage key these notes persist under. */
export const EXTERNAL_CONVERSATION_META_STORAGE_KEY = "little-monkey-external-conversation-meta";

export interface ExternalConversationMeta {
  pinned: boolean;
  unread: boolean;
  archived: boolean;
  groupId: string | null;
  /** The user's own name for it, over whatever the provider called it. */
  title: string | null;
}

export const EMPTY_META: ExternalConversationMeta = {
  pinned: false,
  unread: false,
  archived: false,
  groupId: null,
  title: null,
};

function isEmpty(meta: ExternalConversationMeta): boolean {
  return !meta.pinned && !meta.unread && !meta.archived && meta.groupId === null && meta.title === null;
}

/** Anything unrecognized falls back to the default for that field rather than
 * poisoning the whole entry: a hand-edited or newer-build value must not make
 * a row unpinnable. */
function hydrate(): Record<string, ExternalConversationMeta> {
  try {
    const raw = localStorage.getItem(EXTERNAL_CONVERSATION_META_STORAGE_KEY);
    if (!raw) return {};
    const parsed = JSON.parse(raw) as Record<string, Partial<ExternalConversationMeta>>;
    if (!parsed || typeof parsed !== "object") return {};
    const meta: Record<string, ExternalConversationMeta> = {};
    for (const [key, value] of Object.entries(parsed)) {
      if (!value || typeof value !== "object") continue;
      const entry: ExternalConversationMeta = {
        pinned: value.pinned === true,
        unread: value.unread === true,
        archived: value.archived === true,
        groupId: typeof value.groupId === "string" ? value.groupId : null,
        title: typeof value.title === "string" && value.title.trim() ? value.title : null,
      };
      if (!isEmpty(entry)) meta[key] = entry;
    }
    return meta;
  } catch {
    return {};
  }
}

export interface ExternalConversationMetaState {
  meta: Record<string, ExternalConversationMeta>;
  /** Change some of one conversation's notes. An entry that ends up saying
   * nothing is dropped rather than kept as a row of defaults. */
  update: (key: string, patch: Partial<ExternalConversationMeta>) => void;
  /** Drop every note about a conversation — when the conversation itself is
   * deleted, so a new one under the same key starts clean. */
  forget: (key: string) => void;
}

export const useExternalConversationMetaStore = create<ExternalConversationMetaState>((set, get) => {
  const persist = (meta: Record<string, ExternalConversationMeta>) => {
    set({ meta });
    try {
      localStorage.setItem(EXTERNAL_CONVERSATION_META_STORAGE_KEY, JSON.stringify(meta));
    } catch {
      // Best-effort persistence.
    }
  };
  return {
    meta: hydrate(),
    update: (key, patch) => {
      const next = { ...(get().meta[key] ?? EMPTY_META), ...patch };
      const meta = { ...get().meta };
      if (isEmpty(next)) delete meta[key];
      else meta[key] = next;
      persist(meta);
    },
    forget: (key) => {
      if (!(key in get().meta)) return;
      const meta = { ...get().meta };
      delete meta[key];
      persist(meta);
    },
  };
});

export default useExternalConversationMetaStore;
