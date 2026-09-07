import { describe, expect, it } from "vitest";
import {
  admittedShare,
  doorSection,
  epochSpanLabel,
  executedSection,
  listenerSection,
  portNamed,
  portsBusiestFirst,
  refusedTable,
  stakedShare,
  streamSection,
  verifySection,
  type PathLoss,
} from "./tpuPath";
import type { EpochSpan, ExecutedStage, QuicPort } from "./types";

/** A port with nothing happening on it, to be overridden a field at a time. */
function quiet(over: Partial<QuicPort> = {}): QuicPort {
  return {
    name: "tpu",
    offered: 0,
    shed_all: 0,
    shed_address: 0,
    refused_full: 0,
    handshake_timeout: 0,
    handshake_error: 0,
    handshook: 0,
    add_failed: 0,
    add_failed_staked: 0,
    add_failed_unstaked: 0,
    add_failed_banned: 0,
    admitted_staked: 0,
    admitted_unstaked: 0,
    streams: 0,
    throttled_staked: 0,
    throttled_unstaked: 0,
    read_timeouts: 0,
    read_errors: 0,
    invalid_size: 0,
    handed_on: 0,
    bytes_handed_on: 0,
    queue_full: 0,
    disconnected: 0,
    open: 0,
    active_streams: 0,
    kernel_drops: null,
    ...over,
  };
}

/** A busy mainnet-shaped five minutes, with the gates accounting for the offer. */
const BUSY = quiet({
  offered: 18_420,
  shed_all: 2_140,
  shed_address: 6_880,
  refused_full: 412,
  handshake_timeout: 1_205,
  handshake_error: 338,
  // 18_420 offered, less the 10_975 shed or failed above. Everything that
  // handshook was then refused a table place or admitted, so this sample has
  // nothing falling into either uncounted branch.
  handshook: 7_445,
  add_failed: 7,
  admitted_staked: 1_890,
  admitted_unstaked: 5_548,
  streams: 42_880,
  throttled_unstaked: 3_412,
  read_timeouts: 288,
  read_errors: 41,
  invalid_size: 12,
  handed_on: 38_220,
  queue_full: 40,
  open: 1_284,
  active_streams: 46,
});

function keys(losses: PathLoss[]): string[] {
  return losses.map((loss) => loss.key);
}

describe("doorSection", () => {
  it("draws every gate against what was offered", () => {
    const section = doorSection(BUSY, null);
    expect(section.total).toBe(18_420);
    expect(section.through.count).toBe(7_438);
    const shed = section.losses.find(
      (loss) => loss.key === "door_shed_address",
    );
    expect(shed?.share).toBeCloseTo(6880 / 18420, 10);
  });

  it("lists the losses largest first rather than in the order they happen", () => {
    // The bar carries the order a connection meets the gates, which is the one
    // place it can be read without arithmetic. The list is spent on which of
    // them mattered instead.
    expect(keys(doorSection(BUSY, null).losses)).toEqual([
      "door_shed_address",
      "door_shed_all",
      "door_handshake_timeout",
      "door_refused_full",
      "door_handshake_error",
      "door_add_failed",
    ]);
  });

  it("leaves out the gates that did not fire and counts them instead", () => {
    // A counter at nought is worth knowing and a row of nought is not worth
    // the height. The figure keeps the statement.
    const section = doorSection(
      quiet({ offered: 10, admitted_unstaked: 8, shed_all: 2, handshook: 8 }),
      null,
    );
    expect(keys(section.losses)).toEqual(["door_shed_all"]);
    // Five gates and the four names a refusal is counted under. The two
    // unaccounted rows are derived, so they are not counters and not tallied.
    expect(section.zeros).toBe(9);
  });

  it("caps a gate that ran past the offer rather than reporting over a whole", () => {
    // Which happens: the rate limiter is checked a second time after the
    // handshake, so a connection can be charged to a gate it already passed.
    const section = doorSection(
      quiet({ offered: 100, shed_address: 140 }),
      null,
    );
    expect(section.losses[0].share).toBe(1);
  });

  it("keeps the kernel's datagrams out of the bar and out of the shares", () => {
    // Counted in datagrams while the bar counts connections. A datagram the
    // kernel threw away never became a connection attempt, so it is not a
    // share of anything in this section.
    const section = doorSection(BUSY, 512);
    expect(section.aside?.count).toBe(512);
    expect(section.aside?.unit).toBe("datagrams");
    expect(keys(section.losses)).not.toContain("door_kernel");
  });

  it("has no such line where the port was never found among the sockets", () => {
    // Absent is not nought. A validator behind a port forward binds a port it
    // does not advertise, and a zero would report a clean floor under a door
    // that is being hammered.
    expect(doorSection(BUSY, null).aside).toBeNull();
  });
});

