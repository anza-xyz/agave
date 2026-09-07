/**
 * Holds everything the websocket has told us, and notifies React when it
 * changes.
 *
 * Updates arrive far faster than a person can read, so notifications are
 * coalesced to one per animation frame. Without that, a busy validator would
 * re-render the tree several hundred times a second to no visible effect.
 */

import { leaderAt, NO_LEADER, type LeaderRef } from "./schedule";
import type {
  Displays,
  EpochInfo,
  Envelope,
  NetworkSample,
  Peer,
  SlotEntry,
  ThreadsSample,
  TpsSample,
} from "./types";

/** Slots kept for the strip and sidebar. Matches the server's overview length. */
const MAX_SLOTS = 512;

/**
 * This validator's own leader slots kept beyond that window.
 *
 * A validator leads about four slots in every eight hundred, so five hundred
 * slots of history usually contains none of its own. Kept separately, the
 * sidebar's own-slots view has something to show; pruned with everything else
 * it would be empty almost all the time.
 *
 * Sixty-four, which is what that rail needs. The schedule page reaches ours by
 * searching the packed history instead. Matches `OWN_SLOTS_KEPT` on the server:
 * the two are one figure split across the wire and only agree by being kept the
 * same.
 */
const MAX_OWN_SLOTS = 64;

/** TPS samples kept for the chart. */
const MAX_TPS_SAMPLES = 300;

/** Thread samples kept: the minute the host card draws. */
const MAX_THREAD_SAMPLES = 60;

export type ConnectionState = "connecting" | "open" | "closed";

/**
 * A request sent to the validator and not yet answered.
 *
 * The server answers everything it can parse as a request, including keys it
 * does not recognise, so an entry that is never settled means the connection
 * went away rather than the request being ignored.
 */
interface Pending {
  resolve: (value: unknown) => void;
  reject: (reason: Error) => void;
}

export class Store {
  /** Latest value for each `topic.key`, exactly as published. */
  private values = new Map<string, unknown>();
  private slots = new Map<number, SlotEntry>();
  private tps: TpsSample[] = [];
  private network: NetworkSample[] = [];
  private threads: ThreadsSample[] = [];
  private connection: ConnectionState = "connecting";
  private sender: ((frame: string) => void) | null = null;
  private pending = new Map<number, Pending>();
  private nextRequestId = 1;
  /** The peer list the index below was built from, to know when it is stale. */
  private peers: Peer[] | null = null;
  private peerIndex = new Map<string, Peer>();
  /**
   * Resolved leaders by slot, so that repeated lookups return the same object.
   *
   * Not for speed. The rows that show a leader are memoised on their props, and
   * a freshly built object every render would defeat that and rebuild the whole
   * list on every meter sample. Cleared whenever the epoch or the peer table
   * changes, which is the only way an answer here can change.
   */
  private leaderCache = new Map<number, LeaderRef>();
  /** Us, rebuilt only when one of the three values it is made of changes. */
  private ours: LeaderRef = NO_LEADER;
  private oursFrom = "";
  /**
   * Names and icons for the whole cluster, once something has asked for them.
   *
   * The peer table only reaches the leaders of the window a client holds, so a
   * turn from further back has a key and nothing else until this arrives.
   * Empty until `loadDisplays` is called, which the schedule page does the
   * first time somebody searches.
   */
  private displays = new Map<string, { name: string | null; icon: string | null }>();
  /**
   * Epochs other than the current one, once something has asked for them.
   *
   * The current epoch arrives on its own message; this holds the ones reached
   * by reading back through the history, which crosses a boundary whenever the
   * tip is within a hundred thousand slots of one. `null` for an epoch the
   * validator no longer has a schedule for, remembered so it is asked once.
   */
  private epochs = new Map<number, EpochInfo | null>();
  /**
   * Bumped whenever a leader could newly resolve: an epoch's arrays arriving,
   * the names arriving, the epoch turning.
   *
   * Read by anything memoising over resolved leaders. The store is one object
   * for the life of the page, so a memo keyed on it alone never re-runs, and
   * the turns built before this moved would keep their bare keys.
   */
  private leaderRevision = 0;

