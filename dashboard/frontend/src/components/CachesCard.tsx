import { useEffect, useState, type ReactNode } from "react";
import {
  accountsGloss,
  programGloss,
  rateTone,
  readOpenSections,
  servedFromMemory,
  writeOpenSections,
} from "../caches";
import { bytes, count, percent } from "../format";
import type { AccountsCache, ProgramCache } from "../types";
import { useStore } from "../useStore";
import { Card, Explain, Meter, Stat } from "./primitives";

/**
 * The two things replay waits on, and how well each is going.
 *
 * One panel rather than two because they answer the same question in the same
 * shape: what share of what was asked for came back without the slow path, over
 * the same minute, from counters each subsystem resets and reports itself. Side
 * by side they were a row in the grid, which sizes every card in it to the
 * tallest, and the accounts panel is half again the height of the program cache
 * one, so the shorter of the two sat over a block of nothing.
 *
 * Either section folds to its heading, and both start folded. The two headings
 * are what the panel is for at a glance: a dot, a rate and four figures each,
 * saying whether the thing is healthy without asking anyone to read a grid. The
 * figures under them are for when the answer is no.
 *
 * Which sections are open is remembered per host, on the same reasoning as the
 * sidebar collapse: someone who opened one to watch it wants it open on the
 * next reload rather than having to open it again.
 */
export function CachesCard() {
  const store = useStore();
  const programs = store.get<ProgramCache | null>("summary", "program_cache");
  const accounts = store.get<AccountsCache | null>("summary", "accounts_cache");
  // Read once at the first render. Unlike the theme there is nothing to stamp
  // before the bundle runs: a section that starts closed is what an unstyled
  // page shows anyway, so there is no flash to head off.
  const [open, setOpen] = useState<string[]>(readOpenSections);
  useEffect(() => writeOpenSections(open), [open]);
  if (!programs && !accounts) return null;

  const fold = (key: string) =>
    setOpen((was) => (was.includes(key) ? was.filter((k) => k !== key) : [...was, key]));

  return (
    <Card title="Caches and storage" aside="one-minute counters · reset every bank">
      <div className="caches">
        {programs && (
          <Group
            name="Program cache"
            rate={programs.hit_rate}
            gloss={programGloss(programs)}
            open={open.includes("programs")}
            onFold={() => fold("programs")}
            explain="How often replay found a program already compiled rather than having to build it, over the last minute. A low rate means replay spends its time compiling, which slows a block down and leaves less room to pack the next one. Evictions are the usual cause."
          >
            <ProgramBody cache={programs} />
          </Group>
        )}
        {accounts && (
          <Group
            name="Accounts"
            rate={servedFromMemory(accounts).rate}
            gloss={accountsGloss(accounts)}
            open={open.includes("accounts")}
            onFold={() => fold("accounts")}
            explain="Of every account replay read in the last minute, the share answered without touching a storage file. Counted across both caches, so it matches the split below it. A read that misses both goes to disk, which is orders of magnitude slower, and a falling figure here is what slow replay looks like before anything else shows it."
          >
            <AccountsBody accounts={accounts} />
          </Group>
        )}
      </div>
    </Card>
  );
}

/**
 * One foldable section: a heading that states its own health, and a body.
 *
 * The heading is a row with a button in it rather than a row that is a button,
 * which is how the produced blocks on the slot page do it. The rate here is the
 * one figure on either section that most needs explaining, and an explanation
 * is itself a button, which cannot be nested inside another one. So the chevron
 * carries the control and the row carries a click for the pointer.
 */
function Group({
  name,
  rate,
  gloss,
  open,
  onFold,
  explain,
  children,
}: {
  name: string;
  rate: number | null;
  gloss: string[];
  open: boolean;
  onFold: () => void;
  explain: string;
  children: ReactNode;
}) {
  const tone = rateTone(rate);

  return (
    <section className="cache-group">
      {/* Not a button, so the rate can keep its explanation. Keyboard reaches
          the chevron, which is the control; this is the pointer's larger
          target. */}
      <div className="cache-head" onClick={onFold}>
        <span className="cache-name">
          <i className={`cache-dot tone-${tone}`} aria-hidden="true" />
          {name}
        </span>
        <span className={`cache-rate tone-${tone}`}>
          <Explain text={explain}>{rate === null ? "—" : percent(rate, 2)}</Explain>
        </span>
        <span className="cache-gloss">
          {gloss.map((part) => (
            <i key={part}>{part}</i>
          ))}
        </span>
        <button
          type="button"
          className="cache-fold"
          aria-expanded={open}
          aria-label={`${open ? "Fold" : "Unfold"} ${name}`}
          onClick={(event) => {
            // The row under it toggles too, and two toggles are none.
            event.stopPropagation();
            onFold();
          }}
        >
          {open ? "−" : "+"}
        </button>
      </div>
      {open && <div className="cache-open">{children}</div>}
    </section>
  );
}

