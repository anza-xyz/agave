import { describe, expect, it } from "vitest";
import { leaderAt } from "./schedule";
import {
  entriesOf,
  HAS_BLOCK,
  HAS_CLOCK,
  HAS_REPLAY,
  HAS_REPLAYED,
  HAS_SHREDS,
  HAS_TIPS,
  type SlotRange,
  type WireRow,
} from "./slotHistory";
import type { EpochInfo } from "./types";

const ALICE = "A1ice1111111111111111111111111111111111111";
const BOB = "B0b22222222222222222222222222222222222222222";

function epochOf(over: Partial<EpochInfo> = {}): EpochInfo {
  return {
    epoch: 842,
    start_slot: 1000,
    end_slot: 1015,
    slots_in_epoch: 16,
    my_leader_slots: [],
    leaders: [ALICE, BOB],
    // Four turns of four slots: Alice, Bob, Alice, Bob.
    turns: [0, 1, 0, 1],
    block_cost_limit: 60_000_000,
    account_cost_limit: 12_000_000,
    ...over,
  };
}

function row(over: Partial<Record<number, number>> = {}): WireRow {
  const base: WireRow = [
    3,
    HAS_BLOCK | HAS_CLOCK | HAS_TIPS | HAS_REPLAY | HAS_SHREDS | HAS_REPLAYED,
    66,
    8_752,
    11_877_602,
    44_100_000,
    12_480,
    7_400_000,
    1_000_000,
    47_200,
    1_203,
    63,
    341,
    393,
  ];
  return base.map((value, index) => over[index] ?? value) as WireRow;
}

describe("leaderAt", () => {
  it("finds the leader through one index rather than a search", () => {
    expect(leaderAt(epochOf(), 1000)).toBe(ALICE);
    expect(leaderAt(epochOf(), 1003)).toBe(ALICE);
    expect(leaderAt(epochOf(), 1004)).toBe(BOB);
    expect(leaderAt(epochOf(), 1015)).toBe(BOB);
  });

  it("names nobody outside the epoch the arrays describe", () => {
    // Two epochs are in play either side of a boundary, and answering from the
    // wrong one would name a leader confidently and wrongly.
    expect(leaderAt(epochOf(), 999)).toBeNull();
    expect(leaderAt(epochOf(), 1016)).toBeNull();
  });

  it("names nobody where the validator could not derive the schedule", () => {
    // Sent as an empty array rather than a partial one, so this is the whole
    // of the check.
    expect(leaderAt(epochOf({ turns: [] }), 1000)).toBeNull();
    expect(leaderAt(undefined, 1000)).toBeNull();
  });
});