describe("the connections nothing accounted for", () => {
  // The question that started this: a vote port offered 317 connections,
  // admitted 18, and every gate the listener counts read nought. The 299 are
  // real and the listener does not say where they went.
  const VOTE = { offered: 317, admitted_staked: 18 };

  it("puts them before the handshake where few connections reached one", () => {
    const section = doorSection(quiet({ ...VOTE, handshook: 18 }), null);
    const loss = section.losses.find((l) => l.key === "door_unaccounted_pre");
    expect(loss?.count).toBe(299);
    expect(keys(section.losses)).not.toContain("door_unaccounted_post");
  });

  it("puts them after it where they all reached one", () => {
    // The same 299 and a completely different thing to go and read: these
    // peers completed a handshake and were then dropped by admission control
    // without a word.
    const section = doorSection(quiet({ ...VOTE, handshook: 317 }), null);
    const loss = section.losses.find((l) => l.key === "door_unaccounted_post");
    expect(loss?.count).toBe(299);
    expect(loss?.share).toBeCloseTo(299 / 317, 10);
    expect(keys(section.losses)).not.toContain("door_unaccounted_pre");
  });

  it("says nothing where the listener accounted for everything", () => {
    // Neither row on a clean port, rather than two rows of nought. The pair
    // exists to measure a silence and there is no silence to measure.
    const shown = keys(doorSection(BUSY, null).losses);
    expect(shown).not.toContain("door_unaccounted_pre");
    expect(shown).not.toContain("door_unaccounted_post");
  });

  it("never runs negative when a gate is charged twice", () => {
    // The rate limiter is checked again after the handshake, so the gates can
    // total more than the offer and the subtraction can go the wrong way.
    const section = doorSection(
      quiet({
        offered: 100,
        shed_address: 140,
        handshook: 10,
        admitted_staked: 10,
      }),
      null,
    );
    expect(keys(section.losses)).not.toContain("door_unaccounted_pre");
  });
});

describe("refusedTable", () => {
  it("takes the larger reading rather than adding the overlapping names", () => {
    // An unstaked peer turned away raises both of these, being one refusal
    // counted twice. Added they would report forty refusals for twenty.
    expect(
      refusedTable(quiet({ add_failed: 20, add_failed_unstaked: 20 })),
    ).toBe(20);
  });

  it("adds the three that are mutually exclusive with each other", () => {
    // Different match arms of the listener, so no connection reaches two.
    const q = quiet({
      add_failed_staked: 3,
      add_failed_unstaked: 9,
      add_failed_banned: 2,
    });
    expect(refusedTable(q)).toBe(14);
  });

  it("keeps the one name the vote port has where the others are silent", () => {
    // The vote listener raises none of the stake-weighted names, so on that
    // port this counter is the only refusal signal there is.
    expect(refusedTable(quiet({ add_failed: 6 }))).toBe(6);
  });

  it("lists the names it did not add, and drops the ones at nought", () => {
    const section = doorSection(
      quiet({
        offered: 100,
        handshook: 40,
        add_failed: 20,
        add_failed_unstaked: 20,
      }),
      null,
    );
    expect(keys(section.detail)).toEqual([
      "door_add_failed_unstaked",
      "door_add_failed_insert",
    ]);
    expect(section.detail[0].share).toBeCloseTo(1, 10);
    expect(keys(section.losses)).toContain("door_add_failed");
  });
});

describe("streamSection", () => {
  it("takes what was carried as the streams less what was lost", () => {
    const section = streamSection(BUSY);
    expect(section.total).toBe(42_880);
    expect(section.through.count).toBe(42_880 - 3_412 - 288 - 41 - 12);
  });

  it("marks a staked throttle and leaves an unstaked one unmarked", () => {
    // Throttling unstaked traffic is the limiter working. Throttling staked
    // traffic is the limiter biting on what it is meant to favour.
    const section = streamSection(
      quiet({ streams: 100, throttled_staked: 5, throttled_unstaked: 9 }),
    );
    expect(
      section.losses.find((loss) => loss.key === "stream_throttled_staked")
        ?.warn,
    ).toBe(true);
    expect(
      section.losses.find((loss) => loss.key === "stream_throttled_unstaked")
        ?.warn,
    ).toBe(false);
  });
});

