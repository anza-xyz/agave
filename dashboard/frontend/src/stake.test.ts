import { describe, expect, it } from "vitest";
import { STAKE_TICKS, stakeTicks } from "./stake";

/** Lamports, so the figures read the way the payload carries them. */
const SOL = 1_000_000_000;

describe("stakeTicks", () => {
  it("fills part of a tick rather than rounding a small share up to a whole one", () => {
    // 5.0M delinquent against 405.2M staked is 1.23%, which is well under one
    // tick's two percent.
    const ticks = stakeTicks(5_000_000 * SOL, 405_200_000 * SOL);
    expect(ticks.full).toBe(0);
    expect(ticks.partial).toBeCloseTo(0.617, 3);
  });

  it("draws nothing when no stake is delinquent", () => {
    expect(stakeTicks(0, 400_000_000 * SOL)).toEqual({ full: 0, partial: 0 });
  });

  it("lifts a share too small to see off zero", () => {
    // A twentieth of a percent is a fortieth of a tick, which would draw as an
    // empty strip and so claim that nothing is delinquent.
    const ticks = stakeTicks(0.0005 * 400_000_000 * SOL, 400_000_000 * SOL);
    expect(ticks.full).toBe(0);
    expect(ticks.partial).toBeGreaterThan(0.05);
  });

  it("leaves the part-tick true once a whole tick is already red", () => {
    // 6.4% is three whole ticks and a fifth of a fourth. Nothing is floored
    // here: the three full ticks are visible on their own.
    const ticks = stakeTicks(0.064 * 100, 100);
    expect(ticks.full).toBe(3);
    expect(ticks.partial).toBeCloseTo(0.2, 6);
  });

  it("counts ten and a half ticks for a fifth of the stake", () => {
    const ticks = stakeTicks(21, 100);
    expect(ticks.full).toBe(10);
    expect(ticks.partial).toBeCloseTo(0.5, 6);
  });

  it("fills the strip when every validator is delinquent", () => {
    expect(stakeTicks(400 * SOL, 400 * SOL)).toEqual({ full: STAKE_TICKS, partial: 0 });
  });

  it("does not overflow the strip when the two figures disagree", () => {
    // The counts are tallied from the same walk, but nothing in the types
    // stops a delinquent figure larger than the total it is out of.
    expect(stakeTicks(900, 100)).toEqual({ full: STAKE_TICKS, partial: 0 });
  });

  it("draws nothing rather than dividing by zero before the first tally", () => {
    expect(stakeTicks(0, 0)).toEqual({ full: 0, partial: 0 });
    expect(stakeTicks(5, 0)).toEqual({ full: 0, partial: 0 });
  });

  it("draws nothing for figures that are not numbers", () => {
    expect(stakeTicks(Number.NaN, 100)).toEqual({ full: 0, partial: 0 });
    expect(stakeTicks(5, Number.NaN)).toEqual({ full: 0, partial: 0 });
    expect(stakeTicks(5, Number.POSITIVE_INFINITY)).toEqual({ full: 0, partial: 0 });
  });
});
