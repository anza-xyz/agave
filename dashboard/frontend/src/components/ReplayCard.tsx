import { count, decimal, micros, percent } from "../format";
import { cpuRows, parts, serialRows, verifyRows, type ReplayPart, type ReplayRow } from "../replay";
import type { ReplayWindow } from "../types";
import { useStore } from "../useStore";
import { Card, Explain } from "./primitives";

/**
 * What replay spends its time on, over the last few hundred slots.
 *
 * The only panel here describing work this validator does for every slot the
 * cluster produces rather than for the handful it leads. Everything else on
 * this page watches transactions arriving; this watches the node keep up with
 * everyone else's blocks, which is the half that decides whether it skips.
 *
 * Two resources, and they fail differently. Replay's own thread is serial: if
 * one slot's worth of work there exceeds one slot of time, no number of cores
 * will help. The thread time across the workers is capacity, and is measured
 * in cores rather than in percent because that is what it buys.
 *
 * Absent rather than empty when nothing has arrived. The point behind it is
 * sent with `datapoint_info!`, so a validator configured to log less than the
 * default never sends it, and a card of noughts would read as a node doing no
 * work rather than as a dashboard being told nothing.
 */
export function ReplayCard() {
  const store = useStore();
  const replay = store.get<ReplayWindow | null>("summary", "replay");
  const slotNanos = store.get<number>("summary", "observed_slot_duration_nanos");
  if (!replay) return null;

  const serial = replay.fetch + replay.confirming + replay.completing;
  const cpu =
    replay.execute + replay.load + replay.store + replay.program_cache + replay.checking + replay.other;

  // Against the slot time this cluster is actually keeping, not the nominal
  // one. The two drift apart under load, which is exactly when the figure is
  // being read.
  const slotMicros = slotNanos ? slotNanos / 1000 : null;
  const ofSlot = slotMicros ? serial / slotMicros : null;
  const cores = slotMicros ? cpu / slotMicros : null;

  const inside = parts(replay);

  return (
    <Card
      title="Replay"
      aside={`${count(replay.transactions)} tx/slot`}
      className="replay-body"
    >
      <div className="replay-figures">
        <Figure
          label="Replay thread"
          value={micros(serial)}
          sub="per slot"
          explain="Time replay's own thread spent on the average slot: reading the block, verifying it, dispatching it, and completing the bank. These run one after another, so this is a real duration. It is also the serial limit. If it approaches the slot time, the node falls behind however many cores it has."
        />
        <Figure
          label="Of slot time"
          value={ofSlot === null ? "—" : percent(ofSlot, 1)}
          // Not "of 400 ms". The figure divides by what the cluster is keeping,
          // and the two part company under load, which is when it gets read.
          sub="of observed slot"
          explain="That time as a share of how long a slot is actually lasting on this cluster, rather than of the nominal four hundred milliseconds. Replay works several slots at once, so this reads true at steady state and understates the pressure while the node is catching up."
        />
        <Figure
          label="CPU per slot"
          value={micros(cpu)}
          sub={cores === null ? "across threads" : `${decimal(cores, 2)} cores`}
          explain="Thread time one slot costs across every worker, and what that comes to in cores held busy. Exceeding the slot time is ordinary, and is what running on many cores looks like. Watch which way it moves over days rather than reading much into any one figure."
        />
        <Figure
          label="Worst slot"
          value={micros(replay.serial_peak)}
          sub={`last ${count(replay.slots)} slots`}
          explain="The worst single slot in the window, taken from each slot's own total. Adding up each figure's separate worst would describe a slot that never happened, because those maxima land on different slots."
        />
      </div>

      <Section
        title="Time spent on this slot"
        total={`${micros(serial)} wall clock`}
        rows={serialRows(replay)}
        explain="Replay's own thread, split into the three spans it spends there. Measured one after another, so these add up. Wall clock from first sight of the slot is far longer, but replay works several slots at once and votes and chooses forks in between, so nothing in that gap can be charged to this slot in particular."
      />

      <Section
        title="Verifying effort"
        total="relative shares only, no total"
        // Drawn with the segments held apart. The three overlap one another and
        // each runs across the thread pool, so they come to several times the
        // span they happened in: one continuous bar would say they are parts of
        // a whole, which is the claim this section's own note has to spend a
        // sentence denying.
        broken
        rows={verifyRows(replay)}
        explain="Which part of verification costs more. These are sums of asynchronous jobs that overlap one another and each run across many threads, so together they come to several times the window they happened in and cannot be split out of it. Each is measured the same way as the others, so comparing them to each other is sound. Comparing them to the figures above is not."
      />

      <Section
        title="Execution"
        total={`${micros(cpu)} CPU across threads`}
        rows={cpuRows(replay)}
        explain="Thread time accumulated across the worker threads, so this is CPU rather than wall clock and normally exceeds the slot. The phases are sequential within a thread, so unlike the verification figures these partition cleanly and their total is the CPU one slot costs."
      />

      {/* The four figures that sit inside a phase rather than beside it. A
          segment for any of them would draw the same microseconds twice, so
          they are said in a sentence, where nesting is something prose can
          carry. */}
      <p className="replay-parts">
        Inside running programs: <Part part={inside.bytecode} />, <Part part={inside.serialising} />,{" "}
        <Part part={inside.deserialising} />. Of program loading,{" "}
        <Part part={inside.compiling} verb="is" />.
      </p>
    </Card>
  );
}

