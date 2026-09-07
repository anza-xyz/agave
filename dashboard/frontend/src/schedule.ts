/**
 * Folding the slot list into the leader turns the schedule page shows.
 *
 * Kept out of the component so it can be tested without a DOM, in the same way
 * as the bar scale and the chart windowing.
 */

import type { EpochInfo, SlotEntry } from "./types";

/**
 * Slots the leader schedule hands out at a time.
 *
 * A turn is always these four, and they always share a leader — which is what
 * lets a turn be drawn whole from its first slot alone, with the rest waiting
 * to be filled. A validator drawn twice in a row leads eight consecutive slots,
 * and that is two turns, drawn as two cards: run together they made a card
 * twice the height of every other, and a list whose rows are all different
 * heights has no fixed position to hold.
 */
export const SLOTS_PER_TURN = 4;

/** Who leads a slot, and what the page can call them. */
export interface LeaderRef {
  key: string | null;
  name: string | null;
  icon: string | null;
}

/** Nobody, for a slot outside any epoch the page has the schedule for. */
export const NO_LEADER: LeaderRef = { key: null, name: null, icon: null };

/**
 * Who leads a slot, from the epoch's turn array.
 *
 * The array holds one index per run of consecutive slots, so this is two
 * lookups and no search. Null outside the epoch the arrays describe, and null
 * where the validator could not derive the schedule, which it sends as an empty
 * array rather than as a wrong one.
 */
export function leaderAt(epoch: EpochInfo | undefined, slot: number): string | null {
  if (!epoch || epoch.turns.length === 0) return null;
  if (slot < epoch.start_slot || slot > epoch.end_slot) return null;
  const turn = Math.floor((slot - epoch.start_slot) / SLOTS_PER_TURN);
  const index = epoch.turns[turn];
  if (index === undefined) return null;
  return epoch.leaders[index] ?? null;
}

/** One slot of a turn: what replay found, or nothing yet. */
export interface TurnSlot {
  slot: number;
  /** What replay found, or `null` for a slot of this turn still to come. */
  entry: SlotEntry | null;
}

/** One leader's turn at producing, four slots of it. */
export interface Turn {
  leader: string | null;
  leader_name: string | null;
  leader_icon: string | null;
  mine: boolean;
  /** Newest first, so the turn's own first slot is last. */
  slots: TurnSlot[];
}

/**
 * The turns the held slots belong to, newest first, each drawn whole.
 *
 * A turn appears complete the moment its first slot begins: the other three are
 * the same leader by definition, so they can be drawn as empty rows and filled
 * where they stand. Waiting for each slot to arrive would grow the card three
 * times a turn, and everything below it would move each time.
 *
 * Only forwards, though. A turn at the far end of the window that the list
 * begins part way through keeps the slots there are, rather than inventing
 * rows for slots that happened before anything was being watched.
 */
export function turnsOf(
  held: SlotEntry[],
  leaderOf: (slot: number, mine: boolean) => LeaderRef,
): Turn[] {
  const byTurn = new Map<number, SlotEntry[]>();
  for (const entry of held) {
    const turn = Math.floor(entry.slot / SLOTS_PER_TURN);
    const entries = byTurn.get(turn);
    if (entries) entries.push(entry);
    else byTurn.set(turn, [entry]);
  }

  return [...byTurn.entries()]
    .sort(([a], [b]) => b - a)
    .map(([turn, entries]) => {
      // Asked once for the turn rather than once per slot: all four share a
      // leader by definition, which is what a turn is. Whether it was ours is
      // carried by the slots themselves, and is what lets a turn of ours be
      // named when neither the turn array nor the peer table reaches it.
      const mine = entries.some((entry) => entry.mine);
      const leader = leaderOf(turn * SLOTS_PER_TURN, mine);
      const first = Math.min(...entries.map((entry) => entry.slot));
      const slots: TurnSlot[] = [];
      for (let slot = turn * SLOTS_PER_TURN + SLOTS_PER_TURN - 1; slot >= first; slot--) {
        slots.push({ slot, entry: entries.find((entry) => entry.slot === slot) ?? null });
      }
      return {
        leader: leader.key,
        leader_icon: leader.icon,
        leader_name: leader.name,
        mine,
        slots,
      };
    });
}

/**
 * Whether a turn answers a search.
 *
 * Matches the leader's name or key, or any slot number in the turn, so that
 * pasting either a validator or a slot finds the same card. An empty query
 * matches everything rather than nothing.
 */
export function matchesQuery(turn: Turn, query: string): boolean {
  const needle = query.trim().toLowerCase();
  if (!needle) return true;

  if (turn.leader_name?.toLowerCase().includes(needle)) return true;
  if (turn.leader?.toLowerCase().includes(needle)) return true;
  return turn.slots.some((slot) => String(slot.slot).includes(needle));
}

/**
 * A stable name for a turn, which is what lets a changed list be compared with
 * the one before it.
 *
 * Named by its own first slot rather than by its position: turns arrive above
 * it and fall off below it constantly, and a name that moved with them would
 * identify nothing.
 */
export function turnKey(turn: Turn): string {
  return `turn:${turn.slots.at(-1)?.slot}`;
}

