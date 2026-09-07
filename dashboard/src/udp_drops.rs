//! Per-socket UDP receive counters, read from `/proc/net/udp`.
//!
//! `sk_drops` counts packets the kernel discarded because the receive buffer
//! was full, which the validator's own counters never see and which is the
//! common way shreds go missing. Delivered plus discarded is everything that
//! arrived at a socket, the only denominator a drop count can be judged
//! against; [`crate::metrics_tap`] supplies the delivered half. QUIC runs over
//! UDP, so the TPU sockets appear here too.
//!
//! Counters are cumulative per socket and keyed here by port. Attribution to a
//! service is the caller's job: this file describes every UDP socket in the
//! namespace.

use std::{
    collections::{HashMap, VecDeque},
    io,
    time::{Duration, Instant},
};

/// Kernel receive counters for every socket bound to one port, summed: turbine
/// binds several with `SO_REUSEPORT`, and only the total means anything.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PortCounters {
    /// Datagrams discarded since the sockets were opened, overwhelmingly
    /// because the receive buffer was full when one arrived.
    pub drops: u64,
    /// Bytes sitting unread. A gauge, and the leading indicator: a queue that
    /// stays deep is a reader falling behind.
    pub queued: u64,
}

pub type PortMap = HashMap<u16, PortCounters>;

/// A cumulative per-port counter over a trailing window, so a startup burst
/// ages out and the panel answers whether packets are being lost now. Also
/// holds what each port delivered, driven from the same tick with the same
/// ports so the two windows cover the same span, which the share lost divides
/// them by.
#[derive(Debug)]
pub struct PortWindow {
    span: Duration,
    /// Oldest first, each entry one tick's cumulative totals per port.
    samples: VecDeque<(Instant, HashMap<u16, u64>)>,
}

impl PortWindow {
    pub fn new(span: Duration) -> Self {
        Self {
            span,
            samples: VecDeque::new(),
        }
    }

    /// Records a tick and forgets what has fallen out. The oldest sample kept is
    /// the newest still at least a span old, so the window covers slightly more
    /// than the span rather than under-reporting by a tick.
    pub fn push(&mut self, now: Instant, totals: HashMap<u16, u64>) {
        self.samples.push_back((now, totals));
        while let Some((next, _)) = self.samples.get(1) {
            if now.duration_since(*next) < self.span {
                break;
            }
            self.samples.pop_front();
        }
    }

    /// Time the window actually covers, short until it has filled.
    pub fn covers(&self, now: Instant) -> Duration {
        self.samples
            .front()
            .map(|(at, _)| now.duration_since(*at))
            .unwrap_or_default()
    }

    /// How far `port`'s counter has climbed since the start of the window. Zero
    /// for a socket bound after the window started.
    pub fn since(&self, port: u16, current: u64) -> u64 {
        self.samples
            .front()
            .and_then(|(_, totals)| totals.get(&port))
            // A socket closed and reopened restarts at zero, so a total below
            // the remembered one is read as no movement rather than as a wrap.
            .and_then(|earlier| current.checked_sub(*earlier))
            .unwrap_or(0)
    }
}

/// Reads both address families and merges them by port. `/proc/net/udp6` is
/// absent on a kernel without IPv6, so either file alone is enough.
#[cfg(target_os = "linux")]
pub fn read() -> io::Result<PortMap> {
    let mut ports = PortMap::new();
    let mut rows: usize = 0;
    let mut last_err = None;

    for path in ["/proc/net/udp", "/proc/net/udp6"] {
        match std::fs::read_to_string(path) {
            Ok(contents) => rows = rows.saturating_add(parse_into(&contents, &mut ports)),
            Err(err) => last_err = Some(err),
        }
    }

    // A validator always holds UDP sockets, so understanding no rows at all
    // means the files were unreadable or are not in the format expected.
    if rows == 0 {
        return Err(last_err.unwrap_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "unrecognised /proc/net/udp")
        }));
    }
    Ok(ports)
}

#[cfg(not(target_os = "linux"))]
pub fn read() -> io::Result<PortMap> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "socket counters are only available on Linux",
    ))
}

/// Accumulates one `/proc/net/udp`-format table into `ports`, returning the
/// rows understood. Both families share the layout; only the address width
/// differs, and only the port after the colon is wanted:
///
/// ```text
///  sl  local_address rem_address st tx_queue:rx_queue tr:tm->when retrnsmt uid timeout inode ref pointer drops
/// 308: 00000000:14E9 00000000:0000 07 00000000:00000000 00:00000000 00000000   0        0 22359 2 0000000000000000 0
/// ```
///
/// `drops` is read as the thirteenth column rather than the last, so a kernel
/// that appends a column is ignored rather than misread.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_into(contents: &str, ports: &mut PortMap) -> usize {
    let mut rows: usize = 0;

    for line in contents.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let (Some(local), Some(queues), Some(drops)) =
            (fields.get(1), fields.get(4), fields.get(12))
        else {
            continue;
        };
        // Also how the heading row is rejected: its `local_address` column is
        // the literal word, which has no port to parse.
        let Some(port) = local
            .rsplit_once(':')
            .and_then(|(_, port)| u16::from_str_radix(port, 16).ok())
        else {
            continue;
        };
        let Ok(drops) = drops.parse::<u64>() else {
            continue;
        };
        // `tx_queue:rx_queue`. Only the receive side says whether this validator
        // is keeping up with what arrives.
        let queued = queues
            .rsplit_once(':')
            .and_then(|(_, rx)| u64::from_str_radix(rx, 16).ok())
            .unwrap_or(0);

        let entry = ports.entry(port).or_default();
        entry.drops = entry.drops.saturating_add(drops);
        entry.queued = entry.queued.saturating_add(queued);
        rows = rows.saturating_add(1);
    }

    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two sockets on port 8001 (0x1F41) as `SO_REUSEPORT` gives, and one on 8899
    /// (0x22C3). Left unformatted: a verbatim transcript, and the column positions
    /// are what is under test.
    #[rustfmt::skip]
    const V4: &str = "\
   sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops
  308: 00000000:1F41 00000000:0000 07 00000000:00000100 00:00000000 00000000     0        0 22359 2 0000000000000000 12
  309: 00000000:1F41 00000000:0000 07 00000000:00000200 00:00000000 00000000     0        0 22360 2 0000000000000000 30
  310: 0100007F:22C3 00000000:0000 07 00000000:00000000 00:00000000 00000000     0        0 22361 2 0000000000000000 0
