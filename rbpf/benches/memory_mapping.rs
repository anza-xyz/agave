// Copyright 2020 Solana Maintainers <maintainers@solana.com>
//
// Licensed under the Apache License, Version 2.0 <http://www.apache.org/licenses/LICENSE-2.0> or
// the MIT license <http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

#![allow(clippy::arithmetic_side_effects)]
#![feature(test)]

extern crate rand;
extern crate solana_rbpf;
extern crate test;

use {
    rand::{rngs::SmallRng, Rng, SeedableRng},
    solana_rbpf::{
        memory_region::{
            AccessType, AlignedMemoryMapping, MemoryRegion, MemoryState, UnalignedMemoryMapping,
        },
        program::SBPFVersion,
        vm::Config,
    },
    test::Bencher,
};

fn generate_memory_regions(
    entries: usize,
    state: MemoryState,
    mut prng: Option<&mut SmallRng>,
) -> (Vec<MemoryRegion>, u64) {
    let mut memory_regions = Vec::with_capacity(entries);
    let mut offset = 0x100000000;
    for _ in 0..entries {
        let length = match &mut prng {
            Some(prng) => (*prng).gen::<u8>() as u64 + 4,
            None => 4,
        };
        let content = vec![0; length as usize];
        memory_regions.push(MemoryRegion::new_for_testing(
            &content[..],
            offset,
            0,
            state,
        ));
        offset += 0x100000000;
    }
    (memory_regions, offset)
}

macro_rules! new_prng {
    ( ) => {
        SmallRng::from_seed([0; 32])
    };
}

#[bench]
fn bench_prng(bencher: &mut Bencher) {
    let mut prng = new_prng!();
    bencher.iter(|| prng.gen::<u64>());
}

macro_rules! bench_gapped_randomized_access_with_1024_entries {
    (do_bench, $name:ident, $mem:tt) => {
        #[bench]
        fn $name(bencher: &mut Bencher) {
            let frame_size: u64 = 2;
            let frame_count: u64 = 1024;
            let content = vec![0; (frame_size * frame_count * 2) as usize];
            bencher
                .bench(|bencher| {
                    let memory_regions = vec![MemoryRegion::new_for_testing(
                        &content[..],
                        0x100000000,
                        frame_size,
                        MemoryState::Readable,
                    )];
                    let config = Config::default();
                    let memory_mapping =
                        $mem::new(memory_regions, &config, &SBPFVersion::V2).unwrap();
                    let mut prng = new_prng!();
                    bencher.iter(|| {
                        assert!(memory_mapping
                            .map(
                                AccessType::Load,
                                0x100000000 + (prng.gen::<u64>() % frame_count * (frame_size * 2)),
                                1,
                            )
                            .is_ok());
                    });
                    Ok(())
                })
                .unwrap();
        }
    };
    () => {
        bench_gapped_randomized_access_with_1024_entries!(
            do_bench,
            bench_gapped_randomized_access_with_1024_entries_aligned,
            AlignedMemoryMapping
        );
        bench_gapped_randomized_access_with_1024_entries!(
            do_bench,
            bench_gapped_randomized_access_with_1024_entries_unaligned,
            UnalignedMemoryMapping
        );
    };
}
bench_gapped_randomized_access_with_1024_entries!();

macro_rules! bench_randomized_access_with_0001_entry {
    (do_bench, $name:ident, $mem:tt) => {
        #[bench]
        fn $name(bencher: &mut Bencher) {
            let content = vec![0; 1024 * 2];
            let memory_regions = vec![MemoryRegion::new_readonly(&content[..], 0x100000000)];
            let config = Config::default();
            let memory_mapping = $mem::new(memory_regions, &config, &SBPFVersion::V2).unwrap();
            let mut prng = new_prng!();
            bencher.iter(|| {
                let _ = memory_mapping.map(
                    AccessType::Load,
                    0x100000000 + (prng.gen::<u64>() % content.len() as u64),
                    1,
                );
            });
        }
    };
    () => {
        bench_randomized_access_with_0001_entry!(
            do_bench,
            bench_randomized_access_with_0001_entry_aligned,
            AlignedMemoryMapping
        );
        bench_randomized_access_with_0001_entry!(
            do_bench,
            bench_randomized_access_with_0001_entry_unaligned,
            UnalignedMemoryMapping
        );
    };
}
bench_randomized_access_with_0001_entry!();

