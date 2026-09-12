#!/usr/bin/env python3
"""Parse GGGGGGGGGG push-propagation logs (crds inserts, ingress prunes, active-set rotations).

The point of these logs is to say *why* a peer's contact info went stale in the
wait-for-supermajority check: because the peer stopped gossiping, or because
push propagation to us stopped and only the 15s pull cycle was still delivering
it.  H_01_PRUNE predicts the latter, i.e. gaps that end on a `pull_resp` insert.

Usage:
    gossip_routes.py validator.log                       # per-origin summary
    gossip_routes.py --join validator.log validator.log  # join to RRRRRRRRRR episodes
    gossip_routes.py --timeline --pubkey FT9... validator.log
    gossip_routes.py --prunes validator.log              # ingress prune table
    gossip_routes.py --rotations validator.log           # active-set churn

--join takes the log holding the RRRRRRRRRR episodes; usually the same file.
"""

import argparse
import gzip
import re
import sys
from collections import defaultdict
from datetime import datetime

TAG = "GGGGGGGGGG"

TS_RE = re.compile(r"^\[(?P<ts>[^\s\]]+)")
INSERT_RE = re.compile(
    rf"{TAG} crds_insert: origin=(?P<origin>\S+), route=(?P<route>\w+), "
    r"from=(?P<from>\S+), entry=(?P<entry>\w+), "
    r"wallclock_age=(?P<age>\d+)ms, ordinal=(?P<ordinal>\d+)"
)
PRUNE_RE = re.compile(
    rf"{TAG} prune_ingress: origin=(?P<origin>\S+), ingress=(?P<ingress>\d+), "
    r"pruned=(?P<pruned>\d+), retained=(?P<retained>\d+), "
    r"min_ingress_nodes=(?P<min_nodes>\d+), min_ingress_stake=(?P<min_stake>\d+)"
)
ROTATE_RE = re.compile(
    rf"{TAG} active_set_rotate: bucket=(?P<bucket>\d+), size=(?P<size>\d+), "
    r"added=\[(?P<added>[^\]]*)\], evicted=\[(?P<evicted>[^\]]*)\]"
)

ROUTES = ("push", "pull_resp", "pull_req", "local")


def parse_ts(raw):
    # 2026-09-11T10:15:08.082429266Z -> python only takes 6 fractional digits
    head, _, frac = raw.rstrip("Z").partition(".")
    return datetime.fromisoformat(f"{head}.{frac[:6]:0<6}")


def open_log(path):
    if path == "-":
        return sys.stdin
    if path.endswith(".gz"):
        return gzip.open(path, "rt", errors="replace")
    return open(path, errors="replace")


def parse(paths):
    """-> (inserts, prunes, rotations); inserts keyed by origin, in log order."""
    inserts = defaultdict(list)
    prunes = defaultdict(list)
    rotations = []
    for path in paths:
        with open_log(path) as handle:
            for line in handle:
                if TAG not in line:
                    continue
                stamp = TS_RE.match(line)
                ts = parse_ts(stamp["ts"]) if stamp else None
                if match := INSERT_RE.search(line):
                    inserts[match["origin"]].append(
                        {
                            "ts": ts,
                            "route": match["route"],
                            "from": match["from"],
                            "entry": match["entry"],
                            "age": int(match["age"]),
                        }
                    )
                elif match := PRUNE_RE.search(line):
                    prunes[match["origin"]].append(
                        {
                            "ts": ts,
                            "ingress": int(match["ingress"]),
                            "pruned": int(match["pruned"]),
                            "retained": int(match["retained"]),
                            "min_nodes": int(match["min_nodes"]),
                            "min_stake": int(match["min_stake"]),
                        }
                    )
                elif match := ROTATE_RE.search(line):
                    rotations.append(
                        {
                            "ts": ts,
                            "bucket": int(match["bucket"]),
                            "size": int(match["size"]),
                            "added": match["added"].split(", ") if match["added"] else [],
                            "evicted": (
                                match["evicted"].split(", ") if match["evicted"] else []
                            ),
                        }
                    )
    return inserts, prunes, rotations


