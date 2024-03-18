//! Arguments for controlling the number of threads allocated for various tasks

use {
    clap::{value_t_or_exit, Arg, ArgMatches},
    solana_clap_utils::{hidden_unless_forced, input_validators::is_within_range},
    solana_core::replay_stage::MAX_CONCURRENT_FORKS_TO_REPLAY,
    solana_rayon_threadlimit::get_max_thread_count,
    std::num::NonZeroUsize,
};

pub struct DefaultThreadArgs {
    pub replay_forks_threads: String,
    pub replay_transactions_threads: String,
}

impl Default for DefaultThreadArgs {
    fn default() -> Self {
        let num_total_threads = get_max_thread_count();

        Self {
            replay_forks_threads: 1.to_string(),
            replay_transactions_threads: num_total_threads.to_string(),
        }
    }
}

pub struct NumThreadConfig {
    pub replay_forks_threads: NonZeroUsize,
    pub replay_transactions_threads: NonZeroUsize,
}

pub fn thread_args<'a>(defaults: &DefaultThreadArgs) -> Vec<Arg<'_, 'a>> {
    // Do not let any threadpool size scale over the number of threads
    vec![
        Arg::with_name("replay_forks_threads")
            .long("replay-forks-threads")
            .takes_value(true)
            .value_name("NUMBER")
            .default_value(&defaults.replay_forks_threads)
            .validator(|num| is_within_range(num, 1..=MAX_CONCURRENT_FORKS_TO_REPLAY))
            .hidden(hidden_unless_forced())
            .help("Number of threads to use for replay of blocks on different forks"),
        Arg::with_name("replay_transactions_threads")
            .long("replay-transactions-threads")
            .takes_value(true)
            .value_name("NUMBER")
            .default_value(&defaults.replay_transactions_threads)
            .validator(|num| is_within_range(num, 1..=get_max_thread_count()))
            .hidden(hidden_unless_forced())
            .help("Number of threads to use for transaction replay"),
    ]
}

pub fn parse_num_threads_args(matches: &ArgMatches) -> NumThreadConfig {
    NumThreadConfig {
        replay_forks_threads: if matches.is_present("replay_slots_concurrently") {
            NonZeroUsize::new(4).expect("4 is non-zero")
        } else {
            value_t_or_exit!(matches, "replay_forks_threads", NonZeroUsize)
        },
        replay_transactions_threads: value_t_or_exit!(
            matches,
            "replay_transactions_threads",
            NonZeroUsize
        ),
    }
}
