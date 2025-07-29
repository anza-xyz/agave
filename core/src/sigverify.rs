//! The `sigverify` module provides digital signature verification functions.
//! By default, signatures are verified in parallel using all available CPU
//! cores.  When perf-libs are available signature verification is offloaded
//! to the GPU.
//!

pub use solana_perf::sigverify::{
    count_packets_in_batches, ed25519_verify_cpu, ed25519_verify_disabled, init, TxOffset,
};
use {
    crate::{
        banking_trace::BankingPacketSender,
        sigverify_stage::{SigVerifier, SigVerifyServiceError},
    },
    agave_banking_stage_ingress_types::BankingPacketBatch,
    agave_transaction_view::transaction_view::SanitizedTransactionView,
    crossbeam_channel::Sender,
    solana_compute_budget_instruction::compute_budget_instruction_details::ComputeBudgetInstructionDetails,
    solana_fee::{FeeFeatures, SignatureCounts},
    solana_perf::{
        cuda_runtime::PinnedVec,
        packet::{PacketBatch, PacketRefMut},
        recycler::Recycler,
        sigverify,
    },
    solana_runtime::{bank::Bank, bank_forks::BankForks},
    solana_runtime_transaction::signature_details::get_precompile_signature_details,
    solana_svm::account_loader::validate_fee_payer,
    std::{
        sync::{Arc, RwLock},
        time::{Duration, Instant},
    },
};

pub struct TransactionSigVerifier {
    banking_stage_sender: BankingPacketSender,
    forward_stage_sender: Option<Sender<(BankingPacketBatch, bool)>>,
    recycler: Recycler<TxOffset>,
    recycler_out: Recycler<PinnedVec<u8>>,
    reject_non_vote: bool,

    bank_forks: Option<Arc<RwLock<BankForks>>>,
    cached_working_bank: Option<Arc<Bank>>,
    last_bank_cache_time: Instant,
}

impl TransactionSigVerifier {
    pub fn new_reject_non_vote(
        packet_sender: BankingPacketSender,
        forward_stage_sender: Option<Sender<(BankingPacketBatch, bool)>>,
        bank_forks: Option<Arc<RwLock<BankForks>>>,
    ) -> Self {
        let mut new_self = Self::new(packet_sender, forward_stage_sender, bank_forks);
        new_self.reject_non_vote = true;
        new_self
    }

    pub fn new(
        banking_stage_sender: BankingPacketSender,
        forward_stage_sender: Option<Sender<(BankingPacketBatch, bool)>>,
        bank_forks: Option<Arc<RwLock<BankForks>>>,
    ) -> Self {
        init();
        let cached_working_bank = bank_forks
            .as_ref()
            .map(|bank_forks| bank_forks.read().unwrap().working_bank());
        Self {
            banking_stage_sender,
            forward_stage_sender,
            recycler: Recycler::warmed(50, 4096),
            recycler_out: Recycler::warmed(50, 4096),
            reject_non_vote: false,
            bank_forks,
            cached_working_bank,
            last_bank_cache_time: Instant::now(),
        }
    }
}

impl SigVerifier for TransactionSigVerifier {
    type SendType = BankingPacketBatch;

    fn send_packets(
        &mut self,
        packet_batches: Vec<PacketBatch>,
    ) -> Result<(), SigVerifyServiceError<Self::SendType>> {
        let banking_packet_batch = BankingPacketBatch::new(packet_batches);
        if let Some(forward_stage_sender) = &self.forward_stage_sender {
            self.banking_stage_sender
                .send(banking_packet_batch.clone())?;
            let _ = forward_stage_sender.try_send((banking_packet_batch, self.reject_non_vote));
        } else {
            self.banking_stage_sender.send(banking_packet_batch)?;
        }

        Ok(())
    }

    fn verify_batches(
        &mut self,
        mut batches: Vec<PacketBatch>,
        valid_packets: usize,
    ) -> Vec<PacketBatch> {
        let bank_with_fee_features = self.get_cached_working_bank().map(|bank| {
            let fee_features = FeeFeatures::from(bank.feature_set.as_ref());
            (bank, fee_features)
        });
        sigverify::ed25519_verify(
            &mut batches,
            &self.recycler,
            &self.recycler_out,
            self.reject_non_vote,
            valid_packets,
            |packet| {
                if let Some((bank, fee_features)) = bank_with_fee_features.as_ref() {
                    check_packet_fee_payer(packet, bank, fee_features)
                } else {
                    true
                }
            },
        );
        batches
    }
}

impl TransactionSigVerifier {
    fn get_cached_working_bank(&mut self) -> Option<Arc<Bank>> {
        const CACHE_DURATION: Duration = Duration::from_millis(25);
        if let Some(bank_forks) = self.bank_forks.as_ref() {
            let now = Instant::now();
            if now.duration_since(self.last_bank_cache_time) > CACHE_DURATION {
                self.cached_working_bank = Some(bank_forks.read().unwrap().working_bank());
                self.last_bank_cache_time = now;
            }

            self.cached_working_bank.clone()
        } else {
            None
        }
    }
}

fn check_packet_fee_payer(packet: &PacketRefMut, bank: &Bank, fee_features: &FeeFeatures) -> bool {
    // Only here to avoid breaking tests that expect zero fees.
    if bank.get_lamports_per_signature() == 0 {
        return true;
    }

    let packet_len = packet.meta().size;
    let Some(packet_data) = packet.data(..packet_len) else {
        return false;
    };
    let Ok(view) = SanitizedTransactionView::try_new_sanitized(packet_data) else {
        return false;
    };
    let Some(total_fee) = calculate_total_fee(&view, bank, fee_features) else {
        return false;
    };
    let fee_payer = &view.static_account_keys()[0];

    let Some((mut fee_payer_account, _slot)) = bank
        .rc
        .accounts
        .accounts_db
        .load_with_fixed_root(&bank.ancestors, fee_payer)
    else {
        return false;
    };

    validate_fee_payer(
        fee_payer,
        &mut fee_payer_account,
        0,
        bank.rent_collector(),
        total_fee,
    )
    .is_ok()
}

fn calculate_total_fee(
    view: &SanitizedTransactionView<&[u8]>,
    bank: &Bank,
    fee_features: &FeeFeatures,
) -> Option<u64> {
    let compute_budget_instruction_details =
        ComputeBudgetInstructionDetails::try_from(view.program_instructions_iter()).ok()?;
    let compute_budget_limits = compute_budget_instruction_details
        .sanitize_and_convert_to_compute_budget_limits(&bank.feature_set)
        .ok()?;
    let priority_fee = u64::from(compute_budget_limits.compute_unit_limit)
        .saturating_mul(compute_budget_limits.compute_unit_price);
    let precompile_details = get_precompile_signature_details(view.program_instructions_iter());

    let signature_counts = SignatureCounts {
        num_transaction_signatures: u64::from(view.num_signatures()),
        num_ed25519_signatures: precompile_details.num_ed25519_instruction_signatures,
        num_secp256k1_signatures: precompile_details.num_secp256k1_instruction_signatures,
        num_secp256r1_signatures: precompile_details.num_secp256r1_instruction_signatures,
    };

    let signature_fee = solana_fee::calculate_signature_fee(
        signature_counts,
        bank.fee_structure().lamports_per_signature,
        fee_features.enable_secp256r1_precompile,
    );

    Some(priority_fee.saturating_add(signature_fee))
}