describe("entriesOf", () => {
  const range = (rows: (WireRow | null)[]): SlotRange => ({ first_slot: 1000, rows });

  it("reads the columns in the order the validator writes them", () => {
    // The one place the wire order is pinned on this side. It is positional, so
    // a silent reordering would put fees in the compute column and nothing
    // would fail until someone read the page.
    const [entry] = entriesOf(range([row()]), epochOf(), undefined);
    expect(entry.level).toBe("rooted");
    expect(entry.block?.non_vote_transactions).toBe(8_752);
    expect(entry.block?.transactions).toBe(66 + 8_752);
    expect(entry.block?.block_cost).toBe(11_877_602);
    expect(entry.block?.total_fees).toBe(44_100_000);
    expect(entry.block?.priority_fees).toBe(12_480);
    expect(entry.block?.tips).toBe(7_400_000);
    expect(entry.block?.replay_micros).toBe(47_200);
    expect(entry.shreds).toEqual({ count: 1_203, repaired: 63, full_millis: 341 });
    expect(entry.replayed_millis).toBe(393);
  });

  it("leaves the arrival and the replay end absent where neither was seen", () => {
    // A fill reported before the dashboard was watching, and a block this
    // validator built, both read as nothing rather than as noughts.
    const [entry] = entriesOf(
      range([row({ 1: HAS_BLOCK | HAS_CLOCK, 10: 1_203, 13: 393 })]),
      epochOf(),
      undefined,
    );
    expect(entry.shreds).toBeNull();
    expect(entry.replayed_millis).toBeNull();
  });

  it("carries an arrival for a slot that filled and never froze", () => {
    // A dead slot has shreds and no block, which is why the arrival is on the
    // entry rather than inside the block.
    const [entry] = entriesOf(range([row({ 1: HAS_CLOCK | HAS_SHREDS })]), epochOf(), undefined);
    expect(entry.block).toBeNull();
    expect(entry.shreds?.count).toBe(1_203);
  });

  it("leaves replay time absent for a block replay never timed", () => {
    // Our own blocks are built rather than replayed, and read as absent rather
    // than as a slot replayed in no time.
    const [entry] = entriesOf(
      range([row({ 1: HAS_BLOCK | HAS_CLOCK, 9: 47_200 })]),
      epochOf(),
      undefined,
    );
    expect(entry.block?.replay_micros).toBeNull();
  });

  it("keeps a tip figure that was never measured apart from one that was nought", () => {
    // The reason for the third flag bit. A turn the searchers passed by is
    // worth drawing; a turn measured on a bank with no parent is not the same
    // thing and must not draw as nought.
    const measured = entriesOf(range([row({ 7: 0 })]), epochOf(), undefined);
    expect(measured[0].block?.tips).toBe(0);

    const unmeasured = entriesOf(
      range([row({ 1: HAS_BLOCK | HAS_CLOCK, 7: 7_400_000 })]),
      epochOf(),
      undefined,
    );
    expect(unmeasured[0].block?.tips).toBeNull();
  });

  it("carries the two kinds of fee apart, so the split survives the trip back", () => {
    // Base is the subtraction. The row carried only the total until the
    // schedule page started drawing them separately, and the split appeared
    // live and vanished in history.
    const [entry] = entriesOf(range([row()]), epochOf(), undefined);
    const base = (entry.block?.total_fees ?? 0) - (entry.block?.priority_fees ?? 0);
    expect(base).toBe(44_087_520);
  });

  it("takes the cost limits from the epoch rather than from the row", () => {
    // They are the same two numbers for the epoch's whole life, which is why
    // they are not on the row at all.
    const [entry] = entriesOf(range([row()]), epochOf(), undefined);
    expect(entry.block?.block_cost_limit).toBe(60_000_000);
  });

  it("works the duration out as the gap to the last slot that had a clock", () => {
    // Not carried, because it is a subtraction of two things that are.
    const entries = entriesOf(
      range([row({ 8: 1_000_000 }), row({ 8: 1_000_400 })]),
      epochOf(),
      undefined,
    );
    expect(entries[0].duration_nanos).toBeNull();
    expect(entries[1].duration_nanos).toBe(400_000_000);
    // The clock itself travels too, for stamping a turn.
    expect(entries[0].time_millis).toBe(1_000_000);
    expect(entries[1].time_millis).toBe(1_000_400);
  });

  it("carries the gap across a slot it has no row for", () => {
    // A skipped slot shows as one long interval rather than as none, which is
    // what the validator's own walk does with it.
    const entries = entriesOf(
      range([row({ 8: 1_000_000 }), null, row({ 8: 1_000_800 })]),
      epochOf(),
      undefined,
    );
    expect(entries).toHaveLength(2);
    expect(entries[1].slot).toBe(1002);
    expect(entries[1].duration_nanos).toBe(800_000_000);
  });

  it("leaves a block out where none was recorded, rather than drawing an empty one", () => {
    const [entry] = entriesOf(range([row({ 1: HAS_CLOCK })]), epochOf(), undefined);
    expect(entry.block).toBeNull();
  });

  it("marks the slots we led", () => {
    const entries = entriesOf(range([row(), null, null, null, row()]), epochOf(), ALICE);
    expect(entries[0].mine).toBe(true);
    expect(entries[1].mine).toBe(false);
  });

  it("says nothing about who led, only whether we did", () => {
    // Naming is no longer this module's job. A fetched slot carries `mine` and
    // the page resolves the leader itself, the same way it does for a live one,
    // so the two agree by construction rather than by both being told.
    const [entry] = entriesOf(range([row()]), epochOf(), ALICE);
    expect(entry.mine).toBe(true);
    expect("leader" in entry).toBe(false);
    expect("leader_name" in entry).toBe(false);
  });
});
