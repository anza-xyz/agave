import { useSyncExternalStore } from "react";

/**
 * The width below which a phone is being held rather than a screen looked at.
 *
 * The same 700px the waterfall rows, the schedule groups and the cache headings
 * change shape at, so a layout does not rearrange itself twice on the way down.
 */
export const NARROW_QUERY = "(max-width: 700px)";

/**
 * Whether the window is narrow, as a value a component can branch on.
 *
 * Most of the frontend answers this in CSS, which is the right place: a media
 * query costs no JavaScript and cannot fall out of step with what is rendered.
 * This exists for the one case CSS cannot reach, which is not *how* a thing is
 * drawn but *whether it is a control at all*. The header's name opens a panel
 * on a phone and opens nothing on a desktop, and a button that opens nothing is
 * worse than no button: it gets pressed once and then nothing on the page is
 * trusted to do anything again.
 *
 * Branching here rather than in CSS also means each figure is rendered once.
 * Hiding a desktop copy and showing a phone copy would put every value in the
 * page twice, where a screen reader reads both.
 */
export function useNarrow(): boolean {
  return useSyncExternalStore(subscribe, isNarrow, alwaysWide);
}

function subscribe(onChange: () => void): () => void {
  // Guarded for the same webviews `isNarrow` guards against. Without this the
  // subscription throws where the query is missing, which takes the header down
  // at render rather than falling back to the wide arrangement.
  if (typeof window.matchMedia !== "function") return () => {};
  const query = window.matchMedia(NARROW_QUERY);
  query.addEventListener("change", onChange);
  return () => query.removeEventListener("change", onChange);
}

/**
 * Whether the window matches, read afresh each time it is asked.
 *
 * Exported for its own sake: it is the only part of this module with a branch
 * in it, and nothing here renders a component, so it is the only part a test
 * can reach.
 */
export function isNarrow(): boolean {
  // Older embedded webviews are missing matchMedia entirely. Answering "wide"
  // there gives a header with everything in it, which is the arrangement that
  // works without the panel.
  return typeof window.matchMedia === "function" && window.matchMedia(NARROW_QUERY).matches;
}

function alwaysWide(): boolean {
  return false;
}