macro_rules! bench_randomized_access_with_n_entries {
    (do_bench, $name:ident, $mem:tt, $n:expr) => {
        #[bench]
        fn $name(bencher: &mut Bencher) {
            let mut prng = new_prng!();
            let (memory_regions, end_address) =
                generate_memory_regions($n, MemoryState::Readable, Some(&mut prng));
            let config = Config::default();
            let memory_mapping = $mem::new(memory_regions, &config, &SBPFVersion::V2).unwrap();
            bencher.iter(|| {
                let _ = memory_mapping.map(
                    AccessType::Load,
                    0x100000000 + (prng.gen::<u64>() % end_address),
                    1,
                );
            });
        }
    };
    ($n:expr, $aligned:ident, $unaligned:ident) => {
        bench_randomized_access_with_n_entries!(do_bench, $aligned, AlignedMemoryMapping, $n);
        bench_randomized_access_with_n_entries!(do_bench, $unaligned, UnalignedMemoryMapping, $n);
    };
}
bench_randomized_access_with_n_entries!(
    4,
    bench_randomized_access_with_0004_entries_aligned,
    bench_randomized_access_with_0004_entries_unaligned
);
bench_randomized_access_with_n_entries!(
    16,
    bench_randomized_access_with_0016_entries_aligned,
    bench_randomized_access_with_0016_entries_unaligned
);
bench_randomized_access_with_n_entries!(
    64,
    bench_randomized_access_with_0064_entries_aligned,
    bench_randomized_access_with_0064_entries_unaligned
);
bench_randomized_access_with_n_entries!(
    256,
    bench_randomized_access_with_0256_entries_aligned,
    bench_randomized_access_with_0256_entries_unaligned
);
bench_randomized_access_with_n_entries!(
    1024,
    bench_randomized_access_with_1024_entries_aligned,
    bench_randomized_access_with_1024_entries_unaligned
);

macro_rules! bench_randomized_mapping_with_n_entries {
    (do_bench, $name:ident, $mem:tt, $n:expr) => {
        #[bench]
        fn $name(bencher: &mut Bencher) {
            let mut prng = new_prng!();
            let (memory_regions, _end_address) =
                generate_memory_regions($n, MemoryState::Readable, Some(&mut prng));
            let config = Config::default();
            let memory_mapping = $mem::new(memory_regions, &config, &SBPFVersion::V2).unwrap();
            bencher.iter(|| {
                let _ = memory_mapping.map(AccessType::Load, 0x100000000, 1);
            });
        }
    };
    ($n:expr, $aligned:ident, $unaligned:ident) => {
        bench_randomized_mapping_with_n_entries!(do_bench, $aligned, AlignedMemoryMapping, $n);
        bench_randomized_mapping_with_n_entries!(do_bench, $unaligned, UnalignedMemoryMapping, $n);
    };
}
bench_randomized_mapping_with_n_entries!(
    1,
    bench_randomized_mapping_with_0001_entries_aligned,
    bench_randomized_mapping_with_0001_entries_unaligned
);
bench_randomized_mapping_with_n_entries!(
    4,
    bench_randomized_mapping_with_0004_entries_aligned,
    bench_randomized_mapping_with_0004_entries_unaligned
);
bench_randomized_mapping_with_n_entries!(
    16,
    bench_randomized_mapping_with_0016_entries_aligned,
    bench_randomized_mapping_with_0016_entries_unaligned
);
bench_randomized_mapping_with_n_entries!(
    64,
    bench_randomized_mapping_with_0064_entries_aligned,
    bench_randomized_mapping_with_0064_entries_unaligned
);
bench_randomized_mapping_with_n_entries!(
    256,
    bench_randomized_mapping_with_0256_entries_aligned,
    bench_randomized_mapping_with_0256_entries_unaligned
);
bench_randomized_mapping_with_n_entries!(
    1024,
    bench_randomized_mapping_with_1024_entries_aligned,
    bench_randomized_mapping_with_1024_entries_unaligned
);

