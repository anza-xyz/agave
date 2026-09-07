import { describe, expect, it } from "vitest";
import { blockAverages, sortBlocks } from "./produced";
import type { ProducedBlock } from "./types";

/** A produced block, to be overridden a field at a time. */
function block(over: Partial<ProducedBlock> = {}): ProducedBlock {
  return {
    slot: 1,
    slot_time_millis: null,
    blockhash: "hash",
    duration_nanos: null,
    transactions: 0,
    non_vote_transactions: 0,
    failed_transactions: 0,
    entries: 0,
    block_cost: 0,
    block_cost_limit: 0,
    account_cost_limit: 0,
    total_fees: 0,
    priority_fees: 0,
    tips: null,
    ...over,
  };
}

describe("blockAverages", () => {
  it("averages each column over the blocks held", () => {
    const avg = blockAverages([
      block({ transactions: 1000, total_fees: 100, block_cost: 40, block_cost_limit: 100 }),
      block({ transactions: 2000, total_fees: 200, block_cost: 60, block_cost_limit: 100 }),
    ]);
    expect(avg.blocks).toBe(2);
    expect(avg.transactions).toBe(1500);
    expect(avg.fees).toBe(150);
    expect(avg.filled).toBeCloseTo(0.5, 10);
  });

  it("averages the blocks' own percentages, not the totals", () => {
    // A block with a larger limit should not count for more in a column of
    // percentages: the figure at the head of the column has to be the mean of
    // what is under it. Totalled instead this would read 20/110, not 55%.
    const avg = blockAverages([
      block({ block_cost: 10, block_cost_limit: 100 }),
      block({ block_cost: 10, block_cost_limit: 10 }),
    ]);
    expect(avg.filled).toBeCloseTo(0.55, 10);
  });

  it("leaves out a block with no figure rather than counting it as nought", () => {
    // One slot never timed, and one with no cost limit read. Counted as noughts
    // they would drag both averages down and describe blocks that never were.
    const avg = blockAverages([
      block({ duration_nanos: 400e6, block_cost: 50, block_cost_limit: 100 }),
      block({ duration_nanos: null, block_cost: 0, block_cost_limit: 0 }),
    ]);
    expect(avg.durationMillis).toBe(400);
    expect(avg.filled).toBeCloseTo(0.5, 10);
    expect(avg.blocks).toBe(2);
  });

  it("says nothing rather than nought when there is nothing to average", () => {
    for (const empty of [blockAverages([]), blockAverages(undefined)]) {
      expect(empty.blocks).toBe(0);
      expect(empty.transactions).toBeNull();
      expect(empty.filled).toBeNull();
      expect(empty.fees).toBeNull();
      expect(empty.durationMillis).toBeNull();
    }
  });
});

describe("sortBlocks", () => {
  const held = [
    block({ slot: 1, transactions: 990, duration_nanos: 302e6 }),
    block({ slot: 2, transactions: 1_214, duration_nanos: null }),
    block({ slot: 3, transactions: 1_063, duration_nanos: 337e6 }),
  ];
  const slots = (blocks: ProducedBlock[]) => blocks.map((b) => b.slot);

  it("orders by the column, highest first by default", () => {
    expect(slots(sortBlocks(held, "transactions", "desc"))).toEqual([2, 3, 1]);
    expect(slots(sortBlocks(held, "transactions", "asc"))).toEqual([1, 3, 2]);
  });

  it("puts a block with no figure last, whichever way round", () => {
    expect(slots(sortBlocks(held, "duration", "desc"))).toEqual([3, 1, 2]);
    expect(slots(sortBlocks(held, "duration", "asc"))).toEqual([1, 3, 2]);
  });

  it("leaves the list it was given alone", () => {
    sortBlocks(held, "fees", "asc");
    expect(slots(held)).toEqual([1, 2, 3]);
  });
});
