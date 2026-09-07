/**
 * Removes the boot splash defined in index.html.
 *
 * The splash is held until the store has something worth showing, so the first
 * frame behind it is a populated dashboard rather than a grid of placeholders.
 */

import type { Store } from "./store";

/** Long enough that a fast local load still reads as a transition, not a flash. */
const MIN_VISIBLE_MS = 650;

/**
 * A validator that is still loading a snapshot may not report a slot for some
 * time. Past this point the dashboard is more useful than the splash, however
 * empty it is.
 */
const MAX_VISIBLE_MS = 6000;

/** Must match the #splash transition duration in index.html. */
const FADE_MS = 450;

export function dismissSplashWhenReady(store: Store): void {
  const splash = document.getElementById("splash");
  if (!splash) return;

  const startedAt = performance.now();
  let unsubscribe: (() => void) | undefined;
  let hidden = false;

  const hide = (): void => {
    if (hidden) return;
    hidden = true;
    unsubscribe?.();
    splash.classList.add("is-leaving");
    // Remove it outright afterwards: a transparent overlay left in place would
    // still swallow every click on the dashboard beneath.
    window.setTimeout(() => splash.remove(), FADE_MS + 50);
  };

  const onChange = (): void => {
    if (hidden || !store.isReady()) return;
    unsubscribe?.();
    unsubscribe = undefined;
    const remaining = MIN_VISIBLE_MS - (performance.now() - startedAt);
    if (remaining > 0) window.setTimeout(hide, remaining);
    else hide();
  };

  unsubscribe = store.subscribe(onChange);
  window.setTimeout(hide, MAX_VISIBLE_MS);
  onChange();
}
