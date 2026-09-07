import { beforeAll, describe, expect, it } from "vitest";
import { Store } from "./store";
import type { Envelope, SlotEntry, TpsSample } from "./types";

beforeAll(() => {
  // The store coalesces notifications onto an animation frame, which node has
  // no concept of. Running the callback at once keeps the assertions plain.
  globalThis.requestAnimationFrame = ((callback: FrameRequestCallback) => {
    callback(0);
    return 0;
  }) as typeof globalThis.requestAnimationFrame;
});

function slot(number: number, level: SlotEntry["level"] = "completed"): SlotEntry {
  return {
    slot: number,
    level,
    block: null,
    duration_nanos: null,
    time_millis: null,
    shreds: null,
    replayed_millis: null,
    mine: false,
  };
}

function envelope(topic: string, key: string, value: unknown): Envelope {
  return { topic, key, value } as Envelope;
}

describe("values", () => {
  it("keeps the latest value for a key", () => {
    const store = new Store();
    store.apply(envelope("summary", "cluster", "testnet"));
    expect(store.get("summary", "cluster")).toBe("testnet");
    store.apply(envelope("summary", "cluster", "mainnet-beta"));
    expect(store.get("summary", "cluster")).toBe("mainnet-beta");
  });

  it("ignores replies to our own requests", () => {
    // A reply carries the id it was asked with. Storing it would let a ping
    // answer overwrite the state under the same key.
    const store = new Store();
    store.apply(envelope("summary", "ping", "state"));
    store.apply({ ...envelope("summary", "ping", "reply"), id: 7 } as Envelope);
    expect(store.get("summary", "ping")).toBe("state");
  });
});

