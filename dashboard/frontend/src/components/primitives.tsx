import { useId, useLayoutEffect, useRef, useState, type ReactNode } from "react";

/** Gap kept between an open explanation and the edge of the window. */
const EDGE_MARGIN = 12;

/**
 * Where the highest value on screen sits, as a fraction of a chart's height.
 *
 * Charts are scaled to leave room above their peak so the peak line has
 * somewhere to be. Scaled to fill, the line would sit exactly on the top edge
 * and read as a border.
 */
export const PEAK_HEADROOM = 0.85;

/**
 * Vertical position of `value` in a chart scaled so `peak` lands on the peak
 * line.
 *
 * Shared by both charts so the line and the series it marks cannot drift apart:
 * they are drawn from opposite ends, the line from the bottom as a percentage
 * and the series from the top in viewBox units, and nothing but this would keep
 * them agreeing.
 */
export function chartY(value: number, peak: number, height: number): number {
  return height - (value / (peak / PEAK_HEADROOM)) * height;
}

/**
 * The dotted line marking the highest value on screen.
 *
 * Shared by the slot strip and both charts so that a peak is drawn and read the
 * same way wherever it appears.
 */
export function PeakLine({ fraction, label }: { fraction: number; label: string }) {
  const height = Math.max(0, Math.min(100, fraction * 100));
  return (
    <div
      // Too near the top and there is no room above the line for its label, so
      // it moves underneath.
      className={`peak-line${height > 88 ? " label-below" : ""}`}
      style={{ bottom: `${height}%` }}
    >
      <span>{label}</span>
    </div>
  );
}

/**
 * How far to slide an open explanation so it sits inside the window.
 *
 * Negative pulls it left off the right edge, positive pushes it right off the
 * left. Overflow on the right wins when a bubble is somehow wider than the
 * window, since the left edge is where reading starts.
 */
export function edgeShift(left: number, right: number, viewportWidth: number): number {
  const past = right - (viewportWidth - EDGE_MARGIN);
  const before = EDGE_MARGIN - left;
  if (past > 0) return -past;
  if (before > 0) return before;
  return 0;
}

/** Space left between an explanation and the label it belongs to. */
const ANCHOR_GAP = 6;

/**
 * Whether an explanation should open above its label instead of below.
 *
 * An explanation hanging off the bottom of the page makes the document taller,
 * which raises a scrollbar on a page that did not have one and shifts the whole
 * layout. It flips up instead, but only when there is room up there: with no
 * room either way, below is the lesser problem, since a bubble running off the
 * top cannot be scrolled to at all.
 */
export function shouldFlipAbove(
  bubbleBottom: number,
  bubbleHeight: number,
  anchorTop: number,
  viewportHeight: number,
): boolean {
  const overflowsBelow = bubbleBottom > viewportHeight - EDGE_MARGIN;
  const fitsAbove = anchorTop - bubbleHeight - ANCHOR_GAP > EDGE_MARGIN;
  return overflowsBelow && fitsAbove;
}

/**
 * Wraps a label with an explanation that opens on tap as well as on hover.
 *
 * The explanations used to be `title` attributes, which a browser only reveals
 * on hover — so on a touch screen there was no way to reach any of them. The
 * trigger is a real button rather than a styled span because that is what
 * reliably takes focus from a tap on iOS, and focus is what holds it open.
 *
 * Hover and focus are tracked separately rather than leaning on CSS. A tap
 * fires both enter and focus, and moving away fires only leave, so a single
 * `:hover`-or-`:focus-within` rule would strand the bubble open on touch.
 */
