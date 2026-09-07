import {
  useEffect,
  useState,
  type Dispatch,
  type ReactNode,
  type SetStateAction,
} from "react";
import { count, percent } from "../format";
import { useNarrow } from "../narrow";
import {
  admittedShare,
  doorSection,
  epochSpanLabel,
  executedSection,
  listenerSection,
  LOSSES_SHOWN,
  LOSSES_SHOWN_NARROW,
  portNamed,
  portsBusiestFirst,
  readOpenPorts,
  stakedShare,
  streamSection,
  verifySection,
  writeOpenPorts,
  type PathLoss,
  type PathSection,
} from "../tpuPath";
import type {
  BundleStage,
  EpochSpan,
  ExecutedStage,
  QuicPaths,
  QuicPort,
  VerifyStage,
} from "../types";
import { useStore } from "../useStore";
import { Card, Explain } from "./primitives";

/**
 * Everything that happens to a transaction before the scheduler sees it.
 *
 * The card is here whenever a QUIC port has taken a connection at any point in
 * this process's life, rather than whenever one took a connection lately. That
 * is the point of it: the two sections at the foot go quiet between leader
 * slots, and the card this replaces left the grid entirely whenever they did,
 * so an operator with few slots could rarely open the dashboard at a moment it
 * was there to read.
 *
 * Two shapes, chosen by whether the address this validator advertises for its
 * TPU is a socket on this host. It is not behind a relayer or a proxy, and the
 * layout for that is on `Elsewhere` below.
 *
 * Five sections, each drawn against its own total. They do not reconcile and
 * are not meant to: the door counts connections, the streams count streams, and
 * the three below count transactions, measured by three subsystems on three
 * cadences. A single chain across them would look authoritative and be quietly
 * wrong, so each restarts at its own hundred percent under its own heading.
 *
 * The last two are counted over a different span as well as against a different
 * total. Both only run while this validator is leader, and five minutes of a
 * stage that fires for a handful of slots every few hours reports whether a
 * leader slot happened to fall inside the window rather than anything about the
 * stage. They are summed over the epoch instead, which is the span the leader
 * schedule is drawn over and the stake behind it is fixed for. The two are
 * bracketed together under one caption that says so, rather than each carrying
 * a label of its own: they are summed on the same tick over the same slots, and
 * two copies of that fact are two chances to disagree about it.
 *
 * What the scheduler did with what arrived is not here. It is per leader slot
 * and it is on the slot page, joined to the block it produced, which is where a
 * figure that only exists during leader slots belongs.
 */
export function TpuPathCard() {
  const store = useStore();
  const paths = store.get<QuicPaths | null>("summary", "quic_paths");
  const verify = store.get<VerifyStage | null>("summary", "verify");
  const executed = store.get<ExecutedStage | null>("summary", "executed");
  const bundles = store.get<BundleStage | null>("summary", "bundles") ?? null;
  const span = store.get<EpochSpan | null>("summary", "epoch_span");
  const [open, setOpen] = useState(readOpenPorts);
  useEffect(() => writeOpenPorts(open), [open]);

  if (!paths) return null;

  // The two stages below the listener, which are the node's own work on
  // whatever it was given and do not depend on how it arrived. Both are absent
  // rather than nought where nothing happened, so a stage that has been quiet
  // costs no height.
  const stages = (
    <EpochStages
      span={span ?? null}
      sections={[
        ...(verify ? [verifySection(verify)] : []),
        ...(executed ? [executedSection(executed, bundles)] : []),
      ]}
    />
  );

  const ports = (
    <PortList
      ports={paths.tpu_offhost ? portsBusiestFirst(paths.ports) : others(paths)}
      open={open}
      setOpen={setOpen}
    />
  );

  if (paths.tpu_offhost) return <Elsewhere paths={paths} stages={stages} ports={ports} />;

  const tpu = portNamed(paths.ports, "tpu");
  // No advertised TPU address at all, which is not the same as one answered
  // elsewhere and has nothing to draw either way.
  if (!tpu) return null;

  const admitted = admittedShare(tpu);
  const staked = stakedShare(tpu);
  const sections = [
    doorSection(tpu, tpu.kernel_drops),
    streamSection(tpu),
    listenerSection(tpu),
  ];

  return (
    <Card
      title="TPU Path"
      aside={`${count(tpu.open)} open · ${count(tpu.active_streams)} streams`}
      className="path-body"
    >
      <div className="path-headline">
        <div className="path-figure">
          <span className="path-figure-value is-through">
            {admitted === null ? "—" : percent(admitted, 1)}
          </span>
          <span className="path-figure-label">
            <Explain text="Of every connection offered to the TPU port, the share this validator let in. The offer says more about the cluster than about this node, since an open port is offered far more than it could ever carry. What the node decides is how much of it to take, and this is that decision as one figure.">
              Admitted of offered
            </Explain>
          </span>
        </div>
        <div className="path-figure">
          <span className="path-figure-value">
            {staked === null ? "—" : percent(staked, 0)}
          </span>
          <span className="path-figure-label">
            <Explain text="Of the connections that were admitted, the share from peers holding stake. This is the figure that says whether stake weighting is doing anything for you: under pressure the limits are meant to keep letting staked peers in while the rest are shed, and a leader watching this fall during a busy slot is watching that fail.">
              Staked
            </Explain>
          </span>
        </div>
      </div>

      {sections.map((section) => (
        <Section key={section.key} section={section} />
      ))}

      {stages}

      {ports}

      <div className="card-footnote">
        Five minutes of the QUIC listener's own counters, except Verify and
        Executed, which are this epoch's. Each section is drawn against its own
        total; the sections do not add up against each other, because nothing
        counts a transaction across all of them. What the scheduler then did
        with a leader slot's traffic is on that slot's own page.
      </div>
    </Card>
  );
}

