//! Arguments for controlling the number of threads allocated for various tasks

use {
    clap::{value_t_or_exit, Arg, ArgMatches},
    solana_accounts_db::{accounts_db, accounts_index},
    solana_clap_utils::{hidden_unless_forced, input_validators::is_within_range},
    solana_core::banking_stage::BankingStage,
    solana_rayon_threadlimit::get_thread_count,
    std::{num::NonZeroUsize, ops::RangeInclusive},
};

// Need this struct to provide &str whose lifetime matches that of the CLAP Arg's
pub struct DefaultThreadArgs {
    pub accounts_db_background_threads: String,
    pub accounts_db_foreground_threads: String,
    pub accounts_index_flush_threads: String,
    pub block_production_num_workers: String,
    pub ip_echo_server_threads: String,
    pub rayon_global_threads: String,
    pub replay_forks_threads: String,
    pub replay_transactions_threads: String,
    pub tpu_transaction_forward_receive_threads: String,
    pub tpu_transaction_receive_threads: String,
    pub tpu_vote_transaction_receive_threads: String,
    pub tvu_receive_threads: String,
    pub tvu_retransmit_threads: String,
    pub tvu_sigverify_threads: String,
}

impl Default for DefaultThreadArgs {
    fn default() -> Self {
        Self {
            accounts_db_background_threads: AccountsDbBackgroundThreadsArg::bounded_default()
                .to_string(),
            accounts_db_foreground_threads: AccountsDbForegroundThreadsArg::bounded_default()
                .to_string(),
            accounts_index_flush_threads: AccountsIndexFlushThreadsArg::bounded_default()
                .to_string(),
            block_production_num_workers: BankingStage::default_num_workers().to_string(),
            ip_echo_server_threads: IpEchoServerThreadsArg::bounded_default().to_string(),
            rayon_global_threads: RayonGlobalThreadsArg::bounded_default().to_string(),
            replay_forks_threads: ReplayForksThreadsArg::bounded_default().to_string(),
            replay_transactions_threads: ReplayTransactionsThreadsArg::bounded_default()
                .to_string(),
            tpu_transaction_forward_receive_threads:
                TpuTransactionForwardReceiveThreadArgs::bounded_default().to_string(),
            tpu_transaction_receive_threads: TpuTransactionReceiveThreads::bounded_default()
                .to_string(),
            tpu_vote_transaction_receive_threads:
                TpuVoteTransactionReceiveThreads::bounded_default().to_string(),
            tvu_receive_threads: TvuReceiveThreadsArg::bounded_default().to_string(),
            tvu_retransmit_threads: TvuRetransmitThreadsArg::bounded_default().to_string(),
            tvu_sigverify_threads: TvuShredSigverifyThreadsArg::bounded_default().to_string(),
        }
    }
}

pub fn thread_args<'a>(defaults: &DefaultThreadArgs) -> Vec<Arg<'_, 'a>> {
    vec![
        new_thread_arg::<AccountsDbBackgroundThreadsArg>(&defaults.accounts_db_background_threads),
        new_thread_arg::<AccountsDbForegroundThreadsArg>(&defaults.accounts_db_foreground_threads),
        new_thread_arg::<AccountsIndexFlushThreadsArg>(&defaults.accounts_index_flush_threads),
        new_thread_arg::<BlockProductionNumWorkersArg>(&defaults.block_production_num_workers),
        new_thread_arg::<IpEchoServerThreadsArg>(&defaults.ip_echo_server_threads),
        new_thread_arg::<RayonGlobalThreadsArg>(&defaults.rayon_global_threads),
        new_thread_arg::<ReplayForksThreadsArg>(&defaults.replay_forks_threads),
        new_thread_arg::<ReplayTransactionsThreadsArg>(&defaults.replay_transactions_threads),
        new_thread_arg::<TpuTransactionForwardReceiveThreadArgs>(
            &defaults.tpu_transaction_forward_receive_threads,
        ),
        new_thread_arg::<TpuTransactionReceiveThreads>(&defaults.tpu_transaction_receive_threads),
        new_thread_arg::<TpuVoteTransactionReceiveThreads>(
            &defaults.tpu_vote_transaction_receive_threads,
        ),
        new_thread_arg::<TvuReceiveThreadsArg>(&defaults.tvu_receive_threads),
        new_thread_arg::<TvuRetransmitThreadsArg>(&defaults.tvu_retransmit_threads),
        new_thread_arg::<TvuShredSigverifyThreadsArg>(&defaults.tvu_sigverify_threads),
    ]
}

