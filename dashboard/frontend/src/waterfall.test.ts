import { describe, expect, it } from "vitest";
import type { ExecutedStage, VerifyStage, Waterfall } from "./types";
import { executedRows, verifyRows, waterfallRows } from "./waterfall";

/** A window in which nothing happened, to be overridden a field at a time. */
function quiet(over: Partial<Waterfall> = {}): Waterfall {
  return {
    received: 0,
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
    buffered: 0,
    queue_full: 0,
    nonce_evicted: 0,
    cleared: 0,
    cleaned: 0,
    scheduled: 0,
    blocked_conflicts: 0,
    blocked_threads: 0,
    finished: 0,
    retried: 0,
    ...over,
  };
}

/** A validator mid-leader-slot, with the receive stretch balancing exactly. */
function busy(): Waterfall {
  return quiet({
    received: 1000,
    not_held: 800,
    check_queue_full: 10,
    unparsable: 20,
    bad_locks: 5,
    compute_budget: 5,
    too_old: 30,
    already_processed: 40,
    fee_payer: 6,
    filtered: 0,
    nonce_conflict: 4,
    buffered: 80,
    queue_full: 12,
    nonce_evicted: 1,
    cleared: 7,
    cleaned: 2,
    scheduled: 60,
    blocked_conflicts: 25,
    blocked_threads: 3,
    finished: 58,
    retried: 2,
  });
}

describe("waterfallRows", () => {
  it("accounts for every received transaction across the first stretch", () => {
    // The identity the validator's own tests assert, restated here because it
    // is what makes the top section an account rather than a list of numbers
    // that happen to sit under a heading. If a counter is ever added upstream
    // and not mapped, this is what notices.
    const w = busy();
    const rows = waterfallRows(w);
    const upToBuffered = rows.slice(
      rows.findIndex((row) => row.key === "not_held"),
      rows.findIndex((row) => row.key === "buffered") + 1,
    );
    const accounted = upToBuffered.reduce((sum, row) => sum + row.count, 0);
    expect(accounted).toBe(w.received);
  });

  it("measures every bar against what arrived, not against the stage above", () => {
    // So the lengths are comparable the whole way down. Renormalising each
    // section against its own heading would draw a loss of eighty out of eighty
    // as long as the received bar itself.
    const rows = waterfallRows(busy());
    const buffered = rows.find((row) => row.key === "buffered");
    expect(buffered?.share).toBeCloseTo(0.08, 10);
    const notHeld = rows.find((row) => row.key === "not_held");
    expect(notHeld?.share).toBeCloseTo(0.8, 10);
  });

  it("draws the same rows in the same order whatever happened", () => {
    // The card must not change height under someone reading it, and a zero is
    // itself worth reading — no fee payer failed is not the same as nothing
    // counts fee payer failures.
    const busyKeys = waterfallRows(busy()).map((row) => row.key);
    const quietKeys = waterfallRows(quiet()).map((row) => row.key);
    expect(quietKeys).toEqual(busyKeys);
    expect(busyKeys.length).toBe(21);
  });

  it("divides nothing by nothing when the window is empty", () => {
    for (const row of waterfallRows(quiet())) {
      expect(Number.isFinite(row.share)).toBe(true);
      expect(row.share).toBe(0);
    }
  });

  it("marks the three stages apart from their reasons", () => {
    const rows = waterfallRows(busy());
    const stages = rows.filter((row) => row.kind === "stage").map((row) => row.key);
    expect(stages).toEqual(["received", "buffered", "scheduled", "finished"]);
  });

  it("counts held-back work as a note rather than a loss", () => {
    // Nothing is lost when the scheduler cannot place a transaction this pass;
    // it waits. Counting it as loss would make a contended slot look like a
    // failing one.
    const rows = waterfallRows(busy());
    const blocked = rows.filter((row) => row.key.startsWith("blocked_"));
    expect(blocked.map((row) => row.kind)).toEqual(["note", "note"]);
    expect(rows.find((row) => row.key === "retried")?.kind).toBe("note");
  });
});

