#![cfg(feature = "agave-unstable-api")]
#[macro_use]
extern crate eager;
use {
    core::fmt,
    enum_iterator::Sequence,
    solana_pubkey::Pubkey,
    std::{
        collections::HashMap,
        num::Saturating,
        ops::{Index, IndexMut},
    },
};

#[derive(Default, Debug, PartialEq, Eq)]
pub struct ProgramTiming {
    pub accumulated_us: Saturating<u64>,
    pub accumulated_units: Saturating<u64>,
    pub count: Saturating<u32>,
    pub errored_txs_compute_consumed: Vec<u64>,
    // Sum of all units in `errored_txs_compute_consumed`
    pub total_errored_units: Saturating<u64>,
}

impl ProgramTiming {
    pub fn coalesce_error_timings(&mut self, current_estimated_program_cost: u64) {
        for tx_error_compute_consumed in self.errored_txs_compute_consumed.drain(..) {
            let compute_units_update =
                std::cmp::max(current_estimated_program_cost, tx_error_compute_consumed);
            self.accumulated_units += compute_units_update;
            self.count += 1;
        }
    }

    pub fn accumulate_program_timings(&mut self, other: &ProgramTiming) {
        self.accumulated_us += other.accumulated_us;
        self.accumulated_units += other.accumulated_units;
        self.count += other.count;
        // Clones the entire vector, maybe not great...
        self.errored_txs_compute_consumed
            .extend(other.errored_txs_compute_consumed.clone());
        self.total_errored_units += other.total_errored_units;
    }
}

/// Used as an index for `Metrics`.
#[derive(Debug, Sequence)]
pub enum ExecuteTimingType {
    CheckUs,
    ValidateFeesUs,
    LoadUs,
    ExecuteUs,
    StoreUs,
    UpdateStakesCacheUs,
    UpdateExecutorsUs,
    NumExecuteBatches,
    CollectLogsUs,
    TotalBatchesLen,
    UpdateTransactionStatuses,
    ProgramCacheUs,
    CheckBlockLimitsUs,
    FilterExecutableUs,
    CollectBalancesUs,
}

pub struct Metrics([Saturating<u64>; ExecuteTimingType::CARDINALITY]);

impl Index<ExecuteTimingType> for Metrics {
    type Output = Saturating<u64>;
    fn index(&self, index: ExecuteTimingType) -> &Self::Output {
        self.0.index(index as usize)
    }
}

impl IndexMut<ExecuteTimingType> for Metrics {
    fn index_mut(&mut self, index: ExecuteTimingType) -> &mut Self::Output {
        self.0.index_mut(index as usize)
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Metrics([Saturating(0); ExecuteTimingType::CARDINALITY])
    }
}

impl core::fmt::Debug for Metrics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

