import { decimal } from "../format";
import {
  direction,
  egressShares,
  NETWORK_WINDOW_SECONDS,
  sharedPeak,
  unitFor,
  type Direction,
} from "../network";
import type { EgressSplit, NetworkSample, XdpConfig } from "../types";
import { RENDER_LAG_MS, useNow, windowed } from "../useNow";
import { useStore } from "../useStore";
import { Card, chartY, Explain } from "./primitives";

const WIDTH = 300;
const HEIGHT = 38;

/**
 * Whole-host interface throughput, summed over every non-loopback interface.
 *
 * Titled and labelled as the host's, not the validator's, because that is what
 * it measures: Linux attributes bytes to an interface and not to a process, so
 * anything else running on the box is counted too. On a dedicated validator the
 * two are near enough the same, which is exactly why the distinction has to be
 * on the card rather than left to the reader to guess.
 *
 * A row each, but one scale across both. Given a band of its own, a direction
 * moving ten kilobytes a second fills it exactly as a direction moving ten
 * megabytes does, and the picture would say the two are equals. Sharing the
 * scale is the reason both are worth putting on one card.
 *
 * The card renders nothing at all when the validator could not read the
 * counters, rather than showing zeros that would look like an idle network.
 */
export function NetworkCard() {
  const store = useStore();
  const rates = store.get<{ received_per_second: number; sent_per_second: number }>(
    "summary",
    "network",
  );
  // Null where the validator was given no XDP config, since the point behind
  // this is only submitted where it was. Absence is the answer rather than
  // something to work out.
  const xdp = store.get<XdpConfig | null>("summary", "xdp");
  // Absent until a sender has reported, and never on a validator whose log
  // level keeps it from submitting points at all.
  const split = store.get<EgressSplit>("summary", "network_egress");
  // Drawn a sample behind live, so the newest point sits past the right edge
  // and the line is continuous across it rather than ending in a notch.
  const edge = useNow() - RENDER_LAG_MS;
  if (!rates) return null;

  const windowMs = NETWORK_WINDOW_SECONDS * 1000;
  const visible = windowed(store.getNetwork(), edge, windowMs, (s) => s.timestamp_nanos);
  const received = visible.map((sample) => sample.received_per_second);
  const sent = visible.map((sample) => sample.sent_per_second);
  const peak = sharedPeak(received, sent);
  const egress = direction(sent) ?? {
    current: rates.sent_per_second,
    average: rates.sent_per_second,
    delta: 0,
    trend: "flat" as const,
  };

  const scope =
    "Every non-loopback interface on this host, not the validator's own traffic: " +
    "Linux counts bytes per interface, not per process.";

  return (
    <Card
      title="Host Network"
      aside={`last ${NETWORK_WINDOW_SECONDS}s`}
      className="network-body"
    >
      <Row
        label="Ingress"
        kind="ingress"
        // Falls back to the live rate before a minute of samples has arrived,
        // so the figure is right from the first second and only the line and
        // the average wait for a window to average over.
        read={direction(received) ?? { current: rates.received_per_second, average: rates.received_per_second, delta: 0, trend: "flat" }}
        samples={visible}
        value={(sample) => sample.received_per_second}
        edge={edge}
        windowMs={windowMs}
        peak={peak}
        explain={scope}
      />
      <Row
        label="Egress"
        kind="egress"
        read={egress}
        samples={visible}
        value={(sample) => sample.sent_per_second}
        edge={edge}
        windowMs={windowMs}
        peak={peak}
        explain={scope}
      />
      {split && <Split total={egress.current} split={split} />}
      {xdp && <Xdp xdp={xdp} />}
    </Card>
  );
}

/**
 * Anything the validator could actually name about the card, in the order it is
 * worth reading.
 *
 * Both of these come back as "unknown" where the lookup failed: the driver from
 * a device that would not answer, the model from a host with no PCI database to
 * resolve the id against. Left out rather than printed, because "unknown" in a
 * line naming hardware reads as a fault in the hardware rather than in the
 * lookup, and the tooltip still says what was and was not read.
 */