pub(crate) fn new_thread_arg<'a, T: ThreadArg>(default: &str) -> Arg<'_, 'a> {
    Arg::with_name(T::NAME)
        .long(T::LONG_NAME)
        .takes_value(true)
        .value_name("NUMBER")
        .default_value(default)
        .validator(|num| is_within_range(num, T::range()))
        .hidden(hidden_unless_forced())
        .help(T::HELP)
}

pub struct NumThreadConfig {
    pub accounts_db_background_threads: NonZeroUsize,
    pub accounts_db_foreground_threads: NonZeroUsize,
    pub accounts_index_flush_threads: NonZeroUsize,
    pub block_production_num_workers: NonZeroUsize,
    pub ip_echo_server_threads: NonZeroUsize,
    pub rayon_global_threads: NonZeroUsize,
    pub replay_forks_threads: NonZeroUsize,
    pub replay_transactions_threads: NonZeroUsize,
    pub tpu_transaction_forward_receive_threads: NonZeroUsize,
    pub tpu_transaction_receive_threads: NonZeroUsize,
    pub tpu_vote_transaction_receive_threads: NonZeroUsize,
    pub tvu_receive_threads: NonZeroUsize,
    pub tvu_retransmit_threads: NonZeroUsize,
    pub tvu_sigverify_threads: NonZeroUsize,
}

pub fn parse_num_threads_args(matches: &ArgMatches) -> NumThreadConfig {
    NumThreadConfig {
        accounts_db_background_threads: value_t_or_exit!(
            matches,
            AccountsDbBackgroundThreadsArg::NAME,
            NonZeroUsize
        ),
        accounts_db_foreground_threads: value_t_or_exit!(
            matches,
            AccountsDbForegroundThreadsArg::NAME,
            NonZeroUsize
        ),
        accounts_index_flush_threads: value_t_or_exit!(
            matches,
            AccountsIndexFlushThreadsArg::NAME,
            NonZeroUsize
        ),
        block_production_num_workers: value_t_or_exit!(
            matches,
            BlockProductionNumWorkersArg::NAME,
            NonZeroUsize
        ),
        ip_echo_server_threads: value_t_or_exit!(
            matches,
            IpEchoServerThreadsArg::NAME,
            NonZeroUsize
        ),
        rayon_global_threads: value_t_or_exit!(matches, RayonGlobalThreadsArg::NAME, NonZeroUsize),
        replay_forks_threads: value_t_or_exit!(matches, ReplayForksThreadsArg::NAME, NonZeroUsize),
        replay_transactions_threads: value_t_or_exit!(
            matches,
            ReplayTransactionsThreadsArg::NAME,
            NonZeroUsize
        ),
        tpu_transaction_forward_receive_threads: value_t_or_exit!(
            matches,
            TpuTransactionForwardReceiveThreadArgs::NAME,
            NonZeroUsize
        ),
        tpu_transaction_receive_threads: value_t_or_exit!(
            matches,
            TpuTransactionReceiveThreads::NAME,
            NonZeroUsize
        ),
        tpu_vote_transaction_receive_threads: value_t_or_exit!(
            matches,
            TpuVoteTransactionReceiveThreads::NAME,
            NonZeroUsize
        ),
        tvu_receive_threads: value_t_or_exit!(matches, TvuReceiveThreadsArg::NAME, NonZeroUsize),
        tvu_retransmit_threads: value_t_or_exit!(
            matches,
            TvuRetransmitThreadsArg::NAME,
            NonZeroUsize
        ),
        tvu_sigverify_threads: value_t_or_exit!(
            matches,
            TvuShredSigverifyThreadsArg::NAME,
            NonZeroUsize
        ),
    }
}