/**
 * One of the four figures across the head of the card.
 *
 * The explanation is required rather than optional. Every one of these four is
 * a measurement whose label cannot say how it was taken, and a dotted underline
 * that opens nothing is worse than no underline at all: it gets tried once and
 * then the rest of them stop being tried.
 */
function Figure({
  label,
  value,
  sub,
  explain,
}: {
  label: string;
  value: string;
  sub: string;
  explain: string;
}) {
  return (
    <div className="replay-figure">
      <span className="replay-figure-label">
        <Explain text={explain}>{label}</Explain>
      </span>
      <span className="replay-figure-value">{value}</span>
      <span className="replay-figure-sub">{sub}</span>
    </div>
  );
}

/**
 * One section: a bar cut into its phases, and the legend that names them.
 *
 * The bar replaced a track per row. Per-row tracks compared each phase to the
 * section total one at a time, which is the same information the percentage
 * beside it already carried; cut into one bar the phases are compared to each
 * other instead, which is the reading that was missing.
 */
function Section({
  title,
  total,
  broken,
  rows,
  explain,
}: {
  title: string;
  total: string;
  /** Segments held apart, for figures that do not partition a whole. */
  broken?: boolean;
  rows: ReplayRow[];
  explain: string;
}) {
  return (
    <div className="replay-section">
      <div className="replay-head">
        <Explain text={explain}>
          <span className="replay-title">{title}</span>
        </Explain>
        <span className="replay-total">{total}</span>
      </div>

      <div className={`replay-bar${broken ? " is-broken" : ""}`} aria-hidden="true">
        {rows.map((row, index) => (
          <i
            key={row.key}
            // Grown from a basis of nothing rather than given a width, so that
            // the gaps in a broken bar come out of the track before the shares
            // are shared out, instead of pushing the total past its width.
            style={{ flexGrow: row.share, ...segment(index) }}
          />
        ))}
      </div>

      <div className="replay-legend">
        {rows.map((row, index) => (
          <div key={row.key} className="replay-item">
            <i className="replay-swatch" style={segment(index)} aria-hidden="true" />
            <Explain text={row.explain} className="replay-name">
              {row.label}
            </Explain>
            <span className="replay-value">{micros(row.micros)}</span>
            <span className="replay-share">{percent(row.share, 0)}</span>
          </div>
        ))}
      </div>
    </div>
  );
}

/**
 * How dark a segment is, by its place in the order.
 *
 * The rows arrive largest first, so this walks from the accent down towards the
 * panel behind it. A ramp rather than a set of hues: these are shares of one
 * quantity, not categories, and giving the sixth of them its own colour would
 * say they differ in kind.
 *
 * It stops well short of the panel. Carried further the last two steps still
 * read as segments of a bar, where they are large and share an edge, but their
 * nine-pixel swatches in the legend fell under a three to one contrast against
 * the card and became holes rather than marks.
 */
const SEGMENT_MIX = [100, 82, 68, 57, 48, 40];

function segment(index: number) {
  const mix = SEGMENT_MIX[Math.min(index, SEGMENT_MIX.length - 1)];
  return { background: `color-mix(in srgb, var(--accent) ${mix}%, var(--panel-raised))` };
}

/** One nested figure, named in the sentence under the card. */
function Part({ part, verb }: { part: ReplayPart; verb?: string }) {
  return (
    <>
      <Explain text={part.explain}>{part.label}</Explain>
      {verb ? ` ${verb} ` : " "}
      {micros(part.micros)}
      {part.peak === undefined ? "" : `, peaking at ${micros(part.peak)}`}
    </>
  );
}
