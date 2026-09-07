/** Mirrors the payloads published by `dashboard/src/collect.rs`. */

export type SlotLevel =
  | "incomplete"
  | "completed"
  | "optimistically_confirmed"
  | "rooted"
  | "finalized"
  | "skipped";

export interface SlotEntry {
  slot: number;
  level: SlotLevel;
  /**
   * True when this validator was the scheduled leader.
   *
   * The leader itself is not here. It was three strings on every slot, the same
   * key, name and icon repeated for all four slots of a turn. The key now comes
   * from the epoch's turn array and the name and icon from the peer table, each
   * holding one copy per leader. Resolve them with `store.leaderOf`.
   */
  mine: boolean;
  /** What replay found in the block. Null for a slot with no block. */
  block: BlockDetail | null;
  duration_nanos: number | null;
  /** When the slot's first shred arrived, in milliseconds. Null for a slot never timed. */
  time_millis: number | null;
  /** How the block's shreds arrived. Null for a slot that never filled. */
  shreds: ShredArrival | null;
  /**
   * Milliseconds from the slot's first shred to replay finishing it. Null for
   * a bank this validator built, which replay never timed.
   */
  replayed_millis: number | null;
}

/**
 * How a block's shreds arrived. Outside `BlockDetail` because a slot fills
 * before it freezes, and a dead slot fills without ever freezing.
 */
export interface ShredArrival {
  /** Data shreds in the block. */
  count: number;
  /** Of those, the ones this validator had to ask for. Nought is the block arriving whole over turbine. */
  repaired: number;
  /** Milliseconds from the first shred to the last. */
  full_millis: number;
}

/**
 * Where this validator's shreds came from over the last five minutes.
 *
 * Turbine should deliver nearly all of them; repair is the fallback for what
 * never arrived. Null while none have arrived at all.
 */
export interface Shreds {
  received: number;
  repaired: number;
  repair_rate: number;
}

/**
 * How often an account replay needed was already in memory, over the last
 * minute.
 *
 * Lifted from the measurements the accounts database submits about itself,
 * which carry a second's work each. Null while nothing has been read.
 */
export interface AccountsCache {
  /**
   * The read cache's own lookups and hit rate, which are narrower than they
   * look and are deliberately not the card's headline.
   *
   * The write cache is consulted first, so these cover only the reads that got
   * past it. Led with, the rate cannot be squared with the three-way split that
   * counts every load. The card derives its headline from that split instead
   * and leaves these to whoever is reading the feed directly.
   */
  read: number;
  hit_rate: number;
  evictions: number;
  cache_bytes: number;
  cache_entries: number;
  /**
   * Where reads were answered from. `from_storage` is the only one that touches
   * a file, and is the nearest thing here to a disk read rate — counted in
   * accounts rather than bytes, because nothing counts the bytes on that path.
   */
  from_write_cache: number;
  from_read_cache: number;
  from_storage: number;
  /** The write side, which does have a byte figure. */
  stored_accounts: number;
  stored_bytes: number;
  /** What the window actually spans, for turning totals into rates. */
  window_seconds: number;
  disk: AccountsDisk | null;
}

export interface AccountsDisk {
  used: number;
  allocated: number;
  /** Dead account data still on disk, which is what shrink reclaims. */
  fragmented: number;
  storages: number;
}

/**
 * How often replay found a program already compiled, over the last minute.
 *
 * The counters behind this are reset for each bank, so `looked_up` is what was
 * seen in the window rather than since startup. Null while nothing has been
 * looked up at all.
 */
export interface ProgramCache {
  looked_up: number;
  hits: number;
  misses: number;
  hit_rate: number;
  evictions: number;
  reloads: number;
  insertions: number;
  lost_insertions: number;
  replacements: number;
  one_hit_wonders: number;
  prunes_orphan: number;
  prunes_environment: number;
  /**
   * The most entries seen loaded at any eviction in the window, against the
   * limit eviction keeps them under. Null until an eviction has happened at
   * all: the figure behind it is only written when one runs.
   */
  peak_entries: number | null;
  entry_limit: number;
}

