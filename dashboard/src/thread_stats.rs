//! Where each of the validator's threads spent the last second, from the
//! scheduler's own accounting under `/proc/self/task`. Read here rather than
//! through the metrics tap for the same reason as the host figures: it works
//! on a node logging below the default.

use {
    serde::Serialize,
    std::{cmp::Ordering, collections::HashMap, io},
};

/// A thread's scheduler counters, cumulative since it started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadReading {
    pub tid: u64,
    /// The name the thread gave itself, as the kernel keeps it: fifteen bytes.
    pub name: String,
    pub on_cpu_nanos: u64,
    pub waiting_nanos: u64,
}

/// One group of threads' second, as shares of it, mean per thread.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ThreadGroup {
    /// The pool's name with its trailing number stripped. Empty on the folded
    /// row.
    pub name: String,
    pub count: usize,
    /// The cores the threads may run on, where that is fewer than the machine
    /// has and the same for every thread in the group.
    pub cores: Option<String>,
    /// Share of the second on a core, and runnable but waiting for one.
    pub on_cpu: f64,
    pub waiting: f64,
    /// True for the one row every group not shown is folded into.
    pub other: bool,
}

#[cfg(target_os = "linux")]
pub fn read() -> io::Result<Vec<ThreadReading>> {
    let mut threads = Vec::new();
    for entry in std::fs::read_dir("/proc/self/task")?.flatten() {
        let Some(tid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u64>().ok())
        else {
            continue;
        };
        let dir = entry.path();
        // A thread can exit between the listing and the read.
        let (Ok(name), Ok(schedstat)) = (
            std::fs::read_to_string(dir.join("comm")),
            std::fs::read_to_string(dir.join("schedstat")),
        ) else {
            continue;
        };
        let Some((on_cpu_nanos, waiting_nanos)) = parse_schedstat(&schedstat) else {
            continue;
        };
        threads.push(ThreadReading {
            tid,
            name: name.trim().to_string(),
            on_cpu_nanos,
            waiting_nanos,
        });
    }
    Ok(threads)
}

#[cfg(not(target_os = "linux"))]
pub fn read() -> io::Result<Vec<ThreadReading>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "thread counters are only available on Linux",
    ))
}

/// The cores a thread may run on, from its status file. `None` where it
/// cannot be read, which includes a thread that has just exited.
#[cfg(target_os = "linux")]
pub fn cores_allowed(tid: u64) -> Option<String> {
    let status = std::fs::read_to_string(format!("/proc/self/task/{tid}/status")).ok()?;
    parse_cpus_allowed(&status)
}

#[cfg(not(target_os = "linux"))]
pub fn cores_allowed(_tid: u64) -> Option<String> {
    None
}

/// `12345678 2345 67`: nanoseconds on a cpu, nanoseconds runnable and waiting
/// for one, and timeslices.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_schedstat(contents: &str) -> Option<(u64, u64)> {
    let mut fields = contents.split_whitespace();
    let on_cpu = fields.next()?.parse().ok()?;
    let waiting = fields.next()?.parse().ok()?;
    Some((on_cpu, waiting))
}

/// `Cpus_allowed_list:\t0-23`, one line among the status file's.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_cpus_allowed(status: &str) -> Option<String> {
    status
        .lines()
        .find_map(|line| line.strip_prefix("Cpus_allowed_list:"))
        .map(|list| list.trim().to_string())
        .filter(|list| !list.is_empty())
}

/// How many cores a list names: `0-3,8` is five.
pub fn cores_in(list: &str) -> usize {
    list.split(',')
        .map(|part| match part.trim().split_once('-') {
            Some((from, to)) => {
                let from: usize = from.parse().unwrap_or(0);
                let to: usize = to.parse().unwrap_or(0);
                to.saturating_sub(from).saturating_add(1)
            }
            None => 1,
        })
        .fold(0, usize::saturating_add)
}

/// A pool's threads share a name and differ by a trailing number.
pub fn group_key(name: &str) -> &str {
    name.trim_end_matches(|c: char| c.is_ascii_digit())
}