/**
 * Every figure here is a rate rather than a standing total. The cache resets its
 * counters each time a bank is made from a parent, two or three times a second,
 * and reports them as it does, so what is summed is a minute of real work rather
 * than anything the cache is holding.
 *
 * The one exception is the entry peak, which is a level and is treated as one:
 * it is written only when an eviction runs, so the highest reading across the
 * window is taken rather than the latest.
 *
 * Size is shown in entries rather than in bytes. This cache is a map on the
 * heap, bounded by how many entries it may hold and given no byte budget at all,
 * so entries against that limit is the only fill figure it can honestly report.
 */
function ProgramBody({ cache }: { cache: ProgramCache }) {
  const filled =
    cache.peak_entries !== null && cache.entry_limit > 0
      ? cache.peak_entries / cache.entry_limit
      : null;

  // Both halves of the same figure. Insertions alone counts only keys the cache
  // had never seen, so shown on its own beside a much larger eviction count it
  // reads as a cache collapsing when it is holding its size: the reloads make up
  // the difference, and they belong here rather than under evictions.
  const compiled = cache.insertions + cache.reloads;
  const compiledBreakdown = [
    `${count(cache.insertions)} new`,
    `${count(cache.reloads)} reloaded`,
    ...(cache.lost_insertions > 0 ? [`${count(cache.lost_insertions)} lost`] : []),
  ].join(" · ");

  return (
    <>
      <div className="cache-figures">
        <Stat
          label="Lookups"
          explain="Every time replay asked the cache for a program in the window, hits and misses together. Not the same as loads: a hit is the cache answering without building anything, and only a miss turns into a compile. Small numbers are ordinary, since a block touches few distinct programs, which is why this is summed over a minute rather than read off a single slot."
          value={count(cache.looked_up)}
          sub={`${count(cache.hits)} hits · ${count(cache.misses)} misses`}
        />
        <Stat
          label="Compiled"
          explain="Every program the cache had to build in the window, which is the work a hit avoids. New ones are keys the cache had not seen. Reloaded ones are keys it already had and had thrown the compiled code away for, so they are the cost of an eviction coming back. Compare this against evictions beside it: a cache in steady state compiles about as many as it drops."
          value={count(compiled)}
          sub={compiledBreakdown}
        />
        <Stat
          label="Evictions"
          explain="Compiled programs dropped to keep the cache within its entry limit. Set this against what was compiled beside it. Roughly equal figures mean a cache holding its size, and evictions running well ahead mean it is shedding programs faster than they are wanted. Used once counts the ones that were compiled, called a single time and then dropped: compilation spent for one transaction, and ordinary in a network with a long tail of programs almost nobody calls."
          value={count(cache.evictions)}
          sub={`${count(cache.one_hit_wonders)} used once`}
        />
        <Stat
          label="Pruned"
          explain="Entries dropped because the fork they belonged to was abandoned, or because they had not been recompiled for the incoming epoch. Neither is a fault; both are the cache keeping up with the chain. The epoch figure rises sharply around an epoch boundary and is expected to."
          value={count(cache.prunes_orphan + cache.prunes_environment)}
          sub={`${count(cache.prunes_orphan)} orphaned · ${count(cache.prunes_environment)} epoch`}
        />
      </div>

      {/* Drawn whether or not an eviction has happened, so the section keeps its
          height. The bar is empty until one has, which is honest: nothing has
          reported where the cache stood. */}
      <div className="cache-storage">
        <div className="cache-storage-head">
          <Explain text="The most entries seen loaded at any eviction in the last minute, against the limit eviction keeps them under. Only measured when an eviction runs, so it is a high-water mark rather than a live reading, and it is empty on a validator that has not had to evict anything. Approaching the limit is what precedes a falling hit rate.">
            <span className="cache-storage-label">Peak entries</span>
          </Explain>
          <span className="cache-storage-value">
            {cache.peak_entries === null ? "—" : count(cache.peak_entries)}
            <span className="cache-storage-limit"> / {count(cache.entry_limit)}</span>
          </span>
        </div>
        <Meter fraction={filled ?? 0} />
      </div>

      {cache.replacements > 0 && (
        <p className="cache-footnote">
          <Explain text="An entry already in the cache compiled a second time. Not harmful, but it is work that need not have happened, and a persistent figure here is worth reporting upstream.">
            {count(cache.replacements)} recompiled needlessly.
          </Explain>
        </p>
      )}
    </>
  );
}