/**
 * What is known about a leader beyond the name its slot rows carry.
 *
 * Published only for the leaders on screen, so a leader may be missing from
 * this table briefly after a reconnection, before the next slow tick.
 */
/**
 * What every validator that published anything calls itself.
 *
 * Asked for rather than pushed: it is a hundred and fifty kilobytes on a
 * cluster this size and most of a session never needs it. Three arrays sharing
 * an index, because an object would carry the words name and icon once per
 * validator.
 */
export interface Displays {
  keys: string[];
  names: (string | null)[];
  icons: (string | null)[];
}

export interface Peer {
  identity: string;
  version: string | null;
  stake: number;
  ip: string | null;
  /** Display name from the validator's on-chain info, if it published one. */
  name: string | null;
  /** Icon URL from the same place. */
  icon: string | null;
}

/**
 * A slot the leader schedule has assigned that has not happened yet.
 *
 * Published on the slow tier, so the front of the list has usually happened by
 * the time it is read. Filter against the completed slot before rendering.
 */
export interface UpcomingSlot {
  slot: number;
  leader: string;
  leader_name: string | null;
  leader_icon: string | null;
  mine: boolean;
}

/**
 * The rates the page needs to turn a measured tip figure into the two it draws.
 *
 * Absent on a validator with no tip payment program, and then no tip column is
 * drawn at all. `commission_bps` is absent where the flag was not set, which
 * leaves the page able to say what a turn paid and not what it earned.
 */
export interface TipRates {
  jito_cut_bps: number;
  commission_bps: number | null;
}

/** What one block contained, as the collector read it off the frozen bank. */
export interface BlockDetail {
  transactions: number;
  non_vote_transactions: number;
  failed_transactions: number;
  entries: number;
  block_cost: number;
  block_cost_limit: number;
  /** The most compute any one account may be charged in a block. */
  account_cost_limit: number;
  total_fees: number;
  priority_fees: number;
  /**
   * Lamports paid into the jito tip accounts during this slot, as measured.
   *
   * The measurement, not anyone's income. What reached a distribution account
   * and what it earned us are worked out from it in `tips.ts`, using rates the
   * validator sends, so that a corrected rate corrects the whole history.
   *
   * `null` where no tip program is configured, or where the slot's bank had no
   * parent to difference against. Distinct from nought, which says the
   * searchers passed that leader by.
   */
  tips: number | null;
  /**
   * Wall time replay's own thread spent on this slot, in microseconds. Null
   * where replay never timed it, which includes a block this validator built.
   */
  replay_micros: number | null;
}

export interface Tps {
  total: number;
  vote: number;
  non_vote_success: number;
  non_vote_failed: number;
}

export interface TpsSample extends Tps {
  slot: number;
  timestamp_nanos: number;
}

export interface StakeSummary {
  activated_stake: number;
  total_stake: number;
  /** This validator's share of total stake, in [0, 1]. */
  share: number;
}

export interface ValidatorCounts {
  total: number;
  delinquent: number;
  rpc_nodes: number;
  non_delinquent_stake: number;
  delinquent_stake: number;
}

export interface VersionShare {
  /** Null for peers reporting no version, and for the folded tail. */
  version: string | null;
  validators: number;
  stake: number;
  /** True only for the row the tail was folded into. */
  other: boolean;
}

export interface EpochInfo {
  epoch: number;
  start_slot: number;
  end_slot: number;
  slots_in_epoch: number;
  my_leader_slots: number[];

  /** Every leader of this epoch, in the order they first take a turn. */
  leaders: string[];
  /**
   * One index into `leaders` per run of consecutive slots given to a single
   * leader, so the leader of a slot is
   * `leaders[turns[(slot - start_slot) / 4]]`, four being the run length the
   * cluster guarantees.
   *
   * Empty where the validator could not derive the schedule, which is not the
   * same as an epoch with no leaders and must not be drawn as one.
   */
  turns: number[];

  /** Consensus limits every block of this epoch is measured against. */
  block_cost_limit: number;
  account_cost_limit: number;
}

export interface Network {
  received_per_second: number;
  sent_per_second: number;
}