/// Sums in the making, before they are turned into shares.
#[derive(Default)]
struct GroupSum {
    count: usize,
    on_cpu_nanos: u64,
    waiting_nanos: u64,
    /// `None` until the first thread; then the pinning every thread so far has
    /// agreed on, or `Some(None)` where they differ or none is pinned.
    cores: Option<Option<String>>,
}

/// Each group's second, from two readings of every thread. A thread with no
/// earlier reading, or one whose id was reused under another name, is left
/// out: there is nothing honest to difference.
pub fn group_shares(
    previous: &HashMap<u64, ThreadReading>,
    current: &[ThreadReading],
    pinning: &HashMap<u64, Option<String>>,
    interval_nanos: u64,
) -> Vec<ThreadGroup> {
    let mut groups: HashMap<&str, GroupSum> = HashMap::new();
    for thread in current {
        let Some(before) = previous.get(&thread.tid) else {
            continue;
        };
        if before.name != thread.name {
            continue;
        }
        let sum = groups.entry(group_key(&thread.name)).or_default();
        sum.count = sum.count.saturating_add(1);
        sum.on_cpu_nanos = sum
            .on_cpu_nanos
            .saturating_add(thread.on_cpu_nanos.saturating_sub(before.on_cpu_nanos));
        sum.waiting_nanos = sum
            .waiting_nanos
            .saturating_add(thread.waiting_nanos.saturating_sub(before.waiting_nanos));
        let allowed = pinning.get(&thread.tid).cloned().flatten();
        match &sum.cores {
            None => sum.cores = Some(allowed),
            Some(held) if *held != allowed => sum.cores = Some(None),
            Some(_) => (),
        }
    }

    let span = interval_nanos as f64;
    groups
        .into_iter()
        .filter(|(_, sum)| sum.count > 0)
        .map(|(name, sum)| ThreadGroup {
            name: name.to_string(),
            count: sum.count,
            cores: sum.cores.flatten(),
            on_cpu: share(sum.on_cpu_nanos, sum.count, span),
            waiting: share(sum.waiting_nanos, sum.count, span),
            other: false,
        })
        .collect()
}

/// Nanoseconds across `count` threads as a mean share of `span`. Clamped:
/// the two clocks are read apart, so a busy thread can read a hair over.
fn share(nanos: u64, count: usize, span: f64) -> f64 {
    if count == 0 || span <= 0.0 {
        return 0.0;
    }
    (nanos as f64 / (count as f64 * span)).min(1.0)
}

/// The groups worth a row: the `top` by their mean share over the window, in
/// that order, and every other group folded into one row. Ranked on the
/// window rather than the second, so the rows do not reorder every tick.
pub fn select_rows(
    mut groups: Vec<ThreadGroup>,
    means: &HashMap<String, f64>,
    top: usize,
) -> Vec<ThreadGroup> {
    let rank = |group: &ThreadGroup| means.get(&group.name).copied().unwrap_or(group.on_cpu);
    groups.sort_by(|a, b| {
        rank(b)
            .partial_cmp(&rank(a))
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.name.cmp(&b.name))
    });
    let rest = groups.split_off(top.min(groups.len()));
    if !rest.is_empty() {
        let count = rest
            .iter()
            .map(|group| group.count)
            .fold(0, usize::saturating_add);
        groups.push(ThreadGroup {
            name: String::new(),
            count,
            cores: None,
            on_cpu: mean_of(&rest, count, |group| group.on_cpu),
            waiting: mean_of(&rest, count, |group| group.waiting),
            other: true,
        });
    }
    groups
}

/// A share across several groups, weighted by how many threads each has, so
/// the folded row is still a mean per thread.
fn mean_of(groups: &[ThreadGroup], count: usize, value: impl Fn(&ThreadGroup) -> f64) -> f64 {
    if count == 0 {
        return 0.0;
    }
    let weighted: f64 = groups
        .iter()
        .map(|group| value(group) * group.count as f64)
        .sum();
    weighted / count as f64
}

