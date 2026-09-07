import { describe, expect, it } from "vitest";
import { lossShare, shareLabel, windowLabel } from "./components/IngestCard";

describe("windowLabel", () => {
  it("names the period actually watched while the window fills", () => {
    // The point of the heading: for the first minute the card must not claim a
    // minute it has not watched, because "0 drops in the last minute" read off
    // a five-second window is a reassurance nobody measured.
    expect(windowLabel(0)).toBe("Last 5s");
    expect(windowLabel(12)).toBe("Last 10s");
    expect(windowLabel(38)).toBe("Last 40s");
  });

  it("settles once the window is full", () => {
    expect(windowLabel(55)).toBe("Last min");
    expect(windowLabel(60)).toBe("Last min");
  });

  it("rounds so the heading does not redraw every tick", () => {
    expect(windowLabel(31)).toBe(windowLabel(32));
  });
});

describe("lossShare", () => {
  it("divides drops by everything that arrived, not by what got through", () => {
    // The two are disjoint: a datagram the kernel discarded never reached the
    // reader that counts them. Dividing by the delivered count alone would
    // overstate the loss, slightly at first and without limit as it grows.
    expect(lossShare(1, 99)).toBeCloseTo(0.01, 10);
    expect(lossShare(50, 50)).toBeCloseTo(0.5, 10);
  });

  it("has nothing to say about a port nothing counts", () => {
    // The QUIC ports and serve repair. Their drop figures stand alone.
    expect(lossShare(12, null)).toBeNull();
  });

  it("refuses to call it total loss when nothing was counted as received", () => {
    // The dangerous case. These counts travel as metrics points, which are only
    // submitted while info logging is on for the crate submitting them, so a
    // validator run quieter than default reports nought received forever. Read
    // literally that is every packet lost, on a node that is perfectly healthy.
    expect(lossShare(12, 0)).toBeNull();
  });

  it("says nothing where there is nothing to say", () => {
    // A share of nought adds nothing to the nought already beside it.
    expect(lossShare(0, 5000)).toBeNull();
    expect(lossShare(0, 0)).toBeNull();
  });
});

describe("shareLabel", () => {
  it("does not round a real loss away to nothing", () => {
    // One in fifty thousand is a reading worth having, and 0.00% denies it —
    // the wrong direction to err in for a figure whose only job is to show that
    // something is being lost.
    expect(shareLabel(1 / 50_000)).toBe("<0.01%");
  });

  it("reads as a percentage once there is one to read", () => {
    expect(shareLabel(0.0125)).toBe("1.25%");
    expect(shareLabel(0.5)).toBe("50.00%");
  });
});