def max_gap(events):
    """Longest interval between consecutive inserts -> (ms, before, after)."""
    worst = (0.0, None, None)
    for before, after in zip(events, events[1:]):
        if before["ts"] is None or after["ts"] is None:
            continue
        gap = (after["ts"] - before["ts"]).total_seconds() * 1000
        if gap > worst[0]:
            worst = (gap, before, after)
    return worst


def summarize(inserts, pubkeys):
    rows = []
    for origin, events in inserts.items():
        if pubkeys and origin not in pubkeys:
            continue
        counts = {route: 0 for route in ROUTES}
        for event in events:
            counts[event["route"]] = counts.get(event["route"], 0) + 1
        upstreams = {event["from"] for event in events if event["from"] != "none"}
        gap_ms, _, closer = max_gap(events)
        rows.append((gap_ms, origin, counts, len(upstreams), closer, len(events)))
    rows.sort(reverse=True)

    header = " ".join(f"{route:>9}" for route in ROUTES)
    print(f"{'ORIGIN':44} {'N':>5} {header} {'UPSTRM':>6} {'MAXGAP':>9} CLOSED_BY")
    for gap_ms, origin, counts, upstreams, closer, total in rows:
        cells = " ".join(f"{counts.get(route, 0):>9}" for route in ROUTES)
        closed = closer["route"] if closer else "-"
        print(
            f"{origin:44} {total:5} {cells} {upstreams:6} {gap_ms:8.0f}ms {closed}"
        )


def join_episodes(inserts, wfsm_logs, old_ms, new_ms):
    """For each RRRRRRRRRR grace-band episode, show the inserts bracketing it."""
    try:
        import wfsm_grace
    except ImportError:
        sys.exit("--join needs wfsm_grace.py importable (run from the repo root)")

    samples = wfsm_grace.parse_samples(wfsm_grace.grep_tag(wfsm_logs))
    if not samples:
        sys.exit("no RRRRRRRRRR samples found in --join logs")
    runs = wfsm_grace.episodes(samples, old_ms, new_ms)

    print(
        f"{'START':8} {'VERDICT':10} {'ORIGIN':44} "
        f"{'BEFORE':>9} {'AFTER':>9} {'BLIND':>9} ROUTES_IN_GAP"
    )
    for run in runs:
        events = inserts.get(run["pubkey"], [])
        before = [e for e in events if e["ts"] and e["ts"] <= run["first_ts"]]
        after = [e for e in events if e["ts"] and e["ts"] > run["last_ts"]]
        prior = before[-1] if before else None
        closer = after[0] if after else None
        # Inserts that landed while the node looked stale. If push propagation
        # stopped, these are pull_resp only -- that is the prediction.
        during = [
            e
            for e in events
            if e["ts"] and run["first_ts"] < e["ts"] <= run["last_ts"]
        ]
        in_gap = sorted({e["route"] for e in during + ([closer] if closer else [])})
        blind = (
            (closer["ts"] - prior["ts"]).total_seconds() * 1000
            if prior and closer
            else float("nan")
        )
        print(
            f"{run['first_ts']:%H:%M:%S} {run['verdict']:10} {run['pubkey']:44} "
            f"{(prior['route'] if prior else '-'):>9} "
            f"{(closer['route'] if closer else '-'):>9} "
            f"{blind:8.0f}ms {','.join(in_gap) or '-'}"
        )


def show_prunes(prunes, pubkeys):
    print(
        f"{'ORIGIN':44} {'N':>4} {'INGRESS':>8} {'PRUNED':>7} "
        f"{'RETAINED':>9} {'AT_FLOOR':>9} {'MIN_STAKE':>12}"
    )
    rows = []
    for origin, events in prunes.items():
        if pubkeys and origin not in pubkeys:
            continue
        # Hitting the floor means the stake band pruned everything it could and
        # only min_ingress_nodes kept the last paths alive.
        at_floor = sum(e["retained"] <= e["min_nodes"] for e in events)
        rows.append(
            (
                at_floor,
                origin,
                len(events),
                max(e["ingress"] for e in events),
                max(e["pruned"] for e in events),
                min(e["retained"] for e in events),
                max(e["min_stake"] for e in events),
            )
        )
    for at_floor, origin, n, ingress, pruned, retained, min_stake in sorted(
        rows, reverse=True
    ):
        print(
            f"{origin:44} {n:4} {ingress:8} {pruned:7} "
            f"{retained:9} {at_floor:9} {min_stake:12}"
        )