/// Each group's mean share over the ticks it appeared in.
pub fn window_means<'a>(
    recent: impl IntoIterator<Item = &'a Vec<(String, f64)>>,
) -> HashMap<String, f64> {
    let mut sums: HashMap<String, (f64, usize)> = HashMap::new();
    for tick in recent {
        for (name, share) in tick {
            let entry = sums.entry(name.clone()).or_insert((0.0, 0));
            entry.0 += share;
            entry.1 = entry.1.saturating_add(1);
        }
    }
    sums.into_iter()
        .map(|(name, (sum, n))| (name, if n == 0 { 0.0 } else { sum / n as f64 }))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(tid: u64, name: &str, on_cpu: u64, waiting: u64) -> ThreadReading {
        ThreadReading {
            tid,
            name: name.to_string(),
            on_cpu_nanos: on_cpu,
            waiting_nanos: waiting,
        }
    }

    fn by_tid(threads: &[ThreadReading]) -> HashMap<u64, ThreadReading> {
        threads
            .iter()
            .cloned()
            .map(|thread| (thread.tid, thread))
            .collect()
    }

    const SECOND: u64 = 1_000_000_000;

    #[test]
    fn test_schedstat_is_two_clocks_and_a_count() {
        assert_eq!(
            parse_schedstat("2894374 20482 143\n"),
            Some((2_894_374, 20_482))
        );
        assert_eq!(parse_schedstat("garbage"), None);
        assert_eq!(parse_schedstat(""), None);
    }

    #[test]
    fn test_the_allowed_cores_are_one_line_of_the_status_file() {
        let status = "Name:\tsolPohTickProd\nState:\tR (running)\nCpus_allowed:\t8\nCpus_allowed_list:\t3\nMems_allowed_list:\t0\n";
        assert_eq!(parse_cpus_allowed(status).as_deref(), Some("3"));
        assert_eq!(parse_cpus_allowed("Name:\tx\n"), None);
    }

    #[test]
    fn test_a_core_list_is_counted_across_ranges() {
        assert_eq!(cores_in("0-23"), 24);
        assert_eq!(cores_in("3"), 1);
        assert_eq!(cores_in("0-3,8-11"), 8);
        assert_eq!(cores_in("0-3,8"), 5);
    }

    #[test]
    fn test_a_pool_is_named_without_its_trailing_number() {
        // What the unified scheduler names its handlers, and what PoH names
        // its one thread.
        assert_eq!(group_key("solScHandleV07"), "solScHandleV");
        assert_eq!(group_key("solPohTickProd"), "solPohTickProd");
        assert_eq!(group_key("solSigVerify3"), "solSigVerify");
    }

    #[test]
    fn test_a_group_is_the_mean_of_its_threads_over_the_second() {
        // Two handlers, one on a core for half the second and one for a
        // quarter: the pool reads three eighths.
        let before = by_tid(&[
            reading(10, "solScHandleV00", 0, 0),
            reading(11, "solScHandleV01", SECOND, 0),
        ]);
        let now = [
            reading(10, "solScHandleV00", SECOND / 2, 10_000_000),
            reading(11, "solScHandleV01", SECOND + SECOND / 4, 0),
        ];
        let groups = group_shares(&before, &now, &HashMap::new(), SECOND);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "solScHandleV");
        assert_eq!(groups[0].count, 2);
        assert!((groups[0].on_cpu - 0.375).abs() < 1e-9);
        assert!((groups[0].waiting - 0.005).abs() < 1e-9);
    }

    #[test]
    fn test_a_thread_with_no_earlier_reading_is_left_out() {
        // Its counters run from its start, not from the last tick.
        let before = by_tid(&[reading(10, "solGossip", 0, 0)]);
        let now = [
            reading(10, "solGossip", SECOND / 10, 0),
            reading(99, "solNewThread", SECOND, 0),
        ];
        let groups = group_shares(&before, &now, &HashMap::new(), SECOND);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "solGossip");
    }

    #[test]
    fn test_a_reused_thread_id_is_not_differenced_against_its_predecessor() {
        let before = by_tid(&[reading(10, "solOldThread", SECOND, 0)]);
        let now = [reading(10, "solNewThread", SECOND / 10, 0)];
        assert!(group_shares(&before, &now, &HashMap::new(), SECOND).is_empty());
    }

    #[test]
    fn test_a_share_cannot_exceed_the_second() {
        // The two clocks are read apart, so a busy thread can read a hair over.
        let before = by_tid(&[reading(1, "solPohTickProd", 0, 0)]);
        let now = [reading(1, "solPohTickProd", SECOND + 1_000, 0)];
        let groups = group_shares(&before, &now, &HashMap::new(), SECOND);
        assert_eq!(groups[0].on_cpu, 1.0);
    }

    #[test]
    fn test_pinning_is_reported_only_where_the_whole_group_shares_it() {
        let before = by_tid(&[
            reading(1, "solPohTickProd", 0, 0),
            reading(10, "solScHandleV00", 0, 0),
            reading(11, "solScHandleV01", 0, 0),
        ]);
        let now = [
            reading(1, "solPohTickProd", 1, 0),
            reading(10, "solScHandleV00", 1, 0),
            reading(11, "solScHandleV01", 1, 0),
        ];
        let pinning = HashMap::from([
            (1, Some("3".to_string())),
            (10, Some("12-22".to_string())),
            (11, None),
        ]);
        let mut groups = group_shares(&before, &now, &pinning, SECOND);
        groups.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(groups[0].name, "solPohTickProd");
        assert_eq!(groups[0].cores.as_deref(), Some("3"));
        assert_eq!(groups[1].name, "solScHandleV");
        assert_eq!(
            groups[1].cores, None,
            "one pinned and one not is not a pinned pool"
        );
    }

    fn group(name: &str, count: usize, on_cpu: f64) -> ThreadGroup {
        ThreadGroup {
            name: name.to_string(),
            count,
            cores: None,
            on_cpu,
            waiting: 0.0,
            other: false,
        }
    }

    #[test]
    fn test_the_rows_are_the_top_groups_by_their_window_mean_and_one_for_the_rest() {
        // Ranked on the window: gossip is quiet this second but was busy all
        // minute, so it keeps its row above the handler that just spiked.
        let groups = vec![
            group("solPohTickProd", 1, 0.94),
            group("solGossip", 6, 0.01),
            group("solScHandleV", 11, 0.30),
            group("solRepairSvc", 1, 0.02),
        ];
        let means = HashMap::from([
            ("solPohTickProd".to_string(), 0.95),
            ("solGossip".to_string(), 0.40),
            ("solScHandleV".to_string(), 0.08),
            ("solRepairSvc".to_string(), 0.02),
        ]);
        let rows = select_rows(groups, &means, 2);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].name, "solPohTickProd");
        assert_eq!(rows[1].name, "solGossip");
        assert!(rows[2].other);
        assert_eq!(rows[2].count, 12, "every thread not shown");
        // A mean per thread across the rest: (0.30 * 11 + 0.02 * 1) / 12.
        assert!((rows[2].on_cpu - (0.30 * 11.0 + 0.02) / 12.0).abs() < 1e-9);
    }

    #[test]
    fn test_no_folded_row_where_every_group_is_shown() {
        let rows = select_rows(
            vec![group("a", 1, 0.5), group("b", 1, 0.2)],
            &HashMap::new(),
            8,
        );
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| !row.other));
    }

    #[test]
    fn test_a_window_mean_covers_only_the_ticks_a_group_appeared_in() {
        let recent = vec![
            vec![("a".to_string(), 0.5), ("b".to_string(), 0.1)],
            vec![("a".to_string(), 0.7)],
        ];
        let means = window_means(&recent);
        assert!((means["a"] - 0.6).abs() < 1e-9);
        assert!((means["b"] - 0.1).abs() < 1e-9);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_this_process_reports_its_own_threads() {
        // The test binary has at least the thread running this test.
        let threads = read().unwrap();
        assert!(!threads.is_empty());
        assert!(threads.iter().all(|thread| !thread.name.is_empty()));
    }
}
