const LAMPORTS_PER_SOL = 1_000_000_000;

/**
 * Number formatters, built once and reused.
 *
 * `Number.prototype.toLocaleString` constructs an `Intl.NumberFormat` on every
 * call, and constructing one is almost all of the cost: measured against this
 * page at 12.8us a call, against 0.35us for a cached formatter, and 18.5us
 * when options are passed. The dashboard formats about eighty numbers per
 * render, several times a second.
 *
 * The output is unchanged. `x.toLocaleString(undefined, opts)` is defined as
 * `new Intl.NumberFormat(undefined, opts).format(x)`, which is what these are.
 */
const PLAIN = new Intl.NumberFormat();
const BY_DIGITS = new Map<number, Intl.NumberFormat>();

function withDigits(digits: number): Intl.NumberFormat {
  const cached = BY_DIGITS.get(digits);
  if (cached) return cached;
  const formatter = new Intl.NumberFormat(undefined, {
    minimumFractionDigits: digits,
    maximumFractionDigits: digits,
  });
  BY_DIGITS.set(digits, formatter);
  return formatter;
}

export function sol(lamports: number | undefined, digits = 2): string {
  if (lamports === undefined) return "—";
  return withDigits(digits).format(lamports / LAMPORTS_PER_SOL);
}

/** Large SOL amounts, abbreviated the way the header shows them. */
export function solCompact(lamports: number | undefined): string {
  if (lamports === undefined) return "—";
  const amount = lamports / LAMPORTS_PER_SOL;
  if (amount >= 1_000_000) return `${(amount / 1_000_000).toFixed(1)}M`;
  if (amount >= 1_000) return `${(amount / 1_000).toFixed(1)}K`;
  return amount.toFixed(1);
}

/**
 * Compute units, abbreviated: `11.8M`.
 *
 * Blocks are measured in tens of millions of these, and the exact figure never
 * matters. What matters is the size against the limit beside it, which reads
 * faster with two significant digits than with eight and a row of separators.
 */
export function units(value: number | null | undefined): string {
  if (value === null || value === undefined || Number.isNaN(value)) return "—";
  if (value >= 1_000_000) return `${(value / 1_000_000).toFixed(1)}M`;
  if (value >= 1_000) return `${(value / 1_000).toFixed(1)}K`;
  return String(value);
}

export function count(value: number | undefined): string {
  return value === undefined ? "—" : PLAIN.format(value);
}

export function decimal(value: number | undefined, digits = 2): string {
  if (value === undefined || Number.isNaN(value)) return "—";
  return withDigits(digits).format(value);
}

export function percent(fraction: number | null | undefined, digits = 2): string {
  if (fraction === null || fraction === undefined) return "—";
  return `${(fraction * 100).toFixed(digits)}%`;
}

/** Compact duration, e.g. `6d 23h 39m` or `2m 51s`. */
export function duration(millis: number | undefined): string {
  if (millis === undefined || millis < 0) return "—";
  const total = Math.floor(millis / 1000);
  const days = Math.floor(total / 86400);
  const hours = Math.floor((total % 86400) / 3600);
  const minutes = Math.floor((total % 3600) / 60);
  const seconds = total % 60;

  if (days > 0) return `${days}d ${hours}h ${minutes}m`;
  if (hours > 0) return `${hours}h ${minutes}m ${seconds}s`;
  if (minutes > 0) return `${minutes}m ${seconds}s`;
  return `${seconds}s`;
}

/**
 * Wall-clock time of a block, to the millisecond, in the viewer's own zone.
 *
 * Milliseconds are appended rather than asked of `toLocaleTimeString`, which
 * has no option for them. Two blocks in a row can be under two hundred
 * milliseconds apart, so seconds alone would show them as the same instant.
 */
export function blockTime(millis: number | null | undefined): string {
  if (millis === null || millis === undefined) return "—";
  const at = new Date(millis);
  if (Number.isNaN(at.getTime())) return "—";
  const day = at.toLocaleDateString(undefined, { day: "numeric", month: "short" });
  const time = at.toLocaleTimeString(undefined, { hour12: false });
  return `${day} ${time}.${String(at.getMilliseconds()).padStart(3, "0")}`;
}

