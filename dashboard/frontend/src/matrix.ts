/**
 * Lighting the transaction matrix: how many rows each series takes in a column.
 *
 * Kept out of the component because the dot geometry is easy to get subtly
 * wrong and none of it needs a DOM.
 */

/** How much history the matrix shows. Matches the network card's window. */
export const MATRIX_WINDOW_SECONDS = 60;

/** Rows in a full-height matrix, and in the shorter one a phone gets. */
export const ROWS_TALL = 11;
export const ROWS_SHORT = 8;

/**
 * How far above the window's peak the top of the scale sits.
 *
 * A fixed ceiling rather than one fitted to each frame. Refitted every sample
 * the whole silhouette rescales whenever a spike arrives and leaves, so the
 * shape moves for reasons that have nothing to do with the traffic.
 */
export const CEILING_HEADROOM = 1.1;

/** The narrowest a column may be before samples start being dropped. */
export const MIN_PITCH = 13;

/**
 * How many rows each series lights, counting from the bottom of the column.
 *
 * Given bottom to top, and returned the same way. The series stack rather than
 * overlap: each one starts where the one beneath it stopped, so the height of
 * the lit part of a column is the total.
 *
 * Any series with something in it lights at least one row. Rounded honestly a
 * small band takes no rows at all, and an unlit band does not read as "too
 * small to draw" but as "this did not happen", which is a wrong statement
 * rather than an imprecise one. Failed transactions are the series this
 * matters for: they are the smallest and the one worth seeing.
 */
export function columnRows(values: number[], ceiling: number, rows: number): number[] {
  if (ceiling <= 0 || rows <= 0) return values.map(() => 0);

  const scale = rows / ceiling;
  let below = 0;
  const lit = values.map((value) => {
    const from = Math.ceil(Math.min(below * scale, rows));
    below += Math.max(0, value);
    const to = Math.ceil(Math.min(below * scale, rows));
    return Math.max(0, to - from);
  });

  for (const [index, value] of values.entries()) {
    if (value > 0 && lit[index] === 0) lit[index] = 1;
  }

  // The guarantee can push a column past the grid it has to fit in. Take the
  // rows back from the largest series, which is the one that loses least by it,
  // and never from a series down to its single guaranteed row.
  let total = lit.reduce((sum, count) => sum + count, 0);
  while (total > rows) {
    let largest = 1;
    let at = -1;
    for (const [index, count] of lit.entries()) {
      if (count > largest) {
        largest = count;
        at = index;
      }
    }
    if (at < 0) break;
    lit[at] -= 1;
    total -= 1;
  }

  return lit;
}

/**
 * How many columns the grid has at this width.
 *
 * Never more than the window holds, so a full minute fills the grid exactly.
 * Sized from the sample count instead, a short history would spread a handful
 * of columns across the whole card with enormous gaps, which reads as a broken
 * chart rather than as one still filling up.
 */
export function slotsFor(width: number): number {
  return Math.max(1, Math.min(MATRIX_WINDOW_SECONDS, Math.floor(width / MIN_PITCH)));
}

/**
 * The samples for each column, newest last, with nulls where nothing has
 * arrived yet.
 *
 * Taken counting back from the newest so the live column is always the last
 * one whatever the stride works out to. Counted forward, the newest sample is
 * dropped whenever the stride does not divide evenly and the leading edge stops
 * moving.
 *
 * The empty columns are returned rather than left out: their unlit dots are
 * what make a validator that has just started look like a grid waiting to fill
 * rather than a panel that has failed.
 */
export function columnsFor<T>(samples: T[], slots: number): Array<T | null> {
  // Rounded down, not up. The window deliberately carries one sample past its
  // left edge so a line can leave the view continuously, which means a full
  // minute arrives here as sixty-one samples against sixty columns. Rounded up
  // that is a stride of two, so every other column goes dark and the grid
  // halves and un-halves as samples arrive. Rounded down it is a stride of one
  // and the oldest sample is simply left out, which costs a second of history
  // and nothing else.
  const stride = Math.max(1, Math.floor(samples.length / slots));
  const kept: T[] = [];
  for (let index = samples.length - 1; index >= 0 && kept.length < slots; index -= stride) {
    kept.unshift(samples[index]);
  }
  const missing = Math.max(0, slots - kept.length);
  return [...(Array(missing).fill(null) as null[]), ...kept];
}

export interface Geometry {
  /** Horizontal space one column gets, dot and gap together. */
  pitch: number;
  rowHeight: number;
  /** Side of the square. */
  dot: number;
}

/**
 * Where the dots go, in pixels.
 *
 * Square, and sized to leave a gap on both axes: the dark grid between them is
 * what makes it read as an instrument rather than as a bar chart with gaps.
 */
export function geometry(width: number, height: number, columns: number, rows: number): Geometry {
  const pitch = columns > 0 ? width / columns : width;
  const rowHeight = rows > 0 ? height / rows : height;
  const dot = Math.max(3, Math.min(pitch - (pitch > 10 ? 2.5 : 1.5), rowHeight - 2, 6));
  return { pitch, rowHeight, dot };
}

/** The scale the columns are drawn against. Never nought. */
export function ceilingFor(peak: number): number {
  return Math.max(1, peak * CEILING_HEADROOM);
}
