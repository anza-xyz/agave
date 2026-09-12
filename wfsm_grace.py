#!/usr/bin/env python3
"""Parse RRRRRRRRRR wait-for-supermajority logs and classify grace-band episodes.

A grace-band episode is a run of consecutive samples in which a node's contact
info is older than the old timeout (15s) but younger than the new one (45s).
An episode ends either because the node refreshed (RECOVERED -- the old timeout
would have wrongly declared it offline) or because it kept aging past the new
timeout (AGED_OUT -- it really went away).

Usage:
    wfsm_grace.py [-e] [--old-ms N] [--new-ms N] validator.log [validator.log ...]
    cat validator.log | wfsm_grace.py -

-e prints one line per episode; default prints only the summary.
"""

import argparse
import re
import subprocess
import sys
from datetime import datetime

TAG = "RRRRRRRRRR"

HEADER_RE = re.compile(
    rf"^\[(?P<ts>[^\]]+)\].*{TAG} (?P<new>[\d.]+)% of active stake visible in gossip with "
    r"(?P<new_ms>\d+)ms timeout, (?P<old>[\d.]+)% with (?P<old_ms>\d+)ms timeout"
    r"(?:, (?P<local>[\d.]+)% with \d+ms local-insert timeout)?"
)
ENTRY_RE = re.compile(
    rf"^\[(?P<ts>[^\]]+)\].*{TAG}\s+(?P<stake>[\d.]+)% - (?P<pubkey>\S+) - "
    r"gossip (?P<addr>\S+) - age (?P<age>\d+)ms"
    r"(?: - local_age (?P<local_age>\d+|none)ms)?"
)


def parse_ts(raw):
    # 2026-09-11T10:15:08.082429266Z -> python only takes 6 fractional digits
    head, _, frac = raw.rstrip("Z").partition(".")
    return datetime.fromisoformat(f"{head}.{frac[:6]:0<6}")


def grep_tag(paths):
    """Pull just the tagged lines out of (possibly huge) logs via grep."""
    if paths == ["-"]:
        return sys.stdin.read().splitlines()
    out = subprocess.run(
        ["grep", "-h", "-F", TAG, *paths], capture_output=True, text=True
    )
    if out.returncode not in (0, 1):  # 1 == no matches, not an error here
        sys.exit(f"grep failed: {out.stderr.strip()}")
    return out.stdout.splitlines()


def parse_samples(lines):
    """-> [(timestamp, new_pct, old_pct, {pubkey: (stake, addr, age_ms)})]"""
    samples = []
    for line in lines:
        header = HEADER_RE.match(line)
        if header:
            samples.append(
                (
                    parse_ts(header["ts"]),
                    float(header["new"]),
                    float(header["old"]),
                    {},
                )
            )
            continue
        entry = ENTRY_RE.match(line)
        if entry:
            if not samples:
                continue  # entry before any header (truncated log)
            raw_local = entry["local_age"]
            samples[-1][3][entry["pubkey"]] = (
                float(entry["stake"]),
                entry["addr"],
                int(entry["age"]),
                int(raw_local) if raw_local not in (None, "none") else None,
            )
    return samples


def episodes(samples, old_ms, new_ms):
    """Close out each node's grace-band run and say how it ended."""
    open_runs = {}  # pubkey -> dict
    done = []
    for index, (ts, _, _, entries) in enumerate(samples):
        for pubkey, (stake, addr, age, local_age) in entries.items():
            run = open_runs.get(pubkey)
            if run is None:
                run = open_runs[pubkey] = {
                    "pubkey": pubkey,
                    "stake": stake,
                    "addr": addr,
                    "first_ts": ts,
                    "first_age": age,
                    "local_ages": [],
                    "offsets": [],
                    "samples": 0,
                }
            run["last_index"] = index
            run["last_ts"] = ts
            run["last_age"] = age
            run["last_local_age"] = local_age
            if local_age is not None:
                run["local_ages"].append(local_age)
                # How long the value had already been alive by the peer's clock
                # when we inserted it: delivery latency, not staleness.
                run["offsets"].append(age - local_age)
            run["samples"] += 1
        for pubkey in [p for p, r in open_runs.items() if r["last_index"] != index]:
            done.append(open_runs.pop(pubkey))
    done.extend(open_runs.values())

    for run in done:
        # Every sample had a recent local insert, so the node was gossiping fine
        # all along and only the peer-authored wallclock was old on arrival. The
        # offsets vary per value, so this is delivery latency, not clock skew --
        # a local-insert-based liveness check would not flag these at all.
        if run["local_ages"] and max(run["local_ages"]) < old_ms:
            run["verdict"] = "LATE_VALUE"
            continue
        next_index = run["last_index"] + 1
        if next_index >= len(samples):
            run["verdict"] = "TRUNCATED"  # log ends mid-episode
            continue
        gap_ms = (samples[next_index][0] - run["last_ts"]).total_seconds() * 1000
        # Had it not refreshed, this is what the age would have been next sample.
        run["verdict"] = "AGED_OUT" if run["last_age"] + gap_ms > new_ms else "RECOVERED"
    done.sort(key=lambda r: (r["first_ts"], -r["stake"]))
    return done


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("logs", nargs="+", help="log files, or - for stdin")
    parser.add_argument("-e", "--episodes", action="store_true", help="list episodes")
    parser.add_argument("--old-ms", type=int, default=15000)
    parser.add_argument("--new-ms", type=int, default=45000)
    args = parser.parse_args()

    samples = parse_samples(grep_tag(args.logs))
    if not samples:
        sys.exit(f"no {TAG} samples found")
    runs = episodes(samples, args.old_ms, args.new_ms)

    if args.episodes:
        for run in runs:
            local = (
                f" local_age {run['local_ages'][0]}->{run['local_ages'][-1]}ms"
                f" offset max {max(run['offsets'])}ms"
                if run["local_ages"]
                else ""
            )
            print(
                f"{run['first_ts']:%H:%M:%S} {run['verdict']:10} {run['stake']:6.3f}% "
                f"{run['pubkey']:44} {run['addr']:22} "
                f"age {run['first_age']}->{run['last_age']}ms "
                f"over {run['samples']} sample(s){local}"
            )
        print()

    by_verdict = {}
    for run in runs:
        count, stake = by_verdict.get(run["verdict"], (0, 0.0))
        by_verdict[run["verdict"]] = (count + 1, stake + run["stake"])

    span = samples[-1][0] - samples[0][0]
    print(f"{len(samples)} samples over {span}, {len(runs)} grace-band episodes")
    for verdict, (count, stake) in sorted(by_verdict.items()):
        print(f"  {verdict:10} {count:4} episode(s), {stake:7.3f}% stake (summed)")
    worst = max(samples, key=lambda s: s[1] - s[2])
    print(
        f"max gap {worst[1] - worst[2]:.3f}pp at {worst[0]:%H:%M:%S} "
        f"({worst[1]:.3f}% at {args.new_ms}ms vs {worst[2]:.3f}% at {args.old_ms}ms)"
    )


if __name__ == "__main__":
    main()