export interface NetworkSample extends Network {
  timestamp_nanos: number;
}

/**
 * The share of egress two senders account for, in bytes per second. Null until
 * a sender has reported. Turbine goes out over XDP and reports no bytes, so
 * the rest of egress cannot be named.
 */
export interface EgressSplit {
  gossip_per_second: number | null;
  repair_per_second: number | null;
}

export interface IngestPath {
  name: string;
  port: number;
  drops_recent: number;
  drops_total: number;
  queued_bytes: number;
  /**
   * Packets the port delivered, over the same window and from the same instant
   * as the drops beside them, so that one can be divided by their sum.
   *
   * Null for a port whose traffic nothing counts in datagrams: the three QUIC
   * ports, whose counters count transactions pulled out of streams, and serve
   * repair, whose receiver keeps counters that nothing reports.
   */
  received_recent: number | null;
  received_total: number | null;
  /**
   * Whether the port speaks QUIC, which decides which card draws it.
   *
   * The socket card takes the ports that do not, where a drop count has a
   * delivered count to be a share of. The TPU path card takes the ones that do,
   * where the listener's own account of what it admitted stands in for the
   * share this list cannot give them.
   */
  quic: boolean;
}

export interface ProducedBlock {
  slot: number;
  slot_time_millis: number | null;
  blockhash: string;
  duration_nanos: number | null;
  transactions: number;
  non_vote_transactions: number;
  failed_transactions: number;
  entries: number;
  block_cost: number;
  block_cost_limit: number;
  /** The most compute any one account may be charged in a block. */
  account_cost_limit: number;
  total_fees: number;
  priority_fees: number;
  /**
   * Lamports paid into the jito tip accounts during this slot, as measured.
   *
   * The measurement, not anyone's income. What reached a distribution account
   * and what it earned us are worked out from it in `tips.ts`, using rates the
   * validator sends, so that a corrected rate corrects the whole history.
   *
   * `null` where no tip program is configured, or where the slot's bank had no
   * parent to difference against. Distinct from nought, which says the
   * searchers passed that leader by.
   */
  tips: number | null;
}

/**
 * Which of the process's schedulers built a slot.
 *
 * A stock validator runs one and always reports `scheduler`. jito runs a second
 * beside it for BAM, which builds the block itself whenever it is connected,
 * and counts what arrived in a different unit.
 */
export type SchedulerSource = "scheduler" | "bam";

/**
 * Where the transactions handed to the banking stage went, over the window.
 *
 * Counts of what happened inside the window, not a queue depth: the scheduler
 * reports these once a second with its own counters reset as it does, and the
 * server sums a window of them.
 *
 * The first stretch is an identity — `received` is exactly `buffered` plus
 * every loss from `not_held` through `nonce_conflict`. The later stretches are
 * not, and cannot be, because the queue holds a standing population: what was
 * scheduled in this window was largely buffered in an earlier one.
 *
 * None of that first stretch holds on a slot BAM built. It counts what it
 * rejected before parsing in batches and everything it rejected after parsing
 * in transactions, so `received` and `not_held` are in a unit of their own and
 * add up to nothing alongside the rest.
 */
export interface Waterfall {
  received: number;

  /**
   * Which scheduler these counts came from. Sent per slot and absent on the
   * live card, which covers both without distinguishing them.
   */
  source?: SchedulerSource;

  /**
   * Lost at the door, before ever being queued. These plus `buffered` are
   * `received` — on a slot the validator's own scheduler built.
   *
   * On a BAM slot none of that holds. `not_held` is fed by a different check
   * there and counts batches BAM sent past their own deadline, so it is neither
   * in the same unit as the rest nor part of any identity with them.
   */
  not_held: number;
  check_queue_full: number;
  unparsable: number;
  bad_locks: number;
  compute_budget: number;
  too_old: number;
  already_processed: number;
  fee_payer: number;
  filtered: number;
  nonce_conflict: number;

  buffered: number;

  /** Lost from the queue, having already been buffered. */
  queue_full: number;
  nonce_evicted: number;
  cleared: number;
  cleaned: number;