describe("listenerSection", () => {
  it("makes the total from the outcomes, having no count of its own", () => {
    const section = listenerSection(
      quiet({ handed_on: 900, queue_full: 80, disconnected: 20 }),
    );
    expect(section.total).toBe(1000);
    expect(section.through.count).toBe(900);
    expect(section.losses[0].share).toBeCloseTo(0.08, 10);
  });

  it("marks both of its losses, since either means something was already in", () => {
    const section = listenerSection(
      quiet({ handed_on: 10, queue_full: 1, disconnected: 1 }),
    );
    expect(section.losses.every((loss) => loss.warn)).toBe(true);
  });
});

describe("verifySection", () => {
  const stage = {
    received: 39_100,
    duplicate: 8_200,
    below_floor: 0,
    verified: 30_480,
  };

  it("works the bad signatures out of what is left over", () => {
    const section = verifySection({ ...stage, evicted_batches: 0 });
    expect(
      section.losses.find((loss) => loss.key === "verify_bad")?.count,
    ).toBe(420);
  });

  it("keeps the evicted batches out of the bar, being a different unit", () => {
    // A batch carries however many transactions were grouped into it and
    // nothing reports that number, so it can be neither added to nor
    // subtracted from the counts beside it.
    const section = verifySection({ ...stage, evicted_batches: 3 });
    expect(section.aside?.count).toBe(3);
    expect(section.aside?.unit).toContain("batches");
    expect(keys(section.losses)).not.toContain("verify_evicted");
  });

  it("says nothing at all where no batch was evicted", () => {
    expect(verifySection({ ...stage, evicted_batches: 0 }).aside).toBeNull();
  });
});

describe("executedSection", () => {
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

  it("keeps the load reasons out of the losses and out of the bar", () => {
    // They roll up into the row above them rather than sitting beside it, so
    // drawing them in the bar would count the same transactions twice.
    const section = executedSection(
      stage({
        attempted: 1000,
        processed: 900,
        succeeded: 850,
        blockhash_old: 60,
      }),
      null,
    );
    expect(keys(section.losses)).not.toContain("exec_blockhash_old");
    expect(keys(section.detail)).toContain("exec_blockhash_old");
  });

  it("draws a load reason against the loads that failed, not everything attempted", () => {
    const section = executedSection(
      stage({
        attempted: 1000,
        processed: 900,
        succeeded: 900,
        blockhash_old: 100,
      }),
      null,
    );
    const reason = section.detail.find(
      (loss) => loss.key === "exec_blockhash_old",
    );
    expect(reason?.share).toBeCloseTo(1, 10);
  });

  it("notes the bundles on the heading without making them a share of the bar", () => {
    // Their transactions are already inside the figures beside it, so a
    // segment or a percentage would be counting the same work twice. It is a
    // statement about what the bar is made of, in a unit the bar is not drawn
    // in, which is what an aside is for.
    const section = executedSection(
      stage({ attempted: 1000, processed: 900, succeeded: 850 }),
      { received: 1284, packets: 4617 },
    );
    expect(section.aside?.label).toContain("1,284 bundles");
    expect(section.aside?.count).toBe(4617);
    expect(section.aside?.unit).toBe("transactions");
    expect(section.aside?.warn).toBe(false);
    expect(keys(section.losses)).not.toContain("exec_bundles");
    expect(keys(section.detail)).not.toContain("exec_bundles");
    expect(section.total).toBe(1000);
  });

  it("says nothing at all where no bundle arrived", () => {
    // Not nought. A validator with no block engine, or one running BAM, has
    // no bundle stage reporting rather than a bundle stage reporting nothing,
    // and the section is left exactly as it stands.
    const section = executedSection(
      stage({ attempted: 1000, processed: 900, succeeded: 850 }),
      null,
    );
    expect(section.aside).toBeNull();
  });

  it("counts a load reason at nought among the quiet counters", () => {
    const section = executedSection(
      stage({ attempted: 10, processed: 10, succeeded: 10 }),
      null,
    );
    expect(section.detail).toHaveLength(0);
    expect(section.zeros).toBeGreaterThanOrEqual(12);
  });
});

