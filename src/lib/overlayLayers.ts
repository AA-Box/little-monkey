/**
 * Stacking order for full-screen overlays.
 *
 * A dialog that merely shows something can afford to be covered. A prompt that
 * a run is *blocked on* cannot: the permission prompt used to sit at the same
 * layer as the settings modal and below the model and pasted-text dialogs, so
 * asking to delete a model from Settings raised an approval behind the window
 * that raised it — answerable only by closing what you were working in.
 *
 * Anything a person must answer before work continues goes on `APPROVAL_LAYER`,
 * above every editor and panel.
 */
export const APPROVAL_LAYER = "z-[200]";