describe("a slot BAM built", () => {
  /** What BAM reports: batches in, transactions from there down. */
  function bamSlot(): Waterfall {
    return quiet({
      source: "bam",
      received: 40,
      // Fed by a different check on this path: batches past their own slot.
      not_held: 5,
      unparsable: 3,
      buffered: 700,
      scheduled: 738,
      finished: 735,
    });
  }

  it("does not draw the rows against a count of batches", () => {
    // The bug this guards. BAM is sent atomic batches and counts them, so
    // drawing 735 finished transactions against 40 received batches puts every
    // row past a hundred percent and reports nothing at all.
    const rows = waterfallRows(bamSlot());
    const parsed = rows.find((r) => r.key === "unparsable")!;
    expect(parsed.share).toBeCloseTo(3 / 700, 5);

    // Against the batch count 735 finished would have been eighteen times the
    // total. Against what parsed it is a slot that dispatched a little more
    // than arrived in it, which is the ordinary reading of a queue that holds
    // work across slots, and gets the ordinary treatment: the count, no
    // percentage.
    const finished = rows.find((r) => r.key === "finished")!;
    expect(finished.over).toBe(true);
    expect(finished.share).toBe(1);
  });

  it("shows the batch count without a share of the transactions", () => {
    const received = waterfallRows(bamSlot()).find((r) => r.key === "received")!;
    expect(received.label).toBe("Batches received");
    expect(received.kind).toBe("count");
    expect(received.count).toBe(40);
    // No share, because there is no total it is a share of.
    expect(received.share).toBe(0);
    expect(received.over).toBe(false);
  });

  it("does not call a late batch a forwarded transaction", () => {
    // Same counter, different check, different unit, different meaning. On
    // this path it holds batches BAM sent that had already missed the slot
    // they named — the one figure on a BAM slot worth acting on, and the last
    // thing that should read as ordinary forwarding.
    const notHeld = waterfallRows(bamSlot()).find((r) => r.key === "not_held")!;
    expect(notHeld.label).toBe("batches too late to schedule");
    expect(notHeld.kind).toBe("count");
    expect(notHeld.count).toBe(5);
    expect(notHeld.share).toBe(0);
  });

  it("changes nothing at all for a validator not running BAM", () => {
    // The whole BAM branch hangs off one field. A stock validator never sends
    // it, and a jito validator sends "scheduler" whenever BAM is not the one
    // building — so the ordinary reading has to survive both spellings
    // untouched, row for row.
    const numbers = { received: 1000, not_held: 5, buffered: 700, finished: 500 };
    const absent = waterfallRows(quiet(numbers));
    const named = waterfallRows(quiet({ ...numbers, source: "scheduler" }));
    expect(named).toEqual(absent);

    // And it is the same reading it always was: every row a share of received.
    expect(absent.find((r) => r.key === "received")!.label).toBe("Received");
    expect(absent.find((r) => r.key === "not_held")!.label).toBe("forwarding, not held");
    expect(absent.every((r) => r.kind !== "count" || r.key === "verify_evicted")).toBe(true);
    expect(absent.find((r) => r.key === "buffered")!.share).toBeCloseTo(0.7, 5);
  });

  it("leaves a slot the validator built alone", () => {
    // Same numbers, no source: the ordinary reading, drawn against received.
    const rows = waterfallRows(
      quiet({ received: 1000, not_held: 5, buffered: 700, finished: 500 }),
    );
    const received = rows.find((r) => r.key === "received")!;
    expect(received.label).toBe("Received");
    expect(received.kind).toBe("stage");
    const notHeld = rows.find((r) => r.key === "not_held")!;
    expect(notHeld.label).toBe("forwarding, not held");
    expect(notHeld.kind).toBe("loss");
    expect(notHeld.share).toBeCloseTo(5 / 1000, 5);
    expect(rows.find((r) => r.key === "finished")!.share).toBeCloseTo(0.5, 5);
  });
});

/** Finds one row by key, so a test names what it is asserting on. */
function rowOf(rows: ReturnType<typeof waterfallRows>, key: string) {
  const row = rows.find((r) => r.key === key);
  if (!row) throw new Error(`no row ${key}`);
  return row;
}

describe("verifyRows", () => {
  const stage = (over: Partial<VerifyStage> = {}): VerifyStage => ({
    received: 0,
    duplicate: 0,
    below_floor: 0,
    verified: 0,
    evicted_batches: 0,
    ...over,
  });

  it("derives bad signatures from what the other outcomes leave over", () => {
    // There is no counter for it. Sigverify stops at the first thing that
    // discards a packet, so each one is deduplicated, or below the floor, or
    // verified, or bad, and the remainder is exactly the bad ones.
    const rows = verifyRows(
      stage({ received: 1000, duplicate: 300, below_floor: 50, verified: 620 }),
    );
    expect(rowOf(rows, "verify_bad").count).toBe(30);
  });

  it("never reports a negative count when the parts do not line up", () => {
    // The four figures are swapped to zero as they are reported and the tap
    // accumulates whatever it is given, so a point arriving mid-reset can put
    // the parts above the total. A negative row would be nonsense on screen.
    const rows = verifyRows(stage({ received: 100, duplicate: 90, verified: 40 }));
    expect(rowOf(rows, "verify_bad").count).toBe(0);
  });

  it("keeps the batch figure out of the transaction arithmetic", () => {
    // It counts batches. Subtracting it from a packet count, or adding it in,
    // would be mixing two units.
    const rows = verifyRows(
      stage({ received: 100, duplicate: 0, verified: 100, evicted_batches: 7 }),
    );
    expect(rowOf(rows, "verify_bad").count).toBe(0);
    expect(rowOf(rows, "verify_evicted").count).toBe(7);
    expect(rowOf(rows, "verify_evicted").kind).toBe("count");
    // And carries no share, for the same reason it is labelled apart.
    expect(rowOf(rows, "verify_evicted").share).toBe(0);
  });
});