/**
 * Wall-clock time of a block to the second, with the zone it is being read in.
 *
 * The row version of [`blockTime`], which carries milliseconds because two
 * blocks in a row can be two hundred of them apart and the detail panel is
 * where that matters. A list wants a stamp that lines up down the column, so
 * this stops at seconds.
 *
 * The zone is the viewer's own, named by whatever abbreviation the browser
 * holds for it. That is `EDT` or `UTC` where English has a common short form
 * and `GMT+4` where it does not, which covers most of the world. Deriving
 * initials from the long name would read better in a few places and be wrong in
 * more: `Gulf Standard Time` gives `GST`, but `United Kingdom Time` gives `UKT`
 * for a zone nobody calls that.
 */
export function blockStamp(millis: number | null | undefined): string {
  if (millis === null || millis === undefined) return "—";
  const at = new Date(millis);
  if (Number.isNaN(at.getTime())) return "—";
  const day = at.toLocaleDateString(undefined, { day: "numeric", month: "short" });
  const time = at.toLocaleTimeString(undefined, { hour12: false, timeZoneName: "short" });
  return `${day} ${time}`;
}

/**
 * The release a version belongs to, e.g. `4.3.0-beta.0` → `4.3.0`.
 *
 * Mirrors `release_of` in `collect.rs`, which is what the cluster version rows
 * are keyed by. Our own version is published in full, because the header shows
 * the exact build, so without this a validator running any pre-release never
 * matches its own row and the `ours` marker goes missing.
 */
export function release(version: string | undefined): string | undefined {
  if (version === undefined) return undefined;
  const at = version.search(/[-+]/);
  return at === -1 ? version : version.slice(0, at);
}

/**
 * How the header names this build, e.g. `Agave v4.3.0-beta.0`.
 *
 * The client leads because it is the half that tells one build from another: a
 * fork ships the version number of the release it follows, so `4.2.1` reads the
 * same whether it is stock Agave, Jito or any of the others, and the header
 * said nothing about which was running.
 *
 * Either half can be absent — a server older than the client field publishes
 * only the version, and neither has arrived before the first message — and
 * whatever is known is shown on its own rather than held back.
 */
export function buildLabel(
  client: string | undefined,
  version: string | undefined,
): string {
  return [client, version && `v${version}`].filter(Boolean).join(" ");
}

/**
 * A duration in microseconds, read in milliseconds, e.g. `205.6 ms`.
 *
 * Always one decimal, whatever the size. The figures it renders sit in a column
 * together and span three orders of magnitude, from a few hundred microseconds
 * of bank completion to hundreds of milliseconds of execution; varying the
 * precision by size would stop them lining up, which is most of what makes a
 * column of numbers readable.
 */
export function micros(us: number | null | undefined): string {
  if (us === null || us === undefined || Number.isNaN(us)) return "—";
  return `${(us / 1000).toFixed(1)} ms`;
}

/** Shortened pubkey, e.g. `J5e4xh…c8FF1`. */
export function shortKey(key: string | null | undefined, lead = 6, tail = 5): string {
  if (!key) return "—";
  if (key.length <= lead + tail + 1) return key;
  return `${key.slice(0, lead)}…${key.slice(-tail)}`;
}

export function bytes(value: number | undefined): string {
  if (value === undefined) return "—";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let scaled = value;
  let unit = 0;
  while (scaled >= 1024 && unit < units.length - 1) {
    scaled /= 1024;
    unit += 1;
  }
  return `${scaled.toFixed(unit === 0 ? 0 : 2)} ${units[unit]}`;
}

/** Signed delta against a reference slot, e.g. `-32` or `+1`. */
export function slotDelta(slot: number | undefined, reference: number | undefined): string {
  if (slot === undefined || reference === undefined) return "";
  const delta = slot - reference;
  return delta === 0 ? "0" : delta > 0 ? `+${delta}` : `${delta}`;
}
