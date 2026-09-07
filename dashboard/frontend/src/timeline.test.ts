import { describe, expect, it } from "vitest";
import { TIMELINE_SPAN_MS, timelineOf } from "./timeline";
import type { SlotEntry } from "./types";

function held(over: Partial<SlotEntry> = {}): SlotEntry {
  return {
    slot: 444_422_652,
    level: "rooted",
    mine: false,
    block: null,
    duration_nanos: null,
    time_millis: null,
    shreds: { count: 928, repaired: 63, full_millis: 921 },
    replayed_millis: 932,
    ...over,
  };
}

describe("timelineOf", () => {
  it("splits the slot into waiting for the block and finishing it", () => {
    // The slot from the trace: most of a second waiting for shreds, then a
    // few milliseconds of replay once they were all there.
    const timeline = timelineOf(held());
    expect(timeline?.wait).toBe(921);
    expect(timeline?.run).toBe(11);
    expect(timeline?.label).toBe("921 + 11 ms");
  });

  it("draws both spans against one fixed track", () => {
    const timeline = timelineOf(held({ shreds: { count: 1_203, repaired: 0, full_millis: 341 }, replayed_millis: 393 }));
    expect(timeline?.waitShare).toBeCloseTo(341 / TIMELINE_SPAN_MS);
    expect(timeline?.runShare).toBeCloseTo(52 / TIMELINE_SPAN_MS);
  });

  it("never draws past the end of the track", () => {
    // A slot replayed long after it filled, which a catch-up produces. The
    // text still says the whole figure; only the bar is clamped.
    const timeline = timelineOf(held({ shreds: { count: 900, repaired: 0, full_millis: 800 }, replayed_millis: 4_800 }));
    expect(timeline?.waitShare).toBe(0.8);
    expect(timeline?.runShare).toBeCloseTo(0.2);
    expect(timeline?.label).toBe("800 + 4000 ms");
  });

  it("has one span where replay's finish was not seen", () => {
    // Every block we built, since replay never times a bank it did not replay.
    const timeline = timelineOf(held({ replayed_millis: null }));
    expect(timeline?.run).toBeNull();
    expect(timeline?.runShare).toBe(0);
    expect(timeline?.label).toBe("921 ms");
  });

  it("does not let two clocks disagreeing read as replay finishing early", () => {
    const timeline = timelineOf(held({ shreds: { count: 900, repaired: 0, full_millis: 400 }, replayed_millis: 399 }));
    expect(timeline?.run).toBe(0);
  });

  it("draws nothing where the block's arrival was never reported", () => {
    expect(timelineOf(held({ shreds: null }))).toBeNull();
    expect(timelineOf(null)).toBeNull();
  });
});
