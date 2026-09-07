import { describe, expect, it } from "vitest";
import { jitoShare, ourShare } from "./tips";
import type { TipRates } from "./types";

function rates(over: Partial<TipRates> = {}): TipRates {
  return { jito_cut_bps: 600, commission_bps: 1_000, ...over };
}

describe("jitoShare", () => {
  it("takes jito's cut off what was paid", () => {
    // 1.4 SOL paid, six per cent to jito, 1.316 reaching the account.
    expect(jitoShare(1_400_000_000, rates())).toBe(1_316_000_000);
  });

  it("returns nothing for a turn that was paid nothing", () => {
    // A real reading, and distinct from a turn never measured, which never
    // reaches here at all.
    expect(jitoShare(0, rates())).toBe(0);
  });

  it("matches the validator's integer arithmetic rather than rounding up", () => {
    // Floored on both sides, so the same lamports do not read one way on the
    // page and another in a log line.
    expect(jitoShare(1_001, rates())).toBe(1_001 - 60);
  });
});

describe("ourShare", () => {
  it("takes the commission of what reached the account, not of what was paid", () => {
    // A tenth of 1.316, not a tenth of 1.4. Taking it of the wrong one
    // overstates by six per cent, which is small enough to look right.
    expect(ourShare(1_400_000_000, rates())).toBe(131_600_000);
  });

  it("gives the whole of it at full commission and none at none", () => {
    expect(ourShare(1_400_000_000, rates({ commission_bps: 10_000 }))).toBe(1_316_000_000);
    expect(ourShare(1_400_000_000, rates({ commission_bps: 0 }))).toBe(0);
  });

  it("answers nothing where no commission is configured", () => {
    // Rather than assuming a figure. The page then shows what the turn paid and
    // claims nothing about what it earned.
    expect(ourShare(1_400_000_000, rates({ commission_bps: null }))).toBeNull();
  });

  it("is never more than what reached the account", () => {
    const paid = 1_400_000_000;
    for (const commission_bps of [0, 1, 500, 1_000, 9_999, 10_000]) {
      const ours = ourShare(paid, rates({ commission_bps }));
      expect(ours).not.toBeNull();
      expect(ours as number).toBeLessThanOrEqual(jitoShare(paid, rates()));
    }
  });
});
