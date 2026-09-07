import { describe, expect, it } from "vitest";
import { matchesQuery, turnKey, turnsOf, type LeaderRef } from "./schedule";
import type { SlotEntry } from "./types";

function held(slot: number): SlotEntry {
  return {
    slot,
    level: "completed",
    mine: false,
    block: null,
    duration_nanos: null,
    time_millis: null,
    shreds: null,
    replayed_millis: null,
  };
}

/**
 * A resolver standing in for the store's, which reads the epoch's turn array
 * and the peer table. Named leaders come from a table here for the same reason
 * they do there: the slot itself no longer carries one.
 */
function resolver(
  leaders: Record<number, LeaderRef> = {},
  fallback: LeaderRef = { key: "alice", name: null, icon: null },
): (slot: number) => LeaderRef {
  return (slot) => leaders[slot] ?? fallback;
}

describe("turnsOf", () => {
  it("draws a turn whole from its first slot alone", () => {
    // The other three share a leader by definition, so they are rows waiting to
    // be filled. Growing the card as each arrives would move everything below.
    const [turn] = turnsOf([held(100)], resolver());
    expect(turn.slots.map((slot) => slot.slot)).toEqual([103, 102, 101, 100]);
    expect(turn.slots.map((slot) => slot.entry !== null)).toEqual([false, false, false, true]);
  });

  it("fills the rows where they stand as the slots arrive", () => {
    const [turn] = turnsOf([held(100), held(101)], resolver());
    expect(turn.slots).toHaveLength(4);
    expect(turn.slots.map((slot) => slot.entry !== null)).toEqual([false, false, true, true]);
  });

  it("splits a leader drawn twice in a row into two turns", () => {
    // Eight consecutive slots is two turns. Run together the card is twice the
    // height of every other, and a list of cards of different heights has no
    // fixed place to hold.
    const slots = [96, 97, 98, 99, 100, 101, 102, 103].map((slot) => held(slot));
    const turns = turnsOf(slots, resolver());
    expect(turns.map((turn) => turn.slots.length)).toEqual([4, 4]);
    expect(turns[0].slots.map((slot) => slot.slot)).toEqual([103, 102, 101, 100]);
  });

  it("puts the newest turn first", () => {
    const turns = turnsOf([held(100), held(200)], resolver());
    expect(turns.map((turn) => turn.slots.at(-1)?.slot)).toEqual([200, 100]);
  });

  it("invents no rows for slots older than the window", () => {
    // A turn the list begins part way through keeps the slots there are. The
    // missing ones happened before anything was watching, not after.
    const [turn] = turnsOf([held(102), held(103)], resolver());
    expect(turn.slots.map((slot) => slot.slot)).toEqual([103, 102]);
  });

  it("asks for the leader once per turn, at the turn's own first slot", () => {
    // All four share a leader by definition, so a turn is one lookup. Asking
    // per slot would be four answers that can only ever agree.
    const asked: number[] = [];
    const [turn] = turnsOf([held(101), held(102)], (slot) => {
      asked.push(slot);
      return { key: "bob", name: "Bob Co", icon: null };
    });
    expect(asked).toEqual([100]);
    expect(turn.leader).toBe("bob");
    expect(turn.leader_name).toBe("Bob Co");
  });

  it("tells the resolver whether the turn was ours", () => {
    // The resolver answers ours from what the validator says about itself,
    // which is the only route that reaches a turn older than the peer table or
    // outside the epoch the page holds arrays for. Dropping this argument is
    // what left our own turns showing a bare key on the schedule page while the
    // sidebar had them right.
    const asked: Array<[number, boolean]> = [];
    const resolve = (slot: number, mine: boolean) => {
      asked.push([slot, mine]);
      return { key: "us", name: "Lantern", icon: null };
    };
    turnsOf([{ ...held(100), mine: true }, held(200)], resolve);
    expect(asked).toEqual([
      [200, false],
      [100, true],
    ]);
  });

  it("names nobody for a turn whose epoch the page has no schedule for", () => {
    // Deep history can reach past the epoch whose arrays the page holds. Better
    // an unknown leader than a confident wrong one.
    const [turn] = turnsOf([held(100)], resolver({}, { key: null, name: null, icon: null }));
    expect(turn.leader).toBeNull();
  });

  it("has nothing to say about an empty list", () => {
    expect(turnsOf([], resolver())).toEqual([]);
  });
});

describe("matchesQuery", () => {
  const [turn] = turnsOf(
    [held(430789128)],
    resolver({}, { key: "J7v9KQ8s", name: "Staking Facilities", icon: null }),
  );

  it("matches a name whatever its case", () => {
    expect(matchesQuery(turn, "staking")).toBe(true);
    expect(matchesQuery(turn, "STAKING")).toBe(true);
  });

  it("matches part of the leader key", () => {
    expect(matchesQuery(turn, "J7v9")).toBe(true);
  });

  it("matches a slot in the turn", () => {
    expect(matchesQuery(turn, "430789128")).toBe(true);
  });

  it("matches everything when nothing was asked", () => {
    expect(matchesQuery(turn, "")).toBe(true);
    expect(matchesQuery(turn, "   ")).toBe(true);
  });

  it("does not match something absent", () => {
    expect(matchesQuery(turn, "nansen")).toBe(false);
  });
});

describe("turnKey", () => {
  it("names a turn by its own first slot, not its position", () => {
    // Turns arrive above and fall off below constantly; a name that moved with
    // them would identify nothing.
    const [turn] = turnsOf([held(100), held(101)], resolver());
    expect(turnKey(turn)).toBe("turn:100");
  });
});
