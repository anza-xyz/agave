import { describe, expect, it } from "vitest";
import {
  blockStamp,
  blockTime,
  buildLabel,
  bytes,
  count,
  decimal,
  micros,
  duration,
  percent,
  release,
  shortKey,
  slotDelta,
  sol,
  solCompact,
} from "./format";

// Anything going through `toLocaleString` is asserted only where the answer
// does not depend on the machine's locale. A test that expects "1,234" passes
// in en-US and fails in de-DE, which would make this suite worse than nothing.

describe("missing values", () => {
  it("all render as an em dash rather than as zero", () => {
    expect(count(undefined)).toBe("—");
    expect(decimal(undefined)).toBe("—");
    expect(decimal(Number.NaN)).toBe("—");
    expect(percent(undefined)).toBe("—");
    expect(percent(null)).toBe("—");
    expect(duration(undefined)).toBe("—");
    expect(bytes(undefined)).toBe("—");
    expect(sol(undefined)).toBe("—");
    expect(solCompact(undefined)).toBe("—");
    expect(shortKey(null)).toBe("—");
  });
});

describe("cached formatters", () => {
  // The formatters are built once rather than per call, which is worth about
  // twelve microseconds each. Compared against toLocaleString rather than
  // against literal strings, so this holds in any locale — what is being
  // pinned is that caching did not change the output.
  it("format numbers exactly as toLocaleString does", () => {
    for (const value of [0, 7, 1234, 340_000_000, -42, 1e15]) {
      expect(count(value)).toBe(value.toLocaleString());
    }
  });

  it("honour the digits they are asked for", () => {
    const opts = (digits: number) => ({
      minimumFractionDigits: digits,
      maximumFractionDigits: digits,
    });
    expect(decimal(1234.5678)).toBe((1234.5678).toLocaleString(undefined, opts(2)));
    expect(decimal(1234.5678, 0)).toBe((1234.5678).toLocaleString(undefined, opts(0)));
    expect(decimal(1234.5678, 4)).toBe((1234.5678).toLocaleString(undefined, opts(4)));
  });

  it("keep one formatter per digit count rather than one for all", () => {
    // The cache is keyed by digits. Keyed by nothing, whichever precision was
    // asked for first would be fixed for every later caller — so asking for
    // four digits after asking for zero has to still give four.
    expect(decimal(1.23456, 0)).toBe("1");
    expect(decimal(1.23456, 4)).toBe(
      (1.23456).toLocaleString(undefined, {
        minimumFractionDigits: 4,
        maximumFractionDigits: 4,
      }),
    );
    expect(decimal(1.23456, 0)).toBe("1");
  });

  it("convert lamports before formatting, not after", () => {
    expect(sol(1_500_000_000)).toBe((1.5).toLocaleString(undefined, {
      minimumFractionDigits: 2,
      maximumFractionDigits: 2,
    }));
  });
});

describe("duration", () => {
  it("drops to the three largest units that apply", () => {
    expect(duration(45_000)).toBe("45s");
    expect(duration(171_000)).toBe("2m 51s");
    expect(duration(3_723_000)).toBe("1h 2m 3s");
    expect(duration(604_740_000)).toBe("6d 23h 59m");
  });

  it("refuses a negative rather than rendering a wrapped figure", () => {
    // Countdowns are derived from a slot estimate that can overshoot.
    expect(duration(-1)).toBe("—");
  });
});

describe("bytes", () => {
  it("shows whole bytes and fractions of anything larger", () => {
    expect(bytes(0)).toBe("0 B");
    expect(bytes(999)).toBe("999 B");
    expect(bytes(1024)).toBe("1.00 KB");
    expect(bytes(1024 * 1024 * 1.5)).toBe("1.50 MB");
  });

  it("stops at the largest unit it knows", () => {
    expect(bytes(1024 ** 6)).toContain("TB");
  });
});

describe("percent", () => {
  it("scales the fraction and keeps the requested digits", () => {
    expect(percent(0)).toBe("0.00%");
    expect(percent(0.0123)).toBe("1.23%");
    expect(percent(1)).toBe("100.00%");
    expect(percent(0.5, 0)).toBe("50%");
  });
});

describe("blockTime", () => {
  it("has nothing to show without a timestamp", () => {
    expect(blockTime(null)).toBe("—");
    expect(blockTime(undefined)).toBe("—");
    expect(blockTime(Number.NaN)).toBe("—");
  });

  it("keeps milliseconds, zero padded", () => {
    // Two blocks can be under two hundred milliseconds apart, so seconds alone
    // would show consecutive slots as the same instant.
    expect(blockTime(Date.UTC(2026, 7, 16, 9, 55, 10, 7))).toMatch(/\.007$/);
    expect(blockTime(Date.UTC(2026, 7, 16, 9, 55, 10, 626))).toMatch(/\.626$/);
  });

  it("distinguishes two blocks in the same second", () => {
    const first = Date.UTC(2026, 7, 16, 9, 55, 10, 120);
    expect(blockTime(first)).not.toBe(blockTime(first + 185));
  });
});