  scheduled: number;
  /** Not losses: work the scheduler had but could not place this pass. */
  blocked_conflicts: number;
  blocked_threads: number;

  finished: number;
  retried: number;
}

/**
 * The three stages either side of the scheduler.
 *
 * Sent under keys of their own and drawn as separate sections rather than as one
 * flow with the scheduler. They are instrumented independently, report on
 * different cadences, and each hands on a population the next does not quite
 * receive, so a single chain across them would imply an arithmetic that does not
 * hold. Each section balances against itself and nothing else.
 */
/**
 * One QUIC listener's account of the traffic offered to it.
 *
 * Three groups of figure in one object, and they are not interchangeable. The
 * first eight are the connection funnel, and they very nearly partition the
 * offer: the listener sheds at each gate in turn and moves on, so an attempt is
 * shed, or fails its handshake, or is admitted. The next six are streams opened
 * on connections that were admitted. The four after that are what came out
 * towards verification.
 *
 * `open` and `active_streams` are levels rather than counts. They say how the
 * port stands at the instant of the last reading and mean nothing summed over
 * the window the rest of this covers.
 */
export interface QuicPort {
  /** Matches the socket row of the same name on the ingest list. */
  name: string;

  offered: number;
  shed_all: number;
  shed_address: number;
  refused_full: number;
  handshake_timeout: number;
  handshake_error: number;
  /** Cleared the handshake and the rate limiters' second look. A checkpoint. */
  handshook: number;
  /**
   * Refused a place in the connection table, under four overlapping names.
   *
   * One refusal can raise two of these — the unstaked path runs through the
   * same insert that raises `add_failed` — so they are never added together.
   * `refusedTable` in `tpuPath.ts` is where they are reconciled.
   */
  add_failed: number;
  add_failed_staked: number;
  add_failed_unstaked: number;
  add_failed_banned: number;
  admitted_staked: number;
  admitted_unstaked: number;

  streams: number;
  throttled_staked: number;
  throttled_unstaked: number;
  read_timeouts: number;
  read_errors: number;
  invalid_size: number;

  handed_on: number;
  bytes_handed_on: number;
  queue_full: number;
  disconnected: number;

  open: number;
  active_streams: number;

  /**
   * Datagrams the kernel discarded on this port, over the same span as the
   * counts above.
   *
   * Null where the port was not found among the bound sockets, which is not the
   * same as a port that dropped nothing. Counted in datagrams while everything
   * else here is connections or transactions, so it is drawn without a bar and
   * never added to anything.
   */
  kernel_drops: number | null;
}

export interface QuicPaths {
  /** What the counts above actually span, which is short until it has filled. */
  window_seconds: number;
  ports: QuicPort[];
  /**
   * Whether the TPU address this validator advertises is a socket on this host.
   *
   * It is not, behind a relayer or a block-assembly proxy: those overwrite the
   * advertised address, so the cluster connects to them and this host's own
   * listener sees almost nothing. True says the address is answered somewhere
   * else and never by what, because the validator cannot tell which of them it
   * is and a guess printed as fact is worse than the silence.
   */
  tpu_offhost: boolean;
}

/**
 * What the two per-epoch sections of the TPU path card cover.
 *
 * In slots rather than as a fraction, so the wording is the panel's own. Two
 * figures rather than one because an epoch is only counted whole where the
 * validator was up for the whole of it: `counted_slots` short of
 * `elapsed_slots` is a restart part way through, and saying so is the
 * difference between a quiet epoch and one that was only watched for its last
 * hour.
 */
export interface EpochSpan {
  epoch: number;
  /** Slots of this epoch that have happened. */
  elapsed_slots: number;
  /** Slots of this epoch the totals were actually summed over. */
  counted_slots: number;
  slots_in_epoch: number;
}

/**
 * Bundles the block engine sent this epoch, and the transactions in them.
 *
 * Counted where they arrive rather than where they execute, so this is an
 * upper bound on how much of the executed section came in this way rather than
 * an exact share of it: some are dropped before a worker ever sees them, and
 * the executed subset is not reported apart. Absent on a validator with no
 * block engine, and on one running BAM, which supersedes that path.
 */