describe("executedRows", () => {
  const stage = (over: Partial<ExecutedStage> = {}): ExecutedStage => ({
    attempted: 0,
    cost_throttled: 0,
    retryable: 0,
    expired_bank: 0,
    processed: 0,
    succeeded: 0,
    too_many_locks: 0,
    account_missing: 0,
    fee_payer_broke: 0,
    fee_payer_invalid: 0,
    blockhash_missing: 0,
    blockhash_old: 0,
    already_processed: 0,
    bad_compute_budget: 0,
    account_data_too_large: 0,
    program_not_executable: 0,
    program_restricted: 0,
    ...over,
  });

  it("accounts for everything the workers took up", () => {
    // The reading from testnet that started this: a hundred and one attempted,
    // thirteen handed back, sixty-three committed, and twenty-five in no row at
    // all — a quarter of the section, under a footnote promising it added up.
    const rows = executedRows(
      stage({ attempted: 101, retryable: 13, processed: 63, succeeded: 63 }),
    );
    const dropped = rowOf(rows, "exec_dropped");
    expect(dropped.count).toBe(25);
    expect(dropped.share).toBeCloseTo(25 / 101, 10);

    // With no reasons reported, the whole of it falls to the gathered row
    // rather than vanishing.
    expect(rowOf(rows, "exec_other_reasons").count).toBe(25);
  });

  it("names the reasons it has and gathers the rest", () => {
    const rows = executedRows(
      stage({
        attempted: 100,
        retryable: 10,
        processed: 60,
        succeeded: 60,
        blockhash_missing: 12,
        fee_payer_broke: 8,
        already_processed: 4,
      }),
    );
    expect(rowOf(rows, "exec_dropped").count).toBe(30);
    expect(rowOf(rows, "exec_blockhash_missing").count).toBe(12);
    expect(rowOf(rows, "exec_fee_payer_broke").count).toBe(8);
    // Thirty lost, twenty-four named, six left over.
    expect(rowOf(rows, "exec_other_reasons").count).toBe(6);
  });

  it("does not go negative when the two points disagree", () => {
    // The outcomes and the reasons are reported separately, so a window can
    // catch more reasons than it caught loss. Nought, not a negative row.
    const rows = executedRows(
      stage({ attempted: 10, retryable: 0, processed: 10, succeeded: 10, account_missing: 4 }),
    );
    expect(rowOf(rows, "exec_dropped").count).toBe(0);
    expect(rowOf(rows, "exec_other_reasons").count).toBe(0);
  });

  it("derives the failures from committed less succeeded", () => {
    // A transaction that returns an error still lands in the block and still
    // pays, so this is a real row rather than a loss to be hidden.
    const rows = executedRows(stage({ attempted: 100, processed: 90, succeeded: 80 }));
    expect(rowOf(rows, "exec_failed").count).toBe(10);
  });

  it("does not go negative if succeeded outruns committed across a reset", () => {
    const rows = executedRows(stage({ attempted: 10, processed: 5, succeeded: 9 }));
    expect(rowOf(rows, "exec_failed").count).toBe(0);
  });
});

describe("a stage fed from the queue", () => {
  it("caps its bar and reports the overflow rather than exceeding the total", () => {
    // Routine over a single slot. The scheduler's queue holds transactions
    // across slots, so a slot can dispatch more than arrived in it by taking
    // the difference from what was already waiting. Twelve received and
    // thirteen scheduled is the case that was reported as a broken figure.
    const rows = waterfallRows(quiet({ received: 12, buffered: 12, scheduled: 13, finished: 13 }));

    const scheduled = rowOf(rows, "scheduled");
    expect(scheduled.count).toBe(13);
    expect(scheduled.share).toBe(1);
    expect(scheduled.over).toBe(true);
  });

  it("leaves a stage inside its total alone", () => {
    const rows = waterfallRows(quiet({ received: 100, buffered: 40 }));
    const buffered = rowOf(rows, "buffered");
    expect(buffered.share).toBeCloseTo(0.4, 10);
    expect(buffered.over).toBe(false);
  });

  it("reports nothing rather than dividing when the stage saw no traffic", () => {
    const rows = waterfallRows(quiet());
    expect(rowOf(rows, "received").share).toBe(0);
    expect(rowOf(rows, "received").over).toBe(false);
  });
});