export function Explain({
  text,
  children,
  className,
  interactive = false,
}: {
  text: ReactNode;
  children: ReactNode;
  className?: string;
  /**
   * Whether the bubble can be reached with the pointer and the keyboard.
   *
   * Off by default, and that default is the right one for an explanation: a
   * bubble that took the pointer would swallow clicks meant for whatever it
   * happens to cover. It is turned on for the few that hold something to be
   * copied, where a bubble you cannot reach is only half of what was wanted.
   *
   * An interactive bubble is not a tooltip in the ARIA sense, since a tooltip
   * may not contain a control, so it drops the role and the trigger says
   * whether it is open instead.
   */
  interactive?: boolean;
}) {
  const id = useId();
  const bubble = useRef<HTMLSpanElement>(null);
  const anchor = useRef<HTMLSpanElement>(null);
  const [hovered, setHovered] = useState(false);
  const [focused, setFocused] = useState(false);
  const [shift, setShift] = useState(0);
  const [above, setAbove] = useState(false);
  const open = hovered || focused;

  // A bubble anchored to its label runs off the side of the window when the
  // label sits near an edge — the right-hand column of a card pushed it two
  // hundred pixels past the viewport and gave the whole page a horizontal
  // scrollbar. Measured on open and slid back inside.
  useLayoutEffect(() => {
    if (!open || !bubble.current || !anchor.current) {
      setShift(0);
      setAbove(false);
      return;
    }
    const box = bubble.current.getBoundingClientRect();
    setShift(edgeShift(box.left, box.right, document.documentElement.clientWidth));
    // Measured while still below, which is where the flip is decided from. The
    // bubble keeps its height when it moves, so one pass settles it.
    setAbove(
      shouldFlipAbove(
        box.bottom,
        box.height,
        anchor.current.getBoundingClientRect().top,
        window.innerHeight,
      ),
    );
  }, [open]);

  return (
    <span
      ref={anchor}
      className={`explain${className ? ` ${className}` : ""}`}
      onPointerEnter={() => setHovered(true)}
      onPointerLeave={() => setHovered(false)}
      onFocus={() => setFocused(true)}
      // Focus moving from the trigger to a control inside the bubble is focus
      // staying within this, and closing on it would take the control away in
      // the moment it was reached for.
      onBlur={(event) => {
        const next = event.relatedTarget;
        if (interactive && next instanceof Node && event.currentTarget.contains(next)) return;
        setFocused(false);
      }}
    >
      <button
        type="button"
        className="explain-trigger"
        aria-describedby={interactive ? undefined : id}
        aria-expanded={interactive ? open : undefined}
        aria-controls={interactive ? id : undefined}
      >
        {children}
      </button>
      {/* Outside the button so its text does not become part of the button's
          own name, and tied back to it by id instead. */}
      <span
        ref={bubble}
        className={`explain-bubble${open ? " is-open" : ""}${above ? " is-above" : ""}${
          interactive ? " is-interactive" : ""
        }`}
        role={interactive ? undefined : "tooltip"}
        id={id}
        style={shift ? { transform: `translateX(${shift}px)` } : undefined}
      >
        {text}
      </span>
    </span>
  );
}

export function Card({
  title,
  aside,
  children,
  className,
}: {
  title?: string;
  /**
   * A figure that belongs to the card rather than to any row in it, set beside
   * the heading. The replay panel's transactions per slot is the denominator
   * every timing below it is an average over, which makes it the wrong shape
   * for the grid those timings sit in.
   */
  aside?: ReactNode;
  children: ReactNode;
  className?: string;
}) {
  // The body is a separate element so that a card can lay its content out as a
  // grid without the heading becoming one of the cells.
  return (
    <section className="card">
      {(title || aside) && (
        <div className="card-head">
          {title && <h2 className="card-title">{title}</h2>}
          {aside && <span className="card-aside">{aside}</span>}
        </div>
      )}
      <div className={`card-body${className ? ` ${className}` : ""}`}>{children}</div>
    </section>
  );
}

export function Stat({
  label,
  value,
  sub,
  tone,
  explain,
}: {
  label: ReactNode;
  value: ReactNode;
  sub?: ReactNode;
  tone?: "good" | "bad" | "warn" | "muted";
  /** Explanation for a figure whose label cannot say enough on its own. */
  explain?: string;
}) {
  return (
    <div className="stat">
      <div className={`stat-label${explain ? " has-explain" : ""}`}>
        {explain ? <Explain text={explain}>{label}</Explain> : label}
      </div>
      <div className={`stat-value${tone ? ` tone-${tone}` : ""}`}>{value}</div>
      {sub !== undefined && <div className="stat-sub">{sub}</div>}
    </div>
  );
}

/** A labelled horizontal progress bar, as used by the epoch countdown. */
export function Meter({ fraction }: { fraction: number }) {
  const clamped = Math.max(0, Math.min(1, Number.isFinite(fraction) ? fraction : 0));
  return (
    <div className="meter" role="progressbar" aria-valuenow={Math.round(clamped * 100)}>
      <div className="meter-fill" style={{ width: `${clamped * 100}%` }} />
    </div>
  );
}
