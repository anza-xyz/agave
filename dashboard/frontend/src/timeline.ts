/**
 * A slot's two spans: waiting for the block, then finishing it.
 *
 * The validator sends when the block was full and when replay finished, both
 * measured from the slot's first shred. This turns them into the two segments
 * the schedule page draws and the text beside them, and is kept out of the
 * component so the arithmetic can be tested without a DOM.
 */

import type { SlotEntry } from "./types";

/**
 * The track every timeline is drawn against, in milliseconds. Fixed rather
 * than fitted to each row, so rows compare: a slot at the configured four
 * hundred milliseconds fills under half, one that waited most of a second for
 * its shreds fills nearly all.
 */
export const TIMELINE_SPAN_MS = 1000;

export interface Timeline {
  /** Milliseconds from the first shred to the last. */
  wait: number;
  /**
   * Milliseconds from the block being full to replay finishing, or null where
   * replay's finish was not seen, which includes every block we built.
   */
  run: number | null;
  /** The two spans as shares of the track, clamped so together they fit it. */
  waitShare: number;
  runShare: number;
  label: string;
}

/**
 * The timeline for a slot, or null where the block's arrival was never
 * reported. The second span is the difference, clamped at nought because the
 * two are stamped by different threads and can disagree by a millisecond.
 */
export function timelineOf(entry: SlotEntry | null): Timeline | null {
  const shreds = entry?.shreds;
  if (!entry || !shreds) return null;

  const wait = shreds.full_millis;
  const run = entry.replayed_millis === null ? null : Math.max(0, entry.replayed_millis - wait);
  const waitShare = Math.min(1, wait / TIMELINE_SPAN_MS);
  const runShare = run === null ? 0 : Math.min(1 - waitShare, run / TIMELINE_SPAN_MS);
  const label = run === null ? `${wait} ms` : `${wait} + ${run} ms`;
  return { wait, run, waitShare, runShare, label };
}
