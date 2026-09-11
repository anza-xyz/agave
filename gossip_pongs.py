#!/usr/bin/env python3
"""Summarize gossip peers which advertise themselves but never send a pong.

The validator logs every generated ping, expired ping, received ping, received
pong, and received ContactInfo, all tagged PPPPPPPPPP. RECVD PINGS is the peer
probing us (we always pong back), so it is the reverse direction from PINGS. By default this script reports
hosts for which at least one ContactInfo was received but no Pong was received.

Companion to wfsm_grace.py, which parses the RRRRRRRRRR wait-for-supermajority
logs. WC_AGE_MAX is the largest age (by the peer's own wallclock, against ours)
that any of its ContactInfos already had on arrival -- values above the 15s
liveness cutoff mean the peer's clock trails ours, not that it went silent.

Usage:
  ./gossip_pongs.py agave-validator.log [more.log ...]
  grep PPPPPPPPPP agave-validator.log | ./gossip_pongs.py -
"""

import argparse
import gzip
import re
import sys
from collections import defaultdict


PEER = (
    r"pubkey=(?P<pubkey>[1-9A-HJ-NP-Za-km-z]+), "
    r"addr=(?P<ip>[^\s,]+):(?P<port>\d+)"
)
PING_RE = re.compile(r"gossip_ping_sent: " + PEER)
PING_RECVD_RE = re.compile(r"gossip_ping_received: " + PEER)
PING_TIMEOUT_RE = re.compile(r"gossip_ping_timeout: " + PEER)
PONG_RE = re.compile(
    r"gossip_pong_received: " + PEER + r", accepted=(?P<accepted>true|false)"
)
CONTACT_INFO_RE = re.compile(
    r"gossip_contact_info_received: "
    + PEER
    + r", software=(?P<software>[^\s,]+), version=(?P<version>[^\s,]+)"
    r"(?:, wallclock_age=(?P<wallclock_age>\d+)ms)?"
)


def open_log(path):
    if path == "-":
        return sys.stdin
    if path.endswith(".gz"):
        return gzip.open(path, "rt", errors="replace")
    return open(path, "rt", errors="replace")


def scan(paths):
    """Return event counts keyed by advertised IP and peer pubkey."""
    pings = defaultdict(int)
    recvd_pings = defaultdict(int)
    expired_pings = defaultdict(int)
    pongs = defaultdict(int)
    accepted_pongs = defaultdict(int)
    contact_infos = defaultdict(int)
    versions = {}
    gossip_ports = {}
    wallclock_ages = defaultdict(int)
    events = (
        ("gossip_ping_sent", PING_RE, pings),
        ("gossip_ping_received", PING_RECVD_RE, recvd_pings),
        ("gossip_ping_timeout", PING_TIMEOUT_RE, expired_pings),
        ("gossip_pong_received", PONG_RE, pongs),
        ("gossip_contact_info_received", CONTACT_INFO_RE, contact_infos),
    )
    for path in paths:
        with open_log(path) as log:
            for line in log:
                for marker, pattern, counts in events:
                    if marker not in line:
                        continue
                    match = pattern.search(line)
                    if match:
                        peer = (match["ip"], match["pubkey"])
                        counts[peer] += 1
                        if marker == "gossip_pong_received" and match["accepted"] == "true":
                            accepted_pongs[peer] += 1
                        elif marker == "gossip_contact_info_received":
                            versions[peer] = (match["software"], match["version"])
                            gossip_ports[peer] = match["port"]
                            age = match["wallclock_age"]
                            if age is not None:
                                wallclock_ages[peer] = max(
                                    wallclock_ages[peer], int(age)
                                )
                    break
    return (
        pings,
        recvd_pings,
        expired_pings,
        pongs,
        accepted_pongs,
        contact_infos,
        versions,
        gossip_ports,
        wallclock_ages,
    )


