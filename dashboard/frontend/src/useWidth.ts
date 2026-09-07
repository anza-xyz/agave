import { useEffect, useState, type RefObject } from "react";

/**
 * How wide an element actually is, in pixels.
 *
 * The dashboard's other charts avoid needing this by drawing into a viewBox and
 * letting it stretch, which is right for a line: a line that has been squashed
 * horizontally is still a line. The transaction matrix cannot do that. Its dots
 * have to stay square, so their geometry has to be worked out in the pixels the
 * element really occupies, and how many columns fit at all depends on the same
 * number.
 *
 * Null until measured, so the first paint draws nothing rather than drawing at
 * a guessed width and jumping.
 */
export function useWidth(ref: RefObject<Element | null>): number | null {
  const [width, setWidth] = useState<number | null>(null);

  useEffect(() => {
    const element = ref.current;
    if (!element) return;

    // Measured once here rather than waiting to be told. Some embedded webviews
    // carry ResizeObserver and never deliver to it, and something that waits
    // for a callback which never arrives never draws at all. One reading is
    // right until the element changes size; the observer is what keeps it right
    // afterwards.
    setWidth(element.getBoundingClientRect().width);
    if (typeof ResizeObserver !== "function") return;

    const observer = new ResizeObserver((entries) => {
      const entry = entries[0];
      if (entry) setWidth(entry.contentRect.width);
    });
    observer.observe(element);
    return () => observer.disconnect();
  }, [ref]);

  return width;
}
