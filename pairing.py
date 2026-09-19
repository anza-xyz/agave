#!/usr/bin/env python3
"""Pair liveness pings with the pongs that answered them.

The WFSM liveness probe logs `gossip_liveness_ping_sent` when it pings a peer
whose ContactInfo is about to leave the 15s window, and the pong comes back on
the pre-existing `gossip_pong_received` line. Neither line carries a token, so
pairing has to follow the ping cache's own bookkeeping:

  - there is one challenge slot per (pubkey, gossip addr). A ping is only
    emitted when the slot is free or its expiry has passed, so at most one ping
    per peer is outstanding at a time and a pong answers that one.
  - `PingCache::add` matches on the ping hash, so a pong arriving after the slot
    was reused answers a hash that is already gone and is logged
    accepted=false. Those are counted as stale, not as a pairing.
  - the slot is shared with ordinary gossip pings (`gossip_ping_sent`, emitted
    by pull requests, active-set refresh and address verification), so those are
    tracked too: a pong which answers one of them cannot also be credited to the
    probe. PROBE/OTHER in the output is which kind of ping the pong answered.
  - `gossip_ping_timeout` is the cache reporting that a challenge expired
    unanswered, which closes the slot.

A probe ping with no pairing is the interesting case: it means the peer did not
answer on its advertised gossip address, so the WFSM criterion
(fresh ContactInfo, or a pong more recent than the ContactInfo) has nothing to
fall back on and the node counts as offline.

Usage:
  ./pairing.py agave-validator.log [more.log ...]
  ./pairing.py --pairs --pubkey <PUBKEY> agave-validator.log
  grep PPPPPPPPPP agave-validator.log | ./pairing.py -
"""

import argparse
import re
import statistics
import sys
from collections import defaultdict

from gossip_pongs import PEER, PING_RE, PING_TIMEOUT_RE, PONG_RE, TS_RE, open_log
from wfsm_grace import parse_ts

LIVENESS_PING_RE = re.compile(r"gossip_liveness_ping_sent: " + PEER)

PROBE = "PROBE"
OTHER = "OTHER"

EVENTS = (
    ("gossip_liveness_ping_sent", LIVENESS_PING_RE),
    ("gossip_ping_sent", PING_RE),
    ("gossip_pong_received", PONG_RE),
    ("gossip_ping_timeout", PING_TIMEOUT_RE),
)


class Peer:
    """Challenge slot and tallies for one (pubkey, addr)."""

    def __init__(self):
        self.outstanding = None  # (kind, timestamp) of the unanswered ping
        self.probes = 0
        self.others = 0
        self.paired = []  # round trip in ms, probe pings only
        self.paired_other = 0
        self.timed_out = 0  # challenges the cache reported as expired
        self.superseded = 0  # slot reused without a pong or a timeout line
        self.stale_pongs = 0  # accepted=false, i.e. answered a dead challenge

    @property
    def unanswered(self):
        return self.probes - len(self.paired) - self.stranded

    @property
    def stranded(self):
        """Probe pings still outstanding at the end of the log."""
        return 1 if self.outstanding and self.outstanding[0] == PROBE else 0


def scan(paths, pubkeys, on_pair=None):
    """-> {(pubkey, addr): Peer}. Calls on_pair for each resolved challenge."""
    peers = defaultdict(Peer)
    for path in paths:
        with open_log(path) as log:
            for line in log:
                for marker, pattern in EVENTS:
                    if marker not in line:
                        continue
                    match = pattern.search(line)
                    if match is None or (pubkeys and match["pubkey"] not in pubkeys):
                        break
                    stamp = TS_RE.match(line)
                    if stamp is None:
                        break
                    now = parse_ts(stamp["ts"])
                    key = (match["pubkey"], f"{match['ip']}:{match['port']}")
                    peer = peers[key]
                    if marker == "gossip_pong_received":
                        resolve_pong(key, peer, now, match["accepted"], on_pair)
                    elif marker == "gossip_ping_timeout":
                        resolve_timeout(key, peer, now, on_pair)
                    else:
                        kind = PROBE if "liveness" in marker else OTHER
                        open_challenge(key, peer, now, kind, on_pair)
                    break
    return peers


def open_challenge(key, peer, now, kind, on_pair):
    if peer.outstanding:
        # A ping is only emitted once the slot's expiry has passed, so an
        # outstanding challenge here went unanswered without a timeout line
        # (the report is drained at a bounded rate, so it can lag or be lost).
        peer.superseded += 1
        if on_pair:
            on_pair(key, peer.outstanding[0], peer.outstanding[1], now, "superseded", None)
    peer.outstanding = (kind, now)
    if kind == PROBE:
        peer.probes += 1
    else:
        peer.others += 1


