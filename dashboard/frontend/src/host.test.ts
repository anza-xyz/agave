import { describe, expect, it } from "vitest";
import {
  availableTone,
  busyTone,
  deviceLabel,
  fullness,
  fullnessTone,
  loadTrend,
  memoryUse,
  swapTone,
  waitTone,
} from "./host";
import type { DeviceLoad, Host } from "./types";

const GB = 1024 ** 3;

function host(over: Partial<Host> = {}): Host {
  return {
    cores: 32,
    load_one: 12.4,
    load_five: 11.8,
    load_fifteen: 10.9,
    threads: 1847,
    running: 14,
    memory_total: 384 * GB,
    memory_available: 88 * GB,
    memory_reclaimable: 64 * GB,
    memory_free: 24 * GB,
    swap: { total: 8 * GB, used: 0 },
    filesystems: [],
    devices: [],
    ...over,
  };
}

function device(over: Partial<DeviceLoad> = {}): DeviceLoad {
  return {
    device: "nvme1n1",
    roles: ["accounts"],
    busy: 0.34,
    wait_ms: 0.21,
    operations_per_second: 18_200,
    read_per_second: 1_400_000_000,
    write_per_second: 240_000_000,
    ...over,
  };
}

describe("memoryUse", () => {
  it("does not count the page cache as spoken for", () => {
    // The convention most tools print is total minus free, which on this box
    // would read 360 of 384 GB and look like a machine about to die.
    const use = memoryUse(host());
    expect(use.inUse).toBe(296 * GB);
    expect(use.reclaimable).toBe(64 * GB);
    expect(use.available).toBe(88 * GB);
  });

  it("keeps the three parts inside the total", () => {
    const use = memoryUse(host());
    expect(use.inUse + use.reclaimable).toBeLessThanOrEqual(use.total);
  });

  it("does not go negative on a reading that arrived mid-change", () => {
    // Free and reclaimable are read from separate lines of one file and can
    // briefly sum past the total.
    const use = memoryUse(
      host({ memory_total: 100, memory_free: 80, memory_reclaimable: 80 }),
    );
    expect(use.inUse).toBe(0);
  });

  it("reports nothing rather than dividing by a total of nought", () => {
    const use = memoryUse(host({ memory_total: 0 }));
    expect(use.total).toBe(0);
    expect(use.inUse).toBe(0);
  });
});

describe("fullness", () => {
  it("is what is gone, not what is left", () => {
    expect(fullness({ name: "ledger", path: "/l", total: 1000, available: 470 })).toBe(0.53);
  });

  it("is nought on a filesystem that reported no size", () => {
    expect(fullness({ name: "x", path: "/x", total: 0, available: 0 })).toBe(0);
  });
});

describe("thresholds", () => {
  it("turns a filesystem amber at four fifths and red at nine tenths", () => {
    expect(fullnessTone(0.53)).toBe("good");
    expect(fullnessTone(0.8)).toBe("warn");
    expect(fullnessTone(0.9)).toBe("bad");
  });

  it("turns a device amber at seventy percent busy and red at eighty five", () => {
    expect(busyTone(0.34)).toBe("good");
    expect(busyTone(0.7)).toBe("warn");
    expect(busyTone(0.85)).toBe("bad");
  });

  it("reads wait against what NVMe should manage", () => {
    expect(waitTone(0.21)).toBe("good");
    expect(waitTone(1)).toBe("warn");
    expect(waitTone(5)).toBe("bad");
  });

  it("says nothing about wait where the device did nothing", () => {
    // Nought would read as an idle device being infinitely fast, and green
    // would claim a health nobody measured.
    expect(waitTone(null)).toBe("muted");
  });

  it("warns as memory left runs down", () => {
    expect(availableTone(88 * GB, 384 * GB)).toBe("good");
    expect(availableTone(30 * GB, 384 * GB)).toBe("warn");
    expect(availableTone(10 * GB, 384 * GB)).toBe("bad");
  });

  it("has no opinion about memory on a box that reported no total", () => {
    expect(availableTone(0, 0)).toBe("muted");
  });

  it("treats any swap at all as worth noticing", () => {
    // There is no healthy amount to allow for: a validator that has begun
    // swapping is already being hurt by it.
    expect(swapTone(0)).toBe("good");
    expect(swapTone(1)).toBe("warn");
  });
});

describe("loadTrend", () => {
  it("reads the one minute figure against the fifteen", () => {
    expect(loadTrend(host({ load_one: 12.4, load_fifteen: 10.9 }))).toBe("rising");
    expect(loadTrend(host({ load_one: 8.1, load_fifteen: 10.9 }))).toBe("falling");
  });

  it("calls a tenth of a core steady rather than a trend", () => {
    expect(loadTrend(host({ load_one: 10.95, load_fifteen: 10.9 }))).toBe("steady");
  });
});

describe("deviceLabel", () => {
  it("names one role", () => {
    expect(deviceLabel(device())).toBe("accounts");
  });

  it("names both where two mounts share a disk", () => {
    // They compete for one queue, so they are one row and the label has to say
    // that rather than picking whichever was resolved first.
    expect(deviceLabel(device({ roles: ["ledger", "accounts"] }))).toBe("ledger and accounts");
  });
});