export function xdpDetail(xdp: XdpConfig): string[] {
  return [xdp.driver, xdp.model].filter((part) => named(part));
}

/** Whether the validator resolved this, rather than saying it could not. */
function named(part: string): boolean {
  return part !== "" && part !== "unknown";
}

/**
 * The whole tooltip: a sentence saying what the line is, then the two things
 * the line itself has no room for.
 *
 * The line names the mode, the driver and the model. The vendor and the kernel
 * are the rest of what was reported, and either can be missing on a host that
 * could not look it up, so this says whichever it has and stops at the sentence
 * where it has neither.
 *
 * The kernel is checked by prefix rather than for an exact "unknown", because a
 * failed `uname` is reported as "unknown" followed by the error it got, and
 * printed after the word kernel that reads as a version number.
 *
 * The sentence is built here rather than in the component so that the casing is
 * covered by the same tests as the content. With no vendor to lead it, the
 * kernel starts the second sentence and has to be capitalised to do so.
 */
export function xdpTooltip(xdp: XdpConfig): string {
  const sentence = "How this validator's XDP transmit path is set up.";
  const parts = [];
  if (named(xdp.vendor)) parts.push(xdp.vendor);
  if (xdp.kernel_version !== "" && !xdp.kernel_version.startsWith("unknown")) {
    parts.push(`kernel ${xdp.kernel_version}`);
  }
  if (parts.length === 0) return sentence;
  const aside = parts.join(", ");
  return `${sentence} ${aside.charAt(0).toUpperCase()}${aside.slice(1)}.`;
}

/**
 * How the transmit path is set up, where it is set up at all.
 *
 * One line, no figures, and nothing on it moves. It belongs on this card
 * because it is about the interface the card is already measuring, and it
 * belongs at the foot because it is the answer to a question asked once when
 * the flags went on rather than something to watch.
 *
 * Untoned throughout. Copy is the slower path, but the card cannot know whether
 * that was the intent or an omission, and an amber row would be calling a
 * working configuration a fault. The mode is the only word set in the body
 * colour, because it is the one thing an operator turned a flag on to get.
 */
function Xdp({ xdp }: { xdp: XdpConfig }) {
  const detail = xdpDetail(xdp);

  return (
    <div className="net-xdp">
      {/* A sentence and the two figures the line cannot fit. What the tooltip
          used to carry beyond that was background about zero-copy and the
          socket bind that an operator running these flags knows already. */}
      <span className="net-xdp-label">
        <Explain text={xdpTooltip(xdp)}>XDP transmit</Explain>
      </span>
      <span className="net-xdp-detail">
        <span className="net-xdp-mode">{xdp.zero_copy ? "zero-copy" : "copy"}</span>
        {detail.map((part) => (
          <span key={part}> · {part}</span>
        ))}
      </span>
    </div>
  );
}

/**
 * How much of egress two senders account for, drawn to the width of the line
 * above it. The rest is hatched and named as unattributed rather than left to
 * read as a third measurement: it is the shred path over XDP, which counts no
 * bytes.
 */
function Split({ total, split }: { total: number; split: EgressSplit }) {
  const shares = egressShares(total, split);
  const whole = Math.max(total, shares.measured, 1);
  const { unit, divisor } = unitFor(total);
  const show = (value: number) => decimal(value / divisor, 2);
  const width = (value: number) => `${((100 * value) / whole).toFixed(2)}%`;

  return (
    <div className="net-split">
      <span className="net-split-label">
        <Explain text="What the gossip and repair senders report putting on the wire, over the window each reports. The remainder is everything else the interface sent, mostly shreds over XDP, which reports no bytes.">
          of which
        </Explain>
      </span>
      <span className="net-split-body">
        <span className="net-split-bar" aria-hidden="true">
          <i className="is-gossip" style={{ width: width(shares.gossip) }} />
          <i className="is-repair" style={{ width: width(shares.repair) }} />
          <i className="is-unattributed" style={{ width: width(shares.remainder) }} />
        </span>
        <span className="net-split-legend">
          <span>
            <i className="is-gossip" />gossip {show(shares.gossip)}
          </span>
          <span>
            <i className="is-repair" />repair {show(shares.repair)}
          </span>
          <span>
            <i className="is-unattributed" />unattributed {show(shares.remainder)}
          </span>
        </span>
      </span>
      <span className="net-split-meta">
        <b>measured</b>
        {show(shares.measured)} {unit}/s
      </span>
    </div>
  );
}