describe("the headline", () => {
  it("is the admitted share of the offer", () => {
    expect(admittedShare(BUSY)).toBeCloseTo((1890 + 5548) / 18420, 10);
  });

  it("is absent on a port nothing has used", () => {
    // Not nought. A port nobody has tried is not a port refusing everyone.
    expect(admittedShare(quiet())).toBeNull();
  });

  it("caps at everything, so a second window's admissions cannot exceed it", () => {
    expect(admittedShare(quiet({ offered: 10, admitted_staked: 14 }))).toBe(1);
  });

  it("reads the staked share against what was admitted, not what was offered", () => {
    expect(stakedShare(BUSY)).toBeCloseTo(1890 / (1890 + 5548), 10);
    expect(stakedShare(quiet())).toBeNull();
  });
});

describe("picking a port out of the list", () => {
  it("finds one by name and returns nothing for a port the node has not got", () => {
    const ports = [quiet({ name: "tpu" }), quiet({ name: "tpu forwards" })];
    expect(portNamed(ports, "tpu forwards")?.name).toBe("tpu forwards");
    expect(portNamed(ports, "tpu vote quic")).toBeNull();
  });
});

describe("ordering the folded ports", () => {
  it("puts the busiest first, whichever port that turns out to be", () => {
    // Behind a relayer the TPU port is the quiet one and the vote port carries
    // everything this host still sees. Sent in a fixed order, that would bury
    // the only row with anything on it under two rows of nought.
    const ports = [
      quiet({ name: "tpu", offered: 1 }),
      quiet({ name: "tpu forwards", offered: 1 }),
      quiet({ name: "tpu vote quic", offered: 296 }),
    ];
    expect(portsBusiestFirst(ports).map((port) => port.name)).toEqual([
      "tpu vote quic",
      "tpu",
      "tpu forwards",
    ]);
  });

  it("leaves ties in the order they were sent, so rows do not swap while read", () => {
    const ports = [
      quiet({ name: "tpu", offered: 4 }),
      quiet({ name: "tpu forwards", offered: 4 }),
      quiet({ name: "tpu vote quic", offered: 4 }),
    ];
    expect(portsBusiestFirst(ports).map((port) => port.name)).toEqual([
      "tpu",
      "tpu forwards",
      "tpu vote quic",
    ]);
  });

  it("does not reorder the list it was given", () => {
    const ports = [
      quiet({ name: "tpu", offered: 1 }),
      quiet({ name: "tpu vote quic", offered: 9 }),
    ];
    portsBusiestFirst(ports);
    expect(ports.map((port) => port.name)).toEqual(["tpu", "tpu vote quic"]);
  });
});

describe("each section is drawn against its own total", () => {
  it("does not measure one stage against another's denominator", () => {
    // The whole reason these are separate sections. The listener handing on
    // nine hundred and verify receiving a thousand is ordinary, being measured
    // either side of the fetch stage's buffering, and the bars must not imply
    // that verify received more than everything.
    const listener = listenerSection(quiet({ handed_on: 900 }));
    const verify = verifySection({
      received: 1000,
      duplicate: 0,
      below_floor: 0,
      verified: 1000,
      evicted_batches: 0,
    });
    expect(listener.through.count / listener.total).toBe(1);
    expect(verify.through.count / verify.total).toBe(1);
  });
});

describe("the span the per-epoch sections are counted over", () => {
  const span = (over: Partial<EpochSpan> = {}): EpochSpan => ({
    epoch: 842,
    elapsed_slots: 264_000,
    counted_slots: 264_000,
    slots_in_epoch: 432_000,
    ...over,
  });

  it("says how far into the epoch the figures reach", () => {
    expect(epochSpanLabel(span())).toBe("Epoch 842, 61% elapsed");
  });

  it("says where counting began where the validator came up part way in", () => {
    // Without this a node that started an hour ago and one that sat idle all
    // epoch read exactly alike, and the second is a fault.
    expect(epochSpanLabel(span({ counted_slots: 21_600 }))).toBe(
      "Epoch 842, 61% elapsed · counted from 56%",
    );
  });

  it("does not caveat an epoch that was only missed by the tick that noticed it", () => {
    // The totals start over on the first tick that reads a bank in the new
    // epoch, a second or two after it turned. A caveat that is always there is
    // one nobody reads when it matters.
    expect(epochSpanLabel(span({ counted_slots: 263_995 }))).toBe(
      "Epoch 842, 61% elapsed",
    );
  });

  it("names the epoch and nothing else where the schedule gives no length", () => {
    // Nought slots in an epoch is not a state the chain reaches, but it is one
    // a division would turn into an infinity printed as a percentage.
    expect(epochSpanLabel(span({ slots_in_epoch: 0 }))).toBe("Epoch 842");
  });
});
