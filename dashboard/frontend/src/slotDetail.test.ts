import { describe, expect, it } from "vitest";
import { capacity, schedulerView, shareOfGroup } from "./slotDetail";
import type { ProducedBlock, SlotCost, SlotWaterfall } from "./types";
import { waterfallRows } from "./waterfall";

function block(over: Partial<ProducedBlock> = {}): ProducedBlock {
  return {
    slot: 443_077_280,
    slot_time_millis: 400,
    blockhash: "hash",
    duration_nanos: 396_000_000,
    transactions: 1_516,
    non_vote_transactions: 851,
    failed_transactions: 94,
    entries: 1_502,
    block_cost: 62_387_500,
    block_cost_limit: 87_500_000,
    account_cost_limit: 35_000_000,
    total_fees: 20_060,
    priority_fees: 12_480,
    tips: null,
    ...over,
  };
}

function cost(over: Partial<SlotCost> = {}): SlotCost {
  return {
    slot: 443_077_280,
    costliest_account: "94qWNrtmfn42h3ZjUZwWvK1MEo9uVmmrBPd2hpNjYDjb",
    costliest_cost: 7_175_000,
    block_cost: 62_387_500,
    accounts: 3_914,
    contended: 0,
    new_account_data: 11_400,
    in_flight: 0,
    ...over,
  };
}

/** A slot the scheduler had a quiet time with, to be overridden a field at a time. */
function slot(over: Partial<SlotWaterfall> = {}): SlotWaterfall {
  return {
    slot: 443_077_280,
    source: "scheduler",
    received: 894,
    not_held: 0,
    check_queue_full: 0,
    unparsable: 0,
    bad_locks: 0,
    compute_budget: 0,
    too_old: 0,
    already_processed: 0,
    fee_payer: 0,
    filtered: 0,
    nonce_conflict: 0,
    buffered: 894,
    queue_full: 0,
    nonce_evicted: 0,
    cleared: 0,
    cleaned: 0,
    scheduled: 856,
    blocked_conflicts: 0,
    blocked_threads: 0,
    finished: 851,
    retried: 0,
    ...over,
  } as SlotWaterfall;
}

describe("how the block's compute limit was spent", () => {
  it("splits the limit into three shares that account for all of it", () => {
    const c = capacity(block(), cost())!;
    expect(c.top + c.rest + c.free).toBeCloseTo(1, 10);
    expect(c.top).toBeCloseTo(7_175_000 / 87_500_000, 10);
    expect(c.rest).toBeCloseTo((62_387_500 - 7_175_000) / 87_500_000, 10);
  });

  it("draws the costliest account against the limit, not against the block", () => {
    // The two differ by however empty the block was, and only the share of the
    // limit can sit on the same bar as the unused headroom. Here the account is
    // 11.5% of the block but 8.2% of the limit.
    const c = capacity(block(), cost())!;
    expect(c.top).toBeLessThan(7_175_000 / 62_387_500);
    expect(c.top).toBeCloseTo(0.082, 3);
  });

  it("gives the whole bar to headroom on an empty block", () => {
    const c = capacity(block({ block_cost: 0 }), undefined)!;
    expect(c).toEqual({ top: 0, rest: 0, free: 1 });
  });

  it("leaves the costliest segment out where no cost was reported", () => {
    const c = capacity(block(), undefined)!;
    expect(c.top).toBe(0);
    expect(c.rest).toBeCloseTo(62_387_500 / 87_500_000, 10);
  });

  it("has nothing to draw where the limit is unknown", () => {
    expect(capacity(block({ block_cost_limit: 0 }), cost())).toBeNull();
  });

  it("never draws past its own track when the two figures disagree", () => {
    // Block cost and the cost tracker's total are captured from different
    // places on the bank, so they can round against each other.
    const c = capacity(block({ block_cost: 99_000_000 }), cost({ costliest_cost: 99_000_000 }))!;
    expect(c.free).toBe(0);
    expect(c.top + c.rest).toBeCloseTo(1, 10);
  });
});