/**
 * One direction: what it is doing now, the shape of the last minute, and what
 * it has averaged.
 *
 * Every figure on the row is printed in the unit the current reading calls for,
 * rather than each choosing its own. Sized separately, an average of 1.02 MB/s
 * prints beside a current 980 KB/s as "980" and "avg 1.02", and the larger
 * number looks like the smaller one.
 */
function Row({
  label,
  kind,
  read,
  samples,
  value,
  edge,
  windowMs,
  peak,
  explain,
}: {
  label: string;
  kind: "ingress" | "egress";
  read: Direction;
  samples: NetworkSample[];
  value: (sample: NetworkSample) => number;
  edge: number;
  windowMs: number;
  peak: number;
  explain: string;
}) {
  const { unit, divisor } = unitFor(read.current);
  const arrow = read.trend === "up" ? "▲" : read.trend === "down" ? "▼" : "·";

  return (
    <div className="net-row">
      <span className="net-label">
        <i className={`net-swatch is-${kind}`} aria-hidden="true" />
        <Explain text={explain}>{label}</Explain>
      </span>
      <span className="net-value">
        {decimal(read.current / divisor, 2)} <small>{unit}/s</small>
      </span>
      <span className="net-spark">
        <Spark
          kind={kind}
          samples={samples}
          value={value}
          edge={edge}
          windowMs={windowMs}
          peak={peak}
        />
      </span>
      <span className="net-meta">
        <b>avg {decimal(read.average / divisor, 2)}</b>
        {/* Untoned on purpose. Throughput going up is neither good nor bad on a
            validator, and a green or red arrow would read as a verdict on an
            ordinary fluctuation. */}
        <em>
          {arrow} {decimal(Math.abs(read.delta) / divisor, 2)}
        </em>
      </span>
    </div>
  );
}

function Spark({
  kind,
  samples,
  value,
  edge,
  windowMs,
  peak,
}: {
  kind: string;
  samples: NetworkSample[];
  value: (sample: NetworkSample) => number;
  edge: number;
  windowMs: number;
  peak: number;
}) {
  if (samples.length < 2) {
    return <span className="net-collecting">collecting…</span>;
  }
  // Placed by timestamp rather than by index, so a second the meter missed
  // leaves a gap of the right width instead of shifting everything after it.
  const points = samples.map((sample): [number, number] => [
    WIDTH * (1 - (edge - sample.timestamp_nanos / 1e6) / windowMs),
    chartY(value(sample), peak, HEIGHT),
  ]);
  const line = points
    .map(([x, y], index) => `${index === 0 ? "M" : "L"}${x.toFixed(1)},${y.toFixed(1)}`)
    .join(" ");

  // Closed under the samples themselves rather than across the full width. Shut
  // at the edges instead, a feed that stopped arriving would draw a long wedge
  // sloping to nothing at the right, which reads as throughput ramping down to
  // zero rather than as a chart with no news.
  const first = points[0][0];
  const last = points[points.length - 1][0];
  const area = `${line} L${last.toFixed(1)},${HEIGHT} L${first.toFixed(1)},${HEIGHT} Z`;

  return (
    <svg viewBox={`0 0 ${WIDTH} ${HEIGHT}`} preserveAspectRatio="none" role="img">
      <path className={`net-fill is-${kind}`} d={area} />
      <path className={`net-line is-${kind}`} d={line} />
    </svg>
  );
}
