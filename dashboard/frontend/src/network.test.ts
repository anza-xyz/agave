import { describe, expect, it } from "vitest";
import { xdpDetail, xdpTooltip } from "./components/NetworkCard";
import { direction, egressShares, NETWORK_WINDOW_SECONDS, sharedPeak, unitFor } from "./network";
import type { XdpConfig } from "./types";

const KB = 1024;
const MB = 1024 * 1024;

describe("direction", () => {
  it("takes the newest reading and the mean of the window", () => {
    const read = direction([10, 20, 30, 40])!;
    expect(read.current).toBe(40);
    expect(read.average).toBe(25);
    expect(read.delta).toBe(15);
  });

  it("does not let one noisy second move the arrow", () => {
    // Throughput jitters by more than this from second to second, and an arrow
    // that flips every time is an arrow nobody reads. The figure itself still
    // shows the spike.
    const jitter = [...Array(59).fill(100), 115];
    expect(direction(jitter)!.trend).toBe("flat");
    expect(direction(jitter)!.current).toBe(115);
  });

  it("fires once the last ten seconds are genuinely above the minute", () => {
    const climbing = [...Array(50).fill(100), ...Array(10).fill(140)];
    expect(direction(climbing)!.trend).toBe("up");
    const falling = [...Array(50).fill(100), ...Array(10).fill(60)];
    expect(direction(falling)!.trend).toBe("down");
  });

  it("still fires on a single second large enough to be an event", () => {
    // A doubling is not jitter. Damped to a tenth it still clears the floor,
    // and it should: something changed.
    expect(direction([...Array(59).fill(100), 300])!.trend).toBe("up");
  });

  it("calls a two percent drift flat", () => {
    expect(direction([...Array(50).fill(100), ...Array(10).fill(101)])!.trend).toBe("flat");
  });

  it("is flat rather than dividing by an average of nought", () => {
    expect(direction([0, 0, 0])!.trend).toBe("flat");
  });

  it("has nothing to report before any samples arrive", () => {
    expect(direction([])).toBeNull();
  });
});

describe("sharedPeak", () => {
  it("is the highest reading either direction took", () => {
    // One scale for both. Given a band each, ten kilobytes a second fills its
    // band exactly as ten megabytes fills the other, and the picture says the
    // two are equals.
    expect(sharedPeak([1 * MB, 2 * MB], [7 * MB, 3 * MB])).toBe(7 * MB);
  });

  it("never returns nought, so nothing divides by it on an idle host", () => {
    expect(sharedPeak([0, 0], [0])).toBe(1);
    expect(sharedPeak([], [])).toBe(1);
  });
});

describe("unitFor", () => {
  it("picks the unit a reading of that size wants", () => {
    expect(unitFor(512)).toEqual({ unit: "B", divisor: 1 });
    expect(unitFor(42 * MB)).toEqual({ unit: "MB", divisor: MB });
  });

  it("is taken from the current reading and used for the rest", () => {
    // An average of 1.02 MB/s printed beside a current 980 KB/s would read as
    // "980" against "avg 1.02", and the second looks like the smaller number.
    const { divisor, unit } = unitFor(980 * KB);
    expect(unit).toBe("KB");
    expect((1.02 * MB) / divisor).toBeCloseTo(1044.48, 2);
  });

  it("handles a negative delta without falling to bytes", () => {
    expect(unitFor(-42 * MB).unit).toBe("MB");
  });

  it("stops at the largest unit it knows", () => {
    expect(unitFor(9 * 1024 ** 5).unit).toBe("TB");
  });
});

describe("the window", () => {
  it("matches the transactions chart, so both read on one timebase", () => {
    expect(NETWORK_WINDOW_SECONDS).toBe(60);
  });
});

describe("the XDP line", () => {
  const config = (over: Partial<XdpConfig> = {}): XdpConfig => ({
    zero_copy: true,
    driver: "ice",
    vendor: "Intel Corporation",
    model: "Ethernet Controller E810-C for QSFP",
    kernel_version: "6.8.0-45-generic",
    ...over,
  });

  it("names the driver and the card, in that order", () => {
    expect(xdpDetail(config())).toEqual(["ice", "Ethernet Controller E810-C for QSFP"]);
  });

  it("leaves out what the validator could not look up", () => {
    // Both come back as the literal string "unknown" where the device would
    // not answer or the host has no PCI database. Printed, it reads as a fault
    // in the card rather than in the lookup.
    expect(xdpDetail(config({ model: "unknown" }))).toEqual(["ice"]);
    expect(xdpDetail(config({ driver: "unknown", model: "unknown" }))).toEqual([]);
  });

  it("leaves out what was never sent at all", () => {
    expect(xdpDetail(config({ driver: "", model: "" }))).toEqual([]);
  });

  it("keeps a card whose name merely contains the word", () => {
    // Only an exact "unknown" is the failure marker. A real model name is not
    // dropped for containing it.
    expect(xdpDetail(config({ model: "Unknown Devices Inc 40G" }))).toContain(
      "Unknown Devices Inc 40G",
    );
  });
});

describe("the XDP tooltip", () => {
  const SAID = "How this validator's XDP transmit path is set up.";
  const config = (over: Partial<XdpConfig> = {}): XdpConfig => ({
    zero_copy: true,
    driver: "ice",
    vendor: "Intel Corporation",
    model: "Ethernet Controller E810-C for QSFP",
    kernel_version: "6.8.0-45-generic",
    ...over,
  });

  it("adds the two things the line has no room for", () => {
    expect(xdpTooltip(config())).toBe(`${SAID} Intel Corporation, kernel 6.8.0-45-generic.`);
  });

  it("capitalises the kernel where there is no vendor to lead", () => {
    // Otherwise a lowercase word opens the second sentence and reads as a typo.
    expect(xdpTooltip(config({ vendor: "unknown" }))).toBe(`${SAID} Kernel 6.8.0-45-generic.`);
  });

  it("keeps the vendor where the kernel is missing", () => {
    expect(xdpTooltip(config({ kernel_version: "" }))).toBe(`${SAID} Intel Corporation.`);
  });

  it("stops at the sentence where it has neither", () => {
    expect(xdpTooltip(config({ vendor: "unknown", kernel_version: "" }))).toBe(SAID);
  });

  it("drops a kernel that is a failed uname rather than a version", () => {
    // uname failing is reported as "unknown" followed by the error it got, and
    // printed after the word kernel that reads as a version number.
    expect(xdpTooltip(config({ kernel_version: "unknown: os error 2" }))).toBe(
      `${SAID} Intel Corporation.`,
    );
  });
});

describe("egressShares", () => {
  it("names what the senders measured and leaves the rest as a remainder", () => {
    const shares = egressShares(57_000_000, {
      gossip_per_second: 4_000_000,
      repair_per_second: 240_000,
    });
    expect(shares.gossip).toBe(4_000_000);
    expect(shares.repair).toBe(240_000);
    expect(shares.measured).toBe(4_240_000);
    expect(shares.remainder).toBe(52_760_000);
  });

  it("treats a sender that has not reported as nought rather than unknown", () => {
    const shares = egressShares(1_000, { gossip_per_second: null, repair_per_second: 300 });
    expect(shares.gossip).toBe(0);
    expect(shares.measured).toBe(300);
    expect(shares.remainder).toBe(700);
  });

  it("never draws a negative remainder", () => {
    // The senders report over their own windows and the interface over the
    // last second, so briefly the parts can exceed the whole.
    const shares = egressShares(100, { gossip_per_second: 90, repair_per_second: 30 });
    expect(shares.remainder).toBe(0);
    expect(shares.measured).toBe(120);
  });
});