// The auxiliary variable that must always be provided to eager_macro_rules! must use the
// identifier `eager_1`. Macros declared with `eager_macro_rules!` can then be used inside
// an eager! block.
eager_macro_rules! { $eager_1
    #[macro_export]
    macro_rules! report_execute_timings {
        ($self: expr, $is_unified_scheduler_enabled: expr) => {
            (
                "validate_transactions_us",
                $self
                    .metrics
                    .index(ExecuteTimingType::CheckUs).0,
                i64
            ),
            (
                "validate_fees_us",
                $self
                    .metrics
                    .index(ExecuteTimingType::ValidateFeesUs).0,
                i64
            ),
            (
                "filter_executable_us",
                $self
                    .metrics
                    .index(ExecuteTimingType::FilterExecutableUs).0,
                i64
            ),
            (
                "program_cache_us",
                $self
                    .metrics
                    .index(ExecuteTimingType::ProgramCacheUs).0,
                i64
            ),
            (
                "load_us",
                $self
                    .metrics
                    .index(ExecuteTimingType::LoadUs).0,
                i64
            ),
            (
                "execute_us",
                $self
                    .metrics
                    .index(ExecuteTimingType::ExecuteUs).0,
                i64
            ),
            (
                "collect_balances_us",
                $self
                    .metrics
                    .index(ExecuteTimingType::CollectBalancesUs).0,
                i64
            ),
            (
                "collect_logs_us",
                $self
                    .metrics
                    .index(ExecuteTimingType::CollectLogsUs).0,
                i64
            ),
            (
                "store_us",
                $self

                    .metrics
                    .index(ExecuteTimingType::StoreUs).0,
                i64
            ),
            (
                "update_stakes_cache_us",
                $self

                    .metrics
                    .index(ExecuteTimingType::UpdateStakesCacheUs).0,
                i64
            ),
            (
                "execute_accessories_update_executors_us",
                $self.metrics.index(ExecuteTimingType::UpdateExecutorsUs).0,
                i64
            ),
            (
                "total_batches_len",
                (if $is_unified_scheduler_enabled {
                    None
                } else {
                    Some($self
                        .metrics
                        .index(ExecuteTimingType::TotalBatchesLen).0)
                }),
                Option<i64>
            ),
            (
                "num_execute_batches",
                (if $is_unified_scheduler_enabled {
                    None
                } else {
                    Some($self
                        .metrics
                        .index(ExecuteTimingType::NumExecuteBatches).0)
                }),
                Option<i64>
            ),
            (
                "update_transaction_statuses",
                $self
                    .metrics
                    .index(ExecuteTimingType::UpdateTransactionStatuses).0,
                i64
            ),
            (
                "check_block_limits_us",
                $self.metrics.index(ExecuteTimingType::CheckBlockLimitsUs).0,
                i64
            ),
            (
                "execute_details_serialize_us",
                $self.details.serialize_us.0,
                i64
            ),
            (
                "execute_details_create_vm_us",
                $self.details.create_vm_us.0,
                i64
            ),
            (
                "execute_details_execute_inner_us",
                $self.details.execute_us.0,
                i64
            ),
            (
                "execute_details_deserialize_us",
                $self.details.deserialize_us.0,
                i64
            ),
            (
                "execute_details_get_or_create_executor_us",
                $self.details.get_or_create_executor_us.0,
                i64
            ),
            (
                "execute_details_changed_account_count",
                $self.details.changed_account_count.0,
                i64
            ),
            (
                "execute_details_total_account_count",
                $self.details.total_account_count.0,
                i64
            ),
            (
                "execute_details_create_executor_register_syscalls_us",
                $self
                    .details
                    .create_executor_register_syscalls_us.0,
                i64
            ),
            (
                "execute_details_create_executor_load_elf_us",
                $self.details.create_executor_load_elf_us.0,
                i64
            ),
            (
                "execute_details_create_executor_verify_code_us",
                $self.details.create_executor_verify_code_us.0,
                i64
            ),
            (
                "execute_details_create_executor_jit_compile_us",
                $self.details.create_executor_jit_compile_us.0,
                i64
            ),
            (
                "execute_accessories_feature_set_clone_us",
                $self
                    .execute_accessories
                    .feature_set_clone_us.0,
                i64
            ),
            (
                "execute_accessories_get_executors_us",
                $self.execute_accessories.get_executors_us.0,
                i64
            ),
            (
                "execute_accessories_process_message_us",
                $self.execute_accessories.process_message_us.0,
                i64
            ),
            (
                "execute_accessories_process_instructions_total_us",
                $self
                    .execute_accessories
                    .process_instructions
                    .total_us.0,
                i64
            ),
            (
                "execute_accessories_process_instructions_verify_caller_us",
                $self
                    .execute_accessories
                    .process_instructions
                    .verify_caller_us.0,
                i64
            ),
            (
                "execute_accessories_process_instructions_process_executable_chain_us",
                $self
                    .execute_accessories
                    .process_instructions
                    .process_executable_chain_us.0,
                i64
            ),
            (
                "execute_accessories_process_instructions_verify_callee_us",
                $self
                    .execute_accessories
                    .process_instructions
                    .verify_callee_us.0,
                i64
            ),
        }
    }
}

