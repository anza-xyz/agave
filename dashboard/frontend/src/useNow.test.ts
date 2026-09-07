import { describe, expect, it } from "vitest";
import { RENDER_LAG_MS, windowed } from "./useNow";

type Sample = { at: number };

const at = (millis: number): Sample => ({ at: millis * 1e6 });
const stamp = (sample: Sample) => sample.at;

describe("render lag", () => {
  const WIDTH = 600;
  const windowMs = 60_000;
  /** The x a chart would place a sample at, for an edge one lag behind live. */
  const at = (timestampMs: number, now: number) =>
    WIDTH * (1 - (now - RENDER_LAG_MS - timestampMs) / windowMs);

  it("keeps the newest sample past the right edge for a whole interval", () => {
    // The point of the lag: with the edge at live, a sample arriving on the
    // second sits exactly on the edge and then retreats from it, leaving the
    // notch that made the chart step once a second.
    const arrived = 1_000_000;
    for (let elapsed = 0; elapsed < RENDER_LAG_MS; elapsed += 100) {
      expect(at(arrived, arrived + elapsed)).toBeGreaterThanOrEqual(WIDTH);
    }
  });

  it("has the sample reach the edge exactly as the next one is due", () => {
    const arrived = 1_000_000;
    expect(at(arrived, arrived + RENDER_LAG_MS)).toBeCloseTo(WIDTH, 6);
  });

  it("still slides at a constant rate", () => {
    const arrived = 1_000_000;
    const first = at(arrived, arrived + 500);
    const second = at(arrived, arrived + 600);
    // A tenth of a second across a sixty second window of six hundred units.
    expect(first - second).toBeCloseTo(1, 6);
  });
});

describe("windowed", () => {
  const now = 60_000;
  const windowMs = 10_000;

  it("keeps one sample older than the window", () => {
    // That extra point is what lets the line leave the chart by sliding under
    // the viewBox edge. Filtering strictly to the window made the leftmost
    // segment vanish the moment its older end expired.
    const samples = [at(30_000), at(45_000), at(52_000), at(58_000)];
    expect(windowed(samples, now, windowMs, stamp)).toEqual([
      at(45_000),
      at(52_000),
      at(58_000),
    ]);
  });

  it("keeps everything when nothing has expired yet", () => {
    const samples = [at(55_000), at(58_000)];
    expect(windowed(samples, now, windowMs, stamp)).toEqual(samples);
  });

  it("returns nothing when every sample is older than the window", () => {
    // Not the last sample: a stale series should empty the chart rather than
    // draw a flat line from a reading minutes old.
    expect(windowed([at(10_000), at(20_000)], now, windowMs, stamp)).toEqual([]);
  });

  it("returns nothing for an empty series", () => {
    expect(windowed([], now, windowMs, stamp)).toEqual([]);
  });
});