/**
 * The two stages counted over the epoch, bracketed under one caption.
 *
 * Bracketed rather than run in with the rest because the sections above are
 * counted over five minutes and the port list below comes back to them. A rule
 * with the span written on it would read as governing everything under it,
 * which here would be wrong twice over.
 *
 * The caption is set as a caption and not as a heading. It sits directly above
 * a section title, and given the titles' own size, case and colour the two read
 * as one heading of two lines rather than as a label and the thing labelled.
 *
 * Nothing at all where neither stage reported, which before the first leader
 * slot of an epoch is the honest state: a span named over no figures says the
 * stages threw everything away.
 */
function EpochStages({
  span,
  sections,
}: {
  span: EpochSpan | null;
  sections: PathSection[];
}) {
  if (sections.length === 0) return null;

  return (
    <div className="path-epoch">
      <div className="path-span">
        <Explain text="What the sections in this box are counted over, which is not what the sections outside it are counted over. Both of these stages only run while this validator is leader, and a five-minute window measures neither: on all but the largest validators it reports whether a leader slot happened to fall inside the last five minutes, and almost always one did not. An epoch is the span the leader schedule is drawn over and the stake behind it is fixed for, so it is the span these are kept over. Where the heading says counted from part way in, this validator was restarted during the epoch and the totals begin there rather than at its first slot.">
          {/* Published alongside the two stages, so it is only missing if one
              of them arrived without it. Named rather than left blank in that
              case: an epoch total under no heading reads as a windowed one. */}
          {span ? epochSpanLabel(span) : "This epoch"}
        </Explain>
      </div>

      {sections.map((section) => (
        <Section key={section.key} section={section} />
      ))}
    </div>
  );
}

/** Every port but the one the sections above were drawn from. */
function others(paths: QuicPaths): QuicPort[] {
  return paths.ports.filter((port) => port.name !== "tpu");
}

/**
 * The card on a validator whose advertised TPU address is answered off-host.
 *
 * Which is what a relayer or a block-assembly proxy does: it overwrites the
 * address in gossip, so the cluster connects to it and this host's own TPU
 * listener is offered almost nothing. Every figure the three port sections
 * would draw is then a true nought, and three empty bars under "connections
 * offered" say a port nobody uses is a port nobody could get through.
 *
 * So the ports fold to a line each and the stages keep the card. Those two
 * count what the scheduler was handed however it arrived, which on this
 * validator is the only part of the path that has anything to say. Verify is
 * kept rather than assumed away: a relayer forwards into the fetch stage and
 * sigverify counts what it sends, while a proxy that verifies its own
 * signatures never reaches that stage at all, and the card cannot tell which it
 * is sitting behind — so it draws the stage when the stage reports and leaves
 * it out when it does not.
 *
 * The headline is dropped with them. It is a rate over the TPU port's offer,
 * and a rate over one or two stray connections is noise wearing the clothes of
 * a measurement.
 */
