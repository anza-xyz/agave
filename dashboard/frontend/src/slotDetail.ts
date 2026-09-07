/**
 * Arranging one produced block's figures for the expanded slot row.
 *
 * The row's body is led by what filled the block rather than by the scheduler's
 * counters. Most of those counters are nought on most slots, and drawn as a
 * flat list of two dozen rows they were several hundred pixels saying nothing.
 * They are still all here, grouped by the stage that dropped them and folded
 * behind a control, so a slot that did lose something says where in one line
 * and shows the detail on request.
 *
 * Kept out of the component so it can be tested without a DOM, like the
 * waterfall rows it is built on.
 */

import type { ProducedBlock, SlotCost, SlotWaterfall } from "./types";
import { waterfallRows, type WaterfallRow } from "./waterfall";

/**
 * How the block's compute limit was spent.
 *
 * Three shares of the limit that add to one, which is the only reading under
 * which unused headroom belongs on the same bar as the used part. Note that
 * this makes the costliest account's segment its share of the *limit*, not of
 * the block: an account can be most of a block and still a sliver of the limit,
 * and those two figures differ by however empty the block was.
 */
export interface Capacity {
  /** The costliest account's share of the limit, or nought where none is known. */
  top: number;
  /** Everything else that was used. */
  rest: number;
  /** Headroom the block never took. */
  free: number;
}

/** Null where the limit is unknown, which leaves nothing to draw a share of. */
export function capacity(block: ProducedBlock, cost: SlotCost | undefined): Capacity | null {
  const limit = block.block_cost_limit;
  if (limit <= 0) return null;
  // Capped at the limit. A block cannot exceed it, but the two figures are
  // captured from different places on the bank and a rounding disagreement
  // would otherwise push the bar past its own track.
  const used = Math.min(1, Math.max(0, block.block_cost) / limit);
  // Capped at what the block used, for the same reason and because the
  // costliest account cannot have spent more than the block did.
  const top = cost ? Math.min(used, Math.max(0, cost.costliest_cost) / limit) : 0;
  return { top, rest: used - top, free: 1 - used };
}

/** The scheduler rows that mark a point every transaction passes through. */
const CHAIN_KEYS = ["received", "buffered", "scheduled", "finished"] as const;

/**
 * Which stage each counter belongs to, by key rather than by position.
 *
 * Listed rather than derived from where a row falls between two stage rows,
 * because on a BAM slot the first two rows are counted in batches and are not
 * stages at all, so there is no stage above them to fall after. A list also
 * fails loudly: the coverage test asserts every row the waterfall produces is
 * either a chain stage or in exactly one group here, so a counter added
 * upstream shows up as a failure rather than quietly vanishing from the drawer.
 */
const GROUPS: { key: string; title: string; members: string[] }[] = [
  {
    key: "intake",
    title: "Intake · dropped",
    members: [
      "not_held",
      "check_queue_full",
      "unparsable",
      "bad_locks",
      "compute_budget",
      "too_old",
      "already_processed",
      "fee_payer",
      "filtered",
      "nonce_conflict",
    ],
  },
  {
    key: "buffer",
    title: "Buffer · cleared",
    members: ["queue_full", "nonce_evicted", "cleared", "cleaned"],
  },
  {
    key: "schedule",
    title: "Schedule · held back",
    members: ["blocked_conflicts", "blocked_threads", "retried"],
  },
];

export interface CounterGroup {
  key: string;
  title: string;
  /**
   * The group's counters: those above nought first, largest down, then the
   * rest in the order the pipeline meets them.
   *
   * Sorted rather than left in pipeline order because the drawer is opened to
   * answer "what went wrong", and on a slot with one bad counter among fifteen
   * quiet ones the answer should not have to be hunted for. The quiet ones keep
   * their canonical order below, where the order is the only thing making a
   * column of noughts readable.
   */
  rows: WaterfallRow[];
  /** How many rows are above nought, and so where the quiet ones begin. */
  hits: number;
  /** The group's own total, in transactions. */
  total: number;
  /**
   * Rows counted in batches rather than transactions, which BAM reports and a
   * stock validator does not.
   *
   * Held apart from `rows` and left out of `total`. A batch carries however
   * many transactions it carries, so adding one to the other, or drawing it as
   * a share of the group, would be combining two units into a figure that means
   * nothing.
   */
  aside: WaterfallRow[];
}

