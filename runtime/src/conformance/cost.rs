//! Transaction cost conformance harness.

use {
    super::versioned_message_from_proto,
    protosol::protos::{
        CostContext as ProtoCostContext, CostResult as ProtoCostResult,
        SanitizedTransaction as ProtoSanitizedTransaction, TxnCostMode as ProtoTxnCostMode,
    },
    solana_cost_model::{cost_model::CostModel, transaction_cost::TransactionCost},
    solana_message::SimpleAddressLoader,
    solana_packet::PACKET_DATA_SIZE,
    solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
    solana_signature::Signature,
    solana_svm::conformance::feature_set::feature_set_from_proto,
    solana_transaction::{
        sanitized::{MessageHash, SanitizedTransaction},
        versioned::VersionedTransaction,
    },
};
#[cfg(not(test))]
use {prost::Message, std::ffi::c_int};

fn runtime_transaction_from_proto(
    proto_tx: &ProtoSanitizedTransaction,
) -> RuntimeTransaction<SanitizedTransaction> {
    let proto_message = proto_tx
        .message
        .as_ref()
        .expect("missing transaction message");
    let message = versioned_message_from_proto(proto_message);

    let mut signatures: Vec<Signature> = proto_tx
        .signatures
        .iter()
        .map(|s| {
            Signature::from(<[u8; 64]>::try_from(s.as_slice()).expect("invalid signature bytes"))
        })
        .collect();
    if signatures.is_empty() {
        signatures.push(Signature::default());
    }

    let versioned_tx = VersionedTransaction {
        signatures,
        message,
    };

    let serialized_size =
        bincode::serialized_size(&versioned_tx).expect("failed to compute serialized size");
    assert!(
        serialized_size <= PACKET_DATA_SIZE as u64,
        "transaction exceeds max packet size",
    );

    RuntimeTransaction::try_create(
        versioned_tx,
        MessageHash::Compute,
        None,
        SimpleAddressLoader::Disabled,
        &std::collections::HashSet::new(),
    )
    .expect("failed to create RuntimeTransaction")
}