describe("shortKey", () => {
  it("elides the middle of a pubkey", () => {
    const key = "J5e4xhLmNoPqRsTuVwXyZaBcDeFgHiJkLmNoPqRsc8FF1";
    expect(shortKey(key)).toBe("J5e4xh…c8FF1");
    expect(shortKey(key, 4, 4)).toBe("J5e4…8FF1");
  });

  it("leaves a key that is already short enough alone", () => {
    // Eliding here would make it longer, not shorter.
    expect(shortKey("abcdefghij")).toBe("abcdefghij");
  });
});

describe("slotDelta", () => {
  it("signs the difference so the direction is readable at a glance", () => {
    expect(slotDelta(100, 132)).toBe("-32");
    expect(slotDelta(133, 132)).toBe("+1");
    expect(slotDelta(132, 132)).toBe("0");
  });

  it("renders nothing when there is no reference to compare against", () => {
    expect(slotDelta(undefined, 132)).toBe("");
    expect(slotDelta(132, undefined)).toBe("");
  });
});

describe("solCompact", () => {
  it("abbreviates above a thousand", () => {
    expect(solCompact(1_500_000_000)).toBe("1.5");
    expect(solCompact(12_340 * 1e9)).toBe("12.3K");
    expect(solCompact(2_500_000 * 1e9)).toBe("2.5M");
  });
});

describe("buildLabel", () => {
  it("names the client ahead of the version", () => {
    // The whole point: two builds carrying the same number are told apart by
    // the half in front of it.
    expect(buildLabel("Agave", "4.3.0-beta.0")).toBe("Agave v4.3.0-beta.0");
    expect(buildLabel("JitoLabs", "4.2.1")).toBe("JitoLabs v4.2.1");
  });

  it("shows whichever half it has", () => {
    // A server older than the client field publishes only the version, and it
    // should read as it always did rather than falling blank.
    expect(buildLabel(undefined, "4.2.1")).toBe("v4.2.1");
    expect(buildLabel("Agave", undefined)).toBe("Agave");
  });

  it("is empty before the first message, so the header shows no stray v", () => {
    expect(buildLabel(undefined, undefined)).toBe("");
  });
});

describe("micros", () => {
  it("reads microseconds as milliseconds", () => {
    expect(micros(205648)).toBe("205.6 ms");
    expect(micros(835)).toBe("0.8 ms");
  });

  it("keeps one decimal at every size, so a column lines up", () => {
    // These sit together and span three orders of magnitude; varying the
    // precision by size is what stops a column of numbers being readable.
    expect(micros(414498)).toBe("414.5 ms");
    expect(micros(1)).toBe("0.0 ms");
  });

  it("says nothing rather than NaN", () => {
    expect(micros(null)).toBe("—");
    expect(micros(undefined)).toBe("—");
    expect(micros(Number.NaN)).toBe("—");
  });
});

describe("release", () => {
  it("strips a pre-release tag so a build matches its own cluster row", () => {
    expect(release("4.3.0-beta.0")).toBe("4.3.0");
    expect(release("4.3.0-rc.1")).toBe("4.3.0");
  });

  it("strips build metadata the same way the server does", () => {
    expect(release("4.3.0+deadbeef")).toBe("4.3.0");
  });

  it("leaves a plain release alone", () => {
    expect(release("4.2.1")).toBe("4.2.1");
    expect(release(undefined)).toBeUndefined();
  });
});

describe("blockStamp", () => {
  it("says nothing when the blockstore held no timing", () => {
    expect(blockStamp(null)).toBe("—");
    expect(blockStamp(undefined)).toBe("—");
    expect(blockStamp(Number.NaN)).toBe("—");
  });

  it("stops at seconds, unlike the detail panel's stamp", () => {
    // The row version. Milliseconds are what let two blocks two hundred apart
    // be told apart in the detail, and what stop a column of these lining up.
    const at = Date.UTC(2026, 7, 22, 9, 29, 57, 626);
    expect(blockStamp(at)).not.toMatch(/\.626/);
    expect(blockTime(at)).toMatch(/\.626$/);
  });

  it("names the zone it is being read in", () => {
    // Whatever abbreviation the browser holds: a short form where English has
    // one, an offset where it does not. Either way the reader is told which
    // clock this is, which a bare time does not.
    const stamp = blockStamp(Date.UTC(2026, 7, 22, 9, 29, 57));
    expect(stamp).toMatch(/\d{2}:\d{2}:\d{2}/);
    expect(stamp.split(" ").length).toBeGreaterThan(2);
  });
});