describe("slots", () => {
  it("replaces everything on an overview and merges an update", () => {
    const store = new Store();
    store.apply(envelope("slot", "overview", [slot(1), slot(2)]));
    expect(store.getSlots().map((entry) => entry.slot)).toEqual([1, 2]);

    store.apply(envelope("slot", "update", slot(3)));
    expect(store.getSlots().map((entry) => entry.slot)).toEqual([1, 2, 3]);

    // A second overview is a resynchronisation, not an addition.
    store.apply(envelope("slot", "overview", [slot(9)]));
    expect(store.getSlots().map((entry) => entry.slot)).toEqual([9]);
  });

  it("upgrades a slot in place as its level advances", () => {
    const store = new Store();
    store.apply(envelope("slot", "update", slot(5, "incomplete")));
    store.apply(envelope("slot", "update", slot(5, "finalized")));
    expect(store.getSlots()).toHaveLength(1);
    expect(store.getSlot(5)?.level).toBe("finalized");
  });

  it("returns slots in order however they arrived", () => {
    const store = new Store();
    for (const number of [7, 5, 9, 6]) store.apply(envelope("slot", "update", slot(number)));
    expect(store.getSlots().map((entry) => entry.slot)).toEqual([5, 6, 7, 9]);
  });

  it("drops the oldest slots rather than growing without bound", () => {
    const store = new Store();
    for (let number = 1; number <= 600; number += 1) {
      store.apply(envelope("slot", "update", slot(number)));
    }
    const slots = store.getSlots();
    expect(slots).toHaveLength(512);
    expect(slots[slots.length - 1].slot).toBe(600);
    // The oldest went, not the newest.
    expect(slots[0].slot).toBe(89);
  });

  it("keeps our own leader slots long after the window has passed them", () => {
    // A validator leads about four slots in eight hundred, so a window of five
    // hundred usually holds none of its own. Without this the sidebar's own
    // slots view would be empty nearly all the time.
    const store = new Store();
    for (const number of [1, 2, 3, 4]) {
      store.apply(envelope("slot", "update", { ...slot(number), mine: true }));
    }
    for (let number = 5; number <= 2000; number += 1) {
      store.apply(envelope("slot", "update", slot(number)));
    }
    const ours = store.getSlots().filter((entry) => entry.mine);
    expect(ours.map((entry) => entry.slot)).toEqual([1, 2, 3, 4]);
  });

  it("names a leader in an epoch the page was never sent, once it is fetched", async () => {
    // Reading back through the history leaves the published epoch whenever the
    // tip is within its depth of a boundary, about a quarter of the time. Every
    // slot on the far side had no leader at all until this.
    const store = new Store();
    const sent: string[] = [];
    store.setSender((frame) => sent.push(frame));
    store.setConnection("open");
    store.apply(
      envelope("epoch", "new", {
        epoch: 2,
        start_slot: 200,
        end_slot: 299,
        slots_in_epoch: 100,
        my_leader_slots: [],
        leaders: ["NOW"],
        turns: Array.from({ length: 25 }, () => 0),
        block_cost_limit: 0,
        account_cost_limit: 0,
      }),
    );
    expect(store.leaderOf(104, false).key).toBeNull();
    const before = store.getLeaderRevision();

    const loading = store.loadEpoch(1);
    const id = (JSON.parse(sent[0]) as { id: number }).id;
    store.apply({
      topic: "epoch",
      key: "query",
      id,
      value: {
        epoch: 1,
        start_slot: 100,
        end_slot: 199,
        slots_in_epoch: 100,
        my_leader_slots: [],
        leaders: ["BEFORE"],
        turns: Array.from({ length: 25 }, () => 0),
        block_cost_limit: 0,
        account_cost_limit: 0,
      },
    });
    await loading;

    expect(store.leaderOf(104, false).key).toBe("BEFORE");
    // The current epoch still answers for its own slots, and first.
    expect(store.leaderOf(204, false).key).toBe("NOW");
    expect(store.getLeaderRevision()).toBeGreaterThan(before);
  });

  it("asks about an epoch it has no schedule for only once", async () => {
    // A validator that has not been up long has nothing for it, and every
    // search would otherwise ask again.
    const store = new Store();
    const sent: string[] = [];
    store.setSender((frame) => sent.push(frame));
    store.setConnection("open");

    const loading = store.loadEpoch(1);
    const id = (JSON.parse(sent[0]) as { id: number }).id;
    store.apply({ topic: "epoch", key: "query", id, value: null });
    await loading;

    await store.loadEpoch(1);
    expect(sent).toHaveLength(1);
  });

  it("names a leader the peer table does not reach, once the table is fetched", async () => {
    // The peer table covers the leaders of the held window. A turn from further
    // back had a key and nothing else, which is what made a search by name find
    // only the last few minutes of a history eleven hours deep.
    const store = new Store();
    const sent: string[] = [];
    store.setSender((frame) => sent.push(frame));
    store.setConnection("open");
    store.apply(
      envelope("epoch", "new", {
        epoch: 1,
        start_slot: 100,
        end_slot: 115,
        slots_in_epoch: 16,
        my_leader_slots: [],
        leaders: ["FARAWAY"],
        turns: [0, 0, 0, 0],
        block_cost_limit: 0,
        account_cost_limit: 0,
      }),
    );
    expect(store.leaderOf(104, false)).toEqual({ key: "FARAWAY", name: null, icon: null });

    const loading = store.loadDisplays();
    const id = (JSON.parse(sent[0]) as { id: number }).id;
    store.apply({
      topic: "summary",
      key: "displays",
      id,
      value: { keys: ["FARAWAY"], names: ["Far Away Co"], icons: [null] },
    });
    await loading;

    expect(store.leaderOf(104, false)).toEqual({
      key: "FARAWAY",
      name: "Far Away Co",
      icon: null,
    });
  });

  it("asks for the display table once and no more", async () => {
    const store = new Store();
    const sent: string[] = [];
    store.setSender((frame) => sent.push(frame));
    store.setConnection("open");

    const loading = store.loadDisplays();
    const id = (JSON.parse(sent[0]) as { id: number }).id;
    store.apply({
      topic: "summary",
      key: "displays",
      id,
      value: { keys: ["A"], names: ["Alpha"], icons: [null] },
    });
    await loading;

    await store.loadDisplays();
    expect(sent).toHaveLength(1);
  });

  it("names a slot of ours from what the validator says about itself", () => {
    // Not from the turn array or the peer table. Both have a reach and our own
    // slots are kept past both: five hundred of them is about eleven hours,
    // outside the peer table's window and often across an epoch boundary, which
    // is where the turn array stops. Live, that showed our own turns as a bare
    // key, or as unknown once the boundary was behind them.
    const store = new Store();
    store.apply(envelope("summary", "identity_key", "OURKEY"));
    store.apply(envelope("summary", "identity_name", "Lantern"));
    store.apply(envelope("summary", "identity_icon", "https://l/i.png"));

    const ours = store.leaderOf(443_227_896, true);
    expect(ours).toEqual({ key: "OURKEY", name: "Lantern", icon: "https://l/i.png" });
  });

  it("gives the same object back for ours until one of its parts changes", () => {
    // The rows that draw a leader are memoised on their props, so a fresh
    // object every render would rebuild the whole list on each meter sample.
    const store = new Store();
    store.apply(envelope("summary", "identity_key", "OURKEY"));
    const first = store.leaderOf(1, true);
    expect(store.leaderOf(2, true)).toBe(first);

    store.apply(envelope("summary", "identity_name", "Lantern"));
    expect(store.leaderOf(1, true)).not.toBe(first);
    expect(store.leaderOf(1, true).name).toBe("Lantern");
  });

  it("still looks a slot that is not ours up the long way", () => {
    const store = new Store();
    store.apply(envelope("summary", "identity_key", "OURKEY"));
    store.apply(
      envelope("epoch", "new", {
        epoch: 1,
        start_slot: 100,
        end_slot: 115,
        slots_in_epoch: 16,
        my_leader_slots: [],
        leaders: ["THEIRKEY"],
        turns: [0, 0, 0, 0],
        block_cost_limit: 0,
        account_cost_limit: 0,
      }),
    );
    expect(store.leaderOf(104, false).key).toBe("THEIRKEY");
    // And a slot outside the epoch the page holds still names nobody, which is
    // the honest answer rather than a confident wrong one.
    expect(store.leaderOf(99, false).key).toBeNull();
  });

  it("answers a request with the reply carrying its id", async () => {
    const store = new Store();
    const sent: string[] = [];
    store.setSender((frame) => sent.push(frame));
    store.setConnection("open");

    const reply = store.request<{ rows: number[] }>("slot", "range", { first_slot: 4 });
    const frame = JSON.parse(sent[0]) as { id: number; topic: string; params: unknown };
    expect(frame.topic).toBe("slot");
    expect(frame.params).toEqual({ first_slot: 4 });

    store.apply({ topic: "slot", key: "range", id: frame.id, value: { rows: [1, 2] } });
    expect(await reply).toEqual({ rows: [1, 2] });
  });

  it("does not fold a reply into the state it happens to be named after", () => {
    // The envelope of a reply and of a push differ only by the id, so without
    // that check a queried range would overwrite the live slot map.
    const store = new Store();
    store.setSender(() => {});
    store.setConnection("open");
    store.apply(envelope("slot", "overview", [slot(900)]));

    store.apply({ topic: "slot", key: "update", id: 77, value: slot(1) });
    expect(store.getSlots().map((entry) => entry.slot)).toEqual([900]);
  });

  it("fails the requests in flight when the connection goes", async () => {
    // Both paths that give up on a socket set the connection state, so this is
    // the one place that has to notice. Left pending, a caller shows a loading
    // state that never resolves.
    const store = new Store();
    store.setSender(() => {});
    store.setConnection("open");

    const reply = store.request("slot", "range", {});
    store.setConnection("closed");
    await expect(reply).rejects.toThrow("connection lost");
  });

  it("refuses a request made with no connection rather than queueing it", async () => {
    // Answered after the next reconnect, it would arrive against a page that
    // has moved on.
    const store = new Store();
    await expect(store.request("slot", "range", {})).rejects.toThrow("not connected");
  });

  it("bounds how many of our own slots it keeps", () => {
    const store = new Store();
    for (let number = 1; number <= 200; number += 1) {
      store.apply(envelope("slot", "update", { ...slot(number), mine: true }));
    }
    for (let number = 201; number <= 1000; number += 1) {
      store.apply(envelope("slot", "update", slot(number)));
    }
    const ours = store.getSlots().filter((entry) => entry.mine);
    expect(ours).toHaveLength(64);
    // The newest sixty-four of ours, not the first sixty-four we ever saw.
    expect(ours[ours.length - 1].slot).toBe(200);
    expect(ours[0].slot).toBe(137);
  });

  it("does not let retained slots displace the recent window", () => {
    // The strip reads the tail of this list. Holding old slots of ours must
    // not push newer ones out of it.
    const store = new Store();
    store.apply(envelope("slot", "update", { ...slot(1), mine: true }));
    for (let number = 2; number <= 1000; number += 1) {
      store.apply(envelope("slot", "update", slot(number)));
    }
    const recent = store.getSlots().slice(-64);
    expect(recent[0].slot).toBe(937);
    expect(recent[recent.length - 1].slot).toBe(1000);
  });
});