def show_rotations(rotations, pubkeys):
    churn = defaultdict(lambda: {"added": 0, "evicted": 0, "buckets": set()})
    for event in rotations:
        for node in event["added"]:
            churn[node]["added"] += 1
            churn[node]["buckets"].add(event["bucket"])
        for node in event["evicted"]:
            churn[node]["evicted"] += 1
            churn[node]["buckets"].add(event["bucket"])
    stamps = [e["ts"] for e in rotations if e["ts"]]
    if stamps:
        print(f"{len(rotations)} rotation event(s) over {stamps[-1] - stamps[0]}")
    print(f"{'DESTINATION':44} {'ADDED':>6} {'EVICTED':>8} BUCKETS")
    for node, stats in sorted(
        churn.items(), key=lambda kv: -kv[1]["evicted"]
    ):
        if pubkeys and node not in pubkeys:
            continue
        buckets = ",".join(str(b) for b in sorted(stats["buckets"]))
        print(f"{node:44} {stats['added']:6} {stats['evicted']:8} {buckets}")


def show_timeline(inserts, prunes, rotations, pubkeys):
    events = []
    for origin, records in inserts.items():
        if pubkeys and origin not in pubkeys:
            continue
        for record in records:
            events.append(
                (
                    record["ts"],
                    "crds_insert",
                    origin,
                    f"route={record['route']} from={record['from']} "
                    f"entry={record['entry']} wallclock_age={record['age']}ms",
                )
            )
    for origin, records in prunes.items():
        if pubkeys and origin not in pubkeys:
            continue
        for record in records:
            events.append(
                (
                    record["ts"],
                    "prune_ingress",
                    origin,
                    f"ingress={record['ingress']} pruned={record['pruned']} "
                    f"retained={record['retained']}",
                )
            )
    for record in rotations:
        touched = set(record["added"]) | set(record["evicted"])
        if pubkeys and not (touched & pubkeys):
            continue
        for node in record["added"]:
            events.append((record["ts"], "active_set_add", node, f"bucket={record['bucket']}"))
        for node in record["evicted"]:
            events.append(
                (record["ts"], "active_set_evict", node, f"bucket={record['bucket']}")
            )
    events.sort(key=lambda e: (e[0] is None, e[0]))
    for ts, kind, pubkey, detail in events:
        stamp = f"{ts:%H:%M:%S.%f}"[:12] if ts else "-"
        print(f"{stamp:<13} {kind:<16} {pubkey:<44} {detail}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("logs", nargs="+", help="log files, or - for stdin")
    parser.add_argument(
        "--pubkey", action="append", default=[], help="restrict to this pubkey (repeatable)"
    )
    parser.add_argument("--join", nargs="+", metavar="LOG", help="RRRRRRRRRR logs to join")
    parser.add_argument("--timeline", action="store_true")
    parser.add_argument("--prunes", action="store_true")
    parser.add_argument("--rotations", action="store_true")
    parser.add_argument("--old-ms", type=int, default=15000)
    parser.add_argument("--new-ms", type=int, default=45000)
    args = parser.parse_args()

    pubkeys = set(args.pubkey)
    inserts, prunes, rotations = parse(args.logs)
    if not (inserts or prunes or rotations):
        sys.exit(f"no {TAG} events found")

    if args.timeline:
        show_timeline(inserts, prunes, rotations, pubkeys)
    elif args.prunes:
        show_prunes(prunes, pubkeys)
    elif args.rotations:
        show_rotations(rotations, pubkeys)
    elif args.join:
        join_episodes(inserts, args.join, args.old_ms, args.new_ms)
    else:
        summarize(inserts, pubkeys)


if __name__ == "__main__":
    main()