";

    /// The v6 table, whose wider address column must not shift the fields read
    /// from it. Unformatted for the same reason as [`V4`].
    #[rustfmt::skip]
    const V6: &str = "\
   sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops
  511: 00000000000000000000000000000000:1F41 00000000000000000000000000000000:0000 07 00000000:00000040 00:00000000 00000000     0        0 22362 2 0000000000000000 5
";

    fn parse(contents: &str) -> PortMap {
        let mut ports = PortMap::new();
        parse_into(contents, &mut ports);
        ports
    }

    #[test]
    fn test_sums_every_socket_sharing_a_port() {
        let ports = parse(V4);
        let reused = ports[&8001];
        assert_eq!(reused.drops, 42);
        // 0x100 and 0x200: the queue columns are hex where drops is decimal.
        assert_eq!(reused.queued, 768);
    }

    #[test]
    fn test_idle_socket_reports_zeroes_rather_than_being_absent() {
        let ports = parse(V4);
        assert_eq!(ports[&8899], PortCounters::default());
    }

    #[test]
    fn test_two_address_families_merge_into_one_port() {
        let mut ports = PortMap::new();
        assert_eq!(parse_into(V4, &mut ports), 3);
        assert_eq!(parse_into(V6, &mut ports), 1);
        // The wider v6 address must not shift which column is read.
        assert_eq!(ports[&8001].drops, 47);
        assert_eq!(ports[&8001].queued, 832);
    }

    #[test]
    fn test_headings_alone_yield_nothing() {
        assert_eq!(parse(V4.lines().next().unwrap()).len(), 0);
        assert_eq!(parse("").len(), 0);
    }

    #[test]
    fn test_malformed_row_does_not_poison_the_total() {
        let text = format!("{V4}  311: garbage\n  312: 00000000:1F41 nonsense here\n");
        let ports = parse(&text);
        assert_eq!(ports[&8001].drops, 42);
        assert_eq!(ports.len(), 2);
    }

    /// One tick's totals for a single port.
    fn totals(port: u16, drops: u64) -> HashMap<u16, u64> {
        HashMap::from([(port, drops)])
    }

    #[test]
    fn test_burst_ages_out_of_the_window() {
        let base = Instant::now();
        let mut window = PortWindow::new(Duration::from_secs(60));

        // A hundred dropped at startup, then nothing for two minutes.
        window.push(base, totals(8001, 0));
        window.push(base + Duration::from_secs(1), totals(8001, 100));
        assert_eq!(window.since(8001, 100), 100);

        for second in 2..=120 {
            window.push(base + Duration::from_secs(second), totals(8001, 100));
        }
        // The total still says a hundred; the window says the validator is fine.
        assert_eq!(window.since(8001, 100), 0);
    }

    #[test]
    fn test_window_covers_at_least_its_span_once_filled() {
        let base = Instant::now();
        let mut window = PortWindow::new(Duration::from_secs(60));
        for second in 0..=120 {
            window.push(base + Duration::from_secs(second), totals(8001, 0));
        }
        let covered = window.covers(base + Duration::from_secs(120));
        assert!(covered >= Duration::from_secs(60), "covered {covered:?}");
        // Never wildly more, or a burst would linger well past its minute.
        assert!(covered <= Duration::from_secs(61), "covered {covered:?}");
    }

    #[test]
    fn test_short_window_reports_the_span_it_has_actually_watched() {
        let base = Instant::now();
        let mut window = PortWindow::new(Duration::from_secs(60));
        window.push(base, totals(8001, 0));
        window.push(base + Duration::from_secs(5), totals(8001, 3));
        assert_eq!(
            window.covers(base + Duration::from_secs(5)),
            Duration::from_secs(5)
        );
        assert_eq!(window.since(8001, 3), 3);
    }

    #[test]
    fn test_port_with_no_baseline_in_the_window_reports_no_drops() {
        // Rather than the cumulative total, which would show a socket bound
        // mid-window as having dropped everything it ever dropped.
        let base = Instant::now();
        let mut window = PortWindow::new(Duration::from_secs(60));
        window.push(base, totals(8001, 0));
        assert_eq!(window.since(8899, 4_000), 0);
    }

    #[test]
    fn test_counter_reset_reads_as_no_drops_rather_than_a_wrap() {
        let base = Instant::now();
        let mut window = PortWindow::new(Duration::from_secs(60));
        window.push(base, totals(8001, 900));
        assert_eq!(window.since(8001, 12), 0);
    }

    #[test]
    fn test_row_missing_its_trailing_columns_is_skipped() {
        // Truncated after `inode`, as a kernel predating the drops column prints it.
        // Reading the last field would report the inode as drops.
        let text = "  308: 00000000:1F41 00000000:0000 07 00000000:00000100 00:00000000 00000000 \
                    0 0 22359\n";
        assert_eq!(parse(text).len(), 0);
    }
}