/** One link in the strip above the drawer. */
export interface ChainLink {
  key: string;
  label: string;
  count: number;
}

export interface SchedulerView {
  chain: ChainLink[];
  groups: CounterGroup[];
  /** Everything the groups lost, in transactions. */
  lost: number;
  /** The single largest counter, or null where nothing was lost. */
  worst: WaterfallRow | null;
  /** How many counters are above nought, out of how many there are. */
  nonZero: number;
  counters: number;
  /**
   * What finished, against the first figure counted in transactions.
   *
   * On a BAM slot that is `buffered` rather than `received`, because `received`
   * is a count of batches there and dividing transactions by batches would
   * produce a percentage of nothing.
   */
  completion: number | null;
}

export function schedulerView(w: SlotWaterfall): SchedulerView {
  const rows = waterfallRows(w);
  const byKey = new Map(rows.map((r) => [r.key, r]));
  const bam = w.source === "bam";

  const chain: ChainLink[] = CHAIN_KEYS.flatMap((key) => {
    const row = byKey.get(key);
    if (!row) return [];
    // Named for its unit where the unit changes. On a BAM slot the first link
    // is batches and the three after it are transactions, and a reader given
    // four bare numbers in a row would take them for one measurement narrowing.
    const label = key === "received" && bam ? "batches" : key;
    return [{ key, label, count: row.count }];
  });

  const groups: CounterGroup[] = GROUPS.map((group) => {
    const members = group.members.flatMap((key) => byKey.get(key) ?? []);
    const aside = members.filter((row) => row.kind === "count");
    const counted = members.filter((row) => row.kind !== "count");
    const hits = counted.filter((row) => row.count > 0);
    const quiet = counted.filter((row) => row.count === 0);
    // Stable across ticks: two counters on the same figure keep the order the
    // pipeline lists them in rather than swapping under whoever is reading.
    hits.sort((a, b) => b.count - a.count);
    return {
      key: group.key,
      title: group.title,
      rows: [...hits, ...quiet],
      hits: hits.length,
      total: counted.reduce((sum, row) => sum + row.count, 0),
      aside,
    };
  });

  const counted = groups.flatMap((group) => group.rows);
  const lost = groups.reduce((sum, group) => sum + group.total, 0);
  const worst = counted.reduce<WaterfallRow | null>(
    (top, row) => (row.count > 0 && (top === null || row.count > top.count) ? row : top),
    null,
  );

  const buffered = byKey.get("buffered")?.count ?? 0;
  const received = byKey.get("received")?.count ?? 0;
  const against = bam ? buffered : received;
  const finished = byKey.get("finished")?.count ?? 0;

  return {
    chain,
    groups,
    lost,
    worst,
    nonZero: counted.filter((row) => row.count > 0).length,
    counters: counted.length,
    // Capped, because the queue holds transactions across slots and a slot can
    // finish more than arrived in it. Null rather than nought where nothing
    // arrived, so an idle slot does not read as one that finished nothing.
    completion: against > 0 ? Math.min(1, finished / against) : null,
  };
}

/**
 * A counter's share of its own group, for the bar beside it.
 *
 * Of the group rather than of everything lost, so the bars answer "what did
 * this stage lose it to" and the group headers answer "which stage lost the
 * most". Drawn against the whole would leave every bar in a quiet group a
 * sliver regardless of how lopsided that group was on its own.
 */
export function shareOfGroup(group: CounterGroup, row: WaterfallRow): number {
  return group.total > 0 ? row.count / group.total : 0;
}