#[derive(Debug, Default)]
pub struct ExecuteTimings {
    pub metrics: Metrics,
    pub details: ExecuteDetailsTimings,
    pub execute_accessories: ExecuteAccessoryTimings,
}

impl ExecuteTimings {
    pub fn accumulate(&mut self, other: &ExecuteTimings) {
        for (t1, t2) in self.metrics.0.iter_mut().zip(other.metrics.0.iter()) {
            *t1 += *t2;
        }
        self.details.accumulate(&other.details);
        self.execute_accessories
            .accumulate(&other.execute_accessories);
    }

    pub fn saturating_add_in_place(&mut self, timing_type: ExecuteTimingType, value_to_add: u64) {
        let idx = timing_type as usize;
        match self.metrics.0.get_mut(idx) {
            Some(elem) => *elem += value_to_add,
            None => debug_assert!(idx < ExecuteTimingType::CARDINALITY, "Index out of bounds"),
        }
    }
}

#[derive(Default, Debug)]
pub struct ExecuteProcessInstructionTimings {
    pub total_us: Saturating<u64>,
    pub verify_caller_us: Saturating<u64>,
    pub process_executable_chain_us: Saturating<u64>,
    pub verify_callee_us: Saturating<u64>,
}

impl ExecuteProcessInstructionTimings {
    pub fn accumulate(&mut self, other: &ExecuteProcessInstructionTimings) {
        self.total_us += other.total_us;
        self.verify_caller_us += other.verify_caller_us;
        self.process_executable_chain_us += other.process_executable_chain_us;
        self.verify_callee_us += other.verify_callee_us;
    }
}

#[derive(Default, Debug)]
pub struct ExecuteAccessoryTimings {
    pub feature_set_clone_us: Saturating<u64>,
    pub get_executors_us: Saturating<u64>,
    pub process_message_us: Saturating<u64>,
    pub process_instructions: ExecuteProcessInstructionTimings,
}

impl ExecuteAccessoryTimings {
    pub fn accumulate(&mut self, other: &ExecuteAccessoryTimings) {
        self.feature_set_clone_us += other.feature_set_clone_us;
        self.get_executors_us += other.get_executors_us;
        self.process_message_us += other.process_message_us;
        self.process_instructions
            .accumulate(&other.process_instructions);
    }
}

#[derive(Default, Debug, PartialEq, Eq)]
pub struct ExecuteDetailsTimings {
    pub serialize_us: Saturating<u64>,
    pub create_vm_us: Saturating<u64>,
    pub execute_us: Saturating<u64>,
    pub deserialize_us: Saturating<u64>,
    pub get_or_create_executor_us: Saturating<u64>,
    pub changed_account_count: Saturating<u64>,
    pub total_account_count: Saturating<u64>,
    pub create_executor_register_syscalls_us: Saturating<u64>,
    pub create_executor_load_elf_us: Saturating<u64>,
    pub create_executor_verify_code_us: Saturating<u64>,
    pub create_executor_jit_compile_us: Saturating<u64>,
    pub per_program_timings: HashMap<Pubkey, ProgramTiming>,
}

impl ExecuteDetailsTimings {
    pub fn accumulate(&mut self, other: &ExecuteDetailsTimings) {
        self.serialize_us += other.serialize_us;
        self.create_vm_us += other.create_vm_us;
        self.execute_us += other.execute_us;
        self.deserialize_us += other.deserialize_us;
        self.get_or_create_executor_us += other.get_or_create_executor_us;
        self.changed_account_count += other.changed_account_count;
        self.total_account_count += other.total_account_count;
        self.create_executor_register_syscalls_us += other.create_executor_register_syscalls_us;
        self.create_executor_load_elf_us += other.create_executor_load_elf_us;
        self.create_executor_verify_code_us += other.create_executor_verify_code_us;
        self.create_executor_jit_compile_us += other.create_executor_jit_compile_us;
        for (id, other) in &other.per_program_timings {
            let program_timing = self.per_program_timings.entry(*id).or_default();
            program_timing.accumulate_program_timings(other);
        }
    }

