use {
    crate::{
        banking_stage::{
            committer::Committer, decision_maker::DecisionMaker, vote_storage::VoteStorage,
            BankingStage,
        },
        validator::{BlockProductionMethod, TransactionStructure},
    },
    agave_banking_stage_ingress_types::BankingPacketReceiver,
    solana_ledger::blockstore_processor::TransactionStatusSender,
    solana_poh::{poh_recorder::PohRecorder, transaction_recorder::TransactionRecorder},
    solana_runtime::{
        bank_forks::BankForks, prioritization_fee_cache::PrioritizationFeeCache,
        vote_sender_types::ReplayVoteSender,
    },
    std::{
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, RwLock,
        },
        thread::{self, JoinHandle},
    },
};

/// Handle to manage block production.
pub struct BlockProductionManager {
    /// Signal to shutdown vote thread.
    vote_shutdown_signal: Arc<AtomicBool>,
    /// Vote thread handle.
    vote_thread_handle: JoinHandle<()>,

    /// Signal to shutdown non-vote thread(s).
    non_vote_shutdown_signal: Arc<AtomicBool>,
    /// Non-vote thread handle(s).
    non_vote_thread_handles: Vec<JoinHandle<()>>,

    context: BlockProductionContext,
}

impl BlockProductionManager {
    pub fn with_context(context: BlockProductionContext) -> Self {
        let vote_shutdown_signal = Arc::new(AtomicBool::new(false));
        let non_vote_shutdown_signal = Arc::new(AtomicBool::new(false));

        let vote_thread_handle = Self::spawn_vote_thread(&context);

        Self {
            vote_shutdown_signal,
            vote_thread_handle,
            non_vote_shutdown_signal,
            non_vote_thread_handles: vec![],
            context,
        }
    }

    /// Perform final shutdown.
    pub fn shutdown(mut self) -> thread::Result<()> {
        self.shutdown_non_vote_threads()?;

        // Signal and wait for vote thread shutdown.
        {
            self.vote_shutdown_signal.store(true, Ordering::Relaxed);
            self.vote_thread_handle.join()?;
        }

        Ok(())
    }

    /// Shtudown and wait for non-vote threads.
    pub fn shutdown_non_vote_threads(&mut self) -> thread::Result<()> {
        self.non_vote_shutdown_signal.store(true, Ordering::Relaxed);
        for handle in self.non_vote_thread_handles.drain(..) {
            handle.join()?;
        }

        Ok(())
    }

    fn spawn_vote_thread(context: &BlockProductionContext) -> JoinHandle<()> {
        BankingStage::spawn_vote_worker(
            context.tpu_vote_receiver.clone(),
            context.gossip_vote_receiver.clone(),
            DecisionMaker::new(context.poh_recorder.clone()),
            context.bank_forks.clone(),
            Committer::new(
                context.transaction_status_sender.clone(),
                context.replay_vote_sender.clone(),
                context.prioritization_fee_cache.clone(),
            ),
            context.transaction_recorder.clone(),
            context.log_messages_bytes_limit,
            VoteStorage::new(context.bank_forks.read().unwrap().working_bank().as_ref()),
        )
    }

    fn spawn_non_vote_threads(
        &mut self,
        block_production_method: BlockProductionMethod,
        transaction_structure: TransactionStructure,
    ) {
        BankingStage::spawn_scheduler_and_workers_with(
            block_production_method,
            transaction_structure,
        )
    }
}

/// Context for creating block-production threads.
pub struct BlockProductionContext {
    poh_recorder: Arc<RwLock<PohRecorder>>,
    transaction_recorder: TransactionRecorder,
    non_vote_receiver: BankingPacketReceiver,
    tpu_vote_receiver: BankingPacketReceiver,
    gossip_vote_receiver: BankingPacketReceiver,
    transaction_status_sender: Option<TransactionStatusSender>,
    replay_vote_sender: ReplayVoteSender,
    log_messages_bytes_limit: Option<usize>,
    bank_forks: Arc<RwLock<BankForks>>,
    prioritization_fee_cache: Arc<PrioritizationFeeCache>,
}