describe("grouping the scheduler's counters", () => {
  it("covers every row the waterfall produces, in exactly one place", () => {
    // The guard on the group lists. A counter added upstream that nobody
    // assigned would otherwise disappear from the drawer without a word.
    for (const source of ["scheduler", "bam"] as const) {
      const rows = waterfallRows(slot({ source }));
      const view = schedulerView(slot({ source }));
      const placed = [
        ...view.chain.map((link) => link.key),
        ...view.groups.flatMap((g) => [...g.rows, ...g.aside].map((r) => r.key)),
      ];
      expect([...placed].sort()).toEqual([...rows.map((r) => r.key)].sort());
    }
  });

  it("puts what was lost at the top of its group, largest first", () => {
    const view = schedulerView(
      slot({ blocked_conflicts: 17, blocked_threads: 71, retried: 7 }),
    );
    const schedule = view.groups.find((g) => g.key === "schedule")!;
    expect(schedule.rows.map((r) => r.count)).toEqual([71, 17, 7]);
    expect(schedule.hits).toBe(3);
    expect(schedule.total).toBe(95);
  });

  it("keeps the quiet counters in pipeline order below the loud ones", () => {
    const view = schedulerView(slot({ already_processed: 6, too_old: 17 }));
    const intake = view.groups.find((g) => g.key === "intake")!;
    expect(intake.rows.slice(0, 2).map((r) => r.key)).toEqual(["too_old", "already_processed"]);
    expect(intake.hits).toBe(2);

    // The rest keep the order a transaction meets them in. Checked against the
    // waterfall's own ordering rather than against a list written out here,
    // because the counters differ by branch: the 4.2 line has three fewer, and
    // a hard-coded list would make this file diverge for good.
    const pipeline = waterfallRows(slot()).map((row) => row.key);
    const quiet = intake.rows.slice(2).map((r) => r.key);
    expect(quiet).toEqual([...quiet].sort((a, b) => pipeline.indexOf(a) - pipeline.indexOf(b)));
    expect(quiet).not.toHaveLength(0);
  });

  it("holds the batch-counted rows apart from the transaction totals", () => {
    // BAM counts what it rejected before parsing in batches. Added to the
    // transaction counters beside it, or drawn as a share of them, it would be
    // two units in one figure.
    const view = schedulerView(slot({ source: "bam", not_held: 4, too_old: 17 }));
    const intake = view.groups.find((g) => g.key === "intake")!;
    expect(intake.aside.map((r) => r.key)).toEqual(["not_held"]);
    expect(intake.rows.map((r) => r.key)).not.toContain("not_held");
    expect(intake.total).toBe(17);
  });

  it("counts that row as an ordinary loss on a stock validator", () => {
    // The same counter holds something else entirely there: traffic forwarded
    // on rather than batches that arrived too late.
    const view = schedulerView(slot({ not_held: 9 }));
    const intake = view.groups.find((g) => g.key === "intake")!;
    expect(intake.aside).toHaveLength(0);
    expect(intake.total).toBe(9);
  });
});

describe("what the strip says", () => {
  it("names the first link for its unit where the unit changes", () => {
    expect(schedulerView(slot()).chain[0].label).toBe("received");
    expect(schedulerView(slot({ source: "bam" })).chain[0].label).toBe("batches");
  });

  it("works completion against the first figure counted in transactions", () => {
    // On a BAM slot that is buffered: received is batches there, and
    // transactions over batches is a percentage of nothing.
    const view = schedulerView(slot({ source: "bam", received: 377, buffered: 900, finished: 855 }));
    expect(view.completion).toBeCloseTo(855 / 900, 10);
  });

  it("caps completion, since a slot can finish more than arrived in it", () => {
    // The queue holds transactions across slots.
    expect(schedulerView(slot({ received: 100, finished: 140 })).completion).toBe(1);
  });

  it("has no completion to report where nothing arrived", () => {
    // Not nought, which would read as a slot that finished nothing.
    expect(schedulerView(slot({ received: 0, buffered: 0, finished: 0 })).completion).toBeNull();
  });

  it("names the worst counter across all the groups", () => {
    const view = schedulerView(slot({ too_old: 17, blocked_threads: 71, cleared: 3 }));
    expect(view.worst?.key).toBe("blocked_threads");
    expect(view.lost).toBe(91);
    expect(view.nonZero).toBe(3);
  });

  it("names none on a slot that lost nothing", () => {
    const view = schedulerView(slot());
    expect(view.worst).toBeNull();
    expect(view.lost).toBe(0);
    expect(view.nonZero).toBe(0);
    expect(view.counters).toBeGreaterThan(0);
  });
});

describe("a counter's share of its group", () => {
  it("is measured against the group rather than against everything lost", () => {
    const view = schedulerView(slot({ blocked_threads: 71, blocked_conflicts: 17, retried: 7 }));
    const schedule = view.groups.find((g) => g.key === "schedule")!;
    expect(shareOfGroup(schedule, schedule.rows[0])).toBeCloseTo(71 / 95, 10);
  });

  it("is nought in a group that lost nothing, rather than a division by it", () => {
    const view = schedulerView(slot());
    const buffer = view.groups.find((g) => g.key === "buffer")!;
    expect(shareOfGroup(buffer, buffer.rows[0])).toBe(0);
  });
});
