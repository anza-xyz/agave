import { describe, expect, it } from "vitest";
import { bootTimes } from "./startup";
import type { StartupProgress } from "./types";

const SECOND = 1e9;

function running(phases: [string, number][]): StartupProgress {
  return {
    phase: "running",
    detail: null,
    running: true,
    fraction: null,
    stake_percent: null,
    phase_elapsed_nanos: 0,
    phases_taken: phases.map(([phase, seconds]) => ({ phase, elapsed_nanos: seconds * SECOND })),
  };
}

// Started at t=1000s, up for 300s, so the server clock reads 1300s.
const UPTIME = 300 * SECOND;
const NOW = 1300 * SECOND;

describe("bootTimes", () => {
  it("names the three long phases and folds the rest together", () => {
    const times = bootTimes(
      running([
        ["initializing", 2],
        ["downloading_snapshot", 108],
        ["loading_ledger", 34],
        ["processing_ledger", 89],
        ["starting_services", 19],
      ]),
      UPTIME,
      NOW,
      undefined,
    );
    expect(times?.startupMillis).toBe(252_000);
    expect(times?.phases).toEqual([
      { label: "snapshot download", millis: 108_000 },
      { label: "loading ledger", millis: 34_000 },
      { label: "ledger replay", millis: 89_000 },
      { label: "everything else", millis: 21_000 },
    ]);
  });

  it("gives no line to a phase that never happened", () => {
    // A boot with a ledger on disk downloads nothing, and the entry is simply
    // absent rather than present at nought.
    const times = bootTimes(running([["processing_ledger", 89]]), UPTIME, NOW, undefined);
    expect(times?.phases.map((phase) => phase.label)).toEqual(["ledger replay"]);
  });

  it("folds a named phase under a second into the rest, and keeps the sum", () => {
    const times = bootTimes(
      running([
        ["downloading_snapshot", 0.4],
        ["processing_ledger", 60],
        ["starting_services", 0.3],
      ]),
      UPTIME,
      NOW,
      undefined,
    );
    expect(times?.phases.map((phase) => phase.label)).toEqual(["ledger replay"]);
    expect(times?.startupMillis).toBe(60_700);
  });

  it("works the start out from uptime and the clock", () => {
    const times = bootTimes(running([]), UPTIME, NOW, undefined);
    expect(times?.startedMillis).toBe(1_000_000);
  });

  it("measures catching up from the end of the boot, not from the start", () => {
    // Running at 1060s, caught up at 1107s: 47s trailing the tip.
    const times = bootTimes(running([["processing_ledger", 60]]), UPTIME, NOW, 1107 * SECOND);
    expect(times?.catchUpMillis).toBe(47_000);
  });

  it("says nothing about catching up until it has happened", () => {
    expect(bootTimes(running([]), UPTIME, NOW, undefined)?.catchUpMillis).toBeNull();
  });

  it("has nothing to say before the validator is running", () => {
    const booting = { ...running([]), running: false, phase: "processing_ledger" };
    expect(bootTimes(booting, UPTIME, NOW, undefined)).toBeNull();
  });
});
