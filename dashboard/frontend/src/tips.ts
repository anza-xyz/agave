/**
 * Turning a measured tip figure into the two the pages draw.
 *
 * The validator stores and sends what it measured: the lamports paid into the
 * jito tip accounts while a leader held the block. Both drawn figures are that
 * number times a rate the validator sends alongside it, worked out here rather
 * than there, so that correcting a rate corrects the whole hundred thousand
 * slots of history instead of only what arrives afterwards.
 *
 * The two are not interchangeable and the difference is the point:
 *
 * - `jitoShare` is what reached a distribution account. It applies to any
 *   leader, because it is still a fact about the slot rather than about
 *   somebody's income, and it is the figure Firedancer's schedule page shows.
 * - `ourShare` is what a turn earned this validator. It applies to our own
 *   slots and nowhere else, because it needs a commission, and another
 *   operator's commission is not ours to know.
 *
 * Both are estimates. `jitoShare` subtracts a single published rate from every
 * leader's turn, which quietly assumes they are all on the arrangement we can
 * see; `ourShare` uses the commission flag as it reads now, while the
 * distribution account was initialised once for the epoch with whatever it read
 * then. Label them as derived wherever they are drawn beside a measured number.
 */

import type { TipRates } from "./types";

/** Basis points in the whole. */
const BPS_WHOLE = 10_000;

/**
 * `amount` scaled by `bps`, floored.
 *
 * Floored rather than rounded to match the validator's integer arithmetic, so
 * the same lamports do not read one way here and another in a log line.
 */
function scale(amount: number, bps: number): number {
  return Math.floor((amount * bps) / BPS_WHOLE);
}

/**
 * What reached a distribution account, of what was paid into the tip accounts.
 *
 * For the validator and its stakers together. Which of them gets what depends
 * on a commission this cannot see.
 */
export function jitoShare(paid: number, rates: TipRates): number {
  return paid - scale(paid, rates.jito_cut_bps);
}

/**
 * What a turn earned this validator, or `null` where no commission is
 * configured and the question cannot be answered.
 *
 * Only ever called for our own slots. Calling it for another leader's turn
 * would apply our commission to their tips and produce a number that looks
 * like a measurement and is not.
 */
export function ourShare(paid: number, rates: TipRates): number | null {
  if (rates.commission_bps === null) return null;
  return scale(jitoShare(paid, rates), rates.commission_bps);
}