describe("tps samples", () => {
  const sample = (number: number): TpsSample => ({
    slot: number,
    timestamp_nanos: number * 1e9,
    total: 0,
    vote: 0,
    non_vote_success: 0,
    non_vote_failed: 0,
  });

  it("keeps the series strictly increasing across the history overlap", () => {
    // The retained history and the live samples overlap by design, so a sample
    // that repeats one already held must not be appended again.
    const store = new Store();
    store.apply(envelope("summary", "tps_history", [sample(1), sample(2), sample(3)]));
    store.apply(envelope("summary", "tps_sample", sample(2)));
    store.apply(envelope("summary", "tps_sample", sample(3)));
    expect(store.getTps().map((entry) => entry.slot)).toEqual([1, 2, 3]);

    store.apply(envelope("summary", "tps_sample", sample(4)));
    expect(store.getTps().map((entry) => entry.slot)).toEqual([1, 2, 3, 4]);
  });
});

describe("isReady", () => {
  it("waits for both the identity and the first slots", () => {
    const store = new Store();
    expect(store.isReady()).toBe(false);
    store.apply(envelope("summary", "identity_key", "abc"));
    expect(store.isReady()).toBe(false);
    store.apply(envelope("slot", "update", slot(1)));
    expect(store.isReady()).toBe(true);
  });

  it("does not wait for a validator that is still booting", () => {
    // A booting validator has no slots and no identity to report, and the boot
    // sequence is exactly what should be on screen, so there is nothing left
    // for the splash to wait for.
    const store = new Store();
    store.apply(envelope("summary", "startup_progress", { running: false }));
    expect(store.isReady()).toBe(true);
  });
});
