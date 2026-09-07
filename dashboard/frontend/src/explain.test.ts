import { describe, expect, it } from "vitest";
import { edgeShift, shouldFlipAbove } from "./components/primitives";

const VIEWPORT = 1400;
const VIEWPORT_HEIGHT = 900;

describe("edgeShift", () => {
  it("leaves a bubble alone when it already fits", () => {
    expect(edgeShift(363, 623, VIEWPORT)).toBe(0);
  });

  it("pulls a bubble back off the right edge", () => {
    // The case that gave the page a horizontal scrollbar: the right-hand
    // column of a card, whose bubble ran 201px past the window.
    expect(edgeShift(1341, 1601, VIEWPORT)).toBe(-213);
    expect(1601 + edgeShift(1341, 1601, VIEWPORT)).toBe(VIEWPORT - 12);
  });

  it("pushes a bubble in off the left edge", () => {
    expect(edgeShift(-30, 230, VIEWPORT)).toBe(42);
    expect(-30 + edgeShift(-30, 230, VIEWPORT)).toBe(12);
  });

  it("keeps the left edge readable when the bubble cannot fit at all", () => {
    // A bubble wider than the window should not be slid so far that its start
    // is off-screen, which is what correcting the right edge alone would do.
    const shift = edgeShift(0, 2000, 300);
    expect(0 + shift).toBeLessThanOrEqual(12);
  });

  it("respects the margin exactly at the boundary", () => {
    expect(edgeShift(12, VIEWPORT - 12, VIEWPORT)).toBe(0);
    expect(edgeShift(11, VIEWPORT - 13, VIEWPORT)).toBe(1);
  });
});

describe("shouldFlipAbove", () => {
  it("stays below when it fits there", () => {
    expect(shouldFlipAbove(600, 80, 500, VIEWPORT_HEIGHT)).toBe(false);
  });

  it("flips up rather than lengthening the page", () => {
    // The Socket Ingest footnote: a trigger near the foot of the page, whose
    // bubble hung past the bottom and raised a scrollbar.
    expect(shouldFlipAbove(960, 80, 870, VIEWPORT_HEIGHT)).toBe(true);
  });

  it("stays below when there is no room above either", () => {
    // Flipping here would put it off the top, where it cannot be scrolled to.
    expect(shouldFlipAbove(960, 400, 300, VIEWPORT_HEIGHT)).toBe(false);
  });

  it("does not flip for a bubble that ends exactly on the margin", () => {
    expect(shouldFlipAbove(VIEWPORT_HEIGHT - 12, 80, 700, VIEWPORT_HEIGHT)).toBe(false);
  });
});