function Elsewhere({
  paths,
  stages,
  ports,
}: {
  paths: QuicPaths;
  stages: ReactNode;
  ports: ReactNode;
}) {
  // Summed across the ports rather than taken from the TPU port, which is not
  // the subject here. What is live on this host is mostly vote connections.
  const live = paths.ports.reduce(
    (total, port) => ({
      open: total.open + port.open,
      streams: total.streams + port.active_streams,
    }),
    { open: 0, streams: 0 },
  );

  return (
    <Card
      title="TPU Path"
      aside={`${count(live.open)} open · ${count(live.streams)} streams`}
      className="path-body"
    >
      <div className="path-notice">
        The TPU address this validator advertises in gossip is{" "}
        <b>not a socket on this host</b>, so the cluster's connections are
        answered somewhere else. The stages below count what the scheduler was
        given however it arrived; the ports at the foot are only what still
        reaches this host directly.
      </div>

      {stages}

      {ports}

      <div className="card-footnote">
        This epoch's totals from this host's own workers, with the ports at the
        foot on the same five minutes as everywhere else. Each section is drawn
        against its own total; the sections do not add up against each other,
        because nothing counts a transaction across all of them. What the
        scheduler then did with a leader slot's traffic is on that slot's own
        page.
      </div>
    </Card>
  );
}

/** The folded port rows, and the one place that remembers which are open. */
function PortList({
  ports,
  open,
  setOpen,
}: {
  ports: QuicPort[];
  open: string[];
  setOpen: Dispatch<SetStateAction<string[]>>;
}) {
  return (
    <>
      {ports.map((port) => (
        <OtherPort
          key={port.name}
          port={port}
          open={open.includes(port.name)}
          onFold={() =>
            setOpen((names) =>
              names.includes(port.name)
                ? names.filter((name) => name !== port.name)
                : [...names, port.name],
            )
          }
        />
      ))}
    </>
  );
}

/**
 * One stage: a bar, what came out of it, and the losses beside it.
 *
 * The heading matters more here than it would in a single list. Every section
 * restarts at a hundred percent against its own total, and without a rule and a
 * name between them the card would read as one cascade that repeatedly climbs
 * back to full.
 */
