import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  accountsGloss,
  CACHES_STORAGE_KEY,
  programGloss,
  rateTone,
  readOpenSections,
  servedFromMemory,
  writeOpenSections,
} from "./caches";
import type { AccountsCache, ProgramCache } from "./types";

/** A mainnet-shaped program cache minute, to be overridden a field at a time. */
function programs(over: Partial<ProgramCache> = {}): ProgramCache {
  return {
    looked_up: 2_245_551,
    hits: 2_244_926,
    misses: 625,
    hit_rate: 0.9997,
    evictions: 621,
    reloads: 589,
    insertions: 32,
    lost_insertions: 0,
    replacements: 0,
    one_hit_wonders: 24,
    prunes_orphan: 0,
    prunes_environment: 0,
    peak_entries: 462,
    entry_limit: 512,
    ...over,
  };
}

function accounts(over: Partial<AccountsCache> = {}): AccountsCache {
  return {
    read: 2_210_362,
    hit_rate: 0.9733,
    evictions: 0,
    cache_bytes: 1_288_490_189,
    cache_entries: 362_690,
    from_write_cache: 2_183_028,
    from_read_cache: 2_151_471,
    from_storage: 58_891,
    stored_bytes: 179_479_183,
    stored_accounts: 71_586,
    window_seconds: 60,
    disk: { used: 504_161_000_000, allocated: 505_203_000_000, fragmented: 1_046_478_848, storages: 423_462 },
    ...over,
  };
}

describe("rateTone", () => {
  it("keeps the bounds both panels already used", () => {
    expect(rateTone(0.9997)).toBe("good");
    expect(rateTone(0.98)).toBe("good");
    expect(rateTone(0.9)).toBe("warn");
    expect(rateTone(0.8999)).toBe("bad");
  });

  it("colours the middle band rather than leaving it plain", () => {
    // The band used to be untoned, which was survivable while the figure was
    // always on screen. Folded, the dot is all that is left of it.
    expect(rateTone(0.95)).toBe("warn");
  });

  it("says nothing at all when there is no rate yet", () => {
    expect(rateTone(null)).toBe("muted");
  });
});

describe("servedFromMemory", () => {
  it("counts every read, not just the ones that got past the write cache", () => {
    const { loads, rate } = servedFromMemory(accounts());
    expect(loads).toBe(4_393_390);
    // 98.66% as the panel prints it.
    expect(rate).toBeCloseTo(0.9866, 4);
  });

  it("is not the read cache's own hit rate", () => {
    // The write cache is consulted first, so `hit_rate` covers a fraction of
    // the reads and cannot be squared with the three-way split.
    const window = accounts();
    expect(servedFromMemory(window).rate).not.toBeCloseTo(window.hit_rate, 3);
  });

  it("has no rate before anything has been read", () => {
    const idle = accounts({ from_write_cache: 0, from_read_cache: 0, from_storage: 0 });
    expect(servedFromMemory(idle)).toEqual({ loads: 0, rate: null });
  });
});

describe("programGloss", () => {
  it("says how much went through, how much missed, how full, and how hard it sheds", () => {
    expect(programGloss(programs())).toEqual([
      "2,245,551 lookups",
      "625 misses",
      "462/512 entries",
      "621 evictions",
    ]);
  });

  it("names the limit rather than a fill it has not measured", () => {
    // Peak entries is written only when an eviction runs. On a validator that
    // has never evicted, "0/512" would claim an empty cache.
    expect(programGloss(programs({ peak_entries: null }))).toContain("512 entry limit");
  });
});

describe("accountsGloss", () => {
  it("leads with reads and what they cost", () => {
    expect(accountsGloss(accounts())).toEqual([
      "4,393,390 reads",
      "58,891 from disk",
      "2.85 MB/s written",
      "469.54 GB/470.51 GB on disk",
    ]);
  });

  it("drops the disk figures rather than showing nought of nought", () => {
    const gloss = accountsGloss(accounts({ disk: null }));
    expect(gloss).toHaveLength(3);
    expect(gloss.join()).not.toContain("on disk");
  });

  it("leads with the two a narrow screen will keep", () => {
    // Only the first two survive below 700px, so they have to be the two worth
    // keeping rather than whichever happened to be written first.
    expect(accountsGloss(accounts()).slice(0, 2)).toEqual([
      "4,393,390 reads",
      "58,891 from disk",
    ]);
  });

  it("does not divide by a window that has not opened", () => {
    expect(accountsGloss(accounts({ window_seconds: 0 }))).toContain("0 B/s written");
  });
});

function storage(): Storage {
  const values = new Map<string, string>();
  return {
    getItem: (key: string) => values.get(key) ?? null,
    setItem: (key: string, value: string) => void values.set(key, value),
    removeItem: (key: string) => void values.delete(key),
    clear: () => values.clear(),
    key: () => null,
    length: 0,
  } as unknown as Storage;
}

describe("which sections are open", () => {
  beforeEach(() => {
    vi.stubGlobal("window", { localStorage: storage() });
  });

  it("starts with both folded when nothing has been chosen", () => {
    expect(readOpenSections()).toEqual([]);
  });

  it("remembers a section across a reload", () => {
    writeOpenSections(["accounts"]);
    expect(readOpenSections()).toEqual(["accounts"]);
  });

  it("remembers both, and remembers folding them again", () => {
    writeOpenSections(["programs", "accounts"]);
    expect(readOpenSections()).toEqual(["programs", "accounts"]);
    writeOpenSections([]);
    expect(readOpenSections()).toEqual([]);
  });

  it("reads nothing out of an empty entry rather than one nameless section", () => {
    // Folding the last open section writes an empty string, which split() turns
    // into an array holding one empty name.
    window.localStorage.setItem(CACHES_STORAGE_KEY, "");
    expect(readOpenSections()).toEqual([]);
  });

  it("survives storage being refused", () => {
    // Private browsing and some embedded webviews throw on access rather than
    // returning null, which would otherwise take the whole panel down at
    // render.
    vi.stubGlobal("window", {
      get localStorage(): Storage {
        throw new Error("denied");
      },
    });
    expect(() => writeOpenSections(["programs"])).not.toThrow();
    expect(readOpenSections()).toEqual([]);
  });
});
