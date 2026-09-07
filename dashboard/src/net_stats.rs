//! Host network counters from `/proc/net/dev`. `SystemMonitorService` reads
//! the same file and refuses to start without it, unless the operator passed
//! `--no-os-network-stats-reporting`, in which case this reports nothing
//! rather than zeros. These are host-wide interface totals, not the
//! validator's own traffic.

use std::io;

/// Cumulative bytes across the host's non-loopback interfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetCounters {
    pub received: u64,
    pub sent: u64,
}

#[cfg(target_os = "linux")]
pub fn read() -> io::Result<NetCounters> {
    parse(&std::fs::read_to_string("/proc/net/dev")?)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unrecognised /proc/net/dev"))
}

#[cfg(not(target_os = "linux"))]
pub fn read() -> io::Result<NetCounters> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "network counters are only available on Linux",
    ))
}

/// Sums every interface except loopback. Receive counters come first on each
/// line and transmit counters start at the ninth field.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse(contents: &str) -> Option<NetCounters> {
    let mut totals = NetCounters {
        received: 0,
        sent: 0,
    };
    let mut seen_any = false;

    for line in contents.lines() {
        let Some((name, counters)) = line.split_once(':') else {
            // The first two lines are column headings and carry no colon.
            continue;
        };
        if name.trim() == "lo" {
            continue;
        }
        let fields: Vec<u64> = counters
            .split_whitespace()
            .map(|field| field.parse().unwrap_or(0))
            .collect();
        let (Some(received), Some(sent)) = (fields.first(), fields.get(8)) else {
            continue;
        };
        totals.received = totals.received.saturating_add(*received);
        totals.sent = totals.sent.saturating_add(*sent);
        seen_any = true;
    }

    seen_any.then_some(totals)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 1000       10    0    0    0     0          0         0     2000      20    0    0    0     0       0          0
  eth0: 5000       50    0    0    0     0          0         0     7000      70    0    0    0     0       0          0
  eth1:  500        5    0    0    0     0          0         0      300       3    0    0    0     0       0          0
";

    #[test]
    fn test_sums_interfaces_and_excludes_loopback() {
        let counters = parse(SAMPLE).unwrap();
        assert_eq!(counters.received, 5500);
        assert_eq!(counters.sent, 7300);
    }

    #[test]
    fn test_headings_alone_yield_nothing() {
        let headings = SAMPLE.lines().take(2).collect::<Vec<_>>().join("\n");
        assert!(parse(&headings).is_none());
        assert!(parse("").is_none());
    }

    #[test]
    fn test_malformed_row_does_not_poison_the_total() {
        let text = format!("{SAMPLE}  eth2: garbage\n");
        // The row parses to zeros rather than being counted as real traffic.
        assert_eq!(parse(&text).unwrap().received, 5500);
    }
}
