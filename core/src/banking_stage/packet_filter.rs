use {
    super::immutable_deserialized_packet::ImmutableDeserializedPacket,
    agave_feature_set::FeatureSet,
    lazy_static::lazy_static,
    solana_builtins_default_costs::get_builtin_instruction_cost,
    solana_sdk::{ed25519_program, saturating_add_assign, secp256k1_program},
    solana_sdk_ids::secp256r1_program,
    thiserror::Error,
};

pub const MAX_ALLOWED_PRECOMPILE_SIGNATURES: u64 = 8;

lazy_static! {
    // To calculate the static_builtin_cost_sum conservatively, an all-enabled dummy feature_set
    // is used. It lowers required minimal compute_unit_limit, aligns with future versions.
    static ref FEATURE_SET: FeatureSet = FeatureSet::all_enabled();
}

#[derive(Debug, Error, PartialEq)]
pub enum PacketFilterFailure {
    #[error("Insufficient compute unit limit")]
    InsufficientComputeLimit,
    #[error("Excessive precompile usage")]
    ExcessivePrecompiles,
}

impl ImmutableDeserializedPacket {
    /// Returns ok if the transaction's compute unit limit is at least as
    /// large as the sum of the static builtins' costs.
    /// This is a simple sanity check so the leader can discard transactions
    /// which are statically known to exceed the compute budget, and will
    /// result in no useful state-change.
    pub fn check_insufficent_compute_unit_limit(&self) -> Result<(), PacketFilterFailure> {
        let mut static_builtin_cost_sum: u64 = 0;
        for (program_id, _) in self.transaction().get_message().program_instructions_iter() {
            if let Some(ix_cost) = get_builtin_instruction_cost(program_id, &FEATURE_SET) {
                saturating_add_assign!(static_builtin_cost_sum, ix_cost);
            }
        }

        if self.compute_unit_limit() >= static_builtin_cost_sum {
            Ok(())
        } else {
            Err(PacketFilterFailure::InsufficientComputeLimit)
        }
    }

    /// Returns ok if the number of precompile signature verifications
    /// performed by the transaction is not excessive.
    pub fn check_excessive_precompiles(&self) -> Result<(), PacketFilterFailure> {
        let mut num_precompile_signatures: u64 = 0;
        for (program_id, ix) in self.transaction().get_message().program_instructions_iter() {
            if secp256k1_program::check_id(program_id)
                || ed25519_program::check_id(program_id)
                || secp256r1_program::check_id(program_id)
            {
                let num_signatures = ix.data.first().map_or(0, |byte| u64::from(*byte));
                saturating_add_assign!(num_precompile_signatures, num_signatures);
            }
        }

        if num_precompile_signatures <= MAX_ALLOWED_PRECOMPILE_SIGNATURES {
            Ok(())
        } else {
            Err(PacketFilterFailure::ExcessivePrecompiles)
        }
    }
}
