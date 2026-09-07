/**
 * Reading a block's cost against the recent blocks around it.
 *
 * Kept out of the component so the counting can be tested without a DOM, in the
 * same way as the waterfall rows and the replay rows.
 */

import type { SlotCost } from "./types";

/** How often one account has been the costliest across the blocks held. */
export interface Recurrence {
  /** Blocks in which this account was the costliest, including the one shown. */
  blocks: number;
  /** Blocks held, which is what those are out of. */
  of: number;
  /** The most it cost in any of them. */
  peakCost: number;
  /** The slot that happened in. */
  peakSlot: number;
}

/**
 * Whether the same account keeps topping this validator's blocks.
 *
 * One block topped by an account says very little: something has to be the
 * largest. The same account topping several says the throttle is standing
 * rather than incidental, which is the difference between a block that happened
 * to be quiet and an account worth doing something about.
 *
 * Counted across every produced block held, not the last few, so the figure
 * does not change meaning as the queue fills after a restart. `of` is sent
 * alongside so the panel can say what the count is out of.
 */
export function recurrence(costs: SlotCost[], account: string): Recurrence | null {
  if (!account) return null;
  const matching = costs.filter((cost) => cost.costliest_account === account);
  if (matching.length === 0) return null;

  let peak = matching[0];
  for (const cost of matching) {
    if (cost.costliest_cost > peak.costliest_cost) peak = cost;
  }

  return {
    blocks: matching.length,
    of: costs.length,
    peakCost: peak.costliest_cost,
    peakSlot: peak.slot,
  };
}