  private listeners = new Set<() => void>();
  private frame: number | null = null;
  /** Bumped on every change so `useSyncExternalStore` sees a new snapshot. */
  private revision = 0;

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  };

  getRevision = (): number => this.revision;

  get<T>(topic: string, key: string): T | undefined {
    return this.values.get(`${topic}.${key}`) as T | undefined;
  }

  getConnection(): ConnectionState {
    return this.connection;
  }

  /**
   * True once enough has arrived for the dashboard to be worth looking at.
   * The identity comes from the static publish on connect and the slots from
   * the retained overview, so both land in the first burst of messages.
   */
  isReady(): boolean {
    // A validator that is still booting has no slots and no identity to report,
    // but the boot sequence is exactly what should be on screen then, so the
    // splash has nothing left to wait for.
    const startup = this.values.get("summary.startup_progress") as
      | { running: boolean }
      | undefined;
    if (startup && !startup.running) return true;

    return this.values.has("summary.identity_key") && this.slots.size > 0;
  }

  setConnection(state: ConnectionState): void {
    this.connection = state;
    if (state !== "open") {
      // A reply can only come back on the socket that carried the request, so
      // losing one ends every request in flight. Left pending they would be
      // promises that never settle, and a caller waiting on one shows a
      // loading state that never resolves.
      this.sender = null;
      const inflight = [...this.pending.values()];
      this.pending.clear();
      for (const pending of inflight) pending.reject(new Error("connection lost"));
    }
    this.touch();
  }

  /**
   * How to write to the current socket, installed by `connect` when one opens.
   *
   * Held rather than reached for, because the store is what callers have and
   * the socket is replaced on every reconnect.
   */
  setSender(sender: (frame: string) => void): void {
    this.sender = sender;
  }

  /**
   * Asks the validator for something, rather than waiting for it to be pushed.
   *
   * For data too large to send to every client on connect and too rarely read
   * to send at all: a span of slot history is the first of it. Rejects rather
   * than queues when there is no connection, since a request made now and
   * answered after the next reconnect would arrive against a page that has
   * moved on.
   */
  request<T>(topic: string, key: string, params: unknown): Promise<T> {
    const sender = this.sender;
    if (sender === null) return Promise.reject(new Error("not connected"));

    const id = this.nextRequestId;
    this.nextRequestId += 1;
    return new Promise<T>((resolve, reject) => {
      this.pending.set(id, { resolve: resolve as (value: unknown) => void, reject });
      try {
        sender(JSON.stringify({ topic, key, id, params }));
      } catch (error) {
        this.pending.delete(id);
        reject(error instanceof Error ? error : new Error(String(error)));
      }
    });
  }

  /**
   * Who leads a slot, and what to call them.
   *
   * The one place that knows how a leader is put back together, because it is
   * no longer in one place on the wire: the key comes from the epoch's turn
   * array and the name and icon from the peer table. A slot carries neither.
   *
   * The peer table covers the leaders of the window a client holds and the ones
   * about to lead, so a live turn is named before it is drawn. A turn from
   * further back than that, or from an epoch whose schedule the page does not
   * have, resolves to a key with no name or to nothing at all, and the callers
   * fall back in that order.
   */
  getLeaderRevision = (): number => this.leaderRevision;

  /**
   * Fetches an epoch's schedule, once.
   *
   * Only the current one is published, it being half a megabyte and wanted by
   * the pages that read back far enough to leave it. Asked for rather than sent
   * for the same reason the names are.
   */
  async loadEpoch(epoch: number): Promise<void> {
    if (this.epochs.has(epoch)) return;
    const record = await this.request<EpochInfo | null>("epoch", "query", { epoch });
    // Held even when nothing came back, so an epoch the validator has no
    // schedule for is asked about once rather than on every search.
    this.epochs.set(epoch, record ?? null);
    if (record) this.leadersChanged();
  }

  /** Every answer already given was given without whatever just arrived. */
  private leadersChanged(): void {
    this.leaderRevision += 1;
    this.leaderCache.clear();
    this.touch();
  }

  leaderOf(slot: number, mine: boolean): LeaderRef {
    // Ours takes no lookup, and must not depend on one. Both sources have a
    // reach, and our own slots are kept well past both: five hundred of them is
    // about eleven hours, which is far outside the peer table's window and
    // crosses an epoch boundary often enough that the turn array cannot answer
    // for them either. The validator tells us who we are directly.
    if (mine) return this.ourLeader();

    const cached = this.leaderCache.get(slot);
    if (cached) return cached;

    const key = this.leaderAtAny(slot);
    // The peer table first, being the fresher of the two: it is rebuilt every
    // few seconds where the display table is fetched once. Both hold the same
    // answer for a leader they both know.
    const shown = key === null ? undefined : (this.peersByIdentity().get(key) ?? this.displays.get(key));
    const leader: LeaderRef =
      key === null ? NO_LEADER : { key, name: shown?.name ?? null, icon: shown?.icon ?? null };
    this.leaderCache.set(slot, leader);
    return leader;
  }

  /**
   * Who leads a slot, from whichever epoch's arrays cover it.
   *
   * The published one first, being the one nearly every slot on the page falls
   * in. The fetched ones are only consulted for a slot outside it, which is a
   * page that has read back past a boundary.
   */
  private leaderAtAny(slot: number): string | null {
    const here = leaderAt(this.values.get("epoch.new") as EpochInfo | undefined, slot);
    if (here !== null) return here;
    for (const past of this.epochs.values()) {
      if (past === null) continue;
      const there = leaderAt(past, slot);
      if (there !== null) return there;
    }
    return null;
  }

  /**
   * Fetches the cluster's names and icons, once.
   *
   * Called when something needs a name for a leader the peer table does not
   * reach, which in practice means a search that has gone into history. Later
   * calls are free: the table does not change often enough to be worth asking
   * twice in a session, and a stale name is a great deal better than none.
   */
  async loadDisplays(): Promise<void> {
    if (this.displays.size > 0) return;
    const table = await this.request<Displays>("summary", "displays", {});
    const next = new Map<string, { name: string | null; icon: string | null }>();
    table.keys.forEach((key, index) => {
      next.set(key, { name: table.names[index] ?? null, icon: table.icons[index] ?? null });
    });
    this.displays = next;
    this.leadersChanged();
  }

  /**
   * This validator, from what it publishes about itself.
   *
   * The same three values the header is drawn from, so a turn of ours is
   * labelled exactly as the header labels us rather than by a second route that
   * can disagree with it.
   */
  private ourLeader(): LeaderRef {
    const key = (this.values.get("summary.identity_key") as string | undefined) ?? null;
    const name = (this.values.get("summary.identity_name") as string | undefined) ?? null;
    const icon = (this.values.get("summary.identity_icon") as string | undefined) ?? null;
    // Rebuilt on change rather than per call: the rows that draw a leader are
    // memoised on their props, and a fresh object each render would defeat it.
    const stamp = `${key} ${name} ${icon}`;
    if (this.oursFrom !== stamp) {
      this.oursFrom = stamp;
      this.ours = { key, name, icon };
    }
    return this.ours;
  }

  /**
   * The peer table by identity, rebuilt only when the table itself changes.
   *
   * Compared by reference: the store replaces the whole array when a new one is
   * published and never mutates it, so a different array is a different table.
   */
  private peersByIdentity(): Map<string, Peer> {
    const peers = (this.values.get("peers.all") as Peer[] | undefined) ?? [];
    if (this.peers !== peers) {
      this.peers = peers;
      this.peerIndex = new Map(peers.map((peer) => [peer.identity, peer]));
    }
    return this.peerIndex;
  }

  /** Slots in ascending order. */
  getSlots(): SlotEntry[] {
    return [...this.slots.values()].sort((a, b) => a.slot - b.slot);
  }

  getSlot(slot: number): SlotEntry | undefined {
    return this.slots.get(slot);
  }

  getTps(): TpsSample[] {
    return this.tps;
  }

  getNetwork(): NetworkSample[] {
    return this.network;
  }

  getThreads(): ThreadsSample[] {
    return this.threads;
  }

  apply(envelope: Envelope): void {
    const { topic, key, value } = envelope;

    // Replies to our own requests carry an id and are not state. An id we are
    // not waiting on is dropped: a reply that outlived its caller is the
    // ordinary result of a reconnect, not something to act on.
    if (envelope.id !== undefined) {
      const pending = this.pending.get(envelope.id);
      if (pending) {
        this.pending.delete(envelope.id);
        pending.resolve(value);
      }
      return;
    }

    if (topic === "slot" && key === "overview") {
      this.slots.clear();
      for (const entry of value as SlotEntry[]) this.slots.set(entry.slot, entry);
      this.trimSlots();
    } else if (topic === "slot" && key === "update") {
      const entry = value as SlotEntry;
      this.slots.set(entry.slot, entry);
      this.trimSlots();
    } else if (topic === "summary" && key === "network_history") {
      this.network = (value as NetworkSample[]).slice(-MAX_TPS_SAMPLES);
    } else if (topic === "summary" && key === "network_sample") {
      const sample = value as NetworkSample;
      const last = this.network[this.network.length - 1];
      if (!last || sample.timestamp_nanos > last.timestamp_nanos) {
        this.network = [...this.network, sample].slice(-MAX_TPS_SAMPLES);
      }
    } else if (topic === "summary" && key === "threads_history") {
      this.threads = (value as ThreadsSample[]).slice(-MAX_THREAD_SAMPLES);
    } else if (topic === "summary" && key === "threads_sample") {
      const sample = value as ThreadsSample;
      const last = this.threads[this.threads.length - 1];
      if (!last || sample.timestamp_nanos > last.timestamp_nanos) {
        this.threads = [...this.threads, sample].slice(-MAX_THREAD_SAMPLES);
      }
    } else if (topic === "summary" && key === "tps_history") {
      this.tps = (value as TpsSample[]).slice(-MAX_TPS_SAMPLES);
    } else if (topic === "summary" && key === "tps_sample") {
      const sample = value as TpsSample;
      // The retained history and the live samples overlap by design, so keep
      // the series strictly increasing instead of trusting arrival order.
      if (this.tps.length === 0 || sample.slot > this.tps[this.tps.length - 1].slot) {
        this.tps = [...this.tps, sample].slice(-MAX_TPS_SAMPLES);
      }
    } else {
      this.values.set(`${topic}.${key}`, value);
      // The two things a resolved leader is made of. Either changing makes
      // every answer already given potentially wrong.
      if (topic === "epoch" || topic === "peers") this.leadersChanged();
    }

    this.touch();
  }

  private trimSlots(): void {
    if (this.slots.size <= MAX_SLOTS) return;
    const ordered = [...this.slots.values()].sort((a, b) => a.slot - b.slot);
    // Split rather than walked oldest-first, because our own slots are kept to
    // a separate depth. Walking one list and skipping ours would have deleted
    // newer slots to make room for the ones it skipped.
    const own = ordered.filter((entry) => entry.mine).slice(-MAX_OWN_SLOTS);
    const rest = ordered.filter((entry) => !entry.mine).slice(-MAX_SLOTS);
    this.slots = new Map([...rest, ...own].map((entry) => [entry.slot, entry]));
  }

  private touch(): void {
    if (this.frame !== null) return;
    this.frame = requestAnimationFrame(() => {
      this.frame = null;
      this.revision += 1;
      for (const listener of this.listeners) listener();
    });
  }
}
