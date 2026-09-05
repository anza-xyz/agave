//! Shared harness for the cryptographic-syscall CU pricing benches.
//!
//! Each bench target starts with:
//!
//! ```ignore
//! #[macro_use]
//! mod common;
//! use common::*;
//! ```

#![allow(dead_code)]

pub use {
    criterion::{measurement::WallTime, BenchmarkGroup, Criterion, Throughput},
    solana_account::AccountSharedData,
    solana_program_runtime::with_mock_invoke_context_with_feature_set,
    solana_pubkey::Pubkey,
    solana_sbpf::{
        memory_region::{MemoryMapping, MemoryRegion},
        program::{BuiltinFunctionDefinition, SBPFVersion},
        vm::{Config, ContextObject},
    },
    solana_sdk_ids::{bpf_loader, native_loader},
    solana_svm_feature_set::SVMFeatureSet,
};
use std::{mem, slice, time::Duration};

pub mod bls12_381;
pub mod bn254;

/// On-VM layout of `VmSlice<u8>`: a pointer and a length, both u64.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VmSliceRaw {
    pub ptr: u64,
    pub len: u64,
}

/// Base virtual address of the `slot`-th memory region. Regions are placed one
/// 4 GiB apart so they can never overlap regardless of how big you make them.
pub const fn va(slot: u64) -> u64 {
    (slot + 1) << 32
}

// Same helpers the crate's own test module defines. They live in `mod tests`,
// so benches need their own copies.

pub fn bytes_of<T>(val: &T) -> *const [u8] {
    core::ptr::slice_from_raw_parts(slice::from_ref(val).as_ptr().cast(), mem::size_of::<T>())
}

pub fn bytes_of_mut<T>(val: &mut T) -> *mut [u8] {
    core::ptr::slice_from_raw_parts_mut(
        slice::from_mut(val).as_mut_ptr().cast(),
        mem::size_of::<T>(),
    )
}

pub fn bytes_of_slice<T>(val: &[T]) -> *const [u8] {
    core::ptr::slice_from_raw_parts(
        val.as_ptr().cast(),
        val.len().wrapping_mul(mem::size_of::<T>()),
    )
}

pub fn bytes_of_slice_mut<T>(val: &mut [T]) -> *mut [u8] {
    core::ptr::slice_from_raw_parts_mut(
        val.as_mut_ptr().cast(),
        val.len().wrapping_mul(mem::size_of::<T>()),
    )
}

/// Uniform criterion settings across every syscall bench, so numbers from
/// different files are comparable.
pub fn configure(group: &mut BenchmarkGroup<'_, WallTime>) {
    group
        .warm_up_time(Duration::from_secs(3))
        .measurement_time(Duration::from_secs(10));
}

/// Build an `InvokeContext` with a pushed instruction frame.
///
/// This has to be a macro, not a function: `with_mock_invoke_context*!`
/// declares `transaction_context` as a local and `InvokeContext` borrows it,
/// so there is no way to return one out of a function.
macro_rules! prepare_mockup {
    ($invoke_context:ident, $features:expr $(,)?) => {
        let loader_key = bpf_loader::id();
        let program_key = Pubkey::new_unique();
        let transaction_accounts = vec![
            (
                loader_key,
                AccountSharedData::new(0, 0, &native_loader::id()),
            ),
            (program_key, AccountSharedData::new(0, 0, &loader_key)),
        ];
        let feature_set = $features;
        let feature_set = &feature_set;
        with_mock_invoke_context_with_feature_set!(
            $invoke_context,
            transaction_context,
            feature_set,
            transaction_accounts
        );
        $invoke_context
            .transaction_context
            .configure_top_level_instruction_for_tests(1, vec![], vec![])
            .unwrap();
        $invoke_context.push().unwrap();
    };
}

/// Run the syscall once against a huge budget and return the CU it charged.
///
/// Also doubles as an input validator: if your test vector is malformed the
/// syscall returns a non-zero status via the cheap error path, and you would
/// otherwise silently benchmark the rejection instead of the crypto.
macro_rules! charged_cu {
    ($invoke_context:expr, $call:expr $(,)?) => {{
        const PROBE_BUDGET: u64 = 1 << 48;
        $invoke_context
            .compute_meter
            .mock_set_remaining(PROBE_BUDGET);
        let status = ($call).expect("syscall returned Err during CU probe");
        assert_eq!(status, 0, "syscall returned a failure status - check your test vector");
        PROBE_BUDGET - ContextObject::get_remaining(&$invoke_context)
    }};
}
