//! Arguments for controlling the number of threads allocated for various tasks

use {
    clap::{value_t_or_exit, Arg, ArgMatches},
    solana_clap_utils::{hidden_unless_forced, input_validators::is_within_range},
    solana_rayon_threadlimit::get_max_thread_count,
    std::{num::NonZeroUsize, ops::RangeInclusive},
};

pub struct DefaultThreadArgs {
    pub replay_forks_threads: String,
    pub replay_transactions_threads: String,
}

impl Default for DefaultThreadArgs {
    fn default() -> Self {
        let num_total_threads = get_max_thread_count();

        Self {
            replay_forks_threads: ReplayForksThreadsArg::default().to_string(),
            replay_transactions_threads: num_total_threads.to_string(),
        }
    }
}

pub struct NumThreadConfig {
    pub replay_forks_threads: NonZeroUsize,
    pub replay_transactions_threads: NonZeroUsize,
}

pub fn thread_args<'a>(defaults: &DefaultThreadArgs) -> Vec<Arg<'_, 'a>> {
    vec![
        new_thread_arg::<ReplayForksThreadsArg>(&defaults.replay_forks_threads),
        Arg::with_name(replay_transactions_threads::NAME)
            .long(replay_transactions_threads::LONG_ARG)
            .takes_value(true)
            .value_name("NUMBER")
            .default_value(&defaults.replay_transactions_threads)
            .validator(|num| is_within_range(num, 1..=get_max_thread_count()))
            .hidden(hidden_unless_forced())
            .help(replay_transactions_threads::HELP),
    ]
}

fn new_thread_arg<'a, T: ThreadArg>(default: &str) -> Arg<'_, 'a> {
    Arg::with_name(T::NAME)
        .long(T::LONG_NAME)
        .takes_value(true)
        .value_name("NUMBER")
        .default_value(default)
        .validator(|num| is_within_range(num, T::range()))
        .hidden(hidden_unless_forced())
        .help(T::HELP)
}

pub fn parse_num_threads_args(matches: &ArgMatches) -> NumThreadConfig {
    NumThreadConfig {
        replay_forks_threads: if matches.is_present("replay_slots_concurrently") {
            NonZeroUsize::new(4).expect("4 is non-zero")
        } else {
            value_t_or_exit!(matches, ReplayForksThreadsArg::NAME, NonZeroUsize)
        },
        replay_transactions_threads: value_t_or_exit!(
            matches,
            replay_transactions_threads::NAME,
            NonZeroUsize
        ),
    }
}

/// Configuration for CLAP arguments that control the number of threads for various functions
trait ThreadArg {
    /// The argument's name
    const NAME: &'static str;
    /// The argument's long name
    const LONG_NAME: &'static str;
    /// The argument's help message
    const HELP: &'static str;

    /// The default number of threads
    fn default() -> usize;
    /// The minimum allowed value of threads (inclusive)
    fn min() -> usize {
        1
    }
    /// The maximum allowed value of threads (inclusive)
    fn max() -> usize {
        // By default, no thread pool should scale over the machine's number of threads
        get_max_thread_count()
    }
    /// The range of allowable values
    fn range() -> RangeInclusive<usize> {
        RangeInclusive::new(Self::min(), Self::max())
    }
}

struct ReplayForksThreadsArg;
impl ThreadArg for ReplayForksThreadsArg {
    const NAME: &'static str = "replay_forks_threads";
    const LONG_NAME: &'static str = "replay-forks-threads";
    const HELP: &'static str = "Number of threads to use for replay of blocks on different forks";

    fn default() -> usize {
        // Default to single threaded fork execution
        1
    }
    fn max() -> usize {
        // Choose a value that is small enough to limit the overhead of having a large thread pool
        // while also being large enough to allow replay of all active forks in most scenarios
        4
    }
}

mod replay_transactions_threads {
    pub const NAME: &str = "replay_transactions_threads";
    pub const LONG_ARG: &str = "replay-transactions-threads";
    pub const HELP: &str = "Number of threads to use for transaction replay";
}