export interface BundleStage {
  received: number;
  packets: number;
}

export interface VerifyStage {
  received: number;
  duplicate: number;
  below_floor: number;
  verified: number;
  /** Batches, not transactions. Never added to the counts beside it. */
  evicted_batches: number;
}

export interface ExecutedStage {
  attempted: number;
  cost_throttled: number;
  retryable: number;
  expired_bank: number;
  processed: number;
  succeeded: number;

  /**
   * Why a transaction the workers took up never reached the block, from the
   * error counters the same worker reports beside the counts above.
   *
   * Only the reasons that end a transaction are here. The ones that hand it
   * back — account in use, and the four cost-limit errors — are already drawn
   * as retries, and an instruction error is a transaction that did reach the
   * block having failed, which is drawn as that. Counting any of them again
   * here would be counting the same transaction twice.
   *
   * These do not sum to the whole of the loss: the long tail of rarer errors is
   * gathered into a derived row rather than given one each.
   */
  too_many_locks: number;
  account_missing: number;
  fee_payer_broke: number;
  fee_payer_invalid: number;
  blockhash_missing: number;
  blockhash_old: number;
  already_processed: number;
  bad_compute_budget: number;
  account_data_too_large: number;
  program_not_executable: number;
  program_restricted: number;
}

/**
 * One leader slot's waterfall, sent as its own list rather than nested on the
 * produced block it belongs to.
 *
 * The two are built on different threads and arrive moments apart in either
 * order — the block when its bank freezes, this when the scheduler notices the
 * leader slot has changed — so they are joined here by slot number instead of
 * one waiting on the other.
 *
 * Only ever present for slots this validator led: the counters behind it are
 * tagged with the bank being produced, and there is no bank unless we are the
 * one producing.
 */
export interface SlotWaterfall extends Waterfall {
  slot: number;
}

/**
 * What replay did with the last few hundred slots.
 *
 * Every figure is microseconds. All but the two peaks are means for one slot,
 * because what one slot costs is what compares against how long a slot lasts.
 *
 * The three groups are measured in three different ways and cannot be mixed.
 * `fetch`, `confirming` and `completing` are disjoint spans of replay's own
 * thread and add up. `poh_verify`, `tx_verify` and `dispatch` are sums of
 * overlapping asynchronous jobs and are worth only relative to each other.
 * Everything from `execute` down is thread time summed across the workers, so
 * it partitions cleanly and routinely exceeds the slot it describes.
 */
/**
 * The machine the validator runs on, sampled once a second from /proc.
 *
 * Three questions that must not be run together. Load and memory are what the
 * process has to work with, `filesystems` is what will run out of room, and
 * `devices` is what will run out of throughput. A box can be in trouble on any
 * one of them while the other two read perfectly healthy.
 */
export interface Host {
  cores: number;
  load_one: number;
  load_five: number;
  load_fifteen: number;
  threads: number;
  running: number;

  memory_total: number;
  memory_available: number;
  /** Page cache and buffers: used, but handed back the moment it is wanted. */
  memory_reclaimable: number;
  memory_free: number;
  /** Absent where the machine has no swap configured at all. */
  swap: { total: number; used: number } | null;

  filesystems: FilesystemUsage[];
  devices: DeviceLoad[];
}

/**
 * One group of the validator's threads over one second, mean per thread.
 *
 * A pool's threads share a name and differ by a trailing number, which the
 * validator strips; `count` says how many stand behind the row.
 */
export interface ThreadGroup {
  /** Empty on the folded row. */
  name: string;
  count: number;
  /** The cores the threads may run on, where that is fewer than the machine has. */
  cores: string | null;
  /** Share of the second on a core, and runnable but waiting for one. */
  on_cpu: number;
  waiting: number;
  /** True for the one row every group not shown is folded into. */
  other: boolean;
}

/** Where the threads spent one second: the busiest groups by their minute's mean, and the rest. */
export interface ThreadsSample {
  timestamp_nanos: number;
  /** Threads in the process, every group included. */
  threads: number;
  groups: ThreadGroup[];
}

