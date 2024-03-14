//! Arguments for controlling the number of threads allocated for various tasks

use {
    clap::Arg, solana_clap_utils::input_validators::is_within_range,
    solana_core::replay_stage::MAX_CONCURRENT_FORKS_TO_REPLAY,
    solana_rayon_threadlimit::get_max_thread_count,
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

pub fn thread_args<'a>(defaults: &DefaultThreadArgs) -> Vec<Arg<'_, 'a>> {
    // Do not let any threadpool size scale over the number of threads
    vec![
        Arg::with_name("replay_forks_threads")
            .long("replay-forks-threads")
            .takes_value(true)
            .value_name("NUMBER")
            .default_value(&defaults.replay_forks_threads)
            .validator(|num| is_within_range(num, 1..=MAX_CONCURRENT_FORKS_TO_REPLAY))
            .help("Number of threads to use for replay of blocks on different forks"),
        Arg::with_name("replay_transactions_threads")
            .long("replay-transactions-threads")
            .takes_value(true)
            .value_name("NUMBER")
            .default_value(&defaults.replay_transactions_threads)
            .validator(|num| is_within_range(num, 1..=get_max_thread_count()))
            .help("Number of threads to use for transaction replay"),
    ]
}
