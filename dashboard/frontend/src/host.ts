/**
 * Reading the machine's figures, and deciding when each starts to matter.
 *
 * Kept out of the component so the thresholds are in one place and can be
 * tested, in the same way as the cache tones and the replay rows.
 */

import type { DeviceLoad, FilesystemUsage, Host } from "./types";

/** How a figure is coloured, matching the tones the rest of the page uses. */
export type HostTone = "good" | "warn" | "bad" | "muted";

/**
 * A filesystem's share used, past which it wants noticing.
 *
 * Amber leaves room to act and red means act now. A validator that fills its
 * ledger partition stops, so this is one of the few places on the dashboard
 * where the panel is trying to reach someone before the thing happens rather
 * than describe it afterwards.
 */
export const FULL_WARN = 0.8;
export const FULL_BAD = 0.9;

/**
 * A device's duty cycle, past which it is running out of throughput.
 *
 * Not a fill, despite also being a percentage. At 0.85 a device is close to
 * having no idle time left to absorb a burst, which is when queueing starts and
 * `wait` climbs.
 */
export const BUSY_WARN = 0.7;
export const BUSY_BAD = 0.85;

/**
 * Milliseconds a request may average before it is worth looking at.
 *
 * Set for NVMe, where a healthy device answers in tenths of a millisecond. A
 * whole millisecond is an order of magnitude out and shows up as replay falling
 * behind before anything else on the page moves. On spinning disks these
 * numbers would be nonsense, but a validator is not run on those.
 */
export const WAIT_WARN_MS = 1;
export const WAIT_BAD_MS = 5;

/** Share of memory left available, below which the machine is under pressure. */
export const AVAILABLE_WARN = 0.1;
export const AVAILABLE_BAD = 0.05;

/**
 * What is genuinely spoken for, and what is only being borrowed.
 *
 * There are two conventions for "used" and they disagree by tens of gigabytes.
 * The one most tools print is `total - free`, which counts the page cache and
 * makes a healthy validator look nearly out of memory. The one used here is
 * `total - free - reclaimable`, so the large figure is memory that is actually
 * committed, and the cache is drawn beside it as the part the kernel gives back
 * on demand.
 */
export function memoryUse(host: Host): {
  inUse: number;
  reclaimable: number;
  available: number;
  total: number;
} {
  const total = Math.max(0, host.memory_total);
  const reclaimable = Math.max(0, Math.min(total, host.memory_reclaimable));
  const free = Math.max(0, Math.min(total, host.memory_free));
  return {
    inUse: Math.max(0, total - free - reclaimable),
    reclaimable,
    available: Math.max(0, Math.min(total, host.memory_available)),
    total,
  };
}

/** How much of a filesystem is gone, in `[0, 1]`. */
export function fullness(filesystem: FilesystemUsage): number {
  if (filesystem.total <= 0) return 0;
  const used = Math.max(0, filesystem.total - filesystem.available);
  return Math.min(1, used / filesystem.total);
}

export function fullnessTone(share: number): HostTone {
  if (share >= FULL_BAD) return "bad";
  if (share >= FULL_WARN) return "warn";
  return "good";
}

export function busyTone(busy: number): HostTone {
  if (busy >= BUSY_BAD) return "bad";
  if (busy >= BUSY_WARN) return "warn";
  return "good";
}

/** Muted rather than green where the device did nothing: nobody waited. */
export function waitTone(waitMs: number | null): HostTone {
  if (waitMs === null) return "muted";
  if (waitMs >= WAIT_BAD_MS) return "bad";
  if (waitMs >= WAIT_WARN_MS) return "warn";
  return "good";
}

export function availableTone(available: number, total: number): HostTone {
  if (total <= 0) return "muted";
  const share = available / total;
  if (share <= AVAILABLE_BAD) return "bad";
  if (share <= AVAILABLE_WARN) return "warn";
  return "good";
}

/**
 * Swap in use is amber whatever the amount.
 *
 * The threshold is zero on purpose. A validator that has begun swapping is
 * already being hurt by it, and there is no healthy quantity to allow for.
 */
export function swapTone(used: number): HostTone {
  return used > 0 ? "warn" : "good";
}

/**
 * Which way load is going, from the three averages.
 *
 * The one-minute figure alone says nothing about direction, and direction is
 * most of what an operator wants from it: 12 on the way down is the end of
 * something, 12 on the way up is the start.
 */
export function loadTrend(host: Host): "rising" | "falling" | "steady" {
  const drift = host.load_one - host.load_fifteen;
  // A tenth of a core is noise on any machine a validator runs on.
  const noise = 0.1;
  if (drift > noise) return "rising";
  if (drift < -noise) return "falling";
  return "steady";
}

/**
 * What to call a device's row.
 *
 * The device name is the thing `iostat` and the kernel use, so it leads. The
 * roles follow, and there can be several: two mounts on one disk are one row,
 * because they compete for one queue.
 */
export function deviceLabel(device: DeviceLoad): string {
  return device.roles.join(" and ");
}