def resolve_pong(key, peer, now, accepted, on_pair):
    if accepted != "true" or peer.outstanding is None:
        # Either the pong's hash was already gone, or we never saw the ping that
        # it answers (log started mid-flight).
        peer.stale_pongs += 1
        if on_pair:
            on_pair(key, None, None, now, "stale-pong", None)
        return
    kind, sent = peer.outstanding
    peer.outstanding = None
    rtt = (now - sent).total_seconds() * 1000
    if kind == PROBE:
        peer.paired.append(rtt)
    else:
        peer.paired_other += 1
    if on_pair:
        on_pair(key, kind, sent, now, "paired", rtt)


def resolve_timeout(key, peer, now, on_pair):
    if peer.outstanding is None:
        return
    kind, sent = peer.outstanding
    peer.outstanding = None
    peer.timed_out += 1
    if on_pair:
        on_pair(key, kind, sent, now, "timed-out", None)


def print_pairs(key, kind, sent, now, verdict, rtt):
    pubkey, addr = key
    rtt = f"{rtt:.1f}ms" if rtt is not None else "-"
    sent = f"{sent.time()}" if sent is not None else "-"
    print(
        f"{now.time()!s:<16} {pubkey:<44} {addr:<22} "
        f"{kind or '-':<6} {verdict:<11} sent={sent:<16} rtt={rtt}"
    )


def report(peers, min_probes, unanswered_only):
    probed = {key: peer for key, peer in peers.items() if peer.probes >= min_probes}
    if unanswered_only:
        probed = {
            key: peer
            for key, peer in probed.items()
            if peer.unanswered or peer.stranded
        }
    if not probed:
        print("no liveness pings matched the filters", file=sys.stderr)
        return
    rtts = [rtt for peer in probed.values() for rtt in peer.paired]
    probes = sum(peer.probes for peer in probed.values())
    share = f"{100.0 * len(rtts) / probes:.1f}%" if probes else "-"
    print(
        f"{len(probed)} peers probed, {probes} liveness pings, {len(rtts)} paired "
        f"({share}), "
        f"{sum(peer.unanswered for peer in probed.values())} unanswered, "
        f"{sum(peer.stranded for peer in probed.values())} still outstanding"
    )
    if rtts:
        rtts.sort()
        print(
            f"probe rtt p50={statistics.median(rtts):.1f}ms "
            f"p90={rtts[int(0.9 * (len(rtts) - 1))]:.1f}ms max={rtts[-1]:.1f}ms"
        )
    print(
        f"{sum(peer.timed_out for peer in probed.values())} challenges timed out, "
        f"{sum(peer.superseded for peer in probed.values())} superseded, "
        f"{sum(peer.stale_pongs for peer in probed.values())} stale pongs, "
        f"{sum(peer.paired_other for peer in probed.values())} pongs answered a "
        "non-probe ping"
    )
    print()
    for key, peer in sorted(probed.items(), key=lambda kv: -kv[1].probes):
        pubkey, addr = key
        rtt = f"{statistics.median(peer.paired):.0f}ms" if peer.paired else "-"
        print(
            f"  {pubkey} {addr:<22} probes={peer.probes:<5} "
            f"paired={len(peer.paired):<5} unanswered={peer.unanswered:<5} "
            f"rtt_p50={rtt:<8} timeouts={peer.timed_out:<5} "
            f"stale={peer.stale_pongs:<5} other_pings={peer.others}"
        )


def main():
    parser = argparse.ArgumentParser(
        description="Pair WFSM liveness pings with the pongs that answered them."
    )
    parser.add_argument(
        "logs",
        nargs="+",
        metavar="LOG",
        help="agave validator log files ('-' for stdin, '.gz' is decompressed)",
    )
    parser.add_argument(
        "--pubkey",
        action="append",
        metavar="PUBKEY",
        help="restrict to this peer (repeatable)",
    )
    parser.add_argument(
        "--pairs",
        action="store_true",
        help="print one line per resolved challenge instead of the summary",
    )
    parser.add_argument(
        "--min-probes",
        type=int,
        default=1,
        help="require at least this many liveness pings (default: 1)",
    )
    parser.add_argument(
        "--unanswered-only",
        action="store_true",
        help="only show peers with a liveness ping that no pong answered",
    )
    args = parser.parse_args()

    peers = scan(
        args.logs,
        set(args.pubkey or ()),
        on_pair=print_pairs if args.pairs else None,
    )
    if not args.pairs:
        report(peers, args.min_probes, args.unanswered_only)


if __name__ == "__main__":
    main()
