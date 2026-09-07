/**
 * Reading the two cache panels down to the one line each shows when folded.
 *
 * Kept out of the component so the thresholds and the summaries can be tested
 * without a DOM, in the same way as the replay rows and the block costs.
 */

import { bytes, count } from "./format";
import type { AccountsCache, ProgramCache } from "./types";

export const CACHES_STORAGE_KEY = "agave-dashboard-caches";

/**
 * Which sections the viewer last left unfolded.
 *
 * Both start folded, so the panel opens as two lines saying whether each
 * subsystem is healthy and nothing else. Whoever wants the figures under one of
 * them wants them on the next reload too, which is the same reasoning the
 * sidebar collapse is remembered under, and the same mechanism.
 *
 * Names rather than a count or a pair of flags, so a section added or removed
 * later cannot silently unfold the wrong one.
 */
export function readOpenSections(): string[] {
  try {
    const stored = window.localStorage.getItem(CACHES_STORAGE_KEY);
    return stored ? stored.split(",").filter(Boolean) : [];
  } catch {
    // Private browsing and some embedded webviews refuse storage outright.
    return [];
  }
}

export function writeOpenSections(open: string[]): void {
  try {
    window.localStorage.setItem(CACHES_STORAGE_KEY, open.join(","));
  } catch {
    // Not being able to remember the choice is not a reason to refuse it.
  }
}

/** How a rate is coloured, matching the tones the rest of the page uses. */
export type RateTone = "good" | "warn" | "bad" | "muted";

/**
 * What colour a hit rate is worth.
 *
 * The two bounds are the ones both panels already used on their headline
 * figure. The middle band is the change: it used to be left untoned, which was
 * fine while the figure was always on screen, and is not once a section can be
 * folded away. Folded, the dot beside the name is the only thing left to carry
 * the state, and a dot that is green at ninety one percent says the opposite of
 * what it should.
 */
export function rateTone(rate: number | null): RateTone {
  if (rate === null) return "muted";
  if (rate >= 0.98) return "good";
  if (rate >= 0.9) return "warn";
  return "bad";
}

/**
 * Every account read in the window, and the share of them answered from memory.
 *
 * The read cache keeps a hit rate of its own and it is the wrong headline: the
 * write cache is consulted first, so that rate is taken over only the reads
 * that got past there, and it cannot be squared with the three way split the
 * section shows. What matters is how much of everything read had to come off a
 * disk.
 */
export function servedFromMemory(accounts: AccountsCache): {
  loads: number;
  rate: number | null;
} {
  const loads = accounts.from_write_cache + accounts.from_read_cache + accounts.from_storage;
  return { loads, rate: loads > 0 ? 1 - accounts.from_storage / loads : null };
}

/**
 * The program cache in one line, for when the section is folded.
 *
 * Four figures rather than the eight the open section carries, chosen for what
 * a passing glance is checking: how much work went through it, how much of that
 * it failed to answer, how full it is, and how hard it is having to shed. The
 * rest are there to explain those once something looks wrong.
 *
 * A list rather than a sentence, and in falling order of what a glance wants,
 * because a narrow screen shows only the first two. Run together as one string
 * the line had to be cut with an ellipsis instead, which lands mid-figure.
 */
export function programGloss(cache: ProgramCache): string[] {
  const entries =
    cache.peak_entries === null
      ? `${count(cache.entry_limit)} entry limit`
      : `${count(cache.peak_entries)}/${count(cache.entry_limit)} entries`;
  return [
    `${count(cache.looked_up)} lookups`,
    `${count(cache.misses)} misses`,
    entries,
    `${count(cache.evictions)} evictions`,
  ];
}

/** The accounts database in one line, on the same principle. */
export function accountsGloss(accounts: AccountsCache): string[] {
  const { loads } = servedFromMemory(accounts);
  const perSecond =
    accounts.window_seconds > 0 ? accounts.stored_bytes / accounts.window_seconds : 0;
  const line = [
    `${count(loads)} reads`,
    `${count(accounts.from_storage)} from disk`,
    `${bytes(Math.round(perSecond))}/s written`,
  ];
  // Absent on a validator whose accounts database has not reported its files
  // yet, rather than shown as nought of nought.
  if (accounts.disk) {
    line.push(`${bytes(accounts.disk.used)}/${bytes(accounts.disk.allocated)} on disk`);
  }
  return line;
}
