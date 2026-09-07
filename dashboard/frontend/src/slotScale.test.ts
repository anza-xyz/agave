import { describe, expect, it } from "vitest";
import { barHeight } from "./slotScale";

const NOMINAL = 400;

describe("barHeight", () => {
  it("puts a nominal slot at half height", () => {
    expect(barHeight(NOMINAL, NOMINAL)).toBe(50);
  });

  it("separates slots a linear scale would have flattened together", () => {
    // The bug this replaced: both of these pinned to the top of the bar, so a
    // one-slot gap and a three-slot gap drew the same.
    const twice = barHeight(800, NOMINAL);
    const three = barHeight(1200, NOMINAL);
    expect(twice).toBeLessThan(three);
    expect(three).toBeLessThan(100);
  });

  it("adds a quarter of the bar per doubling", () => {
    expect(barHeight(800, NOMINAL) - barHeight(400, NOMINAL)).toBeCloseTo(25);
    expect(barHeight(1600, NOMINAL) - barHeight(800, NOMINAL)).toBeCloseTo(25);
  });

  it("keeps a fast slot visible rather than collapsing it", () => {
    // Eight doublings below nominal is far past zero on the raw scale.
    expect(barHeight(1, NOMINAL)).toBe(8);
  });

  it("draws a stub for a slot with no duration", () => {
    // Skipped slots never get a shred, so they never get a timestamp.
    expect(barHeight(null, NOMINAL)).toBe(6);
    expect(barHeight(0, NOMINAL)).toBe(6);
  });

  it("stays within the bar for any input", () => {
    for (const ms of [1, 50, 399, 400, 401, 5_000, 60_000, 1e9]) {
      const height = barHeight(ms, NOMINAL);
      expect(height).toBeGreaterThanOrEqual(6);
      expect(height).toBeLessThanOrEqual(100);
    }
  });
});