function Section({ section }: { section: PathSection }) {
  const narrow = useNarrow();
  const [expanded, setExpanded] = useState(false);
  const cap = narrow ? LOSSES_SHOWN_NARROW : LOSSES_SHOWN;
  const shown = expanded ? section.losses : section.losses.slice(0, cap);
  // The detail rows never appear in the bar and never appear folded: they are
  // reasons behind one of the rows above rather than siblings of it, so showing
  // them alongside would read as another share of the same total.
  const more = section.losses.length - shown.length + (expanded ? 0 : section.detail.length);
  // Whether there is anything to expand at all, which is not the same as
  // whether anything is hidden right now: counted from what is hidden, the
  // control disappears the moment it is used and the section cannot be folded
  // back up again.
  const foldable = section.losses.length > cap || section.detail.length > 0;

  return (
    <section className="path-section">
      <div className="path-section-head">
        <Explain text={section.explain}>
          <span className="path-section-title">{section.title}</span>
        </Explain>
        <span className="path-section-note">{section.note}</span>
        {/* What went in and what came out, which is the whole section in one
            line for anyone not reading the rest of it. The word is the
            section's own: admitted, carried, verified. */}
        <span className="path-section-flow">
          {count(section.total)} in · {count(section.through.count)}{" "}
          {section.through.label}
        </span>
      </div>

      {section.aside && (
        <div className={`path-aside${section.aside.warn ? " tone-warn" : ""}`}>
          <Explain text={section.aside.explain}>
            {section.aside.label} {count(section.aside.count)} {section.aside.unit}
          </Explain>
        </div>
      )}

      {/* One bar cut into what got through and what did not, rather than a bar
          per row. The segments are a single hue stepped by lightness in the
          order the losses are listed, so the ramp says which is larger and
          nothing implies a severity that is not there. Tone lives on the
          figures below, where it can mean something. */}
      <div className="path-bar" aria-hidden="true">
        <i
          className="path-seg is-through"
          style={{ width: `${(section.through.count / Math.max(1, section.total)) * 100}%` }}
        />
        {section.losses.map((loss, index) => (
          <i
            key={loss.key}
            className={`path-seg is-${Math.min(index + 1, LOSSES_SHOWN)}`}
            style={{ width: `${loss.share * 100}%` }}
          />
        ))}
      </div>
      <div className="path-legend">
        {shown.map((loss, index) => (
          <Loss key={loss.key} loss={loss} rank={Math.min(index + 1, LOSSES_SHOWN)} />
        ))}
        {expanded &&
          section.detail.map((loss) => <Loss key={loss.key} loss={loss} rank={null} />)}
      </div>

      {(foldable || section.zeros > 0) && (
        <div className="path-quiet">
          {foldable && (
            <button
              type="button"
              className="path-more"
              aria-expanded={expanded}
              onClick={() => setExpanded((was) => !was)}
            >
              {expanded ? "show fewer" : `+ ${count(more)} more`}
            </button>
          )}
          {section.zeros > 0 && (
            <Explain text="Counters this section watches that stayed at nought over the window. Kept as a figure rather than as rows: a counter at nought is worth knowing, since it is the difference between nothing having gone wrong and nothing being measured, but a column of noughts is most of what made this card too tall to read.">
              <span>
                {count(section.zeros)} counter{section.zeros === 1 ? "" : "s"} at zero
              </span>
            </Explain>
          )}
        </div>
      )}
    </section>
  );
}

/** One loss in the legend, keyed to its segment by colour. */
function Loss({ loss, rank }: { loss: PathLoss; rank: number | null }) {
  return (
    <div className={`path-loss${rank === null ? " is-detail" : ""}`}>
      <i className={`path-swatch${rank === null ? "" : ` is-${rank}`}`} aria-hidden="true" />
      <Explain text={loss.explain} className="path-loss-label">
        {loss.label}
      </Explain>
      <span className={`path-loss-count${loss.warn ? " tone-warn" : ""}`}>
        {count(loss.count)}
      </span>
      <span className="path-loss-share">{percent(loss.share, 1)}</span>
    </div>
  );
}

/**
 * One QUIC port folded to a line.
 *
 * The quieter two always, and all three where the advertised TPU address is
 * answered off this host: there none of them carries the transaction path, and
 * a line each saying so is the whole of what they have to report. Unfolded,
 * each is the two sections the TPU port gets when it is the subject.
 *
 * The head is a row with a button in it rather than a row that is a button,
 * because the share needs an explanation and an explanation is itself a button,
 * which cannot be nested inside another one.
 */
function OtherPort({
  port,
  open,
  onFold,
}: {
  port: QuicPort;
  open: boolean;
  onFold: () => void;
}) {
  const admitted = admittedShare(port);

  return (
    <section className="path-port">
      <div className="path-port-head" onClick={onFold}>
        <span className="path-port-name">{port.name}</span>
        <span className="path-port-note">
          <Explain
            text={`Connections offered to the ${port.name} port over the last five minutes, and the share of them admitted. Every port has its own listener with its own limits, so each is counted on its own rather than added to the others.`}
          >
            {count(port.offered)} offered
            {admitted === null ? "" : ` · ${percent(admitted, 1)} admitted`}
          </Explain>
        </span>
        <button
          type="button"
          className="path-port-fold"
          aria-expanded={open}
          aria-label={`${open ? "Fold" : "Unfold"} ${port.name}`}
          onClick={(event) => {
            // The row under it toggles too, and two toggles are none.
            event.stopPropagation();
            onFold();
          }}
        >
          {open ? "−" : "+"}
        </button>
      </div>
      {open && (
        <div className="path-port-open">
          <Section section={doorSection(port, port.kernel_drops)} />
          <Section section={streamSection(port)} />
        </div>
      )}
    </section>
  );
}
