/**
 * Reading a minute of throughput into the three figures the card shows.
 *
 * Kept out of the component so the arithmetic can be tested without a DOM, in
 * the same way as the replay rows and the host thresholds.
 */

/** How much of the past the card covers. Matches the transactions chart. */
export const NETWORK_WINDOW_SECONDS = 60;

/**
 * How far from the average a reading must be before it counts as a direction.
 *
 * Relative rather than a fixed number of bytes, because this card reads
 * kilobytes a second on a quiet testnet node and tens of megabytes on mainnet,
 * and a threshold that suits one is meaningless on the other.
 */
const TREND_NOISE = 0.02;

/**
 * Seconds of trailing readings the arrow is taken from.
 *
 * Not the newest reading alone. Throughput swings several percent from one
 * second to the next on an ordinary validator, so a single sample against the
 * minute's average changes its mind constantly, and an arrow that flickers
 * every second is an arrow nobody reads. Ten seconds against the whole minute
 * is the question actually being asked: is it busier now than it has been.
 *
 * Ten also sets what a single second has to do to move it. One reading damps to
 * a tenth here, so a second up to about a fifth above the average leaves the
 * arrow alone, and anything larger than that is an event rather than noise.
 */
const TREND_SAMPLES = 10;

export interface Direction {
  /** The newest reading. */
  current: number;
  average: number;
  /** The newest reading less the average, in the same unit. */
  delta: number;
  /**
   * Which way it is going, from the last few seconds against the whole minute.
   *
   * Taken from a trailing mean rather than from `current`, so one noisy second
   * does not flip it. Deliberately not toned anywhere it is drawn: throughput
   * rising is neither good nor bad on a validator, and a green or red arrow
   * would turn ordinary fluctuation into a verdict.
   */
  trend: "up" | "down" | "flat";
}

/** The three figures for one direction of traffic, from its samples. */
export function direction(values: number[]): Direction | null {
  if (values.length === 0) return null;
  const current = values[values.length - 1];
  const total = values.reduce((sum, value) => sum + value, 0);
  const average = total / values.length;
  const delta = current - average;
  const trailing = values.slice(-TREND_SAMPLES);
  const recent = trailing.reduce((sum, value) => sum + value, 0) / trailing.length;
  return { current, average, delta, trend: trendOf(recent, average) };
}

function trendOf(recent: number, average: number): Direction["trend"] {
  if (average <= 0) return "flat";
  const drift = (recent - average) / average;
  if (drift > TREND_NOISE) return "up";
  if (drift < -TREND_NOISE) return "down";
  return "flat";
}

/**
 * The scale both directions are drawn against.
 *
 * One scale rather than one each, which is the whole reason the two lines are
 * worth putting on the same card. Given a band of its own, a direction moving
 * ten kilobytes a second fills it exactly as a direction moving ten megabytes
 * does, and the picture says they are equals when one is a thousand times the
 * other.
 *
 * Never nought, so nothing divides by it before the first samples arrive.
 */
export function sharedPeak(...series: number[][]): number {
  let peak = 0;
  for (const values of series) {
    for (const value of values) {
      if (value > peak) peak = value;
    }
  }
  return Math.max(peak, 1);
}

const UNITS = ["B", "KB", "MB", "GB", "TB"] as const;

/**
 * The unit a reading of this size wants, and what to divide by to get there.
 *
 * Taken once from the current reading and then applied to the average and the
 * delta as well, rather than letting each pick its own. Sized separately, an
 * average of 1.02 MB/s would print beside a current reading of 980 KB/s as
 * "980" and "avg 1.02", and the second looks like the smaller number.
 */
/** Egress cut into what is measured and what is not, none of it below nought. */
export interface EgressShares {
  gossip: number;
  repair: number;
  /** What no sender accounts for, mostly shreds over XDP. */
  remainder: number;
  measured: number;
}

export function egressShares(
  total: number,
  split: { gossip_per_second: number | null; repair_per_second: number | null },
): EgressShares {
  const gossip = Math.max(0, split.gossip_per_second ?? 0);
  const repair = Math.max(0, split.repair_per_second ?? 0);
  const measured = gossip + repair;
  return { gossip, repair, measured, remainder: Math.max(0, total - measured) };
}

export function unitFor(value: number): { unit: string; divisor: number } {
  let divisor = 1;
  let index = 0;
  while (Math.abs(value) / divisor >= 1024 && index < UNITS.length - 1) {
    divisor *= 1024;
    index += 1;
  }
  return { unit: UNITS[index], divisor };
}
