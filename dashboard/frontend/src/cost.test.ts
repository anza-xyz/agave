import { describe, expect, it } from "vitest";
import { recurrence } from "./cost";
import type { SlotCost } from "./types";

/** One produced block's costs, to be overridden a field at a time. */
function cost(over: Partial<SlotCost> = {}): SlotCost {
  return {
    slot: 1,
    costliest_account: "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
    costliest_cost: 1_000_000,
    block_cost: 42_000_000,
    accounts: 3_847,
    contended: 412,
    new_account_data: 421_888,
    in_flight: 0,
    ...over,
  };
}

describe("recurrence", () => {
  it("counts the blocks one account topped, and finds its worst", () => {
    const costs = [
      cost({ slot: 100, costliest_cost: 8_000_000 }),
      cost({ slot: 101, costliest_account: "OtherAccount", costliest_cost: 30_000_000 }),
      cost({ slot: 102, costliest_cost: 21_400_000 }),
      cost({ slot: 103, costliest_cost: 11_800_000 }),
    ];
    const seen = recurrence(costs, "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA")!;

    expect(seen.blocks).toBe(3);
    expect(seen.of).toBe(4);
    expect(seen.peakCost).toBe(21_400_000);
    expect(seen.peakSlot).toBe(102);
  });

  it("does not let another account's larger cost become the peak", () => {
    // The peak has to come from the blocks this account topped. Taking the
    // largest figure in the window would name a slot it had nothing to do with.
    const costs = [
      cost({ slot: 200, costliest_cost: 5_000_000 }),
      cost({ slot: 201, costliest_account: "OtherAccount", costliest_cost: 99_000_000 }),
    ];
    const seen = recurrence(costs, "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA")!;

    expect(seen.peakCost).toBe(5_000_000);
    expect(seen.peakSlot).toBe(200);
  });

  it("says nothing about an account that has topped nothing", () => {
    expect(recurrence([cost()], "NeverCostliest")).toBeNull();
    expect(recurrence([], "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA")).toBeNull();
  });

  it("says nothing when the block named no account", () => {
    // A block that took no transactions has no costliest account, and an empty
    // key would otherwise match every other such block.
    expect(recurrence([cost({ costliest_account: "" })], "")).toBeNull();
  });
});