def main():
    parser = argparse.ArgumentParser(
        description="List hosts from which ContactInfo was received but no Pong "
        "was received."
    )
    parser.add_argument(
        "logs",
        nargs="+",
        metavar="LOG",
        help="agave validator log files ('-' for stdin, '.gz' is decompressed)",
    )
    parser.add_argument(
        "--min-contact-infos",
        type=int,
        default=1,
        help="require at least this many ContactInfo events (default: 1)",
    )
    parser.add_argument(
        "--min-expired-pings",
        "--min-expired-pongs",
        "--min-missed-pongs",
        type=int,
        default=0,
        help="require at least this many expired pings (default: 0)",
    )
    parser.add_argument(
        "--accepted-pongs-only",
        action="store_true",
        help="only count Pongs which matched an outstanding challenge",
    )
    parser.add_argument(
        "--status",
        choices=("accepted", "not-accepted"),
        help="only show peers with or without an accepted Pong",
    )
    parser.add_argument(
        "--all",
        action="store_true",
        help="show every host with a logged event, including hosts which sent a Pong",
    )
    args = parser.parse_args()

    (
        pings,
        recvd_pings,
        expired_pings,
        pongs,
        accepted_pongs,
        contact_infos,
        versions,
        gossip_ports,
        wallclock_ages,
    ) = scan(args.logs)
    counted_pongs = accepted_pongs if args.accepted_pongs_only else pongs
    if args.all:
        peers = (
            set(pings)
            | set(recvd_pings)
            | set(expired_pings)
            | set(pongs)
            | set(contact_infos)
        )
    else:
        peers = {
            peer
            for peer, count in contact_infos.items()
            if count >= args.min_contact_infos
        }
        if args.status is None:
            peers = {peer for peer in peers if counted_pongs[peer] == 0}
    if args.status == "accepted":
        peers = {peer for peer in peers if accepted_pongs[peer] > 0}
    elif args.status == "not-accepted":
        peers = {peer for peer in peers if accepted_pongs[peer] == 0}
    peers = {
        peer for peer in peers if expired_pings[peer] >= args.min_expired_pings
    }
    if not peers:
        print("no matching peers found")
        return

    rows = [
        (
            ip,
            pubkey,
            *versions.get((ip, pubkey), ("-", "-")),
            pings[(ip, pubkey)],
            recvd_pings[(ip, pubkey)],
            expired_pings[(ip, pubkey)],
            accepted_pongs[(ip, pubkey)],
            pongs[(ip, pubkey)] - accepted_pongs[(ip, pubkey)],
            contact_infos[(ip, pubkey)],
            gossip_ports.get((ip, pubkey), "-"),
            wallclock_ages[(ip, pubkey)],
        )
        for ip, pubkey in peers
    ]
    rows.sort(key=lambda row: (-row[9], -row[6], row[0], row[1]))
    ip_width = max(len("IP"), *(len(row[0]) for row in rows))
    software_width = max(len("SOFTWARE"), *(len(row[2]) for row in rows))
    version_width = max(len("VERSION"), *(len(row[3]) for row in rows))
    print(
        f"{'STATUS':<6}  {'IP':<{ip_width}}  {'PORT':>5}  {'PUBKEY':<44}  "
        f"{'SOFTWARE':<{software_width}}  {'VERSION':<{version_width}}  "
        f"{'PINGS':>7}  {'RECVD PINGS':>11}  {'EXPIRED':>7}  {'ACCEPTED PONGS':>14}  "
        f"{'NOT ACCEPTED PONGS':>18}  {'CONTACT INFOS':>13}  {'WC_AGE_MAX':>10}"
    )
    for (
        ip,
        pubkey,
        software,
        version,
        sent,
        recvd,
        expired,
        accepted,
        rejected,
        contacts,
        port,
        wallclock_age,
    ) in rows:
        status = "✓" if accepted else "✗"
        print(
            f"{status:<6}  {ip:<{ip_width}}  {port:>5}  {pubkey:<44}  "
            f"{software:<{software_width}}  {version:<{version_width}}  "
            f"{sent:>7}  {recvd:>11}  {expired:>7}  {accepted:>14}  {rejected:>18}  "
            f"{contacts:>13}  {str(wallclock_age) + 'ms':>10}"
        )
    print(
        f"\n{len(rows)} peer(s), {sum(row[4] for row in rows)} ping(s) sent, "
        f"{sum(row[5] for row in rows)} ping(s) received, "
        f"{sum(row[6] for row in rows)} expired ping(s), "
        f"{sum(row[7] for row in rows)} accepted pong(s), "
        f"{sum(row[8] for row in rows)} not accepted pong(s), and "
        f"{sum(row[9] for row in rows)} ContactInfo event(s)"
    )


if __name__ == "__main__":
    main()