/// Configuration for CLAP arguments that control the number of threads for various functions
pub trait ThreadArg {
    /// The argument's name
    const NAME: &'static str;
    /// The argument's long name
    const LONG_NAME: &'static str;
    /// The argument's help message
    const HELP: &'static str;

    /// The default number of threads
    fn default() -> usize;
    /// The default number of threads, bounded by Self::max()
    /// This prevents potential CLAP issues on low core count machines where
    /// a fixed value in Self::default() could be greater than Self::max()
    fn bounded_default() -> usize {
        std::cmp::min(Self::default(), Self::max())
    }
    /// The minimum allowed number of threads (inclusive)
    fn min() -> usize {
        1
    }
    /// The maximum allowed number of threads (inclusive)
    fn max() -> usize {
        // By default, no thread pool should scale over the number of the machine's threads
        num_cpus::get()
    }
    /// The range of allowed number of threads (inclusive on both ends)
    fn range() -> RangeInclusive<usize> {
        RangeInclusive::new(Self::min(), Self::max())
    }
}

struct AccountsDbBackgroundThreadsArg;
impl ThreadArg for AccountsDbBackgroundThreadsArg {
    const NAME: &'static str = "accounts_db_background_threads";
    const LONG_NAME: &'static str = "accounts-db-background-threads";
    const HELP: &'static str = "Number of threads to use for AccountsDb background tasks";

    fn default() -> usize {
        accounts_db::quarter_thread_count()
    }
}

struct AccountsDbForegroundThreadsArg;
impl ThreadArg for AccountsDbForegroundThreadsArg {
    const NAME: &'static str = "accounts_db_foreground_threads";
    const LONG_NAME: &'static str = "accounts-db-foreground-threads";
    const HELP: &'static str =
        "Number of threads to use for AccountsDb foreground tasks, e.g. transaction processing";

    fn default() -> usize {
        accounts_db::default_num_foreground_threads()
    }
}

struct AccountsIndexFlushThreadsArg;
impl ThreadArg for AccountsIndexFlushThreadsArg {
    const NAME: &'static str = "accounts_index_flush_threads";
    const LONG_NAME: &'static str = "accounts-index-flush-threads";
    const HELP: &'static str = "Number of threads to use for flushing the accounts index";

    fn default() -> usize {
        accounts_index::default_num_flush_threads().get()
    }
}

struct BlockProductionNumWorkersArg;
impl ThreadArg for BlockProductionNumWorkersArg {
    const NAME: &'static str = "block_production_num_workers";
    const LONG_NAME: &'static str = "block-production-num-workers";
    const HELP: &'static str = "Number of worker threads to use for block production";

    fn default() -> usize {
        BankingStage::default_num_workers().get()
    }

    fn min() -> usize {
        1
    }

    fn max() -> usize {
        BankingStage::max_num_workers().get()
    }
}

struct IpEchoServerThreadsArg;
impl ThreadArg for IpEchoServerThreadsArg {
    const NAME: &'static str = "ip_echo_server_threads";
    const LONG_NAME: &'static str = "ip-echo-server-threads";
    const HELP: &'static str = "Number of threads to use for the IP echo server";

    fn default() -> usize {
        solana_net_utils::DEFAULT_IP_ECHO_SERVER_THREADS.get()
    }
    fn min() -> usize {
        solana_net_utils::MINIMUM_IP_ECHO_SERVER_THREADS.get()
    }
}

