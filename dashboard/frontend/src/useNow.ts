import { useEffect, useState } from "react";

/**
 * A wall-clock reading that advances on its own.
 *
 * The charts place points by timestamp, so without this they only move when a
 * sample arrives, which is once a second, and the series jumped a whole second
 * at a time. Only the chart components use this, so the rest of the tree still
 * re-renders solely when the store changes.
 *
 * A 60 second window across 600 units of viewBox scrolls at 10 units a second,
 * so a 100ms tick moves it one unit at a time. Finer than that would cost
 * renders for motion nobody can see.
 */
/**
 * How far behind live the charts draw, in milliseconds.
 *
 * A series plotted right up to the present has nothing to draw between its
 * newest sample and the edge, so the fill ended in a notch that grew for a
 * second and then snapped shut when the next sample landed. Drawing one sample
 * interval behind puts the newest sample past the edge, where the viewBox clips
 * it, and the visible edge always falls between two real samples.
 *
 * The same reason `windowed` keeps one sample older than the window: a line
 * needs a point beyond the boundary to be continuous across it.
 *
 * This has to match how often the validator publishes samples, which is
 * `METER_INTERVAL` in `dashboard/src/meters.rs`. Shorter and the notch comes
 * back; longer and the charts are staler than they need to be. Nothing enforces
 * the pairing across the two languages.
 */
export const RENDER_LAG_MS = 1000;

export function useNow(intervalMs = 100): number {
  const [now, setNow] = useState(() => Date.now());

  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), intervalMs);
    return () => window.clearInterval(timer);
  }, [intervalMs]);

  return now;
}

/**
 * The samples to draw for a window ending at `now`, including the first one
 * that falls outside it.
 *
 * That extra point is what lets the line leave the chart by sliding under the
 * viewBox edge. Filtering strictly to the window made the leftmost segment
 * vanish the moment its older end expired.
 */
export function windowed<T>(
  samples: T[],
  now: number,
  windowMs: number,
  timestampNanos: (sample: T) => number,
): T[] {
  const cutoff = now - windowMs;
  const first = samples.findIndex((sample) => timestampNanos(sample) / 1e6 >= cutoff);
  if (first < 0) return [];
  return samples.slice(Math.max(0, first - 1));
}
