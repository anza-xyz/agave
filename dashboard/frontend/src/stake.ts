/**
 * Turning delinquent stake into the ticks that draw it.
 *
 * Kept out of the component so the arithmetic can be tested without a DOM, in
 * the same way as the waterfall rows and the block costs.
 */

/**
 * Ticks in the strip, which puts one tick at two percent of staked SOL.
 *
 * Fifty is as many as the card can hold at its narrow width and still leave
 * each tick wide enough to see. More would read as a texture rather than as
 * countable marks, which is the whole point of drawing them separately.
 */
export const STAKE_TICKS = 50;

/**
 * The least of a tick that is filled when any stake at all is delinquent.
 *
 * Two percent a tick means a tenth of a percent of delinquent stake fills half
 * a pixel, which draws as nothing and so states that no stake is delinquent.
 * That is a wrong reading rather than an imprecise one, and a trace of red is
 * the true one. The exact share is printed beside the strip either way, so the
 * floor costs no accuracy that a reader could otherwise have had.
 *
 * Deliberately small: it lifts the bottom of the range off zero and nothing
 * else. By three tenths of a percent delinquent the fill is above it and the
 * strip is to scale from there up. Below that it draws around three pixels,
 * which is enough to read as a mark somebody meant to put there rather than as
 * an artefact of rounding a tick's corners.
 */
const MINIMUM_SLIVER = 0.14;

export interface StakeTicks {
  /** Whole ticks given over to delinquent stake, counted from the right. */
  full: number;
  /** How much of the next tick leftward is filled, from 0 to 1. */
  partial: number;
}

/**
 * How much of the strip delinquent stake takes.
 *
 * Whole ticks and a part of one rather than a rounded count of ticks: at two
 * percent each, rounding to whole ticks would report every share below three
 * percent as either nothing or a flat two, and normal is well inside that.
 * The part-filled tick is what lets the strip move at all in the range it
 * spends its time in.
 */
export function stakeTicks(delinquent: number, total: number): StakeTicks {
  const none = { full: 0, partial: 0 };
  if (!Number.isFinite(delinquent) || !Number.isFinite(total)) return none;
  if (total <= 0 || delinquent <= 0) return none;

  const share = Math.min(1, delinquent / total);
  const exact = share * STAKE_TICKS;
  const full = Math.min(STAKE_TICKS, Math.floor(exact));
  if (full >= STAKE_TICKS) return { full: STAKE_TICKS, partial: 0 };

  // The floor only applies where there is nothing else to see. Once a whole
  // tick is red the strip already reads as non-zero, and the part-filled one
  // beside it can be left true.
  const partial = exact - full;
  if (full > 0) return { full, partial };

  return { full, partial: Math.max(MINIMUM_SLIVER, partial) };
}