fn cost_result_to_proto<Tx>(cost: &TransactionCost<'_, Tx>) -> ProtoCostResult
where
    Tx: solana_runtime_transaction::transaction_meta::TransactionMeta,
{
    ProtoCostResult {
        has_cost: true,
        signature_cost: cost.signature_cost(),
        write_lock_cost: cost.write_lock_cost(),
        data_bytes_cost: u64::from(cost.data_bytes_cost()),
        programs_execution_cost: cost.programs_execution_cost(),
        loaded_accounts_data_size_cost: cost.loaded_accounts_data_size_cost(),
        allocated_accounts_data_size: cost.allocated_accounts_data_size(),
        total_cost: cost.sum(),
    }
}

pub fn execute_cost(input: &ProtoCostContext) -> ProtoCostResult {
    let proto_tx = input
        .tx
        .as_ref()
        .expect("ProtoCostContext missing transaction");
    let runtime_tx = runtime_transaction_from_proto(proto_tx);

    let feature_set = input
        .features
        .as_ref()
        .map(feature_set_from_proto)
        .unwrap_or_default();

    let cost = if input.mode == ProtoTxnCostMode::Actual as i32 {
        CostModel::calculate_cost_for_executed_transaction(
            &runtime_tx,
            u64::from(input.actual_programs_execution_cost),
            input.actual_loaded_accounts_data_size_bytes,
            &feature_set,
        )
    } else {
        CostModel::calculate_cost(&runtime_tx, &feature_set)
    };

    cost_result_to_proto(&cost)
}

/// # Safety
///
/// `in_ptr` must point to `in_sz` initialized bytes. `out_ptr` must point
/// to a writable buffer of at least `*out_psz` bytes. On return, `*out_psz`
/// is updated to the number of bytes written.
//
// Excluded from `test` builds: the symbol would otherwise be defined both here
// and in the `path = "."` dev-dependency rlib, producing a duplicate-symbol link
// error. Tests call `execute_cost` directly.
#[cfg(not(test))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sol_compat_txn_cost_v1(
    out_ptr: *mut u8,
    out_psz: *mut u64,
    in_ptr: *mut u8,
    in_sz: u64,
) -> c_int {
    if out_ptr.is_null() || out_psz.is_null() {
        return 0;
    }

    let in_slice = if in_sz == 0 {
        &[]
    } else if in_ptr.is_null() {
        return 0;
    } else {
        unsafe { std::slice::from_raw_parts(in_ptr, in_sz as usize) }
    };

    let Ok(cost_context) = ProtoCostContext::decode(in_slice) else {
        return 0;
    };

    let cost_result = execute_cost(&cost_context);

    let out_vec = cost_result.encode_to_vec();
    let out_cap = unsafe { *out_psz } as usize;
    if out_vec.len() > out_cap {
        return 0;
    }
    let out_slice = unsafe { std::slice::from_raw_parts_mut(out_ptr, out_cap) };
    out_slice[..out_vec.len()].copy_from_slice(&out_vec);
    unsafe { *out_psz = out_vec.len() as u64 };
    1
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        agave_feature_set::FEATURE_NAMES,
        protosol::protos::{
            CompiledInstruction as ProtoCompiledInstruction, FeatureSet as ProtoFeatureSet,
            MessageHeader as ProtoMessageHeader, SanitizedTransaction as ProtoSanitizedTransaction,
            TransactionMessage as ProtoTransactionMessage,
        },
        solana_pubkey::Pubkey,
    };

    const VOTE_PROGRAM_ID: [u8; 32] = [
        7, 97, 72, 29, 53, 116, 116, 187, 124, 77, 118, 36, 235, 211, 189, 179, 216, 53, 94, 115,
        209, 16, 67, 252, 13, 163, 83, 128, 0, 0, 0, 0,
    ];
    const SYSTEM_PROGRAM_ID: [u8; 32] = [0; 32];

    fn feature_u64(pubkey: &Pubkey) -> u64 {
        let b = pubkey.to_bytes();
        u64::from_le_bytes(b[..8].try_into().unwrap())
    }

    fn all_features() -> ProtoFeatureSet {
        ProtoFeatureSet {
            features: FEATURE_NAMES.keys().map(feature_u64).collect(),
        }
    }

    fn simple_transfer_tx() -> ProtoSanitizedTransaction {
        let msg = ProtoTransactionMessage {
            is_legacy: true,
            header: Some(ProtoMessageHeader {
                num_required_signatures: 1,
                num_readonly_signed_accounts: 0,
                num_readonly_unsigned_accounts: 1,
            }),
            account_keys: vec![vec![1; 32], vec![2; 32], SYSTEM_PROGRAM_ID.to_vec()],
            recent_blockhash: vec![0; 32],
            instructions: vec![ProtoCompiledInstruction {
                program_id_index: 2,
                accounts: vec![0, 1],
                data: vec![2, 0, 0, 0],
            }],
            address_table_lookups: vec![],
        };
        ProtoSanitizedTransaction {
            message: Some(msg),
            message_hash: vec![0; 32],
            signatures: vec![vec![0; 64]],
        }
    }

    fn vote_tx() -> ProtoSanitizedTransaction {
        let msg = ProtoTransactionMessage {
            is_legacy: true,
            header: Some(ProtoMessageHeader {
                num_required_signatures: 1,
                num_readonly_signed_accounts: 0,
                num_readonly_unsigned_accounts: 1,
            }),
            account_keys: vec![vec![1; 32], vec![2; 32], VOTE_PROGRAM_ID.to_vec()],
            recent_blockhash: vec![0; 32],
            instructions: vec![ProtoCompiledInstruction {
                program_id_index: 2,
                accounts: vec![0, 1],
                data: vec![0; 16],
            }],
            address_table_lookups: vec![],
        };
        ProtoSanitizedTransaction {
            message: Some(msg),
            message_hash: vec![0; 32],
            signatures: vec![vec![0; 64]],
        }
    }

    fn estimate_context(tx: ProtoSanitizedTransaction) -> ProtoCostContext {
        ProtoCostContext {
            tx: Some(tx),
            features: Some(all_features()),
            mode: ProtoTxnCostMode::Estimate as i32,
            actual_programs_execution_cost: 0,
            actual_loaded_accounts_data_size_bytes: 0,
        }
    }

    fn assert_has_cost(ctx: &ProtoCostContext) -> ProtoCostResult {
        let result = execute_cost(ctx);
        assert!(result.has_cost, "expected has_cost to be true");
        result
    }

    #[test]
    fn test_estimate_simple_transfer() {
        let result = assert_has_cost(&estimate_context(simple_transfer_tx()));
        assert!(result.signature_cost > 0);
        assert!(result.write_lock_cost > 0);
        assert!(result.programs_execution_cost > 0);

        let component_sum = result.signature_cost
            + result.write_lock_cost
            + result.data_bytes_cost
            + result.programs_execution_cost
            + result.loaded_accounts_data_size_cost;
        assert_eq!(result.total_cost, component_sum);
    }

    #[test]
    fn test_estimate_vote() {
        let result = assert_has_cost(&estimate_context(vote_tx()));
        assert!(result.total_cost > 0);
        assert!(result.signature_cost > 0);
    }

    #[test]
    fn test_actual_mode_uses_provided_costs() {
        let exec_cost: u32 = 5000;
        let loaded_size: u32 = 65536;
        let ctx = ProtoCostContext {
            tx: Some(simple_transfer_tx()),
            features: Some(all_features()),
            mode: ProtoTxnCostMode::Actual as i32,
            actual_programs_execution_cost: exec_cost,
            actual_loaded_accounts_data_size_bytes: loaded_size,
        };
        let result = assert_has_cost(&ctx);
        assert_eq!(result.programs_execution_cost, u64::from(exec_cost));

        // Actual loaded_accounts_data_size_cost should differ from estimate.
        let estimate = assert_has_cost(&estimate_context(simple_transfer_tx()));
        assert_ne!(
            result.loaded_accounts_data_size_cost,
            estimate.loaded_accounts_data_size_cost,
        );
    }

    #[test]
    #[should_panic(expected = "ProtoCostContext missing transaction")]
    fn test_missing_transaction_panics() {
        let ctx = ProtoCostContext {
            tx: None,
            features: Some(all_features()),
            mode: ProtoTxnCostMode::Estimate as i32,
            actual_programs_execution_cost: 0,
            actual_loaded_accounts_data_size_bytes: 0,
        };
        execute_cost(&ctx);
    }

    #[test]
    #[should_panic(expected = "transaction exceeds max packet size")]
    fn test_oversized_transaction_panics() {
        let msg = ProtoTransactionMessage {
            is_legacy: true,
            header: Some(ProtoMessageHeader {
                num_required_signatures: 1,
                num_readonly_signed_accounts: 0,
                num_readonly_unsigned_accounts: 1,
            }),
            account_keys: vec![vec![1; 32], SYSTEM_PROGRAM_ID.to_vec()],
            recent_blockhash: vec![0; 32],
            instructions: vec![ProtoCompiledInstruction {
                program_id_index: 1,
                accounts: vec![0],
                data: vec![0u8; 1300],
            }],
            address_table_lookups: vec![],
        };
        let tx = ProtoSanitizedTransaction {
            message: Some(msg),
            message_hash: vec![0; 32],
            signatures: vec![vec![0; 64]],
        };
        let ctx = estimate_context(tx);
        execute_cost(&ctx);
    }
}