macro_rules! bench_mapping_with_n_entries {
    (do_bench, $name:ident, $mem:tt, $n:expr) => {
        #[bench]
        fn $name(bencher: &mut Bencher) {
            let (memory_regions, _end_address) =
                generate_memory_regions($n, MemoryState::Readable, None);
            let config = Config::default();
            let memory_mapping = $mem::new(memory_regions, &config, &SBPFVersion::V2).unwrap();
            bencher.iter(|| {
                let _ = memory_mapping.map(AccessType::Load, 0x100000000, 1);
            });
        }
    };
    ($n:expr, $aligned:ident, $unaligned:ident) => {
        bench_mapping_with_n_entries!(do_bench, $aligned, AlignedMemoryMapping, $n);
        bench_mapping_with_n_entries!(do_bench, $unaligned, UnalignedMemoryMapping, $n);
    };
}
bench_mapping_with_n_entries!(
    1,
    bench_mapping_with_001_entries_aligned,
    bench_mapping_with_001_entries_unaligned
);
bench_mapping_with_n_entries!(
    4,
    bench_mapping_with_004_entries_aligned,
    bench_mapping_with_004_entries_unaligned
);
bench_mapping_with_n_entries!(
    16,
    bench_mapping_with_0016_entries_aligned,
    bench_mapping_with_0016_entries_unaligned
);
bench_mapping_with_n_entries!(
    64,
    bench_mapping_with_0064_entries_aligned,
    bench_mapping_with_0064_entries_unaligned
);
bench_mapping_with_n_entries!(
    256,
    bench_mapping_with_0256_entries_aligned,
    bench_mapping_with_0256_entries_unaligned
);
bench_mapping_with_n_entries!(
    1024,
    bench_mapping_with_1024_entries_aligned,
    bench_mapping_with_1024_entries_unaligned
);

enum MemoryOperation {
    Map,
    Load,
    Store(u64),
}

fn do_bench_mapping_operation(bencher: &mut Bencher, op: MemoryOperation, vm_addr: u64) {
    let mut mem1 = vec![0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18];
    let mut mem2 = vec![0x22; 1];
    let config = Config::default();
    let memory_mapping = UnalignedMemoryMapping::new(
        vec![
            MemoryRegion::new_writable(&mut mem1, 0x100000000),
            MemoryRegion::new_writable(&mut mem2, 0x100000000 + 8),
        ],
        &config,
        &SBPFVersion::V2,
    )
    .unwrap();

    match op {
        MemoryOperation::Map => bencher.iter(|| {
            let _ = memory_mapping.map(AccessType::Load, vm_addr, 8).unwrap();
        }),
        MemoryOperation::Load => bencher.iter(|| {
            let _ = memory_mapping.load::<u64>(vm_addr).unwrap();
        }),
        MemoryOperation::Store(val) => bencher.iter(|| {
            let _ = memory_mapping.store(val, vm_addr).unwrap();
        }),
    }
}

#[bench]
fn bench_mapping_8_byte_map(bencher: &mut Bencher) {
    do_bench_mapping_operation(bencher, MemoryOperation::Map, 0x100000000)
}

#[bench]
fn bench_mapping_8_byte_load(bencher: &mut Bencher) {
    do_bench_mapping_operation(bencher, MemoryOperation::Load, 0x100000000)
}

#[bench]
fn bench_mapping_8_byte_load_non_contiguous(bencher: &mut Bencher) {
    do_bench_mapping_operation(bencher, MemoryOperation::Load, 0x100000001)
}

#[bench]
fn bench_mapping_8_byte_store(bencher: &mut Bencher) {
    do_bench_mapping_operation(bencher, MemoryOperation::Store(42), 0x100000000)
}

#[bench]
fn bench_mapping_8_byte_store_non_contiguous(bencher: &mut Bencher) {
    do_bench_mapping_operation(bencher, MemoryOperation::Store(42), 0x100000001)
}