struct RayonGlobalThreadsArg;
impl ThreadArg for RayonGlobalThreadsArg {
    const NAME: &'static str = "rayon_global_threads";
    const LONG_NAME: &'static str = "rayon-global-threads";
    const HELP: &'static str = "Number of threads to use for the global rayon thread pool";

    fn default() -> usize {
        num_cpus::get()
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

struct ReplayTransactionsThreadsArg;
impl ThreadArg for ReplayTransactionsThreadsArg {
    const NAME: &'static str = "replay_transactions_threads";
    const LONG_NAME: &'static str = "replay-transactions-threads";
    const HELP: &'static str = "Number of threads to use for transaction replay";

    fn default() -> usize {
        num_cpus::get()
    }
}

struct TpuTransactionForwardReceiveThreadArgs;
impl ThreadArg for TpuTransactionForwardReceiveThreadArgs {
    const NAME: &'static str = "tpu_transaction_forward_receive_threads";
    const LONG_NAME: &'static str = "tpu-transaction-forward-receive-threads";
    const HELP: &'static str =
        "Number of threads to use for receiving transactions on the TPU forwards port";

    fn default() -> usize {
        solana_streamer::quic::default_num_tpu_transaction_forward_receive_threads()
    }
}

struct TpuTransactionReceiveThreads;
impl ThreadArg for TpuTransactionReceiveThreads {
    const NAME: &'static str = "tpu_transaction_receive_threads";
    const LONG_NAME: &'static str = "tpu-transaction-receive-threads";
    const HELP: &'static str =
        "Number of threads to use for receiving transactions on the TPU port";

    fn default() -> usize {
        solana_streamer::quic::default_num_tpu_transaction_receive_threads()
    }
}

struct TpuVoteTransactionReceiveThreads;
impl ThreadArg for TpuVoteTransactionReceiveThreads {
    const NAME: &'static str = "tpu_vote_transaction_receive_threads";
    const LONG_NAME: &'static str = "tpu-vote-transaction-receive-threads";
    const HELP: &'static str =
        "Number of threads to use for receiving transactions on the TPU vote port";

    fn default() -> usize {
        solana_streamer::quic::default_num_tpu_vote_transaction_receive_threads()
    }
}

struct TvuReceiveThreadsArg;
impl ThreadArg for TvuReceiveThreadsArg {
    const NAME: &'static str = "tvu_receive_threads";
    const LONG_NAME: &'static str = "tvu-receive-threads";
    const HELP: &'static str =
        "Number of threads (and sockets) to use for receiving shreds on the TVU port";

    fn default() -> usize {
        solana_gossip::cluster_info::DEFAULT_NUM_TVU_RECEIVE_SOCKETS.get()
    }
    fn min() -> usize {
        solana_gossip::cluster_info::MINIMUM_NUM_TVU_RECEIVE_SOCKETS.get()
    }
}

struct TvuRetransmitThreadsArg;
impl ThreadArg for TvuRetransmitThreadsArg {
    const NAME: &'static str = "tvu_retransmit_threads";
    const LONG_NAME: &'static str = "tvu-retransmit-threads";
    const HELP: &'static str = "Number of threads (and sockets) to use for retransmitting shreds";

    fn default() -> usize {
        solana_gossip::cluster_info::DEFAULT_NUM_TVU_RETRANSMIT_SOCKETS.get()
    }

    fn min() -> usize {
        solana_gossip::cluster_info::MINIMUM_NUM_TVU_RETRANSMIT_SOCKETS.get()
    }
}

struct TvuShredSigverifyThreadsArg;
impl ThreadArg for TvuShredSigverifyThreadsArg {
    const NAME: &'static str = "tvu_shred_sigverify_threads";
    const LONG_NAME: &'static str = "tvu-shred-sigverify-threads";
    const HELP: &'static str =
        "Number of threads to use for performing signature verification of received shreds";

    fn default() -> usize {
        get_thread_count()
    }
}
