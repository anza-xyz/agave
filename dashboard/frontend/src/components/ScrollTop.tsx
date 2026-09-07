import { useEffect, useLayoutEffect, useRef, useState, type RefObject } from "react";
import { heldScrollTop } from "../scroll";

/**
 * How far a list must be scrolled before the way back is offered.
 *
 * A few rows, so that nudging it does not put a button over it, and so that the
 * button is gone whenever the live edge is already on screen.
 */
const LIVE_EDGE_PX = 120;

/**
 * A pill that returns the slot list to the top, where the newest slot is.
 *
 * The schedule wants the same two things — a way back, and rows that stay put
 * while others arrive above them — but gets both from the virtualised list it
 * is built on. This is the plain version, for the one list short enough not to
 * need virtualising: five hundred rows of one line each.
 *
 * The wrapper carries no height, so the button hangs over the rows instead of
 * moving them, and is sticky rather than absolute so it tracks the list rather
 * than the page.
 */
export function ScrollTop({ scroller }: { scroller: RefObject<HTMLElement | null> }) {
  const [away, setAway] = useState(false);
  // Shared with the hook below: it needs to know where the list was left, to
  // tell its own correction apart from one the browser already made.
  const top = useRef(0);
  useHeldScroll(scroller, top);

  useEffect(() => {
    const element = scroller.current;
    if (!element) return;

    const follow = () => {
      top.current = element.scrollTop;
      setAway(element.scrollTop > LIVE_EDGE_PX);
    };
    element.addEventListener("scroll", follow, { passive: true });
    // The list may already be scrolled when this mounts, which is what happens
    // when the page is switched away from and back.
    follow();
    return () => element.removeEventListener("scroll", follow);
  }, [scroller]);

  return (
    <div className="scroll-top-anchor">
      {away && (
        <button
          type="button"
          className="scroll-top"
          onClick={() =>
            scroller.current?.scrollTo({
              top: 0,
              // Read here rather than set as `scroll-behavior` on the list,
              // which applies to every programmatic scroll and so animated the
              // corrections below as well. Each then took a third of a second,
              // during which every render saw a scroll position it had not set.
              behavior: window.matchMedia("(prefers-reduced-motion: reduce)").matches
                ? "auto"
                : "smooth",
            })
          }
        >
          Top <span aria-hidden="true">↑</span>
        </button>
      )}
    </div>
  );
}

/**
 * Keeps what is on screen still while rows arrive above it.
 *
 * The list is newest first, so every new slot is inserted at the top and pushes
 * everything being read down the screen — two and a half times a second, which
 * makes a scrolled list unusable.
 *
 * Browsers have scroll anchoring for exactly this and it cannot be relied on:
 * Safari does not implement it at all, and Chrome applies it here but not to a
 * virtualised list, for reasons apparent from neither.
 *
 * Corrections are instant. They are not a movement anyone asked for — they
 * exist so that nothing appears to move — and animating them made the page
 * restless rather than still.
 */
function useHeldScroll(
  scroller: RefObject<HTMLElement | null>,
  // A plain box rather than `RefObject`, whose `current` React types as
  // read-only; this one is written on both sides.
  top: { current: number },
): void {
  // Undefined until the first measurement rather than zero, which would read as
  // the list having grown its whole length on the first render.
  const height = useRef<number | undefined>(undefined);

  useLayoutEffect(() => {
    const element = scroller.current;
    if (!element) return;

    const previous = height.current;
    height.current = element.scrollHeight;

    if (previous === undefined) {
      top.current = element.scrollTop;
      return;
    }

    // Compared before `top` is refreshed, or the check for a position something
    // else has already moved could never fire.
    const next = heldScrollTop(element.scrollTop, top.current, previous, element.scrollHeight);
    if (next !== element.scrollTop) {
      element.scrollTop = next;
    }
    top.current = element.scrollTop;
  });
}
