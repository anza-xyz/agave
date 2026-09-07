import { describe, expect, it } from "vitest";
import { barLow, busiest, onCpuTone, pinnedLabel, POH_LOW, threadRows } from "./threads";
import type { ThreadGroup, ThreadsSample } from "./types";

function group(name: string, over: Partial<ThreadGroup> = {}): ThreadGroup {
  return { name, count: 1, cores: null, on_cpu: 0.1, waiting: 0, other: false, ...over };
}

function sample(second: number, groups: ThreadGroup[]): ThreadsSample {
  return { timestamp_nanos: second * 1e9, threads: 1412, groups };
}

describe("threadRows", () => {
  it("reads each row's minute back through the samples", () => {
    const samples = [
      sample(1, [group("solReplayStage", { on_cpu: 0.2 })]),
      sample(2, [group("solReplayStage", { on_cpu: 0.4 })]),
      sample(3, [group("solReplayStage", { on_cpu: 0.3 })]),
    ];
    const [row] = threadRows(samples);
    expect(row.series).toEqual([0.2, 0.4, 0.3]);
    expect(row.now).toBe(0.3);
  });

  it("takes the rows from the last sample and leaves a gap where a group had none", () => {
    // The validator picks the rows on the minute's mean, so membership is
    // steady, but a group can still enter partway through the window.
    const samples = [
      sample(1, [group("solGossip")]),
      sample(2, [group("solGossip"), group("solRepairSvc", { on_cpu: 0.5 })]),
    ];
    const rows = threadRows(samples);
    expect(rows.map((row) => row.name)).toEqual(["solGossip", "solRepairSvc"]);
    expect(rows[1].series).toEqual([null, 0.5]);
  });

  it("carries the minute's worst second of waiting, not the last one", () => {
    const samples = [
      sample(1, [group("solScHandleV", { waiting: 0.014 })]),
      sample(2, [group("solScHandleV", { waiting: 0.001 })]),
    ];
    expect(threadRows(samples)[0].waiting).toBe(0.014);
  });

  it("labels a pool with a star and the folded row with a phrase", () => {
    const rows = threadRows([
      sample(1, [
        group("solScHandleV", { count: 11 }),
        group("solPohTickProd"),
        group("", { count: 1377, other: true }),
      ]),
    ]);
    expect(rows.map((row) => row.label)).toEqual(["solScHandleV*", "solPohTickProd", "everything else"]);
  });

  it("matches the folded row across samples by its flag rather than its name", () => {
    const samples = [
      sample(1, [group("", { other: true, on_cpu: 0.001 })]),
      sample(2, [group("", { other: true, on_cpu: 0.002 })]),
    ];
    expect(threadRows(samples)[0].series).toEqual([0.001, 0.002]);
  });

  it("draws at most a minute", () => {
    const samples = Array.from({ length: 90 }, (_, second) => sample(second, [group("solGossip")]));
    expect(threadRows(samples)[0].series).toHaveLength(60);
  });

  it("has nothing to draw before a sample arrives", () => {
    expect(threadRows([])).toEqual([]);
  });
});

describe("busiest", () => {
  it("is the first row that is not the folded one", () => {
    const rows = threadRows([sample(1, [group("solPohTickProd"), group("", { other: true })])]);
    expect(busiest(rows)?.name).toBe("solPohTickProd");
    expect(busiest(threadRows([sample(1, [group("", { other: true })])]))).toBeUndefined();
  });
});

describe("pinnedLabel", () => {
  it("names the core with the word, so a number never reads as a count", () => {
    // "2" under a heading beside a thread at 100% read as two cores in use.
    expect(pinnedLabel("2")).toBe("core 2");
    expect(pinnedLabel("12-22")).toBe("cores 12-22");
    expect(pinnedLabel("0-3,8")).toBe("cores 0-3,8");
    expect(pinnedLabel(null)).toBe("any");
  });
});

describe("tones", () => {
  const poh = (over: Partial<ThreadGroup>) =>
    threadRows([sample(1, [group("solPohTickProd", over)])])[0];
  const pool = (over: Partial<ThreadGroup>) =>
    threadRows([sample(1, [group("solScHandleV", { count: 11, ...over })])])[0];

  it("leaves on cpu untoned however high it runs", () => {
    // A busy thread is a thread doing its job. Toned high, PoH would be amber
    // for the life of the process and teach everyone to ignore the colour.
    expect(onCpuTone(pool({ on_cpu: 0.99 }))).toBeNull();
    expect(onCpuTone(poh({ on_cpu: 0.94 }))).toBeNull();
  });

  it("tones PoH when it is losing its core", () => {
    expect(onCpuTone(poh({ on_cpu: POH_LOW - 0.01 }))).toBe("warn");
    expect(onCpuTone(pool({ on_cpu: POH_LOW - 0.01 }))).toBeNull();
  });

  it("tones a bar only in PoH's row, and only for a second it lost the core", () => {
    expect(barLow(poh({}), 0.71)).toBe(true);
    expect(barLow(poh({}), 0.95)).toBe(false);
    expect(barLow(pool({}), 0.1)).toBe(false);
  });
});
