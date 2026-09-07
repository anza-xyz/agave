import { duration, percent } from "../format";
import type { StartupProgress } from "../types";
import { Meter } from "./primitives";

/**
 * The boot sequence in the order the validator moves through it, mirroring
 * `ValidatorStartProgress`.
 *
 * Several phases are conditional: a validator starting from a local ledger
 * downloads no snapshot, and one that is not waiting on a supermajority skips
 * that step. Rather than guess which will run, anything above the current
 * phase is shown as done or skipped, which is true either way.
 *
 * Two phases can say how far through they are and the rest cannot. Replay
 * counts slots and the supermajority wait counts stake; unpacking an archive
 * and building an accounts index report nothing at all while they run, and
 * there is no bank forks yet to read them from either. What every phase can
 * offer is how long it has taken, which on a boot that has stopped somewhere is
 * the figure being looked for anyway.
 */
const PHASES: Array<[string, string]> = [
  ["initializing", "Initializing"],
  ["searching_for_rpc_service", "Searching for an RPC service"],
  ["downloading_snapshot", "Downloading a snapshot"],
  ["cleaning_blockstore", "Cleaning the blockstore"],
  ["cleaning_accounts", "Cleaning accounts"],
  ["loading_ledger", "Loading the ledger"],
  ["processing_ledger", "Replaying the ledger"],
  ["starting_services", "Starting services"],
  ["waiting_for_supermajority", "Waiting for a supermajority"],
  ["running", "Running"],
];

export function StartupPhases({ startup }: { startup: StartupProgress }) {
  const current = PHASES.findIndex(([phase]) => phase === startup.phase);
  const taken = new Map(startup.phases_taken.map((t) => [t.phase, t.elapsed_nanos]));

  // `Halted` sits outside the sequence: it is a terminal state reached by
  // request, not a step on the way to running.
  if (current === -1) {
    return (
      <div className="startup">
        <div className="startup-phase is-current">
          <span className="startup-marker" />
          {startup.phase.replace(/_/g, " ")}
        </div>
        {startup.detail && <div className="card-footnote">{startup.detail}</div>}
      </div>
    );
  }

  // Replay measures itself in slots; the supermajority wait measures itself in
  // stake. They are different things and the bar means something different
  // under each, so which one is showing is said rather than left to be assumed.
  const measured =
    startup.phase === "waiting_for_supermajority" && startup.stake_percent !== null
      ? { fraction: startup.stake_percent, label: "of stake visible in gossip" }
      : startup.fraction !== null && startup.fraction !== undefined
        ? { fraction: startup.fraction, label: "of the ledger replayed" }
        : null;

  return (
    <div className="startup">
      <ol className="startup-list">
        {PHASES.map(([phase, label], index) => {
          const state = index < current ? "is-done" : index === current ? "is-current" : "is-todo";
          // A finished phase shows what it took; the current one counts up. A
          // phase that was skipped has no timing and shows nothing, which is
          // how a skipped step is told apart from an instant one.
          const elapsed =
            index === current
              ? startup.phase_elapsed_nanos
              : (taken.get(phase) ?? null);

          return (
            <li className={`startup-phase ${state}`} key={phase}>
              <span className="startup-marker" />
              <span className="startup-label">{label}</span>
              {index === current && startup.detail && (
                <span className="startup-detail">{startup.detail}</span>
              )}
              {elapsed !== null && elapsed > 0 && (
                <span className="startup-elapsed">{duration(elapsed / 1e6)}</span>
              )}
            </li>
          );
        })}
      </ol>
      {measured && (
        <div className="startup-measure">
          <Meter fraction={measured.fraction} />
          <div className="startup-measure-label">
            <span className="startup-measure-value">{percent(measured.fraction, 1)}</span>{" "}
            {measured.label}
          </div>
        </div>
      )}
    </div>
  );
}