    pub fn accumulate_program(
        &mut self,
        program_id: &Pubkey,
        us: u64,
        compute_units_consumed: u64,
        is_error: bool,
    ) {
        let program_timing = self.per_program_timings.entry(*program_id).or_default();
        program_timing.accumulated_us += us;
        if is_error {
            program_timing
                .errored_txs_compute_consumed
                .push(compute_units_consumed);
            program_timing.total_errored_units += compute_units_consumed;
        } else {
            program_timing.accumulated_units += compute_units_consumed;
            program_timing.count += 1;
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn construct_execute_timings_with_program(
        program_id: &Pubkey,
        us: u64,
        compute_units_consumed: u64,
    ) -> ExecuteDetailsTimings {
        let mut execute_details_timings = ExecuteDetailsTimings::default();

        // Accumulate an erroring transaction
        let is_error = true;
        execute_details_timings.accumulate_program(
            program_id,
            us,
            compute_units_consumed,
            is_error,
        );

        // Accumulate a non-erroring transaction
        let is_error = false;
        execute_details_timings.accumulate_program(
            program_id,
            us,
            compute_units_consumed,
            is_error,
        );

        let program_timings = execute_details_timings
            .per_program_timings
            .get(program_id)
            .unwrap();

        // Both error and success transactions count towards `accumulated_us`
        assert_eq!(program_timings.accumulated_us.0, us.saturating_mul(2));
        assert_eq!(program_timings.accumulated_units.0, compute_units_consumed);
        assert_eq!(program_timings.count.0, 1,);
        assert_eq!(
            program_timings.errored_txs_compute_consumed,
            vec![compute_units_consumed]
        );
        assert_eq!(
            program_timings.total_errored_units.0,
            compute_units_consumed,
        );

        execute_details_timings
    }

    #[test]
    fn test_execute_details_timing_acumulate_program() {
        // Acumulate an erroring transaction
        let program_id = Pubkey::new_unique();
        let us = 100;
        let compute_units_consumed = 1;
        construct_execute_timings_with_program(&program_id, us, compute_units_consumed);
    }

    #[test]
    fn test_execute_details_timing_acumulate() {
        // Acumulate an erroring transaction
        let program_id = Pubkey::new_unique();
        let us = 100;
        let compute_units_consumed = 1;
        let mut execute_details_timings = ExecuteDetailsTimings::default();

        // Construct another separate instance of ExecuteDetailsTimings with non default fields
        let mut other_execute_details_timings =
            construct_execute_timings_with_program(&program_id, us, compute_units_consumed);
        let account_count = 1;
        other_execute_details_timings.serialize_us.0 = us;
        other_execute_details_timings.create_vm_us.0 = us;
        other_execute_details_timings.execute_us.0 = us;
        other_execute_details_timings.deserialize_us.0 = us;
        other_execute_details_timings.changed_account_count.0 = account_count;
        other_execute_details_timings.total_account_count.0 = account_count;

        // Accumulate the other instance into the current instance
        execute_details_timings.accumulate(&other_execute_details_timings);

        // Check that the two instances are equal
        assert_eq!(execute_details_timings, other_execute_details_timings);
    }

    #[test]
    fn execute_timings_saturating_add_in_place() {
        let mut timings = ExecuteTimings::default();
        timings.saturating_add_in_place(ExecuteTimingType::CheckUs, 1);
        let check_us = timings.metrics.index(ExecuteTimingType::CheckUs);
        assert_eq!(1, check_us.0);

        timings.saturating_add_in_place(ExecuteTimingType::CheckUs, 2);
        let check_us = timings.metrics.index(ExecuteTimingType::CheckUs);
        assert_eq!(3, check_us.0);
    }
}