/**
 * Reads are counted in accounts and writes in bytes, which is lopsided and
 * deliberate. Agave measures the load path in accounts and never in bytes, so
 * there is nothing to build a read throughput from. The write path is measured
 * both ways, so that one gets a rate.
 *
 * Not built from `/proc/self/io`, which would give true bytes for both and be
 * the wrong number: it is process-wide, so the blockstore's writes, snapshot
 * archiving and the log would all land under a heading saying Accounts.
 */
function AccountsBody({ accounts }: { accounts: AccountsCache }) {
  const perSecond = (total: number) =>
    accounts.window_seconds > 0 ? total / accounts.window_seconds : 0;
  const disk = accounts.disk;
  const live = disk && disk.allocated > 0 ? disk.used / disk.allocated : null;

  return (
    <>
      <div className="cache-section-title">Reads answered from</div>
      <div className="cache-figures">
        <Stat
          label="Write cache"
          explain="Accounts this validator has written recently and not yet flushed to a storage file. The first place a read looks, and the cheapest to answer from."
          value={count(accounts.from_write_cache)}
        />
        <Stat
          label="Read cache"
          explain="Accounts fetched earlier and kept in memory, with what the cache is holding right now beneath. That size is a level rather than a rate, read as it stands rather than summed over the window. Evictions are how it makes room; none at all means it is not under pressure and raising --accounts-db-read-cache-limit would buy nothing."
          value={count(accounts.from_read_cache)}
          sub={`${bytes(accounts.cache_bytes)} · ${count(accounts.cache_entries)} accounts · ${count(accounts.evictions)} evicted`}
        />
        <Stat
          label="Storage"
          explain="Reads that missed both caches and went to a file. The nearest thing here to a disk read rate, counted in accounts because nothing on that path counts bytes."
          value={count(accounts.from_storage)}
          sub={`${count(Math.round(perSecond(accounts.from_storage)))}/s`}
        />
        <Stat
          label="Written to storage"
          explain="Accounts written out of the cache into storage files over the window. This is the one path the accounts database measures in bytes as well as in accounts, which is why the write side has a throughput figure and the read side does not."
          value={`${bytes(Math.round(perSecond(accounts.stored_bytes)))}/s`}
          sub={`${bytes(accounts.stored_bytes)} · ${count(accounts.stored_accounts)} accounts · ${count(Math.round(perSecond(accounts.stored_accounts)))}/s`}
        />
      </div>

      {disk && (
        <>
          <div className="cache-storage">
            <div className="cache-storage-head">
              <Explain text="How much space the storage files take up, and how much of that is still referenced by a live account. The gap between them is dead account data that shrink has not reclaimed yet. Shrink runs continuously as candidates appear rather than on a schedule, so there is no next compaction to count down to.">
                <span className="cache-storage-label">On disk · live of allocated</span>
              </Explain>
              <span className="cache-storage-value">
                {bytes(disk.used)}
                <span className="cache-storage-limit"> / {bytes(disk.allocated)}</span>
              </span>
            </div>
            <Meter fraction={live ?? 0} />
          </div>
          <div className="cache-figures">
            <Stat
              label="Fragmented"
              explain="Allocated bytes no longer referenced by any live account. Shrink rewrites storage files to reclaim this, continuously rather than on a schedule, so a steady figure here is normal and only a growing one is worth watching."
              value={bytes(disk.fragmented)}
              sub={disk.allocated > 0 ? percent(disk.fragmented / disk.allocated, 1) : undefined}
            />
            <Stat
              label="Storage files"
              explain="How many append-only files the accounts data is spread across. Rises with the number of slots held and falls as shrink combines them."
              value={count(disk.storages)}
            />
          </div>
        </>
      )}

      <p className="cache-footnote">
        Reads are counted in accounts and writes in bytes because that is how the
        database counts them. Nothing on the load path counts bytes, so there is
        no read throughput to report.
      </p>
    </>
  );
}
