import { describe, expect, it } from "vitest";
import { chartY, PEAK_HEADROOM } from "./components/primitives";

const HEIGHT = 120;

describe("chartY", () => {
  it("puts the highest sample exactly on the peak line", () => {
    // The line is placed from the bottom as a percentage and the series from
    // the top in viewBox units. This is the only thing keeping them level, so
    // a change to either that forgets the other fails here.
    const lineFromTop = HEIGHT * (1 - PEAK_HEADROOM);
    expect(chartY(4820, 4820, HEIGHT)).toBeCloseTo(lineFromTop, 10);
  });

  it("leaves headroom above the peak", () => {
    expect(chartY(4820, 4820, HEIGHT)).toBeGreaterThan(0);
  });

  it("puts zero on the baseline", () => {
    expect(chartY(0, 4820, HEIGHT)).toBe(HEIGHT);
  });

  it("scales the middle of the range proportionally", () => {
    const half = chartY(2410, 4820, HEIGHT);
    expect(HEIGHT - half).toBeCloseTo((HEIGHT - chartY(4820, 4820, HEIGHT)) / 2, 10);
  });
});