/** How full one filesystem is. A level, so nothing here is a rate. */
export interface FilesystemUsage {
  name: string;
  path: string;
  total: number;
  available: number;
}

/** How hard one block device was worked over the last second. */
export interface DeviceLoad {
  device: string;
  /** Every role whose path is on this device. Two mounts on one disk share a
   *  queue, so they share a row. */
  roles: string[];
  /** Share of the sample the device had a request in flight, in `[0, 1]`. Not
   *  a fill: a device can sit at 1 with the filesystem nearly empty. */
  busy: number;
  /** Mean milliseconds a request waited, null where none did. */
  wait_ms: number | null;
  operations_per_second: number;
  read_per_second: number;
  write_per_second: number;
}

export interface ReplayWindow {
  /** Slots behind the figures, which is short until the window has filled. */
  slots: number;
  transactions: number;

  fetch: number;
  confirming: number;
  completing: number;
  /** The worst single slot's total, not the largest each field reached. */
  serial_peak: number;

  poh_verify: number;
  tx_verify: number;
  dispatch: number;

  execute: number;
  bytecode: number;
  serialising: number;
  deserialising: number;
  load: number;
  store: number;
  program_cache: number;
  compiling: number;
  program_cache_peak: number;
  checking: number;
  other: number;
  cpu_peak: number;
}

/**
 * What one block this validator produced cost, and which account took the most
 * of it.
 *
 * Sent as its own list and joined to produced blocks by slot, because the cost
 * tracker reports as the bank freezes while the block is captured on another
 * thread, so either can arrive first. Only blocks this validator built are
 * sent; the point arrives for every slot replayed, and other people's blocks
 * are not this operator's to act on.
 */
export interface SlotCost {
  slot: number;
  /** Pubkey of the account that consumed the most compute in this block. */
  costliest_account: string;
  costliest_cost: number;
  /** The block's total as the cost tracker counted it. */
  block_cost: number;
  accounts: number;
  /** Accounts within five percent of the per-account ceiling. */
  contended: number;
  new_account_data: number;
  in_flight: number;
}

export interface IngestSummary {
  window_seconds: number;
  paths: IngestPath[];
}

/**
 * How the XDP transmit path is set up, on a validator running one.
 *
 * Absent entirely otherwise: the validator only reports this where it was given
 * a config. Describes the path under turbine, repair and gossip alike despite
 * the flags being named for retransmit, and says nothing about receiving.
 *
 * A configuration rather than a measurement. It answers whether the flags took
 * and on what card, not whether the path is fast. Nothing the validator reports
 * answers that.
 */
export interface XdpConfig {
  /**
   * Whether the socket bound with zero-copy rather than copy.
   *
   * True is trustworthy: the flag goes straight to the bind, which fails
   * outright on a driver that cannot do it rather than falling back, so a
   * validator that is running and reporting this really has it.
   */
  zero_copy: boolean;
  driver: string;
  /** Both of these read "unknown" where the PCI database could not be read. */
  vendor: string;
  model: string;
  kernel_version: string;
}

export interface StartupProgress {
  phase: string;
  detail: string | null;
  running: boolean;
  /** Ledger replay progress from 0 to 1, on the phases that can measure it. */
  fraction: number | null;
  /**
   * Share of the cluster's stake visible in gossip, while waiting for a
   * supermajority. Null in every other phase, and on the many validators that
   * never wait at all.
   */
  stake_percent: number | null;
  /**
   * How long the current phase has been running, and what each finished phase
   * took.
   *
   * Most of the boot sequence cannot say how far through it is — nothing counts
   * the accounts left to index or the archive left to unpack — so how long it
   * has been going stands in. On a boot that has stopped somewhere that is the
   * figure actually being looked for.
   */
  phase_elapsed_nanos: number;
  phases_taken: PhaseTiming[];
}

export interface PhaseTiming {
  phase: string;
  elapsed_nanos: number;
}

export interface Health {
  replay: string;
  vote: string;
}

export interface SkipRate {
  epoch: number;
  rate: number | null;
}

/** The envelope every message arrives in. */
export interface Envelope {
  topic: string;
  key: string;
  id?: number;
  value: unknown;
}
